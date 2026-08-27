use super::*;

impl SubscriptionBillingService {
    /// Runs one complete initial-subscription payment boundary.
    ///
    /// Matching replay and conflict are resolved before host admission. No
    /// database transaction or lock is held across host admission, gateway
    /// resolution, readiness I/O, or the one provider mutation.
    pub async fn enroll(
        &self,
        command: EnrollSubscription,
    ) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionBillingServiceError> {
        match self.preflight(&command).await? {
            SubscriptionEnrollmentPreflightOutcome::Continue => {}
            SubscriptionEnrollmentPreflightOutcome::Replay(attempt) => {
                return self.payment_result(*attempt).await;
            }
            SubscriptionEnrollmentPreflightOutcome::IdempotencyConflict => {
                return Err(SubscriptionBillingServiceError::IdempotencyConflict);
            }
        }

        self.admit_subscriber_mutation(
            command.billing_scope_id(),
            command.subscriber_id(),
            EndUserMutationOperation::SubscriptionInitial,
        )
        .await?;

        let (account, gateway) = self
            .resolve_active_gateway(
                command.billing_scope_id(),
                command.gateway_configuration_id(),
            )
            .await?;
        let mut reservation = SubscriptionEnrollmentReservation::from_command(
            &command,
            &gateway,
            self.required_gateway_account_mode,
        )
        .map_err(map_reservation_build_error)?;

        let attempt = match self.reserve(&reservation).await? {
            SubscriptionEnrollmentReservationOutcome::Reserved(attempt)
            | SubscriptionEnrollmentReservationOutcome::Replay(attempt)
                if attempt.status() == PaymentAttemptStatus::Pending
                    && attempt.state().timestamps().submitted_at().is_none() =>
            {
                attempt
            }
            SubscriptionEnrollmentReservationOutcome::Replay(attempt) => {
                return self.payment_result(attempt).await;
            }
            SubscriptionEnrollmentReservationOutcome::IdempotencyConflict => {
                return Err(SubscriptionBillingServiceError::IdempotencyConflict);
            }
            SubscriptionEnrollmentReservationOutcome::Rejected(
                SubscriptionEnrollmentReservationRejection::GatewayAccountModeChanged,
            ) => {
                return Err(SubscriptionBillingServiceError::GatewayConfigurationChanged);
            }
            SubscriptionEnrollmentReservationOutcome::Rejected(reason) => {
                return Err(SubscriptionBillingServiceError::ReservationRejected(reason));
            }
            SubscriptionEnrollmentReservationOutcome::Reserved(_) => {
                return Err(SubscriptionBillingServiceError::InvalidState(
                    INVALID_SERVICE_STATE,
                ));
            }
        };
        if reservation.identity().attempt_id() != attempt.identity().attempt_id() {
            reservation = SubscriptionEnrollmentReservation::from_command_for_attempt(
                &command,
                &gateway,
                attempt.identity().attempt_id(),
                self.required_gateway_account_mode,
            )
            .map_err(map_reservation_build_error)?;
        }

        if let Some(scope) = self.active_cooldown(&account).await? {
            return self
                .resolve_subscriber_readiness_failure(
                    SubscriberInitiatedReservation::Initial(&reservation),
                    SubscriberReadinessFailure::Cooldown(scope),
                    OutcomeResolutionBoundary::Prepared,
                )
                .await;
        }
        let verified_gateway = match subscriber_gateway_readiness(
            &gateway,
            self.required_gateway_account_mode,
        )
        .await
        {
            Ok(verified_gateway) => verified_gateway,
            Err(failure) => {
                return self
                    .resolve_subscriber_readiness_failure(
                        SubscriberInitiatedReservation::Initial(&reservation),
                        failure,
                        OutcomeResolutionBoundary::Prepared,
                    )
                    .await;
            }
        };

        let admission = match admit_subscription_enrollment_submission(
            &self.pool,
            self.offers.as_ref(),
            &reservation,
        )
        .await?
        {
            SubscriptionEnrollmentAdmissionOutcome::Admitted(admission) => *admission,
            SubscriptionEnrollmentAdmissionOutcome::AlreadyAdmitted(attempt) => {
                return self.payment_result(attempt).await;
            }
            SubscriptionEnrollmentAdmissionOutcome::Rejected {
                reason: SubscriptionEnrollmentSubmissionRejection::GatewayAccountModeChanged,
                ..
            } => {
                return Err(SubscriptionBillingServiceError::GatewayConfigurationChanged);
            }
            SubscriptionEnrollmentAdmissionOutcome::Rejected { reason, .. } => {
                return Err(SubscriptionBillingServiceError::SubmissionRejected(reason));
            }
        };

        if let Some(scope) = self.active_cooldown(&account).await? {
            return self
                .resolve_subscriber_readiness_failure(
                    SubscriberInitiatedReservation::Initial(&reservation),
                    SubscriberReadinessFailure::Cooldown(scope),
                    OutcomeResolutionBoundary::AdmittedNotSubmitted,
                )
                .await;
        }
        match submit_admitted_subscription_enrollment(
            &self.pool,
            self.coordinator.as_ref(),
            admission,
            &command,
            verified_gateway,
        )
        .await?
        {
            SubscriptionEnrollmentProviderResult::Payment(payment) => Ok(payment),
            SubscriptionEnrollmentProviderResult::NotSubmitted { error, .. } => {
                Err(SubscriptionBillingServiceError::GatewayNotSubmitted(error))
            }
        }
    }

    pub(super) async fn preflight(
        &self,
        command: &EnrollSubscription,
    ) -> Result<SubscriptionEnrollmentPreflightOutcome, SubscriptionBillingServiceError> {
        let mut transaction = self.pool.begin().await?;
        let outcome =
            preflight_subscription_enrollment_in_transaction(&mut transaction, command).await?;
        transaction.commit().await?;
        Ok(outcome)
    }

    pub(super) async fn reserve(
        &self,
        reservation: &SubscriptionEnrollmentReservation,
    ) -> Result<SubscriptionEnrollmentReservationOutcome, SubscriptionBillingServiceError> {
        let mut transaction = self.pool.begin().await?;
        let outcome = reserve_subscription_enrollment_in_transaction(
            &mut transaction,
            self.offers.as_ref(),
            reservation,
        )
        .await?;
        transaction.commit().await?;
        Ok(outcome)
    }

    pub(super) async fn payment_result(
        &self,
        attempt: PaymentAttempt,
    ) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionBillingServiceError> {
        let mut transaction = self.pool.begin().await?;
        let result = payment_result_for_attempt(&mut transaction, attempt).await?;
        transaction.commit().await?;
        Ok(result)
    }
}
