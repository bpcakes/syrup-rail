//! PostgreSQL schema contract and transaction orchestration for Syrup Rail.
//!
//! Production service construction must not expose or invoke a migrator; host
//! applications materialize versioned install artifacts as immutable migrations.

#![forbid(unsafe_code)]

mod attempts;
mod cancellation;
mod deletion;
mod discounts;
mod enrollment_application;
mod entitlement;
mod gateway_accounts;
mod grants;
mod reconciliation;
mod renewal;
#[cfg(any(test, feature = "schema-contract-test-support"))]
pub mod schema_contract;
mod subscription_billing_service;
#[cfg(test)]
mod test_support;
mod transactions;

pub use attempts::{
    PaymentAttemptStoreError, admit_subscription_enrollment_submission_in_transaction,
    admit_subscription_payment_method_replacement_in_transaction,
    admit_subscription_recovery_submission_in_transaction,
    admit_subscription_renewal_submission_in_transaction,
    find_payment_attempt_by_id_in_transaction, lock_payment_attempt_by_idempotency_in_transaction,
    preflight_subscription_enrollment_in_transaction,
    preflight_subscription_payment_method_replacement_in_transaction,
    preflight_subscription_recovery_in_transaction, reserve_subscription_enrollment_in_transaction,
    reserve_subscription_payment_method_replacement_in_transaction,
    reserve_subscription_recovery_in_transaction, reserve_subscription_renewal_in_transaction,
};
pub use cancellation::{SubscriptionCancellationError, cancel_subscription_in_transaction};
pub use deletion::{billing_deletion_blockers, scrub_subscriber_billing_data};
pub use discounts::*;
pub use enrollment_application::{
    AdmittedSubscriptionEnrollment, AdmittedSubscriptionPaymentMethodReplacement,
    AdmittedSubscriptionRecovery, AdmittedSubscriptionRenewal,
    SubscriptionEnrollmentAdmissionOutcome, SubscriptionEnrollmentApplicationError,
    SubscriptionEnrollmentProviderResult, SubscriptionPaymentMethodReplacementAdmissionOutcome,
    SubscriptionPaymentMethodReplacementProviderResult, SubscriptionRecoveryAdmissionOutcome,
    SubscriptionRecoveryProviderResult, SubscriptionRenewalAdmissionOutcome,
    SubscriptionRenewalProviderResult, admit_subscription_enrollment_submission,
    admit_subscription_payment_method_replacement, admit_subscription_recovery_submission,
    admit_subscription_renewal_submission,
    apply_reconciled_subscription_enrollment_gateway_outcome,
    apply_reconciled_subscription_payment_method_replacement_gateway_outcome,
    apply_reconciled_subscription_recovery_gateway_outcome,
    apply_reconciled_subscription_renewal_gateway_outcome,
    apply_subscription_enrollment_gateway_outcome,
    apply_subscription_payment_method_replacement_gateway_outcome,
    apply_subscription_recovery_gateway_outcome, apply_subscription_renewal_gateway_outcome,
    submit_admitted_subscription_enrollment,
    submit_admitted_subscription_payment_method_replacement, submit_admitted_subscription_recovery,
    submit_admitted_subscription_renewal,
};
pub use entitlement::{
    EntitlementGuardError, EntitlementQueryError, entitlement, require_entitlement_for_update,
};
pub use gateway_accounts::{activate_gateway_configuration, register_gateway_account};
pub use grants::{
    SubscriptionGrantMutationError, create_subscription_grant, revoke_subscription_grant,
};
pub use reconciliation::{
    fail_stale_unsubmitted_payment_method_replacements,
    fail_stale_unsubmitted_subscription_enrollments, reconciliation_gateway_accounts,
};
pub use renewal::{RenewalStoreError, due_renewals, renewal_attempt_state};
pub use subscription_billing_service::{
    GatewayMutationCooldownScope, SubscriptionBillingService, SubscriptionEnrollmentServiceError,
};
pub use transactions::{
    BillingEventWriteError, BillingTransaction, BillingTransactionCoordinator,
    BillingTransactionError, BillingTransactionSubjectState,
};
