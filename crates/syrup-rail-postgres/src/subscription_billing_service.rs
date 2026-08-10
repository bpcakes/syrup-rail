use std::{fmt, sync::Arc, time::Duration};

use sqlx::PgPool;
use syrup_rail::{
    BillingScopeId, ChargeHostTarget, ChargeRenewal, EndUserMutationAdmission,
    EndUserMutationAdmissionResult, EndUserMutationCommand, EndUserMutationOperation,
    EnrollSubscription, GatewayAccountId, GatewayAccountMode, GatewayDiagnostic, GatewayError,
    GatewayNotSubmittedError, GatewayPaymentDescriptor, GatewayPaymentOutcome, GatewayProviderKey,
    GatewayResolutionError, GatewayResolver, HostChargePaymentResult, HostChargeReservation,
    HostChargeTargetRejection, PaymentAttempt, PaymentAttemptId, PaymentAttemptKind,
    PaymentAttemptStatus, PaymentResolutionCode, ProcessorEvidence, RecoverSubscriptionPayment,
    ReplaceSubscriptionPaymentMethod, SubscriptionEnrollmentPaymentResult,
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

const INVALID_SERVICE_STATE: &str = "canonical subscription enrollment service state is invalid";
const LIVE_READINESS_FAILED_TEXT: &str =
    "Payment was not submitted because the payment processor was not ready for live transactions.";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GatewayMutationCooldownScope {
    Account,
    Provider,
}

#[derive(Error)]
pub enum SubscriptionEnrollmentServiceError {
    #[error("subscription enrollment storage failed")]
    Sql(#[from] sqlx::Error),
    #[error("subscription enrollment attempt storage failed")]
    Attempt(#[from] PaymentAttemptStoreError),
    #[error("subscription enrollment application failed")]
    Application(#[from] SubscriptionEnrollmentApplicationError),
    #[error("host charge application failed")]
    HostChargeApplication(#[from] HostChargeApplicationError),
    #[error("host charge storage failed")]
    HostChargeStore(#[from] HostChargeStoreError),
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

impl fmt::Debug for SubscriptionEnrollmentServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sql(_) => formatter.write_str("SubscriptionEnrollmentServiceError::Sql"),
            Self::Attempt(_) => formatter.write_str("SubscriptionEnrollmentServiceError::Attempt"),
            Self::Application(_) => {
                formatter.write_str("SubscriptionEnrollmentServiceError::Application")
            }
            Self::HostChargeApplication(_) => {
                formatter.write_str("SubscriptionEnrollmentServiceError::HostChargeApplication")
            }
            Self::HostChargeStore(_) => {
                formatter.write_str("SubscriptionEnrollmentServiceError::HostChargeStore")
            }
            Self::HostChargeUnavailable => formatter
                .write_str("SubscriptionEnrollmentServiceError::HostChargeUnavailable"),
            Self::IdempotencyConflict => {
                formatter.write_str("SubscriptionEnrollmentServiceError::IdempotencyConflict")
            }
            Self::AdmissionDenied { retry_after } => formatter
                .debug_struct("SubscriptionEnrollmentServiceError::AdmissionDenied")
                .field("retry_after", retry_after)
                .finish(),
            Self::AdmissionTimeout => {
                formatter.write_str("SubscriptionEnrollmentServiceError::AdmissionTimeout")
            }
            Self::AdmissionUnavailable => {
                formatter.write_str("SubscriptionEnrollmentServiceError::AdmissionUnavailable")
            }
            Self::GatewayConfigurationChanged => formatter
                .write_str("SubscriptionEnrollmentServiceError::GatewayConfigurationChanged"),
            Self::GatewayResolution(error) => formatter
                .debug_tuple("SubscriptionEnrollmentServiceError::GatewayResolution")
                .field(error)
                .finish(),
            Self::ResolvedGatewayIdentityMismatch => formatter
                .write_str("SubscriptionEnrollmentServiceError::ResolvedGatewayIdentityMismatch"),
            Self::GatewayMutationCooldown { scope } => formatter
                .debug_struct("SubscriptionEnrollmentServiceError::GatewayMutationCooldown")
                .field("scope", scope)
                .finish(),
            Self::ReservationRejected(reason) => formatter
                .debug_tuple("SubscriptionEnrollmentServiceError::ReservationRejected")
                .field(reason)
                .finish(),
            Self::SubmissionRejected(reason) => formatter
                .debug_tuple("SubscriptionEnrollmentServiceError::SubmissionRejected")
                .field(reason)
                .finish(),
            Self::HostChargeReservationRejected(reason) => formatter
                .debug_tuple("SubscriptionEnrollmentServiceError::HostChargeReservationRejected")
                .field(reason)
                .finish(),
            Self::HostChargeSubmissionRejected(reason) => formatter
                .debug_tuple("SubscriptionEnrollmentServiceError::HostChargeSubmissionRejected")
                .field(reason)
                .finish(),
            Self::RecoveryReservationRejected(reason) => formatter
                .debug_tuple("SubscriptionEnrollmentServiceError::RecoveryReservationRejected")
                .field(reason)
                .finish(),
            Self::RecoverySubmissionRejected(reason) => formatter
                .debug_tuple("SubscriptionEnrollmentServiceError::RecoverySubmissionRejected")
                .field(reason)
                .finish(),
            Self::RenewalReservationRejected(reason) => formatter
                .debug_tuple("SubscriptionEnrollmentServiceError::RenewalReservationRejected")
                .field(reason)
                .finish(),
            Self::PaymentMethodReplacementReservationRejected(reason) => formatter
                .debug_tuple(
                    "SubscriptionEnrollmentServiceError::PaymentMethodReplacementReservationRejected",
                )
                .field(reason)
                .finish(),
            Self::PaymentMethodReplacementSubmissionRejected(reason) => formatter
                .debug_tuple(
                    "SubscriptionEnrollmentServiceError::PaymentMethodReplacementSubmissionRejected",
                )
                .field(reason)
                .finish(),
            Self::GatewayNotSubmitted(error) => formatter
                .debug_tuple("SubscriptionEnrollmentServiceError::GatewayNotSubmitted")
                .field(error)
                .finish(),
            Self::GatewayReadiness(error) => formatter
                .debug_tuple("SubscriptionEnrollmentServiceError::GatewayReadiness")
                .field(error)
                .finish(),
            Self::InvalidState(detail) => formatter
                .debug_tuple("SubscriptionEnrollmentServiceError::InvalidState")
                .field(detail)
                .finish(),
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
) -> Result<(), SubscriptionEnrollmentServiceError> {
    match result {
        EndUserMutationAdmissionResult::Allowed => Ok(()),
        EndUserMutationAdmissionResult::Denied { retry_after } => {
            Err(SubscriptionEnrollmentServiceError::AdmissionDenied {
                retry_after: retry_after.get(),
            })
        }
        EndUserMutationAdmissionResult::Timeout => {
            Err(SubscriptionEnrollmentServiceError::AdmissionTimeout)
        }
        EndUserMutationAdmissionResult::Unavailable => {
            Err(SubscriptionEnrollmentServiceError::AdmissionUnavailable)
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
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentServiceError> {
    if payment.attempt().state().resolution_code()
        == Some(crate::enrollment_application::not_submitted_resolution_code(&error))
    {
        Err(SubscriptionEnrollmentServiceError::GatewayNotSubmitted(
            error,
        ))
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
) -> SubscriptionEnrollmentServiceError {
    match error {
        SubscriptionEnrollmentReservationBuildError::GatewayIdentityMismatch => {
            SubscriptionEnrollmentServiceError::ResolvedGatewayIdentityMismatch
        }
        SubscriptionEnrollmentReservationBuildError::AttemptKindMismatch
        | SubscriptionEnrollmentReservationBuildError::InvalidCharge
        | SubscriptionEnrollmentReservationBuildError::InvalidTerms => {
            SubscriptionEnrollmentServiceError::InvalidState(INVALID_SERVICE_STATE)
        }
    }
}

#[cfg(test)]
mod tests;
