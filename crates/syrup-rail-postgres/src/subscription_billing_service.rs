use std::{fmt, sync::Arc, time::Duration};

use sqlx::PgPool;
use syrup_rail::{
    BillingEventSubject, BillingScopeId, CancelSubscription, CancelSubscriptionOutcome,
    ChargeHostTarget, ChargeRenewal, ClearSubscriptionDiscount, EndUserMutationAdmission,
    EndUserMutationAdmissionResult, EndUserMutationCommand, EndUserMutationOperation,
    EnrollSubscription, GatewayAccountId, GatewayAccountMode, GatewayDiagnostic, GatewayError,
    GatewayNotSubmittedError, GatewayPaymentDescriptor, GatewayPaymentOutcome, GatewayProviderKey,
    GatewayResolutionError, GatewayResolver, HostChargePaymentResult, HostChargeReservation,
    HostChargeTargetRejection, PaymentAttempt, PaymentAttemptId, PaymentAttemptKind,
    PaymentAttemptStatus, PaymentResolutionCode, ProcessorEvidence, RecoverSubscriptionPayment,
    ReplaceSubscriptionPaymentMethod, SubscriptionDiscountClaim, SubscriptionDiscountClaimOutcome,
    SubscriptionDiscountClearOutcome, SubscriptionEnrollmentPaymentResult,
    SubscriptionEnrollmentPreflightOutcome, SubscriptionEnrollmentReservation,
    SubscriptionEnrollmentReservationBuildError, SubscriptionEnrollmentReservationOutcome,
    SubscriptionEnrollmentReservationRejection, SubscriptionEnrollmentSubmissionRejection,
    SubscriptionPaymentMethodReplacement, SubscriptionPaymentMethodReplacementPreflightOutcome,
    SubscriptionPaymentMethodReplacementRejection,
    SubscriptionPaymentMethodReplacementReservationOutcome,
    SubscriptionPaymentMethodReplacementSubmissionRejection, SubscriptionRecoveryPreflightOutcome,
    SubscriptionRecoveryReservation, SubscriptionRecoveryReservationOutcome,
    SubscriptionRecoveryReservationRejection, SubscriptionRecoverySubmissionRejection,
    SubscriptionRenewalOutcome, SubscriptionRenewalReservation,
    SubscriptionRenewalReservationOutcome, SubscriptionRenewalReservationRejection,
};
use thiserror::Error;

use crate::host_charge_application::{
    HostChargeBeforeSubmissionResolution, resolve_host_charge_before_submission,
};
use crate::{
    BillingTransactionCoordinator, HostChargeAdmissionOutcome, HostChargeApplicationError,
    HostChargePreflightOutcome, HostChargeProviderResult, HostChargeReservationOutcome,
    HostChargeStoreError, HostChargeTargetStore, PaymentAttemptStoreError,
    SubscriptionEnrollmentAdmissionOutcome, SubscriptionEnrollmentApplicationError,
    SubscriptionEnrollmentProviderResult, SubscriptionOfferStore,
    SubscriptionPaymentMethodReplacementAdmissionOutcome,
    SubscriptionPaymentMethodReplacementProviderResult, SubscriptionRecoveryAdmissionOutcome,
    SubscriptionRecoveryProviderResult, SubscriptionRenewalAdmissionOutcome,
    SubscriptionRenewalProviderResult, admit_host_charge_submission,
    admit_subscription_enrollment_submission, admit_subscription_payment_method_replacement,
    admit_subscription_recovery_submission, admit_subscription_renewal_submission,
    apply_reconciled_host_charge_gateway_outcome,
    apply_reconciled_subscription_enrollment_gateway_outcome,
    apply_reconciled_subscription_payment_method_replacement_gateway_outcome,
    apply_reconciled_subscription_recovery_gateway_outcome,
    apply_reconciled_subscription_renewal_gateway_outcome,
    attempts::AttemptResolutionStatus,
    enrollment_application::{
        OutcomeResolutionBoundary, RateLimitCooldown, payment_result_for_attempt,
        resolve_non_approved_outcome, resolve_payment_method_replacement_non_approved_outcome,
        resolve_recovery_non_approved_outcome, resolve_renewal_non_approved_outcome,
    },
    preflight_host_charge_in_transaction, preflight_subscription_enrollment_in_transaction,
    preflight_subscription_payment_method_replacement_in_transaction,
    preflight_subscription_recovery_in_transaction, reserve_host_charge_in_transaction,
    reserve_subscription_enrollment_in_transaction,
    reserve_subscription_payment_method_replacement_in_transaction,
    reserve_subscription_recovery_in_transaction, reserve_subscription_renewal_in_transaction,
    submit_admitted_host_charge, submit_admitted_subscription_enrollment,
    submit_admitted_subscription_payment_method_replacement, submit_admitted_subscription_recovery,
    submit_admitted_subscription_renewal,
};

mod enrollment;
mod host_charge;
mod payment_method_replacement;
mod reconciliation;
mod recovery;
mod renewal;
mod subscriber;
mod subscriber_mutation;

const INVALID_SERVICE_STATE: &str = "canonical subscription billing service state is invalid";
const LIVE_READINESS_FAILED_TEXT: &str =
    "Payment was not submitted because the payment processor was not ready for live transactions.";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GatewayMutationCooldownScope {
    Account,
    Provider,
}

/// A stable, conservative operational category for a
/// [`SubscriptionBillingServiceError`].
///
/// Hosts can use this value to decide whether to show a request/state problem,
/// repair configuration, investigate an internal failure, or resubmit the
/// same idempotent command later. This enum is non-exhaustive so hosts must
/// keep a conservative wildcard branch when matching it.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SubscriptionBillingServiceErrorDisposition {
    /// The command's idempotency identity or authority snapshot no longer
    /// matches current durable state.
    ///
    /// This is not retryable as-is. Hosts generally need to load and rebuild
    /// against current authority, or reconcile the existing idempotency key,
    /// before issuing another command.
    Conflict,
    /// The command cannot proceed because its current request or billing
    /// state is semantically blocked.
    Rejected,
    /// The same idempotent command may safely be resubmitted later, although
    /// another attempt is not guaranteed to succeed.
    TemporarilyUnavailable,
    /// A required gateway, gateway configuration, offer, or optional host
    /// capability is missing or invalid.
    Misconfigured,
    /// A storage, application, transaction, durable-state, or contract fault
    /// requires investigation rather than an automatic retry.
    Internal,
}

/// Failure returned by the high-level subscription billing facade.
///
/// This covers subscriber-owned enrollment, recovery, renewal, payment-method
/// replacement, cancellation, discount mutations, reconciliation, and the
/// optional host-charge capability.
#[non_exhaustive]
#[derive(Error)]
pub enum SubscriptionBillingServiceError {
    #[error("subscription billing storage failed")]
    Sql(#[from] sqlx::Error),
    /// A provider-free local transaction could not acquire capacity or failed
    /// with an explicitly recognized transient SQLSTATE. Replaying the same
    /// idempotent operation after the failed transaction is discarded is safe.
    #[error("subscription billing storage is temporarily unavailable")]
    StorageTemporarilyUnavailable(#[source] sqlx::Error),
    #[error("payment attempt storage failed")]
    Attempt(#[from] PaymentAttemptStoreError),
    #[error("subscription payment application failed")]
    Application(#[from] SubscriptionEnrollmentApplicationError),
    #[error("host charge application failed")]
    HostChargeApplication(#[from] HostChargeApplicationError),
    #[error("host charge storage failed")]
    HostChargeStore(#[from] HostChargeStoreError),
    #[error("subscription cancellation failed")]
    Cancellation(#[source] crate::SubscriptionCancellationError),
    #[error("subscription discount operation failed")]
    Discount(#[source] crate::SubscriptionDiscountOperationError),
    #[error("host billing transaction failed")]
    BillingTransaction(#[from] crate::BillingTransactionError),
    #[error("host billing event append failed")]
    BillingEvent(#[from] crate::BillingEventWriteError),
    #[error("host charge capability is not configured")]
    HostChargeUnavailable,
    #[error("the idempotency key belongs to a different payment request")]
    IdempotencyConflict,
    #[error("end-user mutation admission was denied")]
    AdmissionDenied { retry_after: Duration },
    #[error("end-user mutation admission timed out")]
    AdmissionTimeout,
    #[error("end-user mutation admission is unavailable")]
    AdmissionUnavailable,
    #[error("gateway account or configuration changed")]
    GatewayConfigurationChanged,
    #[error("gateway resolution failed")]
    GatewayResolution(#[from] GatewayResolutionError),
    #[error("gateway resolver returned a different canonical identity")]
    ResolvedGatewayIdentityMismatch,
    #[error("gateway mutation cooldown is active")]
    GatewayMutationCooldown { scope: GatewayMutationCooldownScope },
    #[error("subscription enrollment reservation was rejected")]
    ReservationRejected(SubscriptionEnrollmentReservationRejection),
    #[error("subscription enrollment submission was rejected")]
    SubmissionRejected(SubscriptionEnrollmentSubmissionRejection),
    #[error("host charge reservation was rejected")]
    HostChargeReservationRejected(HostChargeTargetRejection),
    #[error("host charge submission was rejected")]
    HostChargeSubmissionRejected(HostChargeTargetRejection),
    #[error("subscription recovery reservation was rejected")]
    RecoveryReservationRejected(SubscriptionRecoveryReservationRejection),
    #[error("subscription recovery submission was rejected")]
    RecoverySubmissionRejected(SubscriptionRecoverySubmissionRejection),
    #[error("subscription renewal reservation was rejected")]
    RenewalReservationRejected(SubscriptionRenewalReservationRejection),
    #[error("subscription payment method replacement reservation was rejected")]
    PaymentMethodReplacementReservationRejected(SubscriptionPaymentMethodReplacementRejection),
    #[error("subscription payment method replacement submission was rejected")]
    PaymentMethodReplacementSubmissionRejected(
        SubscriptionPaymentMethodReplacementSubmissionRejection,
    ),
    #[error("gateway mutation was not submitted")]
    GatewayNotSubmitted(#[source] GatewayNotSubmittedError),
    #[error("gateway readiness check failed")]
    GatewayReadiness(#[source] GatewayError),
    #[error("{0}")]
    InvalidState(&'static str),
}

impl fmt::Debug for SubscriptionBillingServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sql(_) => formatter.write_str("SubscriptionBillingServiceError::Sql"),
            Self::StorageTemporarilyUnavailable(_) => formatter
                .write_str("SubscriptionBillingServiceError::StorageTemporarilyUnavailable"),
            Self::Attempt(_) => formatter.write_str("SubscriptionBillingServiceError::Attempt"),
            Self::Application(_) => {
                formatter.write_str("SubscriptionBillingServiceError::Application")
            }
            Self::HostChargeApplication(_) => {
                formatter.write_str("SubscriptionBillingServiceError::HostChargeApplication")
            }
            Self::HostChargeStore(_) => {
                formatter.write_str("SubscriptionBillingServiceError::HostChargeStore")
            }
            Self::Cancellation(_) => {
                formatter.write_str("SubscriptionBillingServiceError::Cancellation")
            }
            Self::Discount(_) => formatter.write_str("SubscriptionBillingServiceError::Discount"),
            Self::BillingTransaction(_) => {
                formatter.write_str("SubscriptionBillingServiceError::BillingTransaction")
            }
            Self::BillingEvent(_) => {
                formatter.write_str("SubscriptionBillingServiceError::BillingEvent")
            }
            Self::HostChargeUnavailable => {
                formatter.write_str("SubscriptionBillingServiceError::HostChargeUnavailable")
            }
            Self::IdempotencyConflict => {
                formatter.write_str("SubscriptionBillingServiceError::IdempotencyConflict")
            }
            Self::AdmissionDenied { retry_after } => formatter
                .debug_struct("SubscriptionBillingServiceError::AdmissionDenied")
                .field("retry_after", retry_after)
                .finish(),
            Self::AdmissionTimeout => {
                formatter.write_str("SubscriptionBillingServiceError::AdmissionTimeout")
            }
            Self::AdmissionUnavailable => {
                formatter.write_str("SubscriptionBillingServiceError::AdmissionUnavailable")
            }
            Self::GatewayConfigurationChanged => {
                formatter.write_str("SubscriptionBillingServiceError::GatewayConfigurationChanged")
            }
            Self::GatewayResolution(error) => formatter
                .debug_tuple("SubscriptionBillingServiceError::GatewayResolution")
                .field(error)
                .finish(),
            Self::ResolvedGatewayIdentityMismatch => formatter
                .write_str("SubscriptionBillingServiceError::ResolvedGatewayIdentityMismatch"),
            Self::GatewayMutationCooldown { scope } => formatter
                .debug_struct("SubscriptionBillingServiceError::GatewayMutationCooldown")
                .field("scope", scope)
                .finish(),
            Self::ReservationRejected(reason) => formatter
                .debug_tuple("SubscriptionBillingServiceError::ReservationRejected")
                .field(reason)
                .finish(),
            Self::SubmissionRejected(reason) => formatter
                .debug_tuple("SubscriptionBillingServiceError::SubmissionRejected")
                .field(reason)
                .finish(),
            Self::HostChargeReservationRejected(reason) => formatter
                .debug_tuple("SubscriptionBillingServiceError::HostChargeReservationRejected")
                .field(reason)
                .finish(),
            Self::HostChargeSubmissionRejected(reason) => formatter
                .debug_tuple("SubscriptionBillingServiceError::HostChargeSubmissionRejected")
                .field(reason)
                .finish(),
            Self::RecoveryReservationRejected(reason) => formatter
                .debug_tuple("SubscriptionBillingServiceError::RecoveryReservationRejected")
                .field(reason)
                .finish(),
            Self::RecoverySubmissionRejected(reason) => formatter
                .debug_tuple("SubscriptionBillingServiceError::RecoverySubmissionRejected")
                .field(reason)
                .finish(),
            Self::RenewalReservationRejected(reason) => formatter
                .debug_tuple("SubscriptionBillingServiceError::RenewalReservationRejected")
                .field(reason)
                .finish(),
            Self::PaymentMethodReplacementReservationRejected(reason) => formatter
                .debug_tuple(
                    "SubscriptionBillingServiceError::PaymentMethodReplacementReservationRejected",
                )
                .field(reason)
                .finish(),
            Self::PaymentMethodReplacementSubmissionRejected(reason) => formatter
                .debug_tuple(
                    "SubscriptionBillingServiceError::PaymentMethodReplacementSubmissionRejected",
                )
                .field(reason)
                .finish(),
            Self::GatewayNotSubmitted(error) => formatter
                .debug_tuple("SubscriptionBillingServiceError::GatewayNotSubmitted")
                .field(error)
                .finish(),
            Self::GatewayReadiness(error) => formatter
                .debug_tuple("SubscriptionBillingServiceError::GatewayReadiness")
                .field(error)
                .finish(),
            Self::InvalidState(detail) => formatter
                .debug_tuple("SubscriptionBillingServiceError::InvalidState")
                .field(detail)
                .finish(),
        }
    }
}

impl SubscriptionBillingServiceError {
    /// Returns the stable, conservative operational category for this error.
    ///
    /// The category intentionally does not expose provider diagnostics,
    /// payment values, or durable identifiers.
    pub const fn disposition(&self) -> SubscriptionBillingServiceErrorDisposition {
        match self {
            Self::StorageTemporarilyUnavailable(_) => {
                SubscriptionBillingServiceErrorDisposition::TemporarilyUnavailable
            }
            Self::Sql(_)
            | Self::Attempt(_)
            | Self::Application(_)
            | Self::HostChargeApplication(_)
            | Self::HostChargeStore(_)
            | Self::BillingTransaction(_)
            | Self::BillingEvent(_)
            | Self::ResolvedGatewayIdentityMismatch
            | Self::InvalidState(_) => SubscriptionBillingServiceErrorDisposition::Internal,
            Self::Cancellation(error) => cancellation_error_disposition(error),
            Self::Discount(error) => discount_error_disposition(error),
            Self::HostChargeUnavailable => {
                SubscriptionBillingServiceErrorDisposition::Misconfigured
            }
            Self::IdempotencyConflict => SubscriptionBillingServiceErrorDisposition::Conflict,
            Self::AdmissionDenied { .. } | Self::AdmissionTimeout | Self::AdmissionUnavailable => {
                SubscriptionBillingServiceErrorDisposition::TemporarilyUnavailable
            }
            Self::GatewayConfigurationChanged => {
                SubscriptionBillingServiceErrorDisposition::Conflict
            }
            Self::GatewayResolution(error) => gateway_resolution_disposition(*error),
            Self::GatewayMutationCooldown { scope } => match scope {
                GatewayMutationCooldownScope::Account | GatewayMutationCooldownScope::Provider => {
                    SubscriptionBillingServiceErrorDisposition::TemporarilyUnavailable
                }
            },
            Self::ReservationRejected(reason) => {
                enrollment_reservation_rejection_disposition(*reason)
            }
            Self::SubmissionRejected(reason) => {
                enrollment_submission_rejection_disposition(*reason)
            }
            Self::HostChargeReservationRejected(reason)
            | Self::HostChargeSubmissionRejected(reason) => {
                host_charge_rejection_disposition(*reason)
            }
            Self::RecoveryReservationRejected(reason) => {
                recovery_reservation_rejection_disposition(*reason)
            }
            Self::RecoverySubmissionRejected(reason) => {
                recovery_submission_rejection_disposition(*reason)
            }
            Self::RenewalReservationRejected(reason) => {
                renewal_reservation_rejection_disposition(*reason)
            }
            Self::PaymentMethodReplacementReservationRejected(reason) => {
                payment_method_replacement_reservation_rejection_disposition(*reason)
            }
            Self::PaymentMethodReplacementSubmissionRejected(reason) => {
                payment_method_replacement_submission_rejection_disposition(*reason)
            }
            Self::GatewayNotSubmitted(error) => gateway_not_submitted_disposition(error),
            Self::GatewayReadiness(error) => gateway_readiness_disposition(error),
        }
    }

    /// Returns whether it is safe to resubmit the **same idempotent command**.
    ///
    /// A `true` result means only that Syrup Rail can safely accept another
    /// submission of that unchanged command and idempotency key. It does not
    /// promise that the next attempt will succeed. Do not create a new command
    /// or idempotency key merely because this returns `true`.
    pub const fn is_retryable(&self) -> bool {
        matches!(
            self.disposition(),
            SubscriptionBillingServiceErrorDisposition::TemporarilyUnavailable
        )
    }

    /// Returns whether the command conflicts with an existing idempotency or
    /// authority snapshot.
    ///
    /// Conflicts are not retryable as-is; reload current authority or reconcile
    /// the existing idempotency key before constructing another command.
    pub const fn is_conflict(&self) -> bool {
        matches!(
            self.disposition(),
            SubscriptionBillingServiceErrorDisposition::Conflict
        )
    }

    /// Returns an exact retry delay when the service was given one.
    ///
    /// `None` does not imply that the error is non-retryable. For example,
    /// gateway/account cooldowns are temporarily unavailable but do not carry
    /// an exact delay that this API can safely fabricate.
    pub const fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::AdmissionDenied { retry_after } => Some(*retry_after),
            Self::Sql(_)
            | Self::StorageTemporarilyUnavailable(_)
            | Self::Attempt(_)
            | Self::Application(_)
            | Self::HostChargeApplication(_)
            | Self::HostChargeStore(_)
            | Self::Cancellation(_)
            | Self::Discount(_)
            | Self::BillingTransaction(_)
            | Self::BillingEvent(_)
            | Self::HostChargeUnavailable
            | Self::IdempotencyConflict
            | Self::AdmissionTimeout
            | Self::AdmissionUnavailable
            | Self::GatewayConfigurationChanged
            | Self::GatewayResolution(_)
            | Self::ResolvedGatewayIdentityMismatch
            | Self::GatewayMutationCooldown { .. }
            | Self::ReservationRejected(_)
            | Self::SubmissionRejected(_)
            | Self::HostChargeReservationRejected(_)
            | Self::HostChargeSubmissionRejected(_)
            | Self::RecoveryReservationRejected(_)
            | Self::RecoverySubmissionRejected(_)
            | Self::RenewalReservationRejected(_)
            | Self::PaymentMethodReplacementReservationRejected(_)
            | Self::PaymentMethodReplacementSubmissionRejected(_)
            | Self::GatewayNotSubmitted(_)
            | Self::GatewayReadiness(_)
            | Self::InvalidState(_) => None,
        }
    }
}

impl From<crate::SubscriptionCancellationError> for SubscriptionBillingServiceError {
    fn from(error: crate::SubscriptionCancellationError) -> Self {
        match error {
            crate::SubscriptionCancellationError::Sql(source)
                if is_retryable_provider_free_transaction_error(&source) =>
            {
                Self::StorageTemporarilyUnavailable(source)
            }
            error => Self::Cancellation(error),
        }
    }
}

impl From<crate::SubscriptionDiscountOperationError> for SubscriptionBillingServiceError {
    fn from(error: crate::SubscriptionDiscountOperationError) -> Self {
        match error {
            crate::SubscriptionDiscountOperationError::Sql(source)
                if is_retryable_provider_free_transaction_error(&source) =>
            {
                Self::StorageTemporarilyUnavailable(source)
            }
            error => Self::Discount(error),
        }
    }
}

fn provider_free_transaction_error(error: sqlx::Error) -> SubscriptionBillingServiceError {
    if is_retryable_provider_free_transaction_error(&error) {
        SubscriptionBillingServiceError::StorageTemporarilyUnavailable(error)
    } else {
        SubscriptionBillingServiceError::Sql(error)
    }
}

fn is_retryable_provider_free_transaction_error(error: &sqlx::Error) -> bool {
    if matches!(error, sqlx::Error::PoolTimedOut) {
        return true;
    }
    let sqlx::Error::Database(error) = error else {
        return false;
    };
    error
        .code()
        .is_some_and(|code| is_retryable_provider_free_transaction_sqlstate(code.as_ref()))
}

fn is_retryable_provider_free_transaction_sqlstate(code: &str) -> bool {
    matches!(code, "40001" | "40P01" | "55P03" | "57014")
}

const fn cancellation_error_disposition(
    error: &crate::SubscriptionCancellationError,
) -> SubscriptionBillingServiceErrorDisposition {
    match error {
        crate::SubscriptionCancellationError::Sql(_)
        | crate::SubscriptionCancellationError::InvalidState(_) => {
            SubscriptionBillingServiceErrorDisposition::Internal
        }
    }
}

const fn discount_error_disposition(
    error: &crate::SubscriptionDiscountOperationError,
) -> SubscriptionBillingServiceErrorDisposition {
    match error {
        crate::SubscriptionDiscountOperationError::Sql(_)
        | crate::SubscriptionDiscountOperationError::InvalidState(_) => {
            SubscriptionBillingServiceErrorDisposition::Internal
        }
        crate::SubscriptionDiscountOperationError::OfferUnavailable
        | crate::SubscriptionDiscountOperationError::InvalidConfiguration
        | crate::SubscriptionDiscountOperationError::LimitedDiscountCadence => {
            SubscriptionBillingServiceErrorDisposition::Misconfigured
        }
        crate::SubscriptionDiscountOperationError::OfferPlanMismatch => {
            SubscriptionBillingServiceErrorDisposition::Conflict
        }
    }
}

const fn gateway_resolution_disposition(
    error: GatewayResolutionError,
) -> SubscriptionBillingServiceErrorDisposition {
    match error {
        GatewayResolutionError::NotFound | GatewayResolutionError::InvalidConfiguration => {
            SubscriptionBillingServiceErrorDisposition::Misconfigured
        }
        GatewayResolutionError::ConfigurationChanged => {
            SubscriptionBillingServiceErrorDisposition::Conflict
        }
        GatewayResolutionError::Unavailable => {
            SubscriptionBillingServiceErrorDisposition::TemporarilyUnavailable
        }
    }
}

const fn gateway_readiness_disposition(
    error: &GatewayError,
) -> SubscriptionBillingServiceErrorDisposition {
    match error {
        GatewayError::RequestRejected(_) => SubscriptionBillingServiceErrorDisposition::Rejected,
        // A malformed gateway response violates the adapter contract; retrying
        // the same command would not establish that provider I/O is safe.
        GatewayError::Malformed(_) => SubscriptionBillingServiceErrorDisposition::Internal,
        GatewayError::Configuration(_) => SubscriptionBillingServiceErrorDisposition::Misconfigured,
        GatewayError::Unavailable(_) | GatewayError::RateLimited(_) => {
            SubscriptionBillingServiceErrorDisposition::TemporarilyUnavailable
        }
    }
}

const fn gateway_not_submitted_disposition(
    error: &GatewayNotSubmittedError,
) -> SubscriptionBillingServiceErrorDisposition {
    match error {
        GatewayNotSubmittedError::RequestRejected(_) => {
            SubscriptionBillingServiceErrorDisposition::Rejected
        }
        // A malformed request was definitely not submitted, but it still
        // indicates a service/adapter contract fault rather than a retryable
        // transport condition.
        GatewayNotSubmittedError::Malformed(_) => {
            SubscriptionBillingServiceErrorDisposition::Internal
        }
        GatewayNotSubmittedError::Configuration(_) => {
            SubscriptionBillingServiceErrorDisposition::Misconfigured
        }
        GatewayNotSubmittedError::Unavailable(_) | GatewayNotSubmittedError::RateLimited(_) => {
            SubscriptionBillingServiceErrorDisposition::TemporarilyUnavailable
        }
    }
}

const fn enrollment_reservation_rejection_disposition(
    rejection: SubscriptionEnrollmentReservationRejection,
) -> SubscriptionBillingServiceErrorDisposition {
    match rejection {
        SubscriptionEnrollmentReservationRejection::CurrentSubscription
        | SubscriptionEnrollmentReservationRejection::ActiveGrant
        | SubscriptionEnrollmentReservationRejection::UnresolvedProcessorCharge
        | SubscriptionEnrollmentReservationRejection::AttemptInProgress => {
            SubscriptionBillingServiceErrorDisposition::Rejected
        }
        SubscriptionEnrollmentReservationRejection::EnrollmentTermsChanged
        | SubscriptionEnrollmentReservationRejection::GatewayConfigurationChanged => {
            SubscriptionBillingServiceErrorDisposition::Conflict
        }
    }
}

const fn enrollment_submission_rejection_disposition(
    rejection: SubscriptionEnrollmentSubmissionRejection,
) -> SubscriptionBillingServiceErrorDisposition {
    match rejection {
        SubscriptionEnrollmentSubmissionRejection::BillingStateChanged
        | SubscriptionEnrollmentSubmissionRejection::EnrollmentTermsChanged
        | SubscriptionEnrollmentSubmissionRejection::GatewayConfigurationChanged => {
            SubscriptionBillingServiceErrorDisposition::Conflict
        }
    }
}

const fn host_charge_rejection_disposition(
    rejection: HostChargeTargetRejection,
) -> SubscriptionBillingServiceErrorDisposition {
    match rejection {
        HostChargeTargetRejection::TargetUnavailable | HostChargeTargetRejection::LedgerUnsafe => {
            SubscriptionBillingServiceErrorDisposition::Rejected
        }
        HostChargeTargetRejection::ChargeChanged => {
            SubscriptionBillingServiceErrorDisposition::Conflict
        }
    }
}

const fn recovery_reservation_rejection_disposition(
    rejection: SubscriptionRecoveryReservationRejection,
) -> SubscriptionBillingServiceErrorDisposition {
    match rejection {
        SubscriptionRecoveryReservationRejection::SubscriptionNotFound
        | SubscriptionRecoveryReservationRejection::PaymentNotDue
        | SubscriptionRecoveryReservationRejection::AttemptInProgress
        | SubscriptionRecoveryReservationRejection::PaymentMethodUpdateInProgress => {
            SubscriptionBillingServiceErrorDisposition::Rejected
        }
        SubscriptionRecoveryReservationRejection::GatewayConfigurationChanged => {
            SubscriptionBillingServiceErrorDisposition::Conflict
        }
    }
}

const fn recovery_submission_rejection_disposition(
    rejection: SubscriptionRecoverySubmissionRejection,
) -> SubscriptionBillingServiceErrorDisposition {
    match rejection {
        SubscriptionRecoverySubmissionRejection::BillingStateChanged
        | SubscriptionRecoverySubmissionRejection::GatewayConfigurationChanged => {
            SubscriptionBillingServiceErrorDisposition::Conflict
        }
    }
}

const fn renewal_reservation_rejection_disposition(
    rejection: SubscriptionRenewalReservationRejection,
) -> SubscriptionBillingServiceErrorDisposition {
    match rejection {
        SubscriptionRenewalReservationRejection::SubscriptionNotFound
        | SubscriptionRenewalReservationRejection::PaymentNotDue
        | SubscriptionRenewalReservationRejection::AttemptInProgress
        | SubscriptionRenewalReservationRejection::PaymentMethodUpdateInProgress
        | SubscriptionRenewalReservationRejection::RetryBlocked => {
            SubscriptionBillingServiceErrorDisposition::Rejected
        }
        SubscriptionRenewalReservationRejection::GatewayConfigurationChanged => {
            SubscriptionBillingServiceErrorDisposition::Conflict
        }
    }
}

const fn payment_method_replacement_reservation_rejection_disposition(
    rejection: SubscriptionPaymentMethodReplacementRejection,
) -> SubscriptionBillingServiceErrorDisposition {
    match rejection {
        SubscriptionPaymentMethodReplacementRejection::SubscriptionNotFound
        | SubscriptionPaymentMethodReplacementRejection::SubscriptionIneligible
        | SubscriptionPaymentMethodReplacementRejection::ChargeAttemptInProgress
        | SubscriptionPaymentMethodReplacementRejection::PaymentMethodUpdateInProgress => {
            SubscriptionBillingServiceErrorDisposition::Rejected
        }
        SubscriptionPaymentMethodReplacementRejection::GatewayConfigurationChanged => {
            SubscriptionBillingServiceErrorDisposition::Conflict
        }
    }
}

const fn payment_method_replacement_submission_rejection_disposition(
    rejection: SubscriptionPaymentMethodReplacementSubmissionRejection,
) -> SubscriptionBillingServiceErrorDisposition {
    match rejection {
        SubscriptionPaymentMethodReplacementSubmissionRejection::BillingStateChanged
        | SubscriptionPaymentMethodReplacementSubmissionRejection::GatewayConfigurationChanged => {
            SubscriptionBillingServiceErrorDisposition::Conflict
        }
    }
}

#[derive(Clone)]
pub struct SubscriptionBillingService {
    pool: PgPool,
    offers: Arc<dyn SubscriptionOfferStore>,
    resolver: Arc<dyn GatewayResolver>,
    admission: Arc<dyn EndUserMutationAdmission>,
    coordinator: Arc<dyn BillingTransactionCoordinator>,
    host_charge_targets: Option<Arc<dyn HostChargeTargetStore>>,
}

impl SubscriptionBillingService {
    pub fn new(
        pool: PgPool,
        offers: Arc<dyn SubscriptionOfferStore>,
        resolver: Arc<dyn GatewayResolver>,
        admission: Arc<dyn EndUserMutationAdmission>,
        coordinator: Arc<dyn BillingTransactionCoordinator>,
    ) -> Self {
        Self {
            pool,
            offers,
            resolver,
            admission,
            coordinator,
            host_charge_targets: None,
        }
    }

    pub fn with_host_charge_targets(mut self, targets: Arc<dyn HostChargeTargetStore>) -> Self {
        self.host_charge_targets = Some(targets);
        self
    }
}

/// The subscriber-initiated reservation families share readiness resolution,
/// while keeping their operation-specific durable application functions
/// explicit.
enum SubscriberInitiatedReservation<'a> {
    Initial(&'a SubscriptionEnrollmentReservation),
    Recovery(&'a SubscriptionRecoveryReservation),
    PaymentMethodReplacement(&'a SubscriptionPaymentMethodReplacement),
}

impl SubscriberInitiatedReservation<'_> {
    async fn resolve_non_approved(
        self,
        pool: &PgPool,
        evidence: &ProcessorEvidence,
        code: PaymentResolutionCode,
        cooldown: Option<RateLimitCooldown>,
        boundary: OutcomeResolutionBoundary,
    ) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
        match self {
            Self::Initial(reservation) => {
                resolve_non_approved_outcome(
                    pool,
                    reservation,
                    evidence,
                    AttemptResolutionStatus::Failed,
                    Some(code),
                    cooldown,
                    boundary,
                )
                .await
            }
            Self::Recovery(reservation) => {
                resolve_recovery_non_approved_outcome(
                    pool,
                    reservation,
                    evidence,
                    AttemptResolutionStatus::Failed,
                    Some(code),
                    cooldown,
                    boundary,
                )
                .await
            }
            Self::PaymentMethodReplacement(reservation) => {
                resolve_payment_method_replacement_non_approved_outcome(
                    pool,
                    reservation,
                    evidence,
                    AttemptResolutionStatus::Failed,
                    Some(code),
                    cooldown,
                    boundary,
                )
                .await
            }
        }
    }
}

/// A closed representation of the only pre-submission readiness failures
/// shared by subscriber-initiated mutations.
enum SubscriberReadinessFailure {
    Cooldown(GatewayMutationCooldownScope),
    ProviderRateLimited(GatewayDiagnostic),
    LiveModeUnavailable,
}

impl SubscriberReadinessFailure {
    fn into_detail(self) -> GatewayDiagnostic {
        match self {
            Self::Cooldown(GatewayMutationCooldownScope::Account) => {
                GatewayDiagnostic::new("gateway account mutation cooldown is active")
            }
            Self::Cooldown(GatewayMutationCooldownScope::Provider) => {
                GatewayDiagnostic::new("gateway provider cooldown is active")
            }
            Self::ProviderRateLimited(detail) => detail,
            Self::LiveModeUnavailable => GatewayDiagnostic::new(LIVE_READINESS_FAILED_TEXT),
        }
    }

    const fn resolution_code(&self) -> PaymentResolutionCode {
        match self {
            Self::Cooldown(GatewayMutationCooldownScope::Account) => {
                PaymentResolutionCode::GatewayAccountMutationCooldownBeforeSubmission
            }
            Self::Cooldown(GatewayMutationCooldownScope::Provider)
            | Self::ProviderRateLimited(_) => {
                PaymentResolutionCode::GatewayProviderRateLimitedBeforeSubmission
            }
            Self::LiveModeUnavailable => {
                PaymentResolutionCode::GatewayLiveReadinessFailedBeforeSubmission
            }
        }
    }

    const fn cooldown(&self) -> Option<RateLimitCooldown> {
        match self {
            Self::ProviderRateLimited(_) => Some(RateLimitCooldown::Provider),
            Self::Cooldown(_) | Self::LiveModeUnavailable => None,
        }
    }

    const fn cooldown_error_scope(&self) -> Option<GatewayMutationCooldownScope> {
        match self {
            Self::Cooldown(scope) => Some(*scope),
            Self::ProviderRateLimited(_) => Some(GatewayMutationCooldownScope::Provider),
            Self::LiveModeUnavailable => None,
        }
    }
}

fn map_subscriber_mutation_admission(
    result: EndUserMutationAdmissionResult,
) -> Result<(), SubscriptionBillingServiceError> {
    match result {
        EndUserMutationAdmissionResult::Allowed => Ok(()),
        EndUserMutationAdmissionResult::Denied { retry_after } => {
            Err(SubscriptionBillingServiceError::AdmissionDenied {
                retry_after: retry_after.get(),
            })
        }
        EndUserMutationAdmissionResult::Timeout => {
            Err(SubscriptionBillingServiceError::AdmissionTimeout)
        }
        EndUserMutationAdmissionResult::Unavailable => {
            Err(SubscriptionBillingServiceError::AdmissionUnavailable)
        }
    }
}

async fn subscriber_gateway_readiness_failure(
    gateway: &syrup_rail::ResolvedGateway,
) -> Option<SubscriberReadinessFailure> {
    match gateway.account_mode().await {
        Ok(GatewayAccountMode::Live) => None,
        Ok(GatewayAccountMode::Test) => Some(SubscriberReadinessFailure::LiveModeUnavailable),
        Err(GatewayError::RateLimited(detail)) => {
            Some(SubscriberReadinessFailure::ProviderRateLimited(detail))
        }
        Err(_) => Some(SubscriberReadinessFailure::LiveModeUnavailable),
    }
}

fn preserve_concurrent_terminal_payment(
    payment: SubscriptionEnrollmentPaymentResult,
    error: GatewayNotSubmittedError,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionBillingServiceError> {
    if payment.attempt().state().resolution_code()
        == Some(crate::enrollment_application::not_submitted_resolution_code(&error))
    {
        Err(SubscriptionBillingServiceError::GatewayNotSubmitted(error))
    } else {
        Ok(payment)
    }
}

fn is_retryable_renewal_admission_error(error: &SubscriptionEnrollmentApplicationError) -> bool {
    let sqlstate = match error {
        SubscriptionEnrollmentApplicationError::Sql(sqlx::Error::Database(error)) => error.code(),
        SubscriptionEnrollmentApplicationError::Attempt(PaymentAttemptStoreError::Sql(
            sqlx::Error::Database(error),
        )) => error.code(),
        _ => None,
    };
    matches!(
        sqlstate.as_deref(),
        Some("40001" | "40P01" | "55P03" | "57014")
    )
}

struct GatewayAccountSnapshot {
    account_id: GatewayAccountId,
    provider_key: GatewayProviderKey,
}

/// Canonical identity the resolver must return for subscriber-initiated
/// gateway mutations.
struct ExpectedGatewayIdentity<'a> {
    billing_scope_id: BillingScopeId,
    gateway_account_id: GatewayAccountId,
    gateway_configuration_id: syrup_rail::GatewayConfigurationId,
    provider_key: &'a GatewayProviderKey,
}

impl<'a> ExpectedGatewayIdentity<'a> {
    const fn for_account(
        billing_scope_id: BillingScopeId,
        gateway_configuration_id: syrup_rail::GatewayConfigurationId,
        account: &'a GatewayAccountSnapshot,
    ) -> Self {
        Self {
            billing_scope_id,
            gateway_account_id: account.account_id,
            gateway_configuration_id,
            provider_key: &account.provider_key,
        }
    }

    fn matches(&self, gateway: &syrup_rail::ResolvedGateway) -> bool {
        self.matches_components(
            gateway.billing_scope_id(),
            gateway.gateway_account_id(),
            gateway.gateway_configuration_id(),
            gateway.provider_key(),
        )
    }

    fn matches_components(
        &self,
        billing_scope_id: BillingScopeId,
        gateway_account_id: GatewayAccountId,
        gateway_configuration_id: syrup_rail::GatewayConfigurationId,
        provider_key: &GatewayProviderKey,
    ) -> bool {
        billing_scope_id == self.billing_scope_id
            && gateway_account_id == self.gateway_account_id
            && gateway_configuration_id == self.gateway_configuration_id
            && provider_key == self.provider_key
    }
}

struct RenewalGatewayAccountSnapshot {
    account_id: GatewayAccountId,
    configuration_id: syrup_rail::GatewayConfigurationId,
    provider_key: GatewayProviderKey,
}

impl RenewalGatewayAccountSnapshot {
    fn as_gateway_snapshot(&self) -> GatewayAccountSnapshot {
        GatewayAccountSnapshot {
            account_id: self.account_id,
            provider_key: self.provider_key.clone(),
        }
    }
}

const fn map_reservation_build_error(
    error: SubscriptionEnrollmentReservationBuildError,
) -> SubscriptionBillingServiceError {
    match error {
        SubscriptionEnrollmentReservationBuildError::GatewayIdentityMismatch => {
            SubscriptionBillingServiceError::ResolvedGatewayIdentityMismatch
        }
        SubscriptionEnrollmentReservationBuildError::AttemptKindMismatch
        | SubscriptionEnrollmentReservationBuildError::InvalidCharge
        | SubscriptionEnrollmentReservationBuildError::InvalidTerms => {
            SubscriptionBillingServiceError::InvalidState(INVALID_SERVICE_STATE)
        }
    }
}

#[cfg(test)]
mod tests;
