use crate::{
    PaymentMethodMetadataRefreshError, PaymentMethodMetadataRefreshOutcome,
    RefreshPaymentMethodMetadata, SubscriptionBillingService, refresh_payment_method_metadata,
};

impl SubscriptionBillingService {
    /// Refreshes saved-card display independently of financial approval.
    ///
    /// See [`refresh_payment_method_metadata`] for authorization, bounds, and retry
    /// semantics. Use the same call after new approvals and for historical repair.
    pub async fn refresh_payment_method_metadata(
        &self,
        command: RefreshPaymentMethodMetadata,
    ) -> Result<PaymentMethodMetadataRefreshOutcome, PaymentMethodMetadataRefreshError> {
        refresh_payment_method_metadata(&self.pool, self.resolver.as_ref(), command).await
    }
}
