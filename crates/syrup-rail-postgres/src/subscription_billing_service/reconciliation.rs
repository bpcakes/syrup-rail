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
        let mut transaction = self.pool.begin().await?;
        let attempt = crate::find_payment_attempt_by_id_in_transaction(
            &mut transaction,
            billing_scope_id,
            attempt_id,
        )
        .await?
        .ok_or(SubscriptionBillingServiceError::InvalidState(
            "reconciled subscription payment attempt was not found",
        ))?;
        transaction.commit().await?;
        match attempt.kind() {
            PaymentAttemptKind::SubscriptionInitial => {
                apply_reconciled_subscription_enrollment_gateway_outcome(
                    &self.pool,
                    self.coordinator.as_ref(),
                    billing_scope_id,
                    attempt_id,
                    outcome,
                )
                .await
            }
            PaymentAttemptKind::SubscriptionRecovery => {
                apply_reconciled_subscription_recovery_gateway_outcome(
                    &self.pool,
                    self.coordinator.as_ref(),
                    billing_scope_id,
                    attempt_id,
                    outcome,
                )
                .await
            }
            PaymentAttemptKind::SubscriptionRenewal => {
                apply_reconciled_subscription_renewal_gateway_outcome(
                    &self.pool,
                    self.coordinator.as_ref(),
                    billing_scope_id,
                    attempt_id,
                    outcome,
                )
                .await
            }
            PaymentAttemptKind::SubscriptionPaymentMethodUpdate => {
                apply_reconciled_subscription_payment_method_replacement_gateway_outcome(
                    &self.pool,
                    self.coordinator.as_ref(),
                    billing_scope_id,
                    attempt_id,
                    outcome,
                )
                .await
            }
            _ => Err(SubscriptionEnrollmentApplicationError::InvalidState(
                "attempt kind is not owned by the subscription billing service",
            )),
        }
        .map_err(Into::into)
    }
}
