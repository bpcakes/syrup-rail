use super::*;

impl SubscriptionBillingService {
    /// Applies an already-observed provider outcome without another submission.
    ///
    /// Reconciliation enters the same application authority as foreground
    /// enrollment but reconstructs its secret-free reservation from the exact
    /// durable attempt and canonical gateway account.
    pub async fn apply_reconciled_outcome(
        &self,
        billing_scope_id: BillingScopeId,
        attempt_id: PaymentAttemptId,
        outcome: &GatewayPaymentOutcome,
    ) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionBillingServiceError> {
        apply_reconciled_subscription_gateway_outcome(
            &self.pool,
            self.coordinator.as_ref(),
            billing_scope_id,
            attempt_id,
            outcome,
        )
        .await
        .map_err(|error| match error {
            SubscriptionEnrollmentApplicationError::InvalidState(
                RECONCILED_SUBSCRIPTION_PAYMENT_ATTEMPT_NOT_FOUND,
            ) => SubscriptionBillingServiceError::InvalidState(
                RECONCILED_SUBSCRIPTION_PAYMENT_ATTEMPT_NOT_FOUND,
            ),
            error => error.into(),
        })
    }
}
