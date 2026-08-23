use super::*;

impl SubscriptionBillingService {
    /// Runs one complete stored payment-method replacement boundary.
    pub async fn replace_payment_method(
        &self,
        command: ReplaceSubscriptionPaymentMethod,
    ) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionBillingServiceError> {
        let prepared_attempt = match self.preflight_payment_method_replacement(&command).await? {
            SubscriptionPaymentMethodReplacementPreflightOutcome::Continue => None,
            SubscriptionPaymentMethodReplacementPreflightOutcome::Replay(attempt)
                if attempt_is_prepared(&attempt) =>
            {
                Some(*attempt)
            }
            SubscriptionPaymentMethodReplacementPreflightOutcome::Replay(attempt) => {
                return self.payment_result(*attempt).await;
            }
            SubscriptionPaymentMethodReplacementPreflightOutcome::IdempotencyConflict => {
                return Err(SubscriptionBillingServiceError::IdempotencyConflict);
            }
        };
        self.admit_subscriber_mutation(
            command.billing_scope_id(),
            command.subscriber_id(),
            EndUserMutationOperation::SubscriptionPaymentMethodUpdate,
        )
        .await?;
        let (account, gateway) = self
            .resolve_active_gateway(
                command.billing_scope_id(),
                command.gateway_configuration_id(),
            )
            .await?;
        let (reservation, attempt) = if let Some(attempt) = prepared_attempt {
            payment_method_replacement_from_prepared_attempt(attempt, &gateway)?
        } else {
            match self
                .reserve_payment_method_replacement(&command, &gateway)
                .await?
            {
                SubscriptionPaymentMethodReplacementReservationOutcome::Reserved(
                    reservation,
                    attempt,
                ) => (*reservation, *attempt),
                SubscriptionPaymentMethodReplacementReservationOutcome::Replay(attempt)
                    if attempt_is_prepared(&attempt) =>
                {
                    payment_method_replacement_from_prepared_attempt(*attempt, &gateway)?
                }
                SubscriptionPaymentMethodReplacementReservationOutcome::Replay(attempt) => {
                    return self.payment_result(*attempt).await;
                }
                SubscriptionPaymentMethodReplacementReservationOutcome::IdempotencyConflict => {
                    return Err(SubscriptionBillingServiceError::IdempotencyConflict);
                }
                SubscriptionPaymentMethodReplacementReservationOutcome::Rejected(reason) => {
                    return Err(
                        SubscriptionBillingServiceError::PaymentMethodReplacementReservationRejected(
                            reason,
                        ),
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
                    SubscriberInitiatedReservation::PaymentMethodReplacement(&reservation),
                    GatewayReadinessFailure::for_cooldown(scope),
                    OutcomeResolutionBoundary::Prepared,
                )
                .await;
        }
        if let Some(failure) = gateway_readiness_failure(&gateway).await {
            return self
                .resolve_subscriber_readiness_failure(
                    SubscriberInitiatedReservation::PaymentMethodReplacement(&reservation),
                    failure,
                    OutcomeResolutionBoundary::Prepared,
                )
                .await;
        }
        let admission =
            match admit_subscription_payment_method_replacement(&self.pool, &reservation).await? {
                SubscriptionPaymentMethodReplacementAdmissionOutcome::Admitted(admission) => {
                    *admission
                }
                SubscriptionPaymentMethodReplacementAdmissionOutcome::AlreadyAdmitted(attempt) => {
                    return self.payment_result(attempt).await;
                }
                SubscriptionPaymentMethodReplacementAdmissionOutcome::Rejected {
                    attempt, ..
                } => {
                    return self.payment_result(attempt).await;
                }
            };
        if let Some(scope) = self.active_cooldown(&account).await? {
            return self
                .resolve_subscriber_readiness_failure(
                    SubscriberInitiatedReservation::PaymentMethodReplacement(&reservation),
                    GatewayReadinessFailure::for_cooldown(scope),
                    OutcomeResolutionBoundary::AdmittedNotSubmitted,
                )
                .await;
        }
        if let Some(failure) = gateway_readiness_failure(&gateway).await {
            return self
                .resolve_subscriber_readiness_failure(
                    SubscriberInitiatedReservation::PaymentMethodReplacement(&reservation),
                    failure,
                    OutcomeResolutionBoundary::AdmittedNotSubmitted,
                )
                .await;
        }
        if let Some(scope) = self.active_cooldown(&account).await? {
            return self
                .resolve_subscriber_readiness_failure(
                    SubscriberInitiatedReservation::PaymentMethodReplacement(&reservation),
                    GatewayReadinessFailure::for_cooldown(scope),
                    OutcomeResolutionBoundary::AdmittedNotSubmitted,
                )
                .await;
        }
        match submit_admitted_subscription_payment_method_replacement(
            &self.pool,
            self.coordinator.as_ref(),
            admission,
            &command,
            &gateway,
        )
        .await?
        {
            SubscriptionPaymentMethodReplacementProviderResult::Payment(payment) => Ok(payment),
            SubscriptionPaymentMethodReplacementProviderResult::NotSubmitted { payment, error } => {
                preserve_concurrent_terminal_payment(payment, error)
            }
        }
    }

    pub(super) async fn preflight_payment_method_replacement(
        &self,
        command: &ReplaceSubscriptionPaymentMethod,
    ) -> Result<SubscriptionPaymentMethodReplacementPreflightOutcome, SubscriptionBillingServiceError>
    {
        let mut transaction = self.pool.begin().await?;
        let outcome = preflight_subscription_payment_method_replacement_in_transaction(
            &mut transaction,
            command,
        )
        .await?;
        transaction.commit().await?;
        Ok(outcome)
    }

    pub(super) async fn reserve_payment_method_replacement(
        &self,
        command: &ReplaceSubscriptionPaymentMethod,
        gateway: &syrup_rail::ResolvedGateway,
    ) -> Result<
        SubscriptionPaymentMethodReplacementReservationOutcome,
        SubscriptionBillingServiceError,
    > {
        let mut transaction = self.pool.begin().await?;
        let outcome = reserve_subscription_payment_method_replacement_in_transaction(
            &mut transaction,
            command,
            gateway,
        )
        .await?;
        transaction.commit().await?;
        Ok(outcome)
    }
}

fn payment_method_replacement_from_prepared_attempt(
    attempt: PaymentAttempt,
    gateway: &syrup_rail::ResolvedGateway,
) -> Result<(SubscriptionPaymentMethodReplacement, PaymentAttempt), SubscriptionBillingServiceError>
{
    if !attempt_is_prepared(&attempt) {
        return Err(SubscriptionBillingServiceError::InvalidState(
            INVALID_SERVICE_STATE,
        ));
    }
    if !resolved_gateway_matches_attempt(gateway, &attempt) {
        return Err(SubscriptionBillingServiceError::GatewayConfigurationChanged);
    }
    let reservation = SubscriptionPaymentMethodReplacement::from_attempt(
        &attempt,
        gateway.provider_key().clone(),
    )
    .map_err(|_| SubscriptionBillingServiceError::InvalidState(INVALID_SERVICE_STATE))?;
    Ok((reservation, attempt))
}
