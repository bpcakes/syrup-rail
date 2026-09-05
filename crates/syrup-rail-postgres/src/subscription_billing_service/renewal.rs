use super::*;

impl SubscriptionBillingService {
    /// Runs one complete automatic recurring-renewal boundary.
    ///
    /// Stale, future, canceled, paced, and contended work is a successful
    /// no-op. A dispatch routed to a service with the wrong durable gateway
    /// account mode fails with `GatewayConfigurationChanged`; callers must
    /// route it to the matching service rather than silently discard it. The
    /// operation never invokes end-user admission or the live offer store and
    /// never holds a database lock across provider I/O.
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
        match subscriber_gateway_readiness(&gateway, self.required_gateway_account_mode).await {
            Ok(_) => {}
            Err(SubscriberReadinessFailure::Gateway(GatewayError::RateLimited(_))) => {
                self.extend_provider_cooldown(
                    command.billing_scope_id(),
                    account.account_id,
                    &account.provider_key,
                )
                .await?;
                return Ok(SubscriptionRenewalOutcome::Noop);
            }
            Err(SubscriberReadinessFailure::Gateway(error)) => {
                return Err(SubscriptionBillingServiceError::GatewayReadiness(error));
            }
            Err(SubscriberReadinessFailure::AccountMode(_)) => {
                return Err(SubscriptionBillingServiceError::GatewayReadiness(
                    GatewayError::Configuration(gateway_account_mode_mismatch_detail()),
                ));
            }
            Err(SubscriberReadinessFailure::Cooldown(_)) => {
                return Err(SubscriptionBillingServiceError::InvalidState(
                    INVALID_SERVICE_STATE,
                ));
            }
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
                SubscriptionRenewalReservationRejection::GatewayAccountModeChanged
                | SubscriptionRenewalReservationRejection::GatewayConfigurationChanged,
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
        let Some(verified_gateway) = self
            .renewal_readiness_open(&reservation, &gateway, OutcomeResolutionBoundary::Prepared)
            .await?
        else {
            return Ok(SubscriptionRenewalOutcome::Noop);
        };
        let admission = match admit_subscription_renewal_submission(&self.pool, &reservation).await
        {
            Err(error) if is_retryable_renewal_admission_error(&error) => {
                self.resolve_renewal_non_approved(
                    &reservation,
                    GatewayDiagnostic::new(
                        "subscription billing state could not be locked for final admission",
                    ),
                    PaymentResolutionCode::SubscriptionRenewalRetryStateChangedBeforeCharge,
                    Some(GatewayDiagnostic::new("failed")),
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
            verified_gateway,
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
        let outcome = reserve_subscription_renewal_in_transaction(
            &mut transaction,
            command,
            gateway,
            self.required_gateway_account_mode,
        )
        .await?;
        transaction.commit().await?;
        Ok(outcome)
    }

    #[allow(deprecated)]
    pub(super) async fn renewal_gateway_account(
        &self,
        command: ChargeRenewal,
    ) -> Result<Option<RenewalGatewayAccountSnapshot>, SubscriptionBillingServiceError> {
        let mut transaction = self.pool.begin().await?;
        let row = sqlx::query_as::<_, (uuid::Uuid, uuid::Uuid, String, String)>(
            r#"
            SELECT accounts.id, accounts.gateway_configuration_id, accounts.provider_key,
                subscriptions.required_gateway_account_mode
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
        let Some((account_id, configuration_id, provider_key, required_mode)) = row else {
            transaction.commit().await?;
            return Ok(None);
        };
        let required_mode = required_mode
            .parse::<GatewayAccountMode>()
            .map_err(|_| SubscriptionBillingServiceError::InvalidState(INVALID_SERVICE_STATE))?;
        if required_mode != self.required_gateway_account_mode {
            transaction.commit().await?;
            return Err(SubscriptionBillingServiceError::GatewayConfigurationChanged);
        }
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
            crate::RenewalStoreError::CursorModeMismatch => {
                // `renewal_attempt_state` is an internal cursor-free lookup;
                // public caller misuse is returned by the paging API itself.
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
                    billing_scope_id: command.billing_scope_id(),
                    account_id: GatewayAccountId::new(account_id),
                    configuration_id: syrup_rail::GatewayConfigurationId::new(configuration_id),
                    provider_key: GatewayProviderKey::new(provider_key).map_err(|_| {
                        SubscriptionBillingServiceError::InvalidState(INVALID_SERVICE_STATE)
                    })?,
                })
            })
            .transpose()
    }

    pub(super) async fn resolve_renewal_cooldown(
        &self,
        reservation: &SubscriptionRenewalReservation,
        scope: GatewayMutationCooldownScope,
        boundary: OutcomeResolutionBoundary,
    ) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionBillingServiceError> {
        self.resolve_renewal_readiness_failure(
            reservation,
            SubscriberReadinessFailure::Cooldown(scope),
            boundary,
        )
        .await
    }

    async fn renewal_readiness_open<'gateway>(
        &self,
        reservation: &SubscriptionRenewalReservation,
        gateway: &'gateway syrup_rail::ResolvedGateway,
        boundary: OutcomeResolutionBoundary,
    ) -> Result<Option<ModeVerifiedGateway<'gateway>>, SubscriptionBillingServiceError> {
        match subscriber_gateway_readiness(gateway, self.required_gateway_account_mode).await {
            Ok(verified_gateway) => Ok(Some(verified_gateway)),
            Err(failure) => {
                self.resolve_renewal_readiness_failure(reservation, failure, boundary)
                    .await?;
                Ok(None)
            }
        }
    }

    pub(super) async fn extend_provider_cooldown(
        &self,
        billing_scope_id: BillingScopeId,
        gateway_account_id: GatewayAccountId,
        provider_key: &GatewayProviderKey,
    ) -> Result<(), SubscriptionBillingServiceError> {
        let mut transaction = self.pool.begin().await?;
        set_application_timeouts(&mut transaction).await?;
        match persist_bound_provider_rate_limit_cooldown(
            &mut transaction,
            billing_scope_id,
            gateway_account_id,
            provider_key,
        )
        .await?
        {
            RateLimitCooldownPersistence::Applied => transaction.commit().await?,
            RateLimitCooldownPersistence::IdentityChanged => {
                transaction.rollback().await?;
                tracing::warn!(
                    target: "syrup_rail::gateway_cooldown",
                    billing_scope_id = %billing_scope_id.as_uuid(),
                    gateway_account_id = %gateway_account_id.as_uuid(),
                    provider_key = provider_key.as_str(),
                    "skipped pre-reservation provider cooldown after the gateway account identity changed"
                );
            }
            RateLimitCooldownPersistence::MissingProviderCooldown => {
                transaction.rollback().await?;
                return Err(SubscriptionBillingServiceError::InvalidState(
                    INVALID_SERVICE_STATE,
                ));
            }
        }
        Ok(())
    }

    pub(super) async fn resolve_renewal_readiness_failure(
        &self,
        reservation: &SubscriptionRenewalReservation,
        failure: SubscriberReadinessFailure,
        boundary: OutcomeResolutionBoundary,
    ) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionBillingServiceError> {
        let policy = failure.policy();
        let code = policy.resolution_code();
        let condition = (code != PaymentResolutionCode::GatewayProviderRateLimitedBeforeSubmission)
            .then(|| GatewayDiagnostic::new("failed"));
        self.resolve_renewal_non_approved(
            reservation,
            failure.into_detail(),
            code,
            condition,
            policy.cooldown(),
            boundary,
        )
        .await
    }

    async fn resolve_renewal_non_approved(
        &self,
        reservation: &SubscriptionRenewalReservation,
        detail: GatewayDiagnostic,
        code: PaymentResolutionCode,
        condition: Option<GatewayDiagnostic>,
        cooldown: Option<RateLimitCooldown>,
        boundary: OutcomeResolutionBoundary,
    ) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionBillingServiceError> {
        let evidence = ProcessorEvidence::new(
            None,
            None,
            None,
            None,
            Some(detail),
            condition,
            GatewayPaymentDescriptor::default(),
        )
        .with_approval_evidence(syrup_rail::ProcessorApprovalEvidence::Absent);
        resolve_renewal_non_approved_outcome(
            &self.pool,
            self.coordinator.as_ref(),
            reservation,
            &evidence,
            OutcomeResolutionCommand::non_approved(
                AttemptResolutionStatus::Failed,
                Some(code),
                cooldown,
                boundary,
            ),
        )
        .await
        .map(OutcomeApplication::into_payment)
        .map_err(SubscriptionBillingServiceError::from)
    }
}
