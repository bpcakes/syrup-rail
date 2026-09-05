#![warn(missing_docs)]

use crate::transaction_support::is_transient_sqlstate;

use std::{fmt, sync::Arc, time::Duration};

use sqlx::PgPool;
use syrup_rail::{
    BillingEventSubject, BillingScopeId, CancelSubscription, CancelSubscriptionOutcome,
    ChargeHostTarget, ChargeRenewal, ClearSubscriptionDiscount, EndUserMutationAdmission,
    EndUserMutationAdmissionResult, EndUserMutationCommand, EndUserMutationOperation,
    EnrollSubscription, GatewayAccountId, GatewayAccountIdentity, GatewayAccountMode,
    GatewayDiagnostic, GatewayError, GatewayNotSubmittedError, GatewayPaymentDescriptor,
    GatewayPaymentOutcome, GatewayProviderKey, GatewayResolutionError, GatewayResolver,
    HostChargePaymentResult, HostChargeReservation, HostChargeTargetRejection, PaymentAttempt,
    PaymentAttemptId, PaymentAttemptKind, PaymentAttemptStatus, PaymentResolutionCode,
    ProcessorEvidence, RecoverSubscriptionPayment, ReplaceSubscriptionPaymentMethod,
    SubscriptionDiscountClaim, SubscriptionDiscountClaimOutcome, SubscriptionDiscountClearOutcome,
    SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentPreflightOutcome,
    SubscriptionEnrollmentReservation, SubscriptionEnrollmentReservationBuildError,
    SubscriptionEnrollmentReservationOutcome, SubscriptionEnrollmentReservationRejection,
    SubscriptionEnrollmentSubmissionRejection, SubscriptionPaymentMethodReplacement,
    SubscriptionPaymentMethodReplacementPreflightOutcome,
    SubscriptionPaymentMethodReplacementRejection,
    SubscriptionPaymentMethodReplacementReservationOutcome,
    SubscriptionPaymentMethodReplacementSubmissionRejection, SubscriptionRecoveryPreflightOutcome,
    SubscriptionRecoveryReservation, SubscriptionRecoveryReservationOutcome,
    SubscriptionRecoveryReservationRejection, SubscriptionRecoverySubmissionRejection,
    SubscriptionRenewalOutcome, SubscriptionRenewalReservation,
    SubscriptionRenewalReservationOutcome, SubscriptionRenewalReservationRejection,
};
use thiserror::Error;

use crate::enrollment_application::GatewayNotSubmittedPolicy;
use crate::host_charge_application::{
    HostChargeBeforeSubmissionResolution, resolve_host_charge_before_submission,
};
use crate::mode_verified_gateway::gateway_account_mode_mismatch_detail;
use crate::{
    BillingTransactionCoordinator, GatewayAccountModeVerificationError, HostChargeAdmissionOutcome,
    HostChargeApplicationError, HostChargePreflightOutcome, HostChargeProviderResult,
    HostChargeReservationOutcome, HostChargeStoreError, HostChargeTargetStore, ModeVerifiedGateway,
    PaymentAttemptStoreError, SubscriptionEnrollmentAdmissionOutcome,
    SubscriptionEnrollmentApplicationError, SubscriptionEnrollmentProviderResult,
    SubscriptionOfferStore, SubscriptionPaymentMethodReplacementAdmissionOutcome,
    SubscriptionPaymentMethodReplacementProviderResult, SubscriptionRecoveryAdmissionOutcome,
    SubscriptionRecoveryProviderResult, SubscriptionRenewalAdmissionOutcome,
    SubscriptionRenewalProviderResult, admit_host_charge_submission,
    admit_subscription_enrollment_submission, admit_subscription_payment_method_replacement,
    admit_subscription_recovery_submission, admit_subscription_renewal_submission,
    apply_reconciled_host_charge_gateway_outcome,
    attempts::{
        AttemptReplayDisposition, AttemptResolutionStatus, LocalAttemptPolicy,
        attempt_replay_disposition,
    },
    enrollment_application::{
        OutcomeApplication, OutcomeResolutionBoundary, OutcomeResolutionCommand,
        RECONCILED_SUBSCRIPTION_PAYMENT_ATTEMPT_NOT_FOUND, RateLimitCooldown,
        RateLimitCooldownPersistence, apply_reconciled_subscription_gateway_outcome,
        payment_result_for_attempt, persist_bound_provider_rate_limit_cooldown,
        resolve_non_approved_outcome, resolve_payment_method_replacement_non_approved_outcome,
        resolve_recovery_non_approved_outcome, resolve_renewal_non_approved_outcome,
        set_application_timeouts,
    },
    preflight_host_charge_in_transaction, preflight_subscription_enrollment_in_transaction,
    preflight_subscription_payment_method_replacement_in_transaction,
    preflight_subscription_recovery_in_transaction, reserve_host_charge_in_transaction,
    reserve_subscription_enrollment_in_transaction,
    reserve_subscription_payment_method_replacement_in_transaction,
    reserve_subscription_recovery_in_transaction, reserve_subscription_renewal_in_transaction,
    submit_admitted_host_charge, submit_admitted_subscription_enrollment,
    submit_admitted_subscription_payment_method_replacement, submit_admitted_subscription_recovery,
    submit_admitted_subscription_renewal, verify_gateway_account_mode,
};

mod enrollment;
mod error_disposition;
mod host_charge;
mod payment_method_replacement;
mod reconciliation;
mod recovery;
mod renewal;
mod subscriber;
mod subscriber_mutation;

const INVALID_SERVICE_STATE: &str = "canonical subscription billing service state is invalid";

/// Canonical cooldown level that stopped a gateway mutation before submission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GatewayMutationCooldownScope {
    /// Cooldown applies only to the resolved gateway account.
    Account,
    /// Cooldown applies to every configured account for the provider.
    Provider,
}

impl GatewayMutationCooldownScope {
    const fn from_rate_limit_cooldown(cooldown: RateLimitCooldown) -> Self {
        match cooldown {
            RateLimitCooldown::Account => Self::Account,
            RateLimitCooldown::Provider => Self::Provider,
        }
    }
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
    /// Generic SQL failure whose retry safety is not proven by the owning path.
    #[error("subscription billing storage failed")]
    Sql(#[from] sqlx::Error),
    /// A provider-free local transaction could not acquire capacity or failed
    /// with an explicitly recognized transient SQLSTATE. Replaying the same
    /// idempotent operation after the failed transaction is discarded is safe.
    #[error("subscription billing storage is temporarily unavailable")]
    StorageTemporarilyUnavailable(#[source] sqlx::Error),
    /// Canonical payment-attempt persistence failed.
    #[error("payment attempt storage failed")]
    Attempt(#[from] PaymentAttemptStoreError),
    /// A subscription payment outcome could not be applied atomically.
    #[error("subscription payment application failed")]
    Application(#[from] SubscriptionEnrollmentApplicationError),
    /// A host-charge outcome could not be applied atomically.
    #[error("host charge application failed")]
    HostChargeApplication(#[from] HostChargeApplicationError),
    /// Canonical host-charge reservation or ledger storage failed.
    #[error("host charge storage failed")]
    HostChargeStore(#[from] HostChargeStoreError),
    /// Subscriber cancellation failed.
    #[error("subscription cancellation failed")]
    Cancellation(#[source] crate::SubscriptionCancellationError),
    /// Subscriber discount mutation failed.
    #[error("subscription discount operation failed")]
    Discount(#[source] crate::SubscriptionDiscountOperationError),
    /// The host-prepared billing transaction failed.
    #[error("host billing transaction failed")]
    BillingTransaction(#[from] crate::BillingTransactionError),
    /// The typed event could not be appended to the host outbox.
    #[error("host billing event append failed")]
    BillingEvent(#[from] crate::BillingEventWriteError),
    /// The optional host-charge target capability was not supplied.
    #[error("host charge capability is not configured")]
    HostChargeUnavailable,
    /// The idempotency key already owns a different immutable request.
    #[error("the idempotency key belongs to a different payment request")]
    IdempotencyConflict,
    /// Host admission denied the command with an exact retry delay.
    #[error("end-user mutation admission was denied")]
    AdmissionDenied {
        /// Exact delay supplied by the host admission implementation.
        retry_after: Duration,
    },
    /// Host admission did not complete before its bound.
    #[error("end-user mutation admission timed out")]
    AdmissionTimeout,
    /// Host admission could not evaluate the command.
    #[error("end-user mutation admission is unavailable")]
    AdmissionUnavailable,
    /// Durable gateway authority changed after the command was prepared.
    #[error("gateway account or configuration changed")]
    GatewayConfigurationChanged,
    /// The host could not resolve canonical gateway authority.
    #[error("gateway resolution failed")]
    GatewayResolution(#[from] GatewayResolutionError),
    /// The resolver returned a gateway for a different canonical identity.
    #[error("gateway resolver returned a different canonical identity")]
    ResolvedGatewayIdentityMismatch,
    /// A durable account or provider cooldown stopped submission.
    #[error("gateway mutation cooldown is active")]
    GatewayMutationCooldown {
        /// Canonical cooldown level that stopped the command.
        scope: GatewayMutationCooldownScope,
    },
    /// Initial-enrollment reservation returned a semantic blocker.
    #[error("subscription enrollment reservation was rejected")]
    ReservationRejected(SubscriptionEnrollmentReservationRejection),
    /// Initial-enrollment final admission returned a semantic blocker.
    #[error("subscription enrollment submission was rejected")]
    SubmissionRejected(SubscriptionEnrollmentSubmissionRejection),
    /// Host-charge reservation returned a semantic blocker.
    #[error("host charge reservation was rejected")]
    HostChargeReservationRejected(HostChargeTargetRejection),
    /// Host-charge final admission returned a semantic blocker.
    #[error("host charge submission was rejected")]
    HostChargeSubmissionRejected(HostChargeTargetRejection),
    /// Subscriber recovery reservation returned a semantic blocker.
    #[error("subscription recovery reservation was rejected")]
    RecoveryReservationRejected(SubscriptionRecoveryReservationRejection),
    /// Subscriber recovery final admission returned a semantic blocker.
    #[error("subscription recovery submission was rejected")]
    RecoverySubmissionRejected(SubscriptionRecoverySubmissionRejection),
    /// Automatic-renewal reservation returned a semantic blocker.
    #[error("subscription renewal reservation was rejected")]
    RenewalReservationRejected(SubscriptionRenewalReservationRejection),
    /// Payment-method replacement reservation returned a semantic blocker.
    #[error("subscription payment method replacement reservation was rejected")]
    PaymentMethodReplacementReservationRejected(SubscriptionPaymentMethodReplacementRejection),
    /// Payment-method replacement final admission returned a semantic blocker.
    #[error("subscription payment method replacement submission was rejected")]
    PaymentMethodReplacementSubmissionRejected(
        SubscriptionPaymentMethodReplacementSubmissionRejection,
    ),
    /// Gateway adaptation proved that the mutation was not submitted.
    #[error("gateway mutation was not submitted")]
    GatewayNotSubmitted(#[source] GatewayNotSubmittedError),
    /// The gateway readiness query failed before mutation submission.
    ///
    /// This error does not by itself prove that no durable attempt exists. If
    /// readiness fails after reservation, transient unavailability leaves the
    /// prepared attempt retryable, while a determinate readiness failure may
    /// terminalize it with an exact resolution code before returning this
    /// error. Replaying the same command and idempotency key recovers that
    /// canonical result before admission, resolution, or provider I/O.
    #[error("gateway readiness check failed")]
    GatewayReadiness(#[source] GatewayError),
    /// Durable canonical state violated an invariant required by the facade.
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
        .is_some_and(|code| is_transient_sqlstate(code.as_ref()))
}

/// High-level provider-neutral billing facade for authorized host commands.
///
/// The service owns orchestration and retry classification. Hosts retain
/// authentication, authorization, offer rows, credentials, abuse admission,
/// and the transaction/outbox boundary supplied at construction.
#[derive(Clone)]
pub struct SubscriptionBillingService {
    pool: PgPool,
    offers: Arc<dyn SubscriptionOfferStore>,
    resolver: Arc<dyn GatewayResolver>,
    admission: Arc<dyn EndUserMutationAdmission>,
    coordinator: Arc<dyn BillingTransactionCoordinator>,
    required_gateway_account_mode: GatewayAccountMode,
    host_charge_targets: Option<Arc<dyn HostChargeTargetStore>>,
}

impl SubscriptionBillingService {
    /// Creates a service without the optional host-charge target capability.
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
            required_gateway_account_mode: GatewayAccountMode::Live,
            host_charge_targets: None,
        }
    }

    /// Requires an exact gateway account mode before any provider mutation.
    ///
    /// The default is [`GatewayAccountMode::Live`]. Selecting
    /// [`GatewayAccountMode::Test`] permits test-mode mutations and rejects a
    /// live account before submission. Hosts should bind this requirement to
    /// their trusted deployment environment, never to end-user input.
    ///
    /// This service setting does not automatically partition entitlement or
    /// billing-portal reads. Test-mode subscriptions are ordinary paid
    /// subscriptions to the domain model and can satisfy
    /// `Entitlement::permits_product_access`; constrain `EntitlementQuery` and
    /// `EntitlementGuard` separately when modes share a database, and enforce
    /// any remaining environment isolation before granting production access.
    pub fn with_required_gateway_account_mode(mut self, mode: GatewayAccountMode) -> Self {
        self.required_gateway_account_mode = mode;
        self
    }

    /// Adds the optional host-charge target store to this service instance.
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
    Gateway(GatewayError),
    AccountMode(GatewayAccountMode),
}

/// Failures produced by the gateway readiness check itself. Cooldown is
/// detected by a separate persistence-backed check and therefore cannot occur
/// here.
enum GatewayReadinessFailure {
    Gateway(GatewayError),
    AccountMode(GatewayAccountMode),
}

impl From<GatewayReadinessFailure> for SubscriberReadinessFailure {
    fn from(failure: GatewayReadinessFailure) -> Self {
        match failure {
            GatewayReadinessFailure::Gateway(error) => Self::Gateway(error),
            GatewayReadinessFailure::AccountMode(mode) => Self::AccountMode(mode),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SubscriberReadinessPolicy {
    resolution_code: PaymentResolutionCode,
    cooldown: Option<RateLimitCooldown>,
    cooldown_error_scope: Option<GatewayMutationCooldownScope>,
}

impl SubscriberReadinessPolicy {
    const fn resolution_code(self) -> PaymentResolutionCode {
        self.resolution_code
    }

    const fn cooldown(self) -> Option<RateLimitCooldown> {
        self.cooldown
    }

    const fn cooldown_error_scope(self) -> Option<GatewayMutationCooldownScope> {
        self.cooldown_error_scope
    }
}

impl SubscriberReadinessFailure {
    fn gateway_error(&self) -> Option<GatewayError> {
        let Self::Gateway(error) = self else {
            return None;
        };
        Some(clone_gateway_error(error))
    }

    fn into_detail(self) -> GatewayDiagnostic {
        match self {
            Self::Cooldown(GatewayMutationCooldownScope::Account) => {
                GatewayDiagnostic::new("gateway account mutation cooldown is active")
            }
            Self::Cooldown(GatewayMutationCooldownScope::Provider) => {
                GatewayDiagnostic::new("gateway provider cooldown is active")
            }
            Self::Gateway(error) => error.detail().clone(),
            Self::AccountMode(_) => gateway_account_mode_mismatch_detail(),
        }
    }

    const fn policy(&self) -> SubscriberReadinessPolicy {
        match self {
            Self::Cooldown(GatewayMutationCooldownScope::Account) => SubscriberReadinessPolicy {
                resolution_code:
                    PaymentResolutionCode::GatewayAccountMutationCooldownBeforeSubmission,
                cooldown: None,
                cooldown_error_scope: Some(GatewayMutationCooldownScope::Account),
            },
            Self::Cooldown(GatewayMutationCooldownScope::Provider) => SubscriberReadinessPolicy {
                resolution_code: PaymentResolutionCode::GatewayProviderRateLimitedBeforeSubmission,
                cooldown: None,
                cooldown_error_scope: Some(GatewayMutationCooldownScope::Provider),
            },
            Self::Gateway(error) => {
                let policy = GatewayNotSubmittedPolicy::for_readiness_error(error);
                let cooldown = policy.cooldown();
                SubscriberReadinessPolicy {
                    resolution_code: policy.resolution_code(),
                    cooldown,
                    cooldown_error_scope: match cooldown {
                        Some(cooldown) => Some(
                            GatewayMutationCooldownScope::from_rate_limit_cooldown(cooldown),
                        ),
                        None => None,
                    },
                }
            }
            Self::AccountMode(required) => {
                let policy = GatewayNotSubmittedPolicy::for_account_mode_mismatch(*required);
                SubscriberReadinessPolicy {
                    resolution_code: policy.resolution_code(),
                    cooldown: policy.cooldown(),
                    cooldown_error_scope: None,
                }
            }
        }
    }
}

fn clone_gateway_error(error: &GatewayError) -> GatewayError {
    match error {
        GatewayError::RequestRejected(detail) => GatewayError::RequestRejected(detail.clone()),
        GatewayError::Malformed(detail) => GatewayError::Malformed(detail.clone()),
        GatewayError::Configuration(detail) => GatewayError::Configuration(detail.clone()),
        GatewayError::Unavailable(detail) => GatewayError::Unavailable(detail.clone()),
        GatewayError::RateLimited(detail) => GatewayError::RateLimited(detail.clone()),
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

async fn subscriber_gateway_readiness(
    gateway: &syrup_rail::ResolvedGateway,
    required_mode: GatewayAccountMode,
) -> Result<ModeVerifiedGateway<'_>, GatewayReadinessFailure> {
    match verify_gateway_account_mode(gateway, required_mode).await {
        Ok(verified) => Ok(verified),
        Err(GatewayAccountModeVerificationError::AccountModeMismatch { required, .. }) => {
            Err(GatewayReadinessFailure::AccountMode(required))
        }
        Err(GatewayAccountModeVerificationError::Gateway(error)) => {
            Err(GatewayReadinessFailure::Gateway(error))
        }
    }
}

fn attempt_is_prepared(attempt: &PaymentAttempt) -> bool {
    attempt_replay_disposition(attempt) == AttemptReplayDisposition::ResumePrepared
}

fn resolved_gateway_matches_attempt(
    gateway: &syrup_rail::ResolvedGateway,
    attempt: &PaymentAttempt,
) -> bool {
    let identity = attempt.identity();
    gateway.billing_scope_id() == identity.billing_scope_id()
        && gateway.gateway_account_id() == identity.gateway_account_id()
        && gateway.gateway_configuration_id() == identity.gateway_configuration_id()
}

fn is_retryable_renewal_admission_error(error: &SubscriptionEnrollmentApplicationError) -> bool {
    let sqlstate = match error {
        SubscriptionEnrollmentApplicationError::Sql(sqlx::Error::Database(error)) => error.code(),
        SubscriptionEnrollmentApplicationError::Attempt(PaymentAttemptStoreError::Sql(
            sqlx::Error::Database(error),
        )) => error.code(),
        _ => None,
    };
    sqlstate.as_deref().is_some_and(is_transient_sqlstate)
}

struct GatewayAccountSnapshot {
    identity: GatewayAccountIdentity,
}

struct RenewalGatewayAccountSnapshot {
    billing_scope_id: BillingScopeId,
    account_id: GatewayAccountId,
    configuration_id: syrup_rail::GatewayConfigurationId,
    provider_key: GatewayProviderKey,
}

impl RenewalGatewayAccountSnapshot {
    fn as_gateway_snapshot(&self) -> GatewayAccountSnapshot {
        GatewayAccountSnapshot {
            identity: GatewayAccountIdentity::new(
                self.billing_scope_id,
                self.account_id,
                self.provider_key.clone(),
                self.configuration_id,
            ),
        }
    }
}

impl GatewayAccountSnapshot {
    const fn identity(&self) -> &GatewayAccountIdentity {
        &self.identity
    }

    const fn account_id(&self) -> GatewayAccountId {
        self.identity.gateway_account_id()
    }

    const fn provider_key(&self) -> &GatewayProviderKey {
        self.identity.provider_key()
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
