use super::*;

#[derive(Clone, Copy)]
enum HostChargePreSubmissionStage<'a> {
    Unreserved,
    Prepared {
        targets: &'a dyn HostChargeTargetStore,
        reservation: &'a HostChargeReservation,
    },
}

enum HostChargePreSubmissionOutcome {
    Ready,
    Resolved(Box<HostChargePaymentResult>),
}

impl HostChargePreSubmissionOutcome {
    fn resolved(payment: HostChargePaymentResult) -> Self {
        Self::Resolved(Box::new(payment))
    }
}

impl SubscriptionBillingService {
    /// Charges one host-owned target through the canonical attempt ledger.
    ///
    /// Target eligibility and economics are supplied by the configured host
    /// extension. No transaction or target lock spans gateway I/O.
    pub async fn charge_host_target(
        &self,
        command: ChargeHostTarget,
    ) -> Result<HostChargePaymentResult, SubscriptionBillingServiceError> {
        let targets = self
            .host_charge_targets
            .as_deref()
            .ok_or(SubscriptionBillingServiceError::HostChargeUnavailable)?;
        let (snapshot, prepared_reservation) = match self
            .preflight_host_charge(targets, &command)
            .await?
        {
            HostChargePreflightOutcome::Continue(snapshot) => (snapshot, None),
            HostChargePreflightOutcome::Replay(attempt) if attempt_is_prepared(&attempt) => {
                let reservation = HostChargeReservation::from_attempt(&attempt).map_err(|_| {
                    SubscriptionBillingServiceError::InvalidState(INVALID_SERVICE_STATE)
                })?;
                (reservation.snapshot(), Some(reservation))
            }
            HostChargePreflightOutcome::Replay(attempt) => {
                return Ok(HostChargePaymentResult::new(*attempt));
            }
            HostChargePreflightOutcome::IdempotencyConflict => {
                return Err(SubscriptionBillingServiceError::IdempotencyConflict);
            }
            HostChargePreflightOutcome::Rejected { reason } => {
                return Err(SubscriptionBillingServiceError::HostChargeReservationRejected(reason));
            }
        };

        let account = self
            .gateway_account(
                command.billing_scope_id(),
                command.gateway_configuration_id(),
            )
            .await?;
        if prepared_reservation.as_ref().is_some_and(|reservation| {
            reservation.identity().gateway_account_id() != account.account_id
        }) {
            return Err(SubscriptionBillingServiceError::InvalidState(
                INVALID_SERVICE_STATE,
            ));
        }
        let cooldown_stage = prepared_reservation.as_ref().map_or(
            HostChargePreSubmissionStage::Unreserved,
            |reservation| HostChargePreSubmissionStage::Prepared {
                targets,
                reservation,
            },
        );
        match self.host_charge_cooldown(&account, cooldown_stage).await? {
            HostChargePreSubmissionOutcome::Ready => {}
            HostChargePreSubmissionOutcome::Resolved(payment) => return Ok(*payment),
        }
        let gateway = self
            .resolver
            .resolve(
                command.billing_scope_id(),
                account.account_id,
                command.gateway_configuration_id(),
                account.provider_key.clone(),
            )
            .await?;
        if gateway.billing_scope_id() != command.billing_scope_id()
            || gateway.gateway_account_id() != account.account_id
            || gateway.gateway_configuration_id() != command.gateway_configuration_id()
            || gateway.provider_key() != &account.provider_key
        {
            return Err(SubscriptionBillingServiceError::ResolvedGatewayIdentityMismatch);
        }
        if prepared_reservation.as_ref().is_some_and(|reservation| {
            reservation.identity().gateway_account_id() != gateway.gateway_account_id()
                || reservation.request().gateway_order_id()
                    != &gateway.mutation_reference_factory().for_attempt(
                        PaymentAttemptKind::HostCharge,
                        reservation.identity().attempt_id(),
                    )
        }) {
            return Err(SubscriptionBillingServiceError::InvalidState(
                INVALID_SERVICE_STATE,
            ));
        }
        if prepared_reservation.is_none() {
            match self
                .host_charge_gateway_readiness(
                    &account,
                    &gateway,
                    HostChargePreSubmissionStage::Unreserved,
                )
                .await?
            {
                HostChargePreSubmissionOutcome::Ready => {}
                HostChargePreSubmissionOutcome::Resolved(payment) => return Ok(*payment),
            }
            self.admit_subscriber_mutation(
                command.billing_scope_id(),
                command.subscriber_id(),
                EndUserMutationOperation::HostCharge,
            )
            .await?;
        }

        let candidate_id = prepared_reservation.as_ref().map_or_else(
            || PaymentAttemptId::new(uuid::Uuid::now_v7()),
            |reservation| reservation.identity().attempt_id(),
        );
        let mut reservation = match prepared_reservation {
            Some(reservation) => reservation,
            None => HostChargeReservation::from_command(&command, snapshot, &gateway, candidate_id)
                .map_err(|_| {
                    SubscriptionBillingServiceError::InvalidState(INVALID_SERVICE_STATE)
                })?,
        };
        let attempt = match self.reserve_host_charge(targets, &reservation).await? {
            HostChargeReservationOutcome::Reserved(attempt)
            | HostChargeReservationOutcome::Replay(attempt)
                if attempt_is_prepared(&attempt) =>
            {
                attempt
            }
            HostChargeReservationOutcome::Replay(attempt) => {
                return Ok(HostChargePaymentResult::new(attempt));
            }
            HostChargeReservationOutcome::IdempotencyConflict => {
                return Err(SubscriptionBillingServiceError::IdempotencyConflict);
            }
            HostChargeReservationOutcome::Rejected { reason } => {
                return Err(SubscriptionBillingServiceError::HostChargeReservationRejected(reason));
            }
            HostChargeReservationOutcome::Reserved(_) => {
                return Err(SubscriptionBillingServiceError::InvalidState(
                    INVALID_SERVICE_STATE,
                ));
            }
        };
        if attempt.identity().attempt_id() != candidate_id {
            reservation = HostChargeReservation::from_attempt(&attempt).map_err(|_| {
                SubscriptionBillingServiceError::InvalidState(INVALID_SERVICE_STATE)
            })?;
            if reservation.identity().gateway_account_id() != gateway.gateway_account_id()
                || reservation.request().gateway_order_id()
                    != &gateway.mutation_reference_factory().for_attempt(
                        PaymentAttemptKind::HostCharge,
                        reservation.identity().attempt_id(),
                    )
            {
                return Err(SubscriptionBillingServiceError::InvalidState(
                    INVALID_SERVICE_STATE,
                ));
            }
        }

        let prepared_stage = HostChargePreSubmissionStage::Prepared {
            targets,
            reservation: &reservation,
        };
        match self.host_charge_cooldown(&account, prepared_stage).await? {
            HostChargePreSubmissionOutcome::Ready => {}
            HostChargePreSubmissionOutcome::Resolved(payment) => return Ok(*payment),
        }
        match self
            .host_charge_gateway_readiness(&account, &gateway, prepared_stage)
            .await?
        {
            HostChargePreSubmissionOutcome::Ready => {}
            HostChargePreSubmissionOutcome::Resolved(payment) => return Ok(*payment),
        }

        let admission =
            match admit_host_charge_submission(&self.pool, targets, &reservation).await? {
                HostChargeAdmissionOutcome::Admitted(admission) => *admission,
                HostChargeAdmissionOutcome::AlreadyAdmitted(attempt) => {
                    return Ok(HostChargePaymentResult::new(attempt));
                }
                HostChargeAdmissionOutcome::Rejected { attempt, .. } => {
                    return Ok(HostChargePaymentResult::new(attempt));
                }
            };
        if let Some(scope) = self.active_cooldown(&account).await? {
            return self
                .resolve_host_charge_cooldown(
                    targets,
                    &reservation,
                    &account.provider_key,
                    scope,
                    HostChargeBeforeSubmissionResolution::admitted_not_submitted(),
                )
                .await;
        }
        match submit_admitted_host_charge(
            &self.pool,
            self.coordinator.as_ref(),
            targets,
            admission,
            &command,
            &gateway,
        )
        .await?
        {
            HostChargeProviderResult::Payment(payment) => Ok(payment),
            HostChargeProviderResult::NotSubmitted { payment, error } => {
                if payment.attempt().state().resolution_code()
                    == Some(crate::enrollment_application::not_submitted_resolution_code(&error))
                {
                    Err(SubscriptionBillingServiceError::GatewayNotSubmitted(error))
                } else {
                    Ok(payment)
                }
            }
        }
    }

    /// Applies an exact-query outcome to one host charge without resubmission.
    pub async fn apply_reconciled_host_charge_outcome(
        &self,
        billing_scope_id: BillingScopeId,
        attempt_id: PaymentAttemptId,
        outcome: &GatewayPaymentOutcome,
    ) -> Result<HostChargePaymentResult, SubscriptionBillingServiceError> {
        let targets = self
            .host_charge_targets
            .as_deref()
            .ok_or(SubscriptionBillingServiceError::HostChargeUnavailable)?;
        apply_reconciled_host_charge_gateway_outcome(
            &self.pool,
            self.coordinator.as_ref(),
            targets,
            billing_scope_id,
            attempt_id,
            outcome,
        )
        .await
        .map_err(Into::into)
    }

    pub(super) async fn preflight_host_charge(
        &self,
        targets: &dyn HostChargeTargetStore,
        command: &ChargeHostTarget,
    ) -> Result<HostChargePreflightOutcome, SubscriptionBillingServiceError> {
        let mut transaction = self.pool.begin().await?;
        let outcome =
            preflight_host_charge_in_transaction(&mut transaction, targets, command).await?;
        transaction.commit().await?;
        Ok(outcome)
    }

    pub(super) async fn reserve_host_charge(
        &self,
        targets: &dyn HostChargeTargetStore,
        reservation: &HostChargeReservation,
    ) -> Result<HostChargeReservationOutcome, SubscriptionBillingServiceError> {
        let mut transaction = self.pool.begin().await?;
        let outcome =
            reserve_host_charge_in_transaction(&mut transaction, targets, reservation).await?;
        transaction.commit().await?;
        Ok(outcome)
    }

    async fn host_charge_cooldown(
        &self,
        account: &GatewayAccountSnapshot,
        stage: HostChargePreSubmissionStage<'_>,
    ) -> Result<HostChargePreSubmissionOutcome, SubscriptionBillingServiceError> {
        if let Some(scope) = self.active_cooldown(account).await? {
            return match stage {
                HostChargePreSubmissionStage::Unreserved => {
                    Err(SubscriptionBillingServiceError::GatewayMutationCooldown { scope })
                }
                HostChargePreSubmissionStage::Prepared {
                    targets,
                    reservation,
                } => self
                    .resolve_host_charge_cooldown(
                        targets,
                        reservation,
                        &account.provider_key,
                        scope,
                        HostChargeBeforeSubmissionResolution::prepared(),
                    )
                    .await
                    .map(HostChargePreSubmissionOutcome::resolved),
            };
        }
        Ok(HostChargePreSubmissionOutcome::Ready)
    }

    async fn host_charge_gateway_readiness(
        &self,
        account: &GatewayAccountSnapshot,
        gateway: &syrup_rail::ResolvedGateway,
        stage: HostChargePreSubmissionStage<'_>,
    ) -> Result<HostChargePreSubmissionOutcome, SubscriptionBillingServiceError> {
        let readiness = gateway.account_mode().await;
        match stage {
            HostChargePreSubmissionStage::Unreserved => match readiness {
                Ok(GatewayAccountMode::Live) => Ok(HostChargePreSubmissionOutcome::Ready),
                Ok(GatewayAccountMode::Test) => Err(
                    SubscriptionBillingServiceError::GatewayReadiness(GatewayError::Configuration(
                        GatewayDiagnostic::new(LIVE_READINESS_FAILED_TEXT),
                    )),
                ),
                Err(GatewayError::RateLimited(_)) => {
                    self.extend_provider_cooldown(&account.provider_key).await?;
                    Err(SubscriptionBillingServiceError::GatewayMutationCooldown {
                        scope: GatewayMutationCooldownScope::Provider,
                    })
                }
                Err(error) => Err(SubscriptionBillingServiceError::GatewayReadiness(error)),
            },
            HostChargePreSubmissionStage::Prepared {
                targets,
                reservation,
            } => match readiness {
                Ok(GatewayAccountMode::Live) => Ok(HostChargePreSubmissionOutcome::Ready),
                Ok(GatewayAccountMode::Test) => self
                    .resolve_host_charge_readiness(
                        targets,
                        reservation,
                        GatewayDiagnostic::new(LIVE_READINESS_FAILED_TEXT),
                        PaymentResolutionCode::GatewayLiveReadinessFailedBeforeSubmission,
                        HostChargeBeforeSubmissionResolution::prepared(),
                    )
                    .await
                    .map(HostChargePreSubmissionOutcome::resolved),
                Err(GatewayError::RateLimited(detail)) => self
                    .resolve_host_charge_cooldown_with_detail(
                        targets,
                        reservation,
                        GatewayMutationCooldownScope::Provider,
                        detail,
                        HostChargeBeforeSubmissionResolution::prepared_provider_rate_limited(),
                    )
                    .await
                    .map(HostChargePreSubmissionOutcome::resolved),
                Err(error) if preserves_prepared_attempt_for_retry(&error) => {
                    Err(SubscriptionBillingServiceError::GatewayReadiness(error))
                }
                Err(error) => {
                    let code = gateway_readiness_resolution_code(&error);
                    let payment = self
                        .resolve_host_charge_readiness(
                            targets,
                            reservation,
                            error.detail().clone(),
                            code,
                            HostChargeBeforeSubmissionResolution::prepared(),
                        )
                        .await?;
                    if payment.attempt().state().resolution_code() == Some(code) {
                        Err(SubscriptionBillingServiceError::GatewayReadiness(error))
                    } else {
                        Ok(HostChargePreSubmissionOutcome::resolved(payment))
                    }
                }
            },
        }
    }

    pub(super) async fn resolve_host_charge_readiness(
        &self,
        targets: &dyn HostChargeTargetStore,
        reservation: &HostChargeReservation,
        detail: GatewayDiagnostic,
        code: PaymentResolutionCode,
        resolution: HostChargeBeforeSubmissionResolution,
    ) -> Result<HostChargePaymentResult, SubscriptionBillingServiceError> {
        resolve_host_charge_before_submission(
            &self.pool,
            targets,
            reservation,
            detail,
            code,
            resolution,
        )
        .await
        .map_err(Into::into)
    }

    pub(super) async fn resolve_host_charge_cooldown(
        &self,
        targets: &dyn HostChargeTargetStore,
        reservation: &HostChargeReservation,
        provider_key: &GatewayProviderKey,
        scope: GatewayMutationCooldownScope,
        resolution: HostChargeBeforeSubmissionResolution,
    ) -> Result<HostChargePaymentResult, SubscriptionBillingServiceError> {
        let provider_name = provider_key.as_str().to_ascii_uppercase();
        let detail = match scope {
            GatewayMutationCooldownScope::Account => GatewayDiagnostic::new(&format!(
                "{provider_name} account mutation cooldown is active."
            )),
            GatewayMutationCooldownScope::Provider => GatewayDiagnostic::new(&format!(
                "{provider_name} system provider cooldown is active."
            )),
        };
        self.resolve_host_charge_cooldown_with_detail(
            targets,
            reservation,
            scope,
            detail,
            resolution,
        )
        .await
    }

    pub(super) async fn resolve_host_charge_cooldown_with_detail(
        &self,
        targets: &dyn HostChargeTargetStore,
        reservation: &HostChargeReservation,
        scope: GatewayMutationCooldownScope,
        detail: GatewayDiagnostic,
        resolution: HostChargeBeforeSubmissionResolution,
    ) -> Result<HostChargePaymentResult, SubscriptionBillingServiceError> {
        let code = match scope {
            GatewayMutationCooldownScope::Account => {
                PaymentResolutionCode::GatewayAccountMutationCooldownBeforeSubmission
            }
            GatewayMutationCooldownScope::Provider => {
                PaymentResolutionCode::GatewayProviderRateLimitedBeforeSubmission
            }
        };
        let payment = self
            .resolve_host_charge_readiness(targets, reservation, detail, code, resolution)
            .await?;
        if payment.attempt().state().resolution_code() == Some(code) {
            Err(SubscriptionBillingServiceError::GatewayMutationCooldown { scope })
        } else {
            Ok(payment)
        }
    }
}
