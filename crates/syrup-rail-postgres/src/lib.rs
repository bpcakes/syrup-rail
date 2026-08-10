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
mod host_charge_application;
mod host_charges;
mod lifecycle_quarantine;
mod lifecycle_reconciliation;
mod operator_review;
#[cfg(test)]
mod paid_trial_dunning_tests;
mod processor_charge_persistence;
mod processor_charges;
mod reconciliation;
mod renewal;
mod renewal_failure;
#[cfg(any(test, feature = "schema-contract-test-support"))]
pub mod schema_contract;
mod subscription_billing_service;
mod subscription_persistence;
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
pub use host_charge_application::{
    AdmittedHostCharge, HostChargeAdmissionOutcome, HostChargeApplicationError,
    HostChargeProviderResult, admit_host_charge_submission, apply_host_charge_gateway_outcome,
    apply_reconciled_host_charge_gateway_outcome, submit_admitted_host_charge,
};
pub use host_charges::{
    HostChargeLedgerAdmission, HostChargeLedgerAdmissionError, HostChargeLedgerAdmissionMode,
    HostChargeLedgerAdmissionQuery, HostChargePreflightOutcome, HostChargeReservationDecision,
    HostChargeReservationOutcome, HostChargeStoreError, HostChargeSubmissionAdmission,
    HostChargeSubmissionDecision, HostChargeSubmissionOutcome, HostChargeTargetError,
    HostChargeTargetReservation, HostChargeTargetStore,
    admit_host_charge_submission_in_transaction, host_charge_ledger_admission,
    preflight_host_charge_in_transaction, reserve_host_charge_in_transaction,
};
pub use lifecycle_quarantine::{
    GatewayLifecycleQuarantineAlert, GatewayLifecycleQuarantineResolutionOutcome,
    GatewayLifecycleQuarantineResolutionRecord, GatewayLifecycleQuarantineReviewRecord,
    claim_gateway_lifecycle_quarantine_alert, gateway_lifecycle_quarantine_review_page,
    resolve_gateway_lifecycle_quarantine,
};
pub use lifecycle_reconciliation::{
    GatewayLifecycleApplyOutcome, GatewayLifecycleReconciliationError,
    GatewayLifecycleReconciliationSummary, apply_gateway_lifecycle_evidence,
    apply_staged_gateway_lifecycle_evidence, gateway_lifecycle_reconciliation_start,
    reconcile_gateway_transaction_reports, record_gateway_lifecycle_quarantines,
    save_gateway_lifecycle_reconciliation_cursor, stage_gateway_lifecycle_evidence,
};
pub use operator_review::{
    ExternalReversalAttestationOutcome, ExternalReversalHostStore, ExternalReversalHostStoreError,
    ExternalReversalHostTransitionOutcome, ManualAttemptFailureHostStore,
    ManualAttemptFailureHostStoreError, ManualAttemptFailureHostTransitionOutcome,
    OperatorReviewError, attempt_review_page, attest_external_reversal,
    fail_review_required_attempt, processor_charge_review_page,
};
pub use processor_charges::{
    CompensatingProcessorChargeOutcome, ProcessorChargeObservationOutcome,
    ProcessorChargeStoreError, observe_processor_charge_in_transaction,
    store_compensating_processor_charge, transition_processor_charge_in_transaction,
};
pub use reconciliation::{
    ExactQueryObservation, ProcessorChargeClassificationSummary, apply_exact_query_observation,
    claim_exact_reconciliation_attempts, classify_pending_processor_charges,
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
