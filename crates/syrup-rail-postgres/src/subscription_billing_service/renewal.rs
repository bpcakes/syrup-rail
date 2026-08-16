use super::*;

impl SubscriptionBillingService {
    /// Runs one complete automatic recurring-renewal boundary.
    ///
    /// Stale, future, canceled, paced, and contended work is a successful
    /// no-op. The operation never invokes end-user admission or the live offer
    /// store and never holds a database lock across provider I/O.
    pub async fn renew(
        &self,
        command: ChargeRenewal,
    ) -> Result<SubscriptionRenewalOutcome, SubscriptionBillingServiceError> {
        let Some(account) = self.renewal_gateway_account(command).await? else {
            return Ok(SubscriptionRenewalOutcome::Noop);
        };
        if self
            .active_cooldown(&account.as_gateway_snapshot())
            .await?
            .is_some()
        {
            return Ok(SubscriptionRenewalOutcome::Noop);
        }
        let gateway = self
            .resolver
            .resolve(
                command.billing_scope_id(),
                account.account_id,
                account.configuration_id,
                account.provider_key.clone(),
            )
            .await?;
        if gateway.billing_scope_id() != command.billing_scope_id()
            || gateway.gateway_account_id() != account.account_id
            || gateway.gateway_configuration_id() != account.configuration_id
            || gateway.provider_key() != &account.provider_key
        {
            return Err(SubscriptionBillingServiceError::ResolvedGatewayIdentityMismatch);
        }
        match gateway.account_mode().await {
            Ok(GatewayAccountMode::Live) => {}
            Ok(GatewayAccountMode::Test) => {
                return Err(SubscriptionBillingServiceError::GatewayReadiness(
                    GatewayError::Configuration(GatewayDiagnostic::new(LIVE_READINESS_FAILED_TEXT)),
                ));
            }
            Err(GatewayError::RateLimited(_)) => {
                self.extend_provider_cooldown(&account.provider_key).await?;
                return Ok(SubscriptionRenewalOutcome::Noop);
            }
            Err(error) => return Err(SubscriptionBillingServiceError::GatewayReadiness(error)),
        }

        let (reservation, attempt) = match self.reserve_renewal(command, &gateway).await? {
            SubscriptionRenewalReservationOutcome::Reserved(reservation, attempt) => {
                (*reservation, *attempt)
            }
            SubscriptionRenewalReservationOutcome::Rejected(
                SubscriptionRenewalReservationRejection::PaymentMethodUpdateInProgress,
            ) => {
                return Err(SubscriptionBillingServiceError::RenewalReservationRejected(
                    SubscriptionRenewalReservationRejection::PaymentMethodUpdateInProgress,
                ));
            }
            SubscriptionRenewalReservationOutcome::Rejected(
                SubscriptionRenewalReservationRejection::GatewayConfigurationChanged,
            ) => {
                return Err(SubscriptionBillingServiceError::GatewayConfigurationChanged);
            }
            SubscriptionRenewalReservationOutcome::Rejected(_) => {
                return Ok(SubscriptionRenewalOutcome::Noop);
            }
        };
        if attempt.status() != PaymentAttemptStatus::Pending
            || attempt.state().timestamps().submitted_at().is_some()
            || attempt.identity() != reservation.identity()
        {
            return Err(SubscriptionBillingServiceError::InvalidState(
                INVALID_SERVICE_STATE,
            ));
        }
        if let Some(scope) = self.active_cooldown(&account.as_gateway_snapshot()).await? {
            self.resolve_renewal_cooldown(&reservation, scope, OutcomeResolutionBoundary::Prepared)
                .await?;
            return Ok(SubscriptionRenewalOutcome::Noop);
        }
        if !self
            .renewal_readiness_open(&reservation, &gateway, OutcomeResolutionBoundary::Prepared)
            .await?
        {
            return Ok(SubscriptionRenewalOutcome::Noop);
        }

        let admission = match admit_subscription_renewal_submission(&self.pool, &reservation).await
        {
            Err(error) if is_retryable_renewal_admission_error(&error) => {
                self.resolve_renewal_readiness_failure(
                    &reservation,
                    GatewayDiagnostic::new(
                        "subscription billing state could not be locked for final admission",
                    ),
                    PaymentResolutionCode::SubscriptionRenewalRetryStateChangedBeforeCharge,
                    None,
                    OutcomeResolutionBoundary::Prepared,
                )
                .await?;
                return Ok(SubscriptionRenewalOutcome::Noop);
            }
            Err(error) => return Err(error.into()),
            Ok(outcome) => match outcome {
                SubscriptionRenewalAdmissionOutcome::Admitted(admission) => *admission,
                SubscriptionRenewalAdmissionOutcome::AlreadyAdmitted(attempt) => {
                    return self
                        .payment_result(attempt)
                        .await
                        .map(Box::new)
                        .map(SubscriptionRenewalOutcome::Payment);
                }
                SubscriptionRenewalAdmissionOutcome::Rejected { attempt, .. } => {
                    return self
                        .payment_result(attempt)
                        .await
                        .map(Box::new)
                        .map(SubscriptionRenewalOutcome::Payment);
                }
            },
        };
        if let Some(scope) = self.active_cooldown(&account.as_gateway_snapshot()).await? {
            self.resolve_renewal_cooldown(
                &reservation,
                scope,
                OutcomeResolutionBoundary::AdmittedNotSubmitted,
            )
            .await?;
            return Ok(SubscriptionRenewalOutcome::Noop);
        }
        match submit_admitted_subscription_renewal(
            &self.pool,
            self.coordinator.as_ref(),
            admission,
            &gateway,
        )
        .await?
        {
            SubscriptionRenewalProviderResult::Payment(payment) => {
                Ok(SubscriptionRenewalOutcome::Payment(Box::new(payment)))
            }
            SubscriptionRenewalProviderResult::NotSubmitted { payment, error } => {
                Ok(SubscriptionRenewalOutcome::NotSubmitted {
                    payment: Box::new(payment),
                    error,
                })
            }
        }
    }

    pub(super) async fn reserve_renewal(
        &self,
        command: ChargeRenewal,
        gateway: &syrup_rail::ResolvedGateway,
    ) -> Result<SubscriptionRenewalReservationOutcome, SubscriptionBillingServiceError> {
        let mut transaction = self.pool.begin().await?;
        let outcome =
            reserve_subscription_renewal_in_transaction(&mut transaction, command, gateway).await?;
        transaction.commit().await?;
        Ok(outcome)
    }

    pub(super) async fn renewal_gateway_account(
        &self,
        command: ChargeRenewal,
    ) -> Result<Option<RenewalGatewayAccountSnapshot>, SubscriptionBillingServiceError> {
        let mut transaction = self.pool.begin().await?;
        let row = sqlx::query_as::<_, (uuid::Uuid, uuid::Uuid, String)>(
            r#"
            SELECT accounts.id, accounts.gateway_configuration_id, accounts.provider_key
            FROM billing_subscriptions AS subscriptions
            JOIN billing_gateway_accounts AS accounts
                ON accounts.billing_scope_id = subscriptions.billing_scope_id
                AND accounts.id = subscriptions.gateway_account_id
            WHERE subscriptions.billing_scope_id = $1 AND subscriptions.id = $2
                AND subscriptions.status IN ('active', 'past_due')
                AND subscriptions.next_renewal_at = $3
                AND subscriptions.next_payment_attempt_at <= clock_timestamp()
            "#,
        )
        .bind(command.billing_scope_id().as_uuid())
        .bind(command.subscription_id().as_uuid())
        .bind(command.period_start_at())
        .fetch_optional(&mut *transaction)
        .await?;
        let Some((account_id, configuration_id, provider_key)) = row else {
            transaction.commit().await?;
            return Ok(None);
        };
        let attempt_state = crate::renewal_attempt_state(
            &mut transaction,
            command.subscription_id(),
            *command.period_start_at(),
            None,
        )
        .await
        .map_err(|error| match error {
            crate::RenewalStoreError::Sql(error) => SubscriptionBillingServiceError::Sql(error),
            crate::RenewalStoreError::MissingProviderCooldown => {
                SubscriptionBillingServiceError::InvalidState(INVALID_SERVICE_STATE)
            }
        })?;
        let now = sqlx::query_scalar("SELECT clock_timestamp()")
            .fetch_one(&mut *transaction)
            .await?;
        let payment_method_update_policy =
            LocalAttemptPolicy::for_kind(PaymentAttemptKind::SubscriptionPaymentMethodUpdate);
        let has_payment_method_update: bool = sqlx::query_scalar(
            r#"
            SELECT EXISTS (
                SELECT 1 FROM billing_payment_attempts
                WHERE subscription_id = $1
                    AND attempt_kind = 'subscription_payment_method_update'
                    AND status IN ('pending', 'unknown', 'review_required')
                    AND NOT (
                        status = ANY($2::text[]) AND submitted_at IS NULL
                        AND created_at <= clock_timestamp()
                            - ($3::bigint * interval '1 second')
                    )
            )
            "#,
        )
        .bind(command.subscription_id().as_uuid())
        .bind(LocalAttemptPolicy::expirable_status_values())
        .bind(payment_method_update_policy.stale_after_seconds())
        .fetch_one(&mut *transaction)
        .await?;
        transaction.commit().await?;
        if attempt_state.blocks_automatic_retry(now) || has_payment_method_update {
            if has_payment_method_update {
                return Err(SubscriptionBillingServiceError::RenewalReservationRejected(
                    SubscriptionRenewalReservationRejection::PaymentMethodUpdateInProgress,
                ));
            }
            return Ok(None);
        }
        Some((account_id, configuration_id, provider_key))
            .map(|(account_id, configuration_id, provider_key)| {
                Ok(RenewalGatewayAccountSnapshot {
                    account_id: GatewayAccountId::new(account_id),
                    configuration_id: syrup_rail::GatewayConfigurationId::new(configuration_id),
                    provider_key: GatewayProviderKey::new(provider_key).map_err(|_| {
                        SubscriptionBillingServiceError::InvalidState(INVALID_SERVICE_STATE)
                    })?,
                })
            })
            .transpose()
    }

    pub(super) async fn extend_provider_cooldown(
        &self,
        provider_key: &GatewayProviderKey,
    ) -> Result<(), SubscriptionBillingServiceError> {
        let result = sqlx::query(
            r#"
            UPDATE billing_gateway_provider_rate_limits
            SET rate_limited_until = GREATEST(
                    rate_limited_until,
                    clock_timestamp() + make_interval(secs => $2)
                )
            WHERE provider_key = $1
            "#,
        )
        .bind(provider_key.as_str())
        .bind(syrup_rail::RENEWAL_PROVIDER_RATE_LIMIT_RETRY_AFTER_SECONDS)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() != 1 {
            return Err(SubscriptionBillingServiceError::InvalidState(
                INVALID_SERVICE_STATE,
            ));
        }
        Ok(())
    }

    pub(super) async fn resolve_renewal_cooldown(
        &self,
        reservation: &SubscriptionRenewalReservation,
        scope: GatewayMutationCooldownScope,
        boundary: OutcomeResolutionBoundary,
    ) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionBillingServiceError> {
        let (message, code) = match scope {
            GatewayMutationCooldownScope::Account => (
                "gateway account mutation cooldown is active",
                PaymentResolutionCode::GatewayAccountMutationCooldownBeforeSubmission,
            ),
            GatewayMutationCooldownScope::Provider => (
                "gateway provider cooldown is active",
                PaymentResolutionCode::GatewayProviderRateLimitedBeforeSubmission,
            ),
        };
        self.resolve_renewal_readiness_failure(
            reservation,
            GatewayDiagnostic::new(message),
            code,
            None,
            boundary,
        )
        .await
    }

    pub(super) async fn renewal_readiness_open(
        &self,
        reservation: &SubscriptionRenewalReservation,
        gateway: &syrup_rail::ResolvedGateway,
        boundary: OutcomeResolutionBoundary,
    ) -> Result<bool, SubscriptionBillingServiceError> {
        match gateway.account_mode().await {
            Ok(GatewayAccountMode::Live) => Ok(true),
            Ok(GatewayAccountMode::Test) => {
                self.resolve_renewal_readiness_failure(
                    reservation,
                    GatewayDiagnostic::new(LIVE_READINESS_FAILED_TEXT),
                    PaymentResolutionCode::GatewayLiveReadinessFailedBeforeSubmission,
                    None,
                    boundary,
                )
                .await?;
                Ok(false)
            }
            Err(GatewayError::RateLimited(detail)) => {
                self.resolve_renewal_readiness_failure(
                    reservation,
                    detail,
                    PaymentResolutionCode::GatewayProviderRateLimitedBeforeSubmission,
                    Some(RateLimitCooldown::Provider),
                    boundary,
                )
                .await?;
                Ok(false)
            }
            Err(error) => {
                let code = gateway_readiness_resolution_code(&error);
                self.resolve_renewal_readiness_failure(
                    reservation,
                    error.detail().clone(),
                    code,
                    None,
                    boundary,
                )
                .await?;
                Ok(false)
            }
        }
    }

    pub(super) async fn resolve_renewal_readiness_failure(
        &self,
        reservation: &SubscriptionRenewalReservation,
        detail: GatewayDiagnostic,
        code: PaymentResolutionCode,
        cooldown: Option<RateLimitCooldown>,
        boundary: OutcomeResolutionBoundary,
    ) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionBillingServiceError> {
        let condition = (code != PaymentResolutionCode::GatewayProviderRateLimitedBeforeSubmission)
            .then(|| GatewayDiagnostic::new("failed"));
        let evidence = ProcessorEvidence::new(
            None,
            None,
            None,
            None,
            Some(detail),
            condition,
            GatewayPaymentDescriptor::default(),
        );
        resolve_renewal_non_approved_outcome(
            self.coordinator.as_ref(),
            reservation,
            &evidence,
            AttemptResolutionStatus::Failed,
            Some(code),
            cooldown,
            boundary,
        )
        .await
        .map_err(SubscriptionBillingServiceError::from)
    }
}
