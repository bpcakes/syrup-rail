//! PostgreSQL schema contract and transaction orchestration for Syrup Rail.
//!
//! Production service construction must not expose or invoke a migrator; host
//! applications materialize versioned install artifacts as immutable migrations.

#![forbid(unsafe_code)]

mod attempts;
mod cancellation;
mod deletion;
mod discounts;
mod entitlement;
mod gateway_accounts;
mod grants;
#[cfg(any(test, feature = "schema-contract-test-support"))]
pub mod schema_contract;
#[cfg(test)]
mod test_support;

pub use attempts::{
    PaymentAttemptStoreError, find_payment_attempt_by_id_in_transaction,
    lock_payment_attempt_by_idempotency_in_transaction,
};
pub use cancellation::{SubscriptionCancellationError, cancel_subscription_in_transaction};
pub use deletion::{billing_deletion_blockers, scrub_subscriber_billing_data};
pub use discounts::*;
pub use entitlement::{
    EntitlementGuardError, EntitlementQueryError, entitlement, require_entitlement_for_update,
};
pub use gateway_accounts::{activate_gateway_configuration, register_gateway_account};
pub use grants::{
    SubscriptionGrantMutationError, create_subscription_grant, revoke_subscription_grant,
};
