use std::{error::Error, fmt};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool, Postgres, Row, Transaction, postgres::PgRow};
use syrup_rail::{
    ActorId, AttemptReviewCursor, AttemptReviewPage, BillingEventSubject, BillingScopeId,
    CurrencyCode, ExternalReversalAttestation, ExternalReversalHostChargeRelease,
    ExternalReversalKind, ExternalReversalReason, ExternalReversalResolution, GatewayAccountId,
    GatewayDiagnostic, GatewayOrderId, GatewayPaymentDescriptor, GatewayPaymentMethodReference,
    GatewayTransactionId, HostChargeTargetId, ManualAttemptFailureOutcome, ManualFailureHostCharge,
    Money, OperatorReviewPageLimit, PaymentAttempt, PaymentAttemptId, PaymentAttemptKind,
    PaymentAttemptStatus, PlanKey, ProcessorCharge, ProcessorChargeId, ProcessorChargeProgression,
    ProcessorChargeReviewCursor, ProcessorChargeReviewItem, ProcessorChargeReviewPage,
    ProcessorChargeRole, ProcessorEvidence, SubscriberId,
    review_required_attempt_can_be_manually_failed, review_required_manual_failure_evidence,
};
use thiserror::Error;
use uuid::Uuid;

#[cfg(test)]
use syrup_rail::BillingEvent;

use crate::attempts::{
    lock_payment_attempt_by_id_on_connection, lock_subscription_aggregate,
    payment_attempt_from_row, set_enrollment_timeouts,
};
use crate::host_error::{BoxError, RedactedHostErrorSource};
use crate::processor_charge_persistence::{
    ProcessorChargePersistenceError, attestation_by_charge, attestation_matches_source,
    expected_reversal_resolution, parse_charge_state_code, parse_kind, parse_progression,
    parse_role, processor_charge_from_row,
};
use crate::renewal_failure::{
    RenewalFailureApplication, RenewalFailureStoreError, apply_resolved_automatic_renewal_failure,
};
use crate::transactions::{
    BillingEventWriteError, BillingTransactionCoordinator, BillingTransactionError,
};

mod external_reversal;
mod manual_failure;
mod pages;

pub use external_reversal::{
    ExternalReversalAttestationOutcome, ExternalReversalHostStore, ExternalReversalHostStoreError,
    ExternalReversalHostTransitionOutcome, attest_external_reversal,
};
pub use manual_failure::{
    ManualAttemptFailureHostStore, ManualAttemptFailureHostStoreError,
    ManualAttemptFailureHostTransitionOutcome, fail_review_required_attempt,
};
pub use pages::{attempt_review_page, processor_charge_review_page};

#[cfg(test)]
use external_reversal::{charge_locator, lock_processor_charge};

pub(crate) const INVALID_OPERATOR_STATE: &str = "canonical operator review state is invalid";

#[derive(Debug, Error)]
pub enum OperatorReviewError {
    #[error("operator review storage operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("{0}")]
    InvalidState(&'static str),
    #[error(transparent)]
    Host(#[from] ExternalReversalHostStoreError),
    #[error(transparent)]
    ManualFailureHost(#[from] ManualAttemptFailureHostStoreError),
    #[error(transparent)]
    BillingTransaction(#[from] BillingTransactionError),
    #[error(transparent)]
    BillingEvent(#[from] BillingEventWriteError),
}

impl From<ProcessorChargePersistenceError> for OperatorReviewError {
    fn from(error: ProcessorChargePersistenceError) -> Self {
        match error {
            ProcessorChargePersistenceError::Sql(error) => Self::Sql(error),
            ProcessorChargePersistenceError::InvalidState(message) => Self::InvalidState(message),
        }
    }
}

impl From<crate::PaymentAttemptStoreError> for OperatorReviewError {
    fn from(error: crate::PaymentAttemptStoreError) -> Self {
        match error {
            crate::PaymentAttemptStoreError::Sql(error) => Self::Sql(error),
            crate::PaymentAttemptStoreError::InvalidState(_) => {
                Self::InvalidState(INVALID_OPERATOR_STATE)
            }
        }
    }
}

impl From<RenewalFailureStoreError> for OperatorReviewError {
    fn from(error: RenewalFailureStoreError) -> Self {
        match error {
            RenewalFailureStoreError::Sql(error) => Self::Sql(error),
            RenewalFailureStoreError::Attempt(error) => error.into(),
            RenewalFailureStoreError::InvalidState(_) => Self::InvalidState(INVALID_OPERATOR_STATE),
        }
    }
}

#[cfg(test)]
mod tests;
