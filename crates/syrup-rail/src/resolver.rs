use std::{fmt, sync::Arc};

use async_trait::async_trait;
use thiserror::Error;

use crate::{
    BillingScopeId, GatewayAccountId, GatewayAccountMode, GatewayConfigurationId, GatewayError,
    GatewayLifecycleQueryPolicy, GatewayMutationError, GatewayMutationReferenceFactory,
    GatewayPaymentOutcome, GatewayProviderKey, GatewayQueryRequest, GatewaySaleRequest,
    GatewayStorePaymentMethodRequest, GatewayTransactionReport, GatewayTransactionReportRequest,
    PaymentGateway,
};

#[derive(Clone)]
pub struct ResolvedGateway {
    billing_scope_id: BillingScopeId,
    gateway_account_id: GatewayAccountId,
    gateway_configuration_id: GatewayConfigurationId,
    provider_key: GatewayProviderKey,
    lifecycle_query_policy: GatewayLifecycleQueryPolicy,
    mutation_reference_factory: Arc<dyn GatewayMutationReferenceFactory>,
    gateway: Arc<dyn PaymentGateway>,
}

impl ResolvedGateway {
    pub fn new(
        billing_scope_id: BillingScopeId,
        gateway_account_id: GatewayAccountId,
        gateway_configuration_id: GatewayConfigurationId,
        provider_key: GatewayProviderKey,
        lifecycle_query_policy: GatewayLifecycleQueryPolicy,
        mutation_reference_factory: Arc<dyn GatewayMutationReferenceFactory>,
        gateway: Arc<dyn PaymentGateway>,
    ) -> Self {
        Self {
            billing_scope_id,
            gateway_account_id,
            gateway_configuration_id,
            provider_key,
            lifecycle_query_policy,
            mutation_reference_factory,
            gateway,
        }
    }

    pub const fn billing_scope_id(&self) -> BillingScopeId {
        self.billing_scope_id
    }

    pub const fn gateway_account_id(&self) -> GatewayAccountId {
        self.gateway_account_id
    }

    pub const fn gateway_configuration_id(&self) -> GatewayConfigurationId {
        self.gateway_configuration_id
    }

    pub const fn provider_key(&self) -> &GatewayProviderKey {
        &self.provider_key
    }

    pub const fn lifecycle_query_policy(&self) -> &GatewayLifecycleQueryPolicy {
        &self.lifecycle_query_policy
    }

    pub fn mutation_reference_factory(&self) -> Arc<dyn GatewayMutationReferenceFactory> {
        Arc::clone(&self.mutation_reference_factory)
    }

    pub async fn account_mode(&self) -> Result<GatewayAccountMode, GatewayError> {
        self.gateway.account_mode().await
    }

    pub async fn sale(
        &self,
        request: GatewaySaleRequest,
    ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
        self.gateway.sale(request).await
    }

    pub async fn store_payment_method(
        &self,
        request: GatewayStorePaymentMethodRequest,
    ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
        self.gateway.store_payment_method(request).await
    }

    pub async fn query_transaction(
        &self,
        request: GatewayQueryRequest,
    ) -> Result<Option<GatewayPaymentOutcome>, GatewayError> {
        self.gateway.query_transaction(request).await
    }

    pub async fn query_transaction_reports(
        &self,
        request: GatewayTransactionReportRequest,
    ) -> Result<Vec<GatewayTransactionReport>, GatewayError> {
        self.gateway.query_transaction_reports(request).await
    }
}

impl fmt::Debug for ResolvedGateway {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedGateway")
            .field("billing_scope_id", &self.billing_scope_id)
            .field("gateway_account_id", &self.gateway_account_id)
            .field("gateway_configuration_id", &self.gateway_configuration_id)
            .field("provider_key", &self.provider_key)
            .field("lifecycle_query_policy", &self.lifecycle_query_policy)
            .field("has_mutation_reference_factory", &true)
            .field("has_gateway", &true)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum GatewayResolutionError {
    #[error("gateway configuration was not found")]
    NotFound,
    #[error("gateway configuration changed")]
    ConfigurationChanged,
    #[error("gateway configuration is invalid")]
    InvalidConfiguration,
    #[error("gateway resolution is temporarily unavailable")]
    Unavailable,
}

#[async_trait]
pub trait GatewayResolver: Send + Sync {
    async fn resolve(
        &self,
        billing_scope_id: BillingScopeId,
        gateway_account_id: GatewayAccountId,
        gateway_configuration_id: GatewayConfigurationId,
        provider_key: GatewayProviderKey,
    ) -> Result<ResolvedGateway, GatewayResolutionError>;
}

#[cfg(test)]
mod tests {
    use chrono::Duration;
    use uuid::Uuid;

    use super::*;
    use crate::{GatewayLifecycleCursorKey, GatewayOrderId, PaymentAttemptId, PaymentAttemptKind};

    struct NeverCalledGateway;

    #[async_trait]
    impl PaymentGateway for NeverCalledGateway {
        async fn account_mode(&self) -> Result<GatewayAccountMode, GatewayError> {
            panic!("resolved gateway construction must not perform provider I/O")
        }

        async fn sale(
            &self,
            _request: GatewaySaleRequest,
        ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
            panic!("resolved gateway construction must not perform provider I/O")
        }

        async fn store_payment_method(
            &self,
            _request: GatewayStorePaymentMethodRequest,
        ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
            panic!("resolved gateway construction must not perform provider I/O")
        }

        async fn query_transaction(
            &self,
            _request: GatewayQueryRequest,
        ) -> Result<Option<GatewayPaymentOutcome>, GatewayError> {
            panic!("resolved gateway construction must not perform provider I/O")
        }

        async fn query_transaction_reports(
            &self,
            _request: GatewayTransactionReportRequest,
        ) -> Result<Vec<GatewayTransactionReport>, GatewayError> {
            panic!("resolved gateway construction must not perform provider I/O")
        }
    }

    struct NeverCalledReferenceFactory;

    impl GatewayMutationReferenceFactory for NeverCalledReferenceFactory {
        fn for_attempt(
            &self,
            _kind: PaymentAttemptKind,
            _attempt_id: PaymentAttemptId,
        ) -> GatewayOrderId {
            panic!("resolved gateway construction must not format a mutation reference")
        }
    }

    #[test]
    fn resolved_gateway_preserves_exact_identity_without_provider_io() {
        let scope = BillingScopeId::new(Uuid::from_u128(1));
        let account = GatewayAccountId::new(Uuid::from_u128(2));
        let configuration = GatewayConfigurationId::new(Uuid::from_u128(3));
        let provider = GatewayProviderKey::new("test_gateway").expect("valid provider key");
        let policy = GatewayLifecycleQueryPolicy::new(
            GatewayLifecycleCursorKey::new("test_cursor").expect("valid cursor key"),
            Duration::minutes(1),
            10,
            2,
            2,
            20,
        )
        .expect("valid lifecycle policy");

        let resolved = ResolvedGateway::new(
            scope,
            account,
            configuration,
            provider.clone(),
            policy.clone(),
            Arc::new(NeverCalledReferenceFactory),
            Arc::new(NeverCalledGateway),
        );

        assert_eq!(resolved.billing_scope_id(), scope);
        assert_eq!(resolved.gateway_account_id(), account);
        assert_eq!(resolved.gateway_configuration_id(), configuration);
        assert_eq!(resolved.provider_key(), &provider);
        assert_eq!(resolved.lifecycle_query_policy(), &policy);
        let debug = format!("{resolved:?}");
        assert!(debug.contains("has_mutation_reference_factory: true"));
        assert!(debug.contains("has_gateway: true"));
    }
}
