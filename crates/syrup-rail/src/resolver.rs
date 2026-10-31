use std::{fmt, sync::Arc};

use async_trait::async_trait;
use thiserror::Error;

use crate::{
    BillingScopeId, GatewayAccountId, GatewayAccountMode, GatewayConfigurationId, GatewayError,
    GatewayLifecycleQueryPolicy, GatewayMutationError, GatewayMutationReferenceFactory,
    GatewayPaymentMethodMetadata, GatewayPaymentOutcome, GatewayProviderKey, GatewayQueryRequest,
    GatewaySaleRequest, GatewayStorePaymentMethodRequest, GatewayTransactionDiagnostics,
    GatewayTransactionDiagnosticsRequest, GatewayTransactionReport,
    GatewayTransactionReportRequest, PaymentGateway, StatementDescriptorPolicy,
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
    statement_descriptors: Option<StatementDescriptorPolicy>,
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
            statement_descriptors: None,
        }
    }

    /// Prints host-chosen statement descriptors on every sale this gateway
    /// submits, where the provider's acquirer supports dynamic descriptors.
    ///
    /// Descriptors are a property of the merchant account behind this gateway,
    /// so the host configures them here rather than on each payment command.
    /// A sale that already carries an explicit descriptor keeps it. Without a
    /// policy, sales carry no descriptor and the account's static descriptor
    /// prints, exactly as before dynamic descriptors were introduced.
    pub fn with_statement_descriptors(mut self, policy: StatementDescriptorPolicy) -> Self {
        self.statement_descriptors = Some(policy);
        self
    }

    pub const fn statement_descriptors(&self) -> Option<&StatementDescriptorPolicy> {
        self.statement_descriptors.as_ref()
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
        self.gateway
            .sale(self.with_policy_statement_descriptor(request))
            .await
    }

    /// Applies the configured descriptor policy to a sale that has no explicit
    /// descriptor, selecting by the sale's intent.
    fn with_policy_statement_descriptor(&self, request: GatewaySaleRequest) -> GatewaySaleRequest {
        match (&self.statement_descriptors, request.descriptor()) {
            (Some(policy), None) => {
                let descriptor = policy.descriptor_for(request.intent()).clone();
                request.with_descriptor(Some(descriptor))
            }
            _ => request,
        }
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

    /// Resolves display metadata independently from financial query evidence.
    pub async fn query_payment_method_metadata(
        &self,
        request: GatewayQueryRequest,
    ) -> Result<Option<GatewayPaymentMethodMetadata>, GatewayError> {
        self.gateway.query_payment_method_metadata(request).await
    }

    /// Reads read-only processor and verification diagnostics for one
    /// historical transaction through the resolved provider.
    pub async fn query_transaction_diagnostics(
        &self,
        request: GatewayTransactionDiagnosticsRequest,
    ) -> Result<GatewayTransactionDiagnostics, GatewayError> {
        self.gateway.query_transaction_diagnostics(request).await
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
            .field("statement_descriptors", &self.statement_descriptors)
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
mod tests;
