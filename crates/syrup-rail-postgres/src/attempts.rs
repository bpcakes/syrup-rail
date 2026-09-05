use std::fmt;

use chrono::{DateTime, Utc};
use sqlx::{PgConnection, Postgres, Row, Transaction, postgres::PgRow};
use syrup_rail::{
    BillingContactSnapshot, BillingPeriod, BillingScopeId, ChargeAmount, CumulativeRefundCents,
    CurrencyCode, DiscountClaimId, DiscountCodeId, GatewayAccountId, GatewayAccountMode,
    GatewayConfigurationId, GatewayDiagnostic, GatewayLifecycleState, GatewayOrderId,
    GatewayPaymentDescriptor, GatewayPaymentMethodReference, GatewayProviderKey,
    GatewayTransactionId, HostChargeTargetId, IdempotencyKey, LimitedDiscountMonths, Money,
    PaidTrialTerms, PaymentAttempt, PaymentAttemptFingerprint, PaymentAttemptId,
    PaymentAttemptIdentity, PaymentAttemptKind, PaymentAttemptLifecycle, PaymentAttemptRequest,
    PaymentAttemptState, PaymentAttemptStatus, PaymentAttemptTarget, PaymentAttemptTimestamps,
    PaymentMethodId, PaymentMethodUpdateSnapshot, PaymentResolutionCode, PercentOffBasisPoints,
    PlanKey, PositiveDiscountCents, ProcessorEvidence, RecurringSubscriptionTerms, SubscriberId,
    SubscriptionDiscountCode, SubscriptionDiscountDuration, SubscriptionDiscountKind,
    SubscriptionDiscountSnapshot, SubscriptionEnrollmentDiscountSnapshot,
    SubscriptionEnrollmentPreflightOutcome, SubscriptionEnrollmentReservation,
    SubscriptionEnrollmentReservationOutcome, SubscriptionEnrollmentReservationRejection,
    SubscriptionEnrollmentSubmissionOutcome, SubscriptionEnrollmentSubmissionRejection,
    SubscriptionEnrollmentTermsVersion, SubscriptionId, SubscriptionInitialApplication,
    SubscriptionOffer, SubscriptionPaymentMethodReplacement,
    SubscriptionPaymentMethodReplacementLockedTerms,
    SubscriptionPaymentMethodReplacementPreflightOutcome,
    SubscriptionPaymentMethodReplacementRejection,
    SubscriptionPaymentMethodReplacementReservationOutcome,
    SubscriptionPaymentMethodReplacementSubmissionOutcome,
    SubscriptionPaymentMethodReplacementSubmissionRejection, SubscriptionPaymentStateSnapshot,
    SubscriptionRecoveryLockedTerms, SubscriptionRecoveryPreflightOutcome,
    SubscriptionRecoveryReservation, SubscriptionRecoveryReservationOutcome,
    SubscriptionRecoveryReservationRejection, SubscriptionRecoverySubmissionOutcome,
    SubscriptionRecoverySubmissionRejection, SubscriptionRenewalLockedTerms,
    SubscriptionRenewalReservation, SubscriptionRenewalReservationOutcome,
    SubscriptionRenewalReservationRejection, SubscriptionRenewalSubmissionOutcome,
    SubscriptionRenewalSubmissionRejection, SubscriptionStart, SubscriptionStatus,
};
use thiserror::Error;
use uuid::Uuid;

use crate::subscription_persistence::{
    RenewalFailurePolicyScalars, SubscriptionPeriodRuleScalars, SubscriptionPersistenceCodecError,
    renewal_failure_policy_from_scalars, subscription_period_rule_from_scalars,
};

mod initial;
mod payment_method_replacement;
mod persistence;
mod recovery;
mod renewal;
mod shared;
mod transitions;

pub use initial::{
    admit_subscription_enrollment_submission_in_transaction,
    preflight_subscription_enrollment_in_transaction,
    reserve_subscription_enrollment_in_transaction,
};
pub use payment_method_replacement::{
    admit_subscription_payment_method_replacement_in_transaction,
    preflight_subscription_payment_method_replacement_in_transaction,
    reserve_subscription_payment_method_replacement_in_transaction,
};
pub(crate) use persistence::find_payment_attempt_by_idempotency_in_transaction;
pub(crate) use persistence::{
    PAYMENT_ATTEMPT_SELECT, find_payment_attempt_by_id_on_connection,
    lock_payment_attempt_by_id_on_connection, payment_attempt_from_row,
    processor_evidence_from_row,
};
pub use persistence::{
    find_payment_attempt_by_id_in_transaction, lock_payment_attempt_by_idempotency_in_transaction,
};
use persistence::{
    find_payment_attempt_by_idempotency, insert_subscription_charge_attempt,
    lock_payment_attempt_by_idempotency, map_subscription_persistence_error,
};
pub use recovery::{
    admit_subscription_recovery_submission_in_transaction,
    preflight_subscription_recovery_in_transaction, reserve_subscription_recovery_in_transaction,
};
pub use renewal::{
    admit_subscription_renewal_submission_in_transaction,
    reserve_subscription_renewal_in_transaction,
};
use shared::*;
pub(crate) use shared::{
    AttemptReplayDisposition, LocalAttemptPolicy, STALE_UNSUBMITTED_RECOVERY_TEXT,
    STALE_UNSUBMITTED_RENEWAL_TEXT, attempt_replay_disposition,
    blocking_payment_method_update_exists, expire_stale_initial_attempts,
    fail_stale_unsubmitted_payment_method_updates, fail_stale_unsubmitted_subscription_charges,
    lock_initial_attempt_rows, lock_initial_attempt_rows_on_connection, lock_initial_charge_rows,
    lock_subscription_aggregate, prepared_replay_required_mode_changed, set_enrollment_timeouts,
    try_lock_subscription_aggregate,
};
use transitions::reject_prepared_attempt;
pub(crate) use transitions::{
    AttemptApproval, AttemptResolutionStatus, AttemptTransition, admit_prepared_attempt,
    persist_attempt_transition,
};

const INVALID_ATTEMPT_STATE: &str = "canonical payment attempt state is invalid";
#[derive(Error)]
pub enum PaymentAttemptStoreError {
    #[error("payment attempt storage operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("{0}")]
    InvalidState(&'static str),
}

impl fmt::Debug for PaymentAttemptStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sql(_) => formatter.write_str("PaymentAttemptStoreError::Sql"),
            Self::InvalidState(detail) => formatter
                .debug_tuple("PaymentAttemptStoreError::InvalidState")
                .field(detail)
                .finish(),
        }
    }
}

const fn invalid_state() -> PaymentAttemptStoreError {
    PaymentAttemptStoreError::InvalidState(INVALID_ATTEMPT_STATE)
}

#[cfg(test)]
mod tests;
