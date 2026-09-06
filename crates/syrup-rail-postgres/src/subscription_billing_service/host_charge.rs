use super::*;

#[derive(Clone, Copy)]
enum HostChargePreSubmissionStage<'a> {
    Unreserved,
    Reserved {
        targets: &'a dyn HostChargeTargetStore,
        reservation: &'a HostChargeReservation,
        resolution: HostChargeBeforeSubmissionResolution,
    },
}

enum HostChargePreSubmissionOutcome<T = ()> {
    Ready(T),
    Resolved(Box<HostChargePaymentResult>),
}

#[derive(Clone, Copy)]
struct HostChargeCooldownSurface {
    scope: GatewayMutationCooldownScope,
    code: PaymentResolutionCode,
}

impl<T> HostChargePreSubmissionOutcome<T> {
    fn resolved(payment: HostChargePaymentResult) -> Self {
        Self::Resolved(Box::new(payment))
    }
}

fn host_charge_payment_result(
    attempt: PaymentAttempt,
) -> Result<HostChargePaymentResult, SubscriptionBillingServiceError> {
    HostChargePaymentResult::new(attempt)
        .map_err(|_| SubscriptionBillingServiceError::InvalidState(INVALID_SERVICE_STATE))
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
                return host_charge_payment_result(*attempt);
            }
            HostChargePreflightOutcome::IdempotencyConflict => {
                return Err(SubscriptionBillingServiceError::IdempotencyConflict);
            }
            HostChargePreflightOutcome::Rejected { reason } => {
                return Err(SubscriptionBillingServiceError::HostChargeReservationRejected(reason));
            }
        };
        if prepared_reservation.as_ref().is_some_and(|reservation| {
            reservation.identity().required_gateway_account_mode()
                != self.required_gateway_account_mode
        }) {
            return Err(SubscriptionBillingServiceError::GatewayConfigurationChanged);
        }

        let account = self
            .gateway_account(
                command.billing_scope_id(),
                command.gateway_configuration_id(),
            )
            .await?;
        if prepared_reservation.as_ref().is_some_and(|reservation| {
            reservation.identity().gateway_account_id() != account.account_id()
        }) {
            return Err(SubscriptionBillingServiceError::InvalidState(
                INVALID_SERVICE_STATE,
            ));
        }
        let cooldown_stage = prepared_reservation.as_ref().map_or(
            HostChargePreSubmissionStage::Unreserved,
            |reservation| HostChargePreSubmissionStage::Reserved {
                targets,
                reservation,
                resolution: HostChargeBeforeSubmissionResolution::prepared(),
            },
        );
        match self.host_charge_cooldown(&account, cooldown_stage).await? {
            HostChargePreSubmissionOutcome::Ready(()) => {}
            HostChargePreSubmissionOutcome::Resolved(payment) => return Ok(*payment),
        }
        let gateway = self
            .resolver
            .resolve(
                command.billing_scope_id(),
                account.account_id(),
                command.gateway_configuration_id(),
                account.provider_key().clone(),
            )
            .await?;
        if gateway.billing_scope_id() != command.billing_scope_id()
            || gateway.gateway_account_id() != account.account_id()
            || gateway.gateway_configuration_id() != command.gateway_configuration_id()
            || gateway.provider_key() != account.provider_key()
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
        let verified_gateway = if let Some(reservation) = prepared_reservation.as_ref() {
            match self
                .host_charge_prepared_gateway_readiness(&gateway, targets, reservation)
                .await?
            {
                HostChargePreSubmissionOutcome::Ready(verified_gateway) => verified_gateway,
                HostChargePreSubmissionOutcome::Resolved(payment) => return Ok(*payment),
            }
        } else {
            self.host_charge_unreserved_gateway_readiness(&account, &gateway)
                .await?
        };
        if prepared_reservation.is_none() {
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
            None => HostChargeReservation::from_command(
                &command,
                snapshot,
                &gateway,
                candidate_id,
                self.required_gateway_account_mode,
            )
            .map_err(|_| SubscriptionBillingServiceError::InvalidState(INVALID_SERVICE_STATE))?,
        };
        let attempt = match self.reserve_host_charge(targets, &reservation).await? {
            HostChargeReservationOutcome::Reserved(attempt)
            | HostChargeReservationOutcome::Replay(attempt)
                if attempt_is_prepared(&attempt) =>
            {
                attempt
            }
            HostChargeReservationOutcome::Replay(attempt) => {
                return host_charge_payment_result(attempt);
            }
            HostChargeReservationOutcome::IdempotencyConflict => {
                return Err(SubscriptionBillingServiceError::IdempotencyConflict);
            }
            HostChargeReservationOutcome::GatewayAccountModeChanged => {
                return Err(SubscriptionBillingServiceError::GatewayConfigurationChanged);
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
            if reservation.identity().required_gateway_account_mode()
                != self.required_gateway_account_mode
            {
                return Err(SubscriptionBillingServiceError::GatewayConfigurationChanged);
            }
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

        let prepared_stage = HostChargePreSubmissionStage::Reserved {
            targets,
            reservation: &reservation,
            resolution: HostChargeBeforeSubmissionResolution::prepared(),
        };
        match self.host_charge_cooldown(&account, prepared_stage).await? {
            HostChargePreSubmissionOutcome::Ready(()) => {}
            HostChargePreSubmissionOutcome::Resolved(payment) => return Ok(*payment),
        }

        let admission =
            match admit_host_charge_submission(&self.pool, targets, &reservation).await? {
                HostChargeAdmissionOutcome::Admitted(admission) => *admission,
                HostChargeAdmissionOutcome::AlreadyAdmitted(attempt) => {
                    return host_charge_payment_result(attempt);
                }
                HostChargeAdmissionOutcome::Rejected { attempt, .. } => {
                    return host_charge_payment_result(attempt);
                }
            };
        let admitted_stage = HostChargePreSubmissionStage::Reserved {
            targets,
            reservation: &reservation,
            resolution: HostChargeBeforeSubmissionResolution::admitted_not_submitted(),
        };
        match self.host_charge_cooldown(&account, admitted_stage).await? {
            HostChargePreSubmissionOutcome::Ready(()) => {}
            HostChargePreSubmissionOutcome::Resolved(payment) => return Ok(*payment),
        }
        match submit_admitted_host_charge(
            &self.pool,
            self.coordinator.as_ref(),
            targets,
            admission,
            &command,
            verified_gateway,
        )
        .await?
        {
            HostChargeProviderResult::Payment(payment) => Ok(payment),
            HostChargeProviderResult::NotSubmitted { error, .. } => {
                Err(SubscriptionBillingServiceError::GatewayNotSubmitted(error))
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
        let outcome = preflight_host_charge_in_transaction(
            &mut transaction,
            targets,
            command,
            self.required_gateway_account_mode,
        )
        .await?;
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
                HostChargePreSubmissionStage::Reserved {
                    targets,
                    reservation,
                    resolution,
                } => self
                    .resolve_host_charge_cooldown(
                        targets,
                        reservation,
                        account.provider_key(),
                        scope,
                        resolution,
                    )
                    .await
                    .map(HostChargePreSubmissionOutcome::resolved),
            };
        }
        Ok(HostChargePreSubmissionOutcome::Ready(()))
    }

    async fn host_charge_unreserved_gateway_readiness<'gateway>(
        &self,
        account: &GatewayAccountSnapshot,
        gateway: &'gateway syrup_rail::ResolvedGateway,
    ) -> Result<ModeVerifiedGateway<'gateway>, SubscriptionBillingServiceError> {
        match verify_gateway_account_mode(gateway, self.required_gateway_account_mode).await {
            Ok(verified_gateway) => Ok(verified_gateway),
            Err(GatewayAccountModeVerificationError::AccountModeMismatch { .. }) => {
                Err(SubscriptionBillingServiceError::GatewayReadiness(
                    GatewayError::Configuration(gateway_account_mode_mismatch_detail()),
                ))
            }
            Err(GatewayAccountModeVerificationError::Gateway(GatewayError::RateLimited(_))) => {
                self.extend_provider_cooldown(
                    gateway.billing_scope_id(),
                    gateway.gateway_account_id(),
                    account.provider_key(),
                )
                .await?;
                Err(SubscriptionBillingServiceError::GatewayMutationCooldown {
                    scope: GatewayMutationCooldownScope::Provider,
                })
            }
            Err(GatewayAccountModeVerificationError::Gateway(error)) => {
                Err(SubscriptionBillingServiceError::GatewayReadiness(error))
            }
        }
    }

    async fn host_charge_prepared_gateway_readiness<'gateway>(
        &self,
        gateway: &'gateway syrup_rail::ResolvedGateway,
        targets: &dyn HostChargeTargetStore,
        reservation: &HostChargeReservation,
    ) -> Result<
        HostChargePreSubmissionOutcome<ModeVerifiedGateway<'gateway>>,
        SubscriptionBillingServiceError,
    > {
        let readiness =
            verify_gateway_account_mode(gateway, self.required_gateway_account_mode).await;
        let resolution = HostChargeBeforeSubmissionResolution::prepared();
        match readiness {
            Ok(verified_gateway) => Ok(HostChargePreSubmissionOutcome::Ready(verified_gateway)),
            Err(GatewayAccountModeVerificationError::AccountModeMismatch { required, .. }) => {
                let detail = gateway_account_mode_mismatch_detail();
                let policy = GatewayNotSubmittedPolicy::for_account_mode_mismatch(required);
                let code = policy.resolution_code();
                let payment = self
                    .resolve_host_charge_readiness(
                        targets,
                        reservation,
                        gateway.provider_key(),
                        detail.clone(),
                        code,
                        resolution.with_not_submitted_policy(policy),
                    )
                    .await?;
                if payment.attempt().state().resolution_code() == Some(code) {
                    Err(SubscriptionBillingServiceError::GatewayReadiness(
                        GatewayError::Configuration(detail),
                    ))
                } else {
                    Ok(HostChargePreSubmissionOutcome::resolved(payment))
                }
            }
            Err(GatewayAccountModeVerificationError::Gateway(error)) => {
                let policy = GatewayNotSubmittedPolicy::for_readiness_error(&error);
                if policy.restores_prepared_attempt_when_supported() {
                    return Err(SubscriptionBillingServiceError::GatewayReadiness(error));
                }
                let resolution = resolution.with_not_submitted_policy(policy);
                if let Some(cooldown) = policy.cooldown() {
                    return self
                        .resolve_host_charge_cooldown_with_detail(
                            targets,
                            reservation,
                            gateway.provider_key(),
                            error.detail().clone(),
                            HostChargeCooldownSurface {
                                scope: GatewayMutationCooldownScope::from_rate_limit_cooldown(
                                    cooldown,
                                ),
                                code: policy.resolution_code(),
                            },
                            resolution,
                        )
                        .await
                        .map(HostChargePreSubmissionOutcome::resolved);
                }
                let code = policy.resolution_code();
                let payment = self
                    .resolve_host_charge_readiness(
                        targets,
                        reservation,
                        gateway.provider_key(),
                        error.detail().clone(),
                        code,
                        resolution,
                    )
                    .await?;
                if payment.attempt().state().resolution_code() == Some(code) {
                    Err(SubscriptionBillingServiceError::GatewayReadiness(error))
                } else {
                    Ok(HostChargePreSubmissionOutcome::resolved(payment))
                }
            }
        }
    }

    pub(super) async fn resolve_host_charge_readiness(
        &self,
        targets: &dyn HostChargeTargetStore,
        reservation: &HostChargeReservation,
        provider_key: &GatewayProviderKey,
        detail: GatewayDiagnostic,
        code: PaymentResolutionCode,
        resolution: HostChargeBeforeSubmissionResolution,
    ) -> Result<HostChargePaymentResult, SubscriptionBillingServiceError> {
        resolve_host_charge_before_submission(
            &self.pool,
            targets,
            reservation,
            provider_key,
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
        let code = match scope {
            GatewayMutationCooldownScope::Account => {
                PaymentResolutionCode::GatewayAccountMutationCooldownBeforeSubmission
            }
            GatewayMutationCooldownScope::Provider => {
                PaymentResolutionCode::GatewayProviderRateLimitedBeforeSubmission
            }
        };
        self.resolve_host_charge_cooldown_with_detail(
            targets,
            reservation,
            provider_key,
            detail,
            HostChargeCooldownSurface { scope, code },
            resolution,
        )
        .await
    }

    async fn resolve_host_charge_cooldown_with_detail(
        &self,
        targets: &dyn HostChargeTargetStore,
        reservation: &HostChargeReservation,
        provider_key: &GatewayProviderKey,
        detail: GatewayDiagnostic,
        cooldown: HostChargeCooldownSurface,
        resolution: HostChargeBeforeSubmissionResolution,
    ) -> Result<HostChargePaymentResult, SubscriptionBillingServiceError> {
        let payment = self
            .resolve_host_charge_readiness(
                targets,
                reservation,
                provider_key,
                detail,
                cooldown.code,
                resolution,
            )
            .await?;
        if payment.attempt().state().resolution_code() == Some(cooldown.code) {
            Err(SubscriptionBillingServiceError::GatewayMutationCooldown {
                scope: cooldown.scope,
            })
        } else {
            Ok(payment)
        }
    }
}
