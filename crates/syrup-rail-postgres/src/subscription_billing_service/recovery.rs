use super::*;

impl SubscriptionBillingService {
    /// Runs one complete subscriber-initiated recovery payment boundary.
    ///
    /// The command carries only the owner, requested plan/configuration, and
    /// memory-only token/contact. Reservation derives the exact due period,
    /// amount, subscription, and payment-state snapshot under lock.
    pub async fn recover(
        &self,
        command: RecoverSubscriptionPayment,
    ) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionBillingServiceError> {
        let prepared_attempt = match self.preflight_recovery(&command).await? {
            SubscriptionRecoveryPreflightOutcome::Continue => None,
            SubscriptionRecoveryPreflightOutcome::Replay(attempt)
                if attempt_is_prepared(&attempt) =>
            {
                Some(*attempt)
            }
            SubscriptionRecoveryPreflightOutcome::Replay(attempt) => {
                return self.payment_result(*attempt).await;
            }
            SubscriptionRecoveryPreflightOutcome::IdempotencyConflict => {
                return Err(SubscriptionBillingServiceError::IdempotencyConflict);
            }
        };

        self.admit_subscriber_mutation(
            command.billing_scope_id(),
            command.subscriber_id(),
            EndUserMutationOperation::SubscriptionRecovery,
        )
        .await?;

        let (account, gateway) = self
            .resolve_active_gateway(
                command.billing_scope_id(),
                command.gateway_configuration_id(),
            )
            .await?;

        let (reservation, attempt) = if let Some(attempt) = prepared_attempt {
            recovery_reservation_from_prepared_attempt(attempt, &gateway)?
        } else {
            match self.reserve_recovery(&command, &gateway).await? {
                SubscriptionRecoveryReservationOutcome::Reserved(reservation, attempt) => {
                    (*reservation, *attempt)
                }
                SubscriptionRecoveryReservationOutcome::Replay(attempt)
                    if attempt_is_prepared(&attempt) =>
                {
                    recovery_reservation_from_prepared_attempt(*attempt, &gateway)?
                }
                SubscriptionRecoveryReservationOutcome::Replay(attempt) => {
                    return self.payment_result(*attempt).await;
                }
                SubscriptionRecoveryReservationOutcome::IdempotencyConflict => {
                    return Err(SubscriptionBillingServiceError::IdempotencyConflict);
                }
                SubscriptionRecoveryReservationOutcome::Rejected(reason) => {
                    return Err(
                        SubscriptionBillingServiceError::RecoveryReservationRejected(reason),
                    );
                }
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

        if let Some(scope) = self.active_cooldown(&account).await? {
            return self
                .resolve_subscriber_readiness_failure(
                    SubscriberInitiatedReservation::Recovery(&reservation),
                    SubscriberReadinessFailure::Cooldown(scope),
                    OutcomeResolutionBoundary::Prepared,
                )
                .await;
        }
        if let Some(failure) = subscriber_gateway_readiness_failure(&gateway).await {
            return self
                .resolve_subscriber_readiness_failure(
                    SubscriberInitiatedReservation::Recovery(&reservation),
                    failure,
                    OutcomeResolutionBoundary::Prepared,
                )
                .await;
        }

        let admission =
            match admit_subscription_recovery_submission(&self.pool, &reservation).await? {
                SubscriptionRecoveryAdmissionOutcome::Admitted(admission) => *admission,
                SubscriptionRecoveryAdmissionOutcome::AlreadyAdmitted(attempt) => {
                    return self.payment_result(attempt).await;
                }
                SubscriptionRecoveryAdmissionOutcome::Rejected { attempt, .. } => {
                    return self.payment_result(attempt).await;
                }
            };
        if let Some(scope) = self.active_cooldown(&account).await? {
            return self
                .resolve_subscriber_readiness_failure(
                    SubscriberInitiatedReservation::Recovery(&reservation),
                    SubscriberReadinessFailure::Cooldown(scope),
                    OutcomeResolutionBoundary::AdmittedNotSubmitted,
                )
                .await;
        }
        if let Some(failure) = subscriber_gateway_readiness_failure(&gateway).await {
            return self
                .resolve_subscriber_readiness_failure(
                    SubscriberInitiatedReservation::Recovery(&reservation),
                    failure,
                    OutcomeResolutionBoundary::AdmittedNotSubmitted,
                )
                .await;
        }
        if let Some(scope) = self.active_cooldown(&account).await? {
            return self
                .resolve_subscriber_readiness_failure(
                    SubscriberInitiatedReservation::Recovery(&reservation),
                    SubscriberReadinessFailure::Cooldown(scope),
                    OutcomeResolutionBoundary::AdmittedNotSubmitted,
                )
                .await;
        }
        match submit_admitted_subscription_recovery(
            &self.pool,
            self.coordinator.as_ref(),
            admission,
            &command,
            &gateway,
        )
        .await?
        {
            SubscriptionRecoveryProviderResult::Payment(payment) => Ok(payment),
            SubscriptionRecoveryProviderResult::NotSubmitted { payment, error } => {
                preserve_concurrent_terminal_payment(payment, error)
            }
        }
    }

    pub(super) async fn preflight_recovery(
        &self,
        command: &RecoverSubscriptionPayment,
    ) -> Result<SubscriptionRecoveryPreflightOutcome, SubscriptionBillingServiceError> {
        let mut transaction = self.pool.begin().await?;
        let outcome =
            preflight_subscription_recovery_in_transaction(&mut transaction, command).await?;
        transaction.commit().await?;
        Ok(outcome)
    }

    pub(super) async fn reserve_recovery(
        &self,
        command: &RecoverSubscriptionPayment,
        gateway: &syrup_rail::ResolvedGateway,
    ) -> Result<SubscriptionRecoveryReservationOutcome, SubscriptionBillingServiceError> {
        let mut transaction = self.pool.begin().await?;
        let outcome =
            reserve_subscription_recovery_in_transaction(&mut transaction, command, gateway)
                .await?;
        transaction.commit().await?;
        Ok(outcome)
    }
}

fn recovery_reservation_from_prepared_attempt(
    attempt: PaymentAttempt,
    gateway: &syrup_rail::ResolvedGateway,
) -> Result<(SubscriptionRecoveryReservation, PaymentAttempt), SubscriptionBillingServiceError> {
    if !attempt_is_prepared(&attempt) {
        return Err(SubscriptionBillingServiceError::InvalidState(
            INVALID_SERVICE_STATE,
        ));
    }
    if !resolved_gateway_matches_attempt(gateway, &attempt) {
        return Err(SubscriptionBillingServiceError::GatewayConfigurationChanged);
    }
    let reservation =
        SubscriptionRecoveryReservation::from_attempt(&attempt, gateway.provider_key().clone())
            .map_err(|_| SubscriptionBillingServiceError::InvalidState(INVALID_SERVICE_STATE))?;
    Ok((reservation, attempt))
}
