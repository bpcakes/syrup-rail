use crate::{BillingScopeId, GatewayAccountId};

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
