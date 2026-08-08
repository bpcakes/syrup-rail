use crate::{BillingScopeId, GatewayAccountId, GatewayProviderKey};

/// Exact account identity used by provider-keyed reconciliation storage.
///
/// Unlike a dispatch candidate, this value includes the provider identity
/// reloaded by the worker. Cursor, pending-evidence, and quarantine operations
/// use all three fields so a stale or cross-provider worker cannot mutate the
/// account's reconciliation state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GatewayLifecycleAccount {
    billing_scope_id: BillingScopeId,
    gateway_account_id: GatewayAccountId,
    provider_key: GatewayProviderKey,
}

impl GatewayLifecycleAccount {
    pub fn new(
        billing_scope_id: BillingScopeId,
        gateway_account_id: GatewayAccountId,
        provider_key: GatewayProviderKey,
    ) -> Self {
        Self {
            billing_scope_id,
            gateway_account_id,
            provider_key,
        }
    }

    pub const fn billing_scope_id(&self) -> BillingScopeId {
        self.billing_scope_id
    }

    pub const fn gateway_account_id(&self) -> GatewayAccountId {
        self.gateway_account_id
    }

    pub const fn provider_key(&self) -> &GatewayProviderKey {
        &self.provider_key
    }
}

/// One registered gateway account that requires host reconciliation dispatch.
///
/// Hosts deliberately receive only the stable scope/account locator. They
/// reload their current configuration when filtering and again in the worker,
/// so a queued compatibility payload cannot freeze a stale configuration.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct GatewayAccountReconciliationCandidate {
    billing_scope_id: BillingScopeId,
    gateway_account_id: GatewayAccountId,
}

impl GatewayAccountReconciliationCandidate {
    pub const fn new(
        billing_scope_id: BillingScopeId,
        gateway_account_id: GatewayAccountId,
    ) -> Self {
        Self {
            billing_scope_id,
            gateway_account_id,
        }
    }

    pub const fn billing_scope_id(self) -> BillingScopeId {
        self.billing_scope_id
    }

    pub const fn gateway_account_id(self) -> GatewayAccountId {
        self.gateway_account_id
    }
}
