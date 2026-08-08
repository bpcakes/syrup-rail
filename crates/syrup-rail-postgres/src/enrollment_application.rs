use std::{fmt, time::Duration};

use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool, Row};
use syrup_rail::{
    BillingEvent, BillingEventSubject, BillingPeriod, BillingScopeId, EnrollSubscription,
    GatewayDiagnostic, GatewayMutationError, GatewayNotSubmittedError, GatewayPaymentDescriptor,
    GatewayPaymentOutcome, GatewayPaymentStatus, GatewayProviderKey, GatewaySaleIntent,
    GatewaySaleRequest, GatewayStorePaymentMethodRequest, GatewayTransactionId, PaymentAttempt,
    PaymentAttemptId, PaymentAttemptKind, PaymentAttemptStatus, PaymentCardDisplay,
    PaymentMethodId, PaymentResolutionCode, PlanKey, ProcessorEvidence, RecoverSubscriptionPayment,
    ReplaceSubscriptionPaymentMethod, ResolvedGateway, SubscriberId, Subscription,
    SubscriptionDiscountDuration, SubscriptionDiscountKind, SubscriptionEnrollmentPaymentResult,
    SubscriptionEnrollmentReservation, SubscriptionEnrollmentSubmissionOutcome,
    SubscriptionEnrollmentSubmissionRejection, SubscriptionId,
    SubscriptionPaymentMethodReplacement, SubscriptionPaymentMethodReplacementSubmissionOutcome,
    SubscriptionPaymentMethodReplacementSubmissionRejection, SubscriptionRecoveryReservation,
    SubscriptionRecoverySubmissionOutcome, SubscriptionRecoverySubmissionRejection,
    SubscriptionStatus, next_monthly_billing_period,
};
use thiserror::Error;
use uuid::Uuid;

use crate::{
    BillingTransactionCoordinator, BillingTransactionError, BillingTransactionSubjectState,
    attempts::{
        PaymentAttemptStoreError, find_payment_attempt_by_id_on_connection,
        lock_payment_attempt_by_id_on_connection,
    },
};

const BILLING_LOCK_TIMEOUT: Duration = Duration::from_millis(250);
const BILLING_ROW_LOCK_TIMEOUT: &str = "250ms";
const BILLING_OPERATION_TIMEOUT: &str = "5s";
const APPROVED_EVIDENCE_WRITE_ATTEMPTS: usize = 3;
const APPROVED_EVIDENCE_RETRY_DELAY: Duration = Duration::from_millis(50);
const APPROVED_APPLICATION_ATTEMPTS: usize = 3;
const PROVIDER_RATE_LIMIT_RETRY_AFTER_SECONDS: i64 = 60;
const INVALID_APPLICATION_STATE: &str = "canonical initial-enrollment application state is invalid";
const CURRENT_SUBSCRIPTION_CONFLICT_TEXT: &str =
    "Approved subscription enrollment conflicts with a current subscription.";
const CURRENT_GRANT_CONFLICT_TEXT: &str =
    "Approved subscription enrollment conflicts with an active subscription grant.";
const INCOMPLETE_APPROVAL_TEXT: &str =
    "Approved subscription enrollment is missing required processor identity.";
const APPROVED_STORAGE_FAILURE_TEXT: &str =
    "Approved subscription enrollment could not be applied; manual review is required.";
const TERMINAL_APPROVAL_RACE_TEXT: &str =
    "Approved processor evidence arrived after the enrollment attempt became terminal.";
const RECOVERY_INCOMPLETE_APPROVAL_TEXT: &str =
    "Approved subscription recovery is missing required processor identity.";
const RECOVERY_APPROVED_STORAGE_FAILURE_TEXT: &str =
    "Approved subscription recovery could not be applied; manual review is required.";
const RECOVERY_STALE_STATE_TEXT: &str = "Approved subscription recovery could not update billing state because the subscription changed.";
const PAYMENT_METHOD_REPLACEMENT_INCOMPLETE_APPROVAL_TEXT: &str =
    "Approved payment method replacement is missing required processor identity.";
const PAYMENT_METHOD_REPLACEMENT_STORAGE_FAILURE_TEXT: &str =
    "Approved payment method replacement could not be applied; manual review is required.";
const PAYMENT_METHOD_REPLACEMENT_STALE_STATE_TEXT: &str =
    "Approved payment method replacement could not attach because the subscription changed.";

#[derive(Error)]
pub enum SubscriptionEnrollmentApplicationError {
    #[error("subscription enrollment application storage failed")]
    Sql(#[from] sqlx::Error),
    #[error("subscription enrollment attempt storage failed")]
    Attempt(#[from] PaymentAttemptStoreError),
    #[error("host billing transaction failed")]
    Transaction(#[from] BillingTransactionError),
    #[error("host billing event append failed")]
    Event(#[from] crate::BillingEventWriteError),
    #[error("approved subscription enrollment could not be durably applied or parked")]
    ApprovedEvidenceNotDurable,
    #[error("admitted subscription enrollment does not match the submission command or gateway")]
    SubmissionIdentityMismatch,
    #[error("{0}")]
    InvalidState(&'static str),
}

impl fmt::Debug for SubscriptionEnrollmentApplicationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sql(_) => formatter.write_str("SubscriptionEnrollmentApplicationError::Sql"),
            Self::Attempt(_) => {
                formatter.write_str("SubscriptionEnrollmentApplicationError::Attempt")
            }
            Self::Transaction(_) => {
                formatter.write_str("SubscriptionEnrollmentApplicationError::Transaction")
            }
            Self::Event(_) => formatter.write_str("SubscriptionEnrollmentApplicationError::Event"),
            Self::ApprovedEvidenceNotDurable => formatter
                .write_str("SubscriptionEnrollmentApplicationError::ApprovedEvidenceNotDurable"),
            Self::SubmissionIdentityMismatch => formatter
                .write_str("SubscriptionEnrollmentApplicationError::SubmissionIdentityMismatch"),
            Self::InvalidState(detail) => formatter
                .debug_tuple("SubscriptionEnrollmentApplicationError::InvalidState")
                .field(detail)
                .finish(),
        }
    }
}

/// One committed final-admission result that authorizes exactly one immediate
/// provider submission by consuming this value.
pub struct AdmittedSubscriptionEnrollment {
    reservation: SubscriptionEnrollmentReservation,
    attempt: PaymentAttempt,
}

impl AdmittedSubscriptionEnrollment {
    pub const fn attempt(&self) -> &PaymentAttempt {
        &self.attempt
    }
}

impl fmt::Debug for AdmittedSubscriptionEnrollment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AdmittedSubscriptionEnrollment")
            .field("attempt", &self.attempt)
            .field("has_submission_authority", &true)
            .finish()
    }
}

#[derive(Debug)]
pub enum SubscriptionEnrollmentAdmissionOutcome {
    Admitted(Box<AdmittedSubscriptionEnrollment>),
    AlreadyAdmitted(PaymentAttempt),
    Rejected {
        attempt: PaymentAttempt,
        reason: SubscriptionEnrollmentSubmissionRejection,
    },
}

#[derive(Debug)]
pub enum SubscriptionEnrollmentProviderResult {
    Payment(SubscriptionEnrollmentPaymentResult),
    NotSubmitted {
        payment: SubscriptionEnrollmentPaymentResult,
        error: GatewayNotSubmittedError,
    },
}

impl SubscriptionEnrollmentProviderResult {
    pub const fn payment(&self) -> &SubscriptionEnrollmentPaymentResult {
        match self {
            Self::Payment(payment) | Self::NotSubmitted { payment, .. } => payment,
        }
    }

    pub fn into_payment(self) -> SubscriptionEnrollmentPaymentResult {
        match self {
            Self::Payment(payment) | Self::NotSubmitted { payment, .. } => payment,
        }
    }
}

/// One committed final-admission result authorizing exactly one immediate
/// subscription-recovery submission.
pub struct AdmittedSubscriptionRecovery {
    reservation: SubscriptionRecoveryReservation,
    attempt: PaymentAttempt,
}

impl AdmittedSubscriptionRecovery {
    pub const fn attempt(&self) -> &PaymentAttempt {
        &self.attempt
    }
}

impl fmt::Debug for AdmittedSubscriptionRecovery {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AdmittedSubscriptionRecovery")
            .field("attempt", &self.attempt)
            .field("has_submission_authority", &true)
            .finish()
    }
}

#[derive(Debug)]
pub enum SubscriptionRecoveryAdmissionOutcome {
    Admitted(Box<AdmittedSubscriptionRecovery>),
    AlreadyAdmitted(PaymentAttempt),
    Rejected {
        attempt: PaymentAttempt,
        reason: SubscriptionRecoverySubmissionRejection,
    },
}

#[derive(Debug)]
pub enum SubscriptionRecoveryProviderResult {
    Payment(SubscriptionEnrollmentPaymentResult),
    NotSubmitted {
        payment: SubscriptionEnrollmentPaymentResult,
        error: GatewayNotSubmittedError,
    },
}

impl SubscriptionRecoveryProviderResult {
    pub const fn payment(&self) -> &SubscriptionEnrollmentPaymentResult {
        match self {
            Self::Payment(payment) | Self::NotSubmitted { payment, .. } => payment,
        }
    }

    pub fn into_payment(self) -> SubscriptionEnrollmentPaymentResult {
        match self {
            Self::Payment(payment) | Self::NotSubmitted { payment, .. } => payment,
        }
    }
}

/// One committed final-admission result authorizing exactly one immediate
/// Customer Vault mutation.
pub struct AdmittedSubscriptionPaymentMethodReplacement {
    reservation: SubscriptionPaymentMethodReplacement,
    attempt: PaymentAttempt,
}

impl AdmittedSubscriptionPaymentMethodReplacement {
    pub const fn attempt(&self) -> &PaymentAttempt {
        &self.attempt
    }
}

impl fmt::Debug for AdmittedSubscriptionPaymentMethodReplacement {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AdmittedSubscriptionPaymentMethodReplacement")
            .field("attempt", &self.attempt)
            .field("has_submission_authority", &true)
            .finish()
    }
}

#[derive(Debug)]
pub enum SubscriptionPaymentMethodReplacementAdmissionOutcome {
    Admitted(Box<AdmittedSubscriptionPaymentMethodReplacement>),
    AlreadyAdmitted(PaymentAttempt),
    Rejected {
        attempt: PaymentAttempt,
        reason: SubscriptionPaymentMethodReplacementSubmissionRejection,
    },
}

#[derive(Debug)]
pub enum SubscriptionPaymentMethodReplacementProviderResult {
    Payment(SubscriptionEnrollmentPaymentResult),
    NotSubmitted {
        payment: SubscriptionEnrollmentPaymentResult,
        error: GatewayNotSubmittedError,
    },
}

impl SubscriptionPaymentMethodReplacementProviderResult {
    pub const fn payment(&self) -> &SubscriptionEnrollmentPaymentResult {
        match self {
            Self::Payment(payment) | Self::NotSubmitted { payment, .. } => payment,
        }
    }

    pub fn into_payment(self) -> SubscriptionEnrollmentPaymentResult {
        match self {
            Self::Payment(payment) | Self::NotSubmitted { payment, .. } => payment,
        }
    }
}

/// Owns and commits final enrollment admission before exposing a one-shot
/// submission capability. A rolled-back transaction can never yield the
/// capability consumed by [`submit_admitted_subscription_enrollment`].
pub async fn admit_subscription_enrollment_submission(
    pool: &PgPool,
    offers: &dyn crate::SubscriptionOfferStore,
    reservation: &SubscriptionEnrollmentReservation,
) -> Result<SubscriptionEnrollmentAdmissionOutcome, SubscriptionEnrollmentApplicationError> {
    let mut transaction = pool.begin().await?;
    let outcome = crate::admit_subscription_enrollment_submission_in_transaction(
        &mut transaction,
        offers,
        reservation,
    )
    .await?;
    transaction.commit().await?;
    Ok(match outcome {
        SubscriptionEnrollmentSubmissionOutcome::Admitted(attempt) => {
            SubscriptionEnrollmentAdmissionOutcome::Admitted(Box::new(
                AdmittedSubscriptionEnrollment {
                    reservation: reservation.clone(),
                    attempt,
                },
            ))
        }
        SubscriptionEnrollmentSubmissionOutcome::AlreadyAdmitted(attempt) => {
            SubscriptionEnrollmentAdmissionOutcome::AlreadyAdmitted(attempt)
        }
        SubscriptionEnrollmentSubmissionOutcome::Rejected { attempt, reason } => {
            SubscriptionEnrollmentAdmissionOutcome::Rejected { attempt, reason }
        }
    })
}

/// Performs the one provider sale authorized by a committed final admission,
/// then applies or durably parks its result.
///
/// This function holds no database transaction or lock across provider I/O.
pub async fn submit_admitted_subscription_enrollment(
    pool: &PgPool,
    coordinator: &dyn BillingTransactionCoordinator,
    admission: AdmittedSubscriptionEnrollment,
    command: &EnrollSubscription,
    gateway: &ResolvedGateway,
) -> Result<SubscriptionEnrollmentProviderResult, SubscriptionEnrollmentApplicationError> {
    let reconstructed = SubscriptionEnrollmentReservation::from_command_for_attempt(
        command,
        gateway,
        admission.attempt.identity().attempt_id(),
    )
    .map_err(|_| SubscriptionEnrollmentApplicationError::SubmissionIdentityMismatch)?;
    if reconstructed != admission.reservation
        || admission.attempt.status() != PaymentAttemptStatus::Pending
        || admission
            .attempt
            .state()
            .timestamps()
            .submitted_at()
            .is_none()
    {
        return Err(SubscriptionEnrollmentApplicationError::SubmissionIdentityMismatch);
    }

    let charge = syrup_rail::ChargeAmount::new(
        admission.attempt.request().amount().cents(),
        admission.attempt.request().amount().currency(),
    )
    .map_err(|_| SubscriptionEnrollmentApplicationError::InvalidState(INVALID_APPLICATION_STATE))?;
    let request = GatewaySaleRequest::new(
        charge,
        admission.attempt.request().gateway_order_id().clone(),
        GatewaySaleIntent::InitialStoredCredential {
            payment_token: command.payment_token().clone(),
        },
        Some(command.billing_contact().clone()),
    );
    match gateway.sale(request).await {
        Ok(outcome) => apply_subscription_enrollment_gateway_outcome(
            pool,
            coordinator,
            &admission.reservation,
            &outcome,
        )
        .await
        .map(SubscriptionEnrollmentProviderResult::Payment),
        Err(GatewayMutationError::NotSubmitted(error)) => {
            let evidence = mutation_error_evidence(error.detail());
            let cooldown = matches!(error, GatewayNotSubmittedError::RateLimited(_))
                .then_some(RateLimitCooldown::Account);
            let payment = resolve_non_approved_outcome(
                pool,
                &admission.reservation,
                &evidence,
                PaymentAttemptStatus::Failed,
                Some(not_submitted_resolution_code(&error)),
                cooldown,
                OutcomeResolutionBoundary::AdmittedNotSubmitted,
            )
            .await?;
            Ok(SubscriptionEnrollmentProviderResult::NotSubmitted { payment, error })
        }
        Err(GatewayMutationError::RateLimitedIndeterminate(detail)) => resolve_unknown_outcome(
            pool,
            &admission.reservation,
            &mutation_error_evidence(&detail),
            Some(RateLimitCooldown::Provider),
        )
        .await
        .map(SubscriptionEnrollmentProviderResult::Payment),
        Err(GatewayMutationError::Indeterminate(detail)) => resolve_unknown_outcome(
            pool,
            &admission.reservation,
            &mutation_error_evidence(&detail),
            None,
        )
        .await
        .map(SubscriptionEnrollmentProviderResult::Payment),
    }
}

/// Applies one initial-enrollment gateway outcome to the durable billing ledger.
///
/// Approved application begins through the host coordinator so its recipient
/// authorization lock precedes every shared lock. If the atomic application
/// fails, this operation returns pending confirmation only after either the
/// review-required attempt or immutable processor charge has committed.
pub async fn apply_subscription_enrollment_gateway_outcome(
    pool: &PgPool,
    coordinator: &dyn BillingTransactionCoordinator,
    reservation: &SubscriptionEnrollmentReservation,
    outcome: &GatewayPaymentOutcome,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    match outcome.status() {
        GatewayPaymentStatus::Approved => {
            if outcome.transaction_id().is_none() || outcome.payment_method_reference().is_none() {
                return park_approved_outcome(
                    pool,
                    reservation,
                    outcome.evidence(),
                    INCOMPLETE_APPROVAL_TEXT,
                )
                .await;
            }

            for attempt_index in 0..APPROVED_APPLICATION_ATTEMPTS {
                match apply_approved_outcome(coordinator, reservation, outcome.evidence()).await {
                    Ok(result) => return Ok(result),
                    Err(_) if attempt_index + 1 < APPROVED_APPLICATION_ATTEMPTS => {
                        tokio::time::sleep(APPROVED_EVIDENCE_RETRY_DELAY).await;
                    }
                    Err(_) => break,
                }
            }
            park_approved_outcome(
                pool,
                reservation,
                outcome.evidence(),
                APPROVED_STORAGE_FAILURE_TEXT,
            )
            .await
        }
        GatewayPaymentStatus::Declined => {
            resolve_non_approved_outcome(
                pool,
                reservation,
                outcome.evidence(),
                PaymentAttemptStatus::Declined,
                None,
                None,
                OutcomeResolutionBoundary::Submitted,
            )
            .await
        }
        GatewayPaymentStatus::Failed => {
            resolve_non_approved_outcome(
                pool,
                reservation,
                outcome.evidence(),
                PaymentAttemptStatus::Failed,
                None,
                None,
                OutcomeResolutionBoundary::Submitted,
            )
            .await
        }
        GatewayPaymentStatus::Unknown => {
            resolve_unknown_outcome(pool, reservation, outcome.evidence(), None).await
        }
    }
}

/// Applies an already-observed provider outcome to an exact durable enrollment attempt.
///
/// This is the reconciliation counterpart to foreground enrollment. It rebuilds
/// the secret-free reservation from immutable attempt state and the canonical
/// gateway account, then enters the same atomic application path. It never
/// resolves a live gateway or submits another provider mutation.
pub async fn apply_reconciled_subscription_enrollment_gateway_outcome(
    pool: &PgPool,
    coordinator: &dyn BillingTransactionCoordinator,
    billing_scope_id: BillingScopeId,
    attempt_id: PaymentAttemptId,
    outcome: &GatewayPaymentOutcome,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    let mut transaction = pool.begin().await?;
    let attempt = crate::find_payment_attempt_by_id_in_transaction(
        &mut transaction,
        billing_scope_id,
        attempt_id,
    )
    .await?
    .ok_or(SubscriptionEnrollmentApplicationError::InvalidState(
        "subscription enrollment attempt was not found",
    ))?;
    let provider_key = sqlx::query_scalar::<_, String>(
        r#"
        SELECT provider_key
        FROM billing_gateway_accounts
        WHERE billing_scope_id = $1 AND id = $2
        "#,
    )
    .bind(billing_scope_id.as_uuid())
    .bind(attempt.identity().gateway_account_id().as_uuid())
    .fetch_optional(&mut *transaction)
    .await?
    .ok_or(SubscriptionEnrollmentApplicationError::InvalidState(
        "subscription enrollment gateway account was not found",
    ))?;
    transaction.commit().await?;

    let provider_key = GatewayProviderKey::new(provider_key).map_err(|_| {
        SubscriptionEnrollmentApplicationError::InvalidState(
            "subscription enrollment gateway provider key is invalid",
        )
    })?;
    let reservation = SubscriptionEnrollmentReservation::from_attempt(&attempt, provider_key)
        .map_err(|_| {
            SubscriptionEnrollmentApplicationError::InvalidState(
                "reconciled attempt is not a valid subscription enrollment",
            )
        })?;
    apply_subscription_enrollment_gateway_outcome(pool, coordinator, &reservation, outcome).await
}

/// Commits final recovery admission before exposing its one-shot capability.
pub async fn admit_subscription_recovery_submission(
    pool: &PgPool,
    reservation: &SubscriptionRecoveryReservation,
) -> Result<SubscriptionRecoveryAdmissionOutcome, SubscriptionEnrollmentApplicationError> {
    let mut transaction = pool.begin().await?;
    let outcome =
        crate::admit_subscription_recovery_submission_in_transaction(&mut transaction, reservation)
            .await?;
    transaction.commit().await?;
    Ok(match outcome {
        SubscriptionRecoverySubmissionOutcome::Admitted(attempt) => {
            SubscriptionRecoveryAdmissionOutcome::Admitted(Box::new(AdmittedSubscriptionRecovery {
                reservation: reservation.clone(),
                attempt,
            }))
        }
        SubscriptionRecoverySubmissionOutcome::AlreadyAdmitted(attempt) => {
            SubscriptionRecoveryAdmissionOutcome::AlreadyAdmitted(attempt)
        }
        SubscriptionRecoverySubmissionOutcome::Rejected { attempt, reason } => {
            SubscriptionRecoveryAdmissionOutcome::Rejected { attempt, reason }
        }
    })
}

/// Performs the one provider sale authorized by committed recovery admission,
/// then applies or durably parks its result without holding a database lock
/// across provider I/O.
pub async fn submit_admitted_subscription_recovery(
    pool: &PgPool,
    coordinator: &dyn BillingTransactionCoordinator,
    admission: AdmittedSubscriptionRecovery,
    command: &RecoverSubscriptionPayment,
    gateway: &ResolvedGateway,
) -> Result<SubscriptionRecoveryProviderResult, SubscriptionEnrollmentApplicationError> {
    let identity = admission.reservation.identity();
    if admission.attempt.identity() != identity
        || admission.attempt.request() != admission.reservation.request()
        || admission.attempt.status() != PaymentAttemptStatus::Pending
        || admission
            .attempt
            .state()
            .timestamps()
            .submitted_at()
            .is_none()
        || command.billing_scope_id() != identity.billing_scope_id()
        || command.subscriber_id() != identity.subscriber_id()
        || command.plan_key() != admission.reservation.plan_key()
        || command.gateway_configuration_id() != identity.gateway_configuration_id()
        || gateway.billing_scope_id() != identity.billing_scope_id()
        || gateway.gateway_account_id() != identity.gateway_account_id()
        || gateway.gateway_configuration_id() != identity.gateway_configuration_id()
        || gateway.provider_key() != admission.reservation.provider_key()
    {
        return Err(SubscriptionEnrollmentApplicationError::SubmissionIdentityMismatch);
    }
    let charge =
        syrup_rail::ChargeAmount::try_from(admission.attempt.request().amount()).map_err(|_| {
            SubscriptionEnrollmentApplicationError::InvalidState(INVALID_APPLICATION_STATE)
        })?;
    let request = GatewaySaleRequest::new(
        charge,
        admission.attempt.request().gateway_order_id().clone(),
        GatewaySaleIntent::InitialStoredCredential {
            payment_token: command.payment_token().clone(),
        },
        Some(command.billing_contact().clone()),
    );
    match gateway.sale(request).await {
        Ok(outcome) => apply_subscription_recovery_gateway_outcome(
            pool,
            coordinator,
            &admission.reservation,
            &outcome,
        )
        .await
        .map(SubscriptionRecoveryProviderResult::Payment),
        Err(GatewayMutationError::NotSubmitted(error)) => {
            let evidence = mutation_error_evidence(error.detail());
            let cooldown = matches!(error, GatewayNotSubmittedError::RateLimited(_))
                .then_some(RateLimitCooldown::Account);
            let payment = resolve_recovery_non_approved_outcome(
                pool,
                &admission.reservation,
                &evidence,
                PaymentAttemptStatus::Failed,
                Some(not_submitted_resolution_code(&error)),
                cooldown,
                OutcomeResolutionBoundary::AdmittedNotSubmitted,
            )
            .await?;
            Ok(SubscriptionRecoveryProviderResult::NotSubmitted { payment, error })
        }
        Err(GatewayMutationError::RateLimitedIndeterminate(detail)) => {
            resolve_recovery_unknown_outcome(
                pool,
                &admission.reservation,
                &mutation_error_evidence(&detail),
                Some(RateLimitCooldown::Provider),
            )
            .await
            .map(SubscriptionRecoveryProviderResult::Payment)
        }
        Err(GatewayMutationError::Indeterminate(detail)) => resolve_recovery_unknown_outcome(
            pool,
            &admission.reservation,
            &mutation_error_evidence(&detail),
            None,
        )
        .await
        .map(SubscriptionRecoveryProviderResult::Payment),
    }
}

/// Applies one recovery outcome through the canonical charge ledger and host
/// billing transaction.
pub async fn apply_subscription_recovery_gateway_outcome(
    pool: &PgPool,
    coordinator: &dyn BillingTransactionCoordinator,
    reservation: &SubscriptionRecoveryReservation,
    outcome: &GatewayPaymentOutcome,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    match outcome.status() {
        GatewayPaymentStatus::Approved => {
            if outcome.transaction_id().is_none() || outcome.payment_method_reference().is_none() {
                return park_recovery_approved_outcome(
                    pool,
                    reservation,
                    outcome.evidence(),
                    RECOVERY_INCOMPLETE_APPROVAL_TEXT,
                )
                .await;
            }
            for attempt_index in 0..APPROVED_APPLICATION_ATTEMPTS {
                match apply_recovery_approved_outcome(coordinator, reservation, outcome.evidence())
                    .await
                {
                    Ok(result) => return Ok(result),
                    Err(_) if attempt_index + 1 < APPROVED_APPLICATION_ATTEMPTS => {
                        tokio::time::sleep(APPROVED_EVIDENCE_RETRY_DELAY).await;
                    }
                    Err(_) => break,
                }
            }
            park_recovery_approved_outcome(
                pool,
                reservation,
                outcome.evidence(),
                RECOVERY_APPROVED_STORAGE_FAILURE_TEXT,
            )
            .await
        }
        GatewayPaymentStatus::Declined => {
            resolve_recovery_non_approved_outcome(
                pool,
                reservation,
                outcome.evidence(),
                PaymentAttemptStatus::Declined,
                None,
                None,
                OutcomeResolutionBoundary::Submitted,
            )
            .await
        }
        GatewayPaymentStatus::Failed => {
            resolve_recovery_non_approved_outcome(
                pool,
                reservation,
                outcome.evidence(),
                PaymentAttemptStatus::Failed,
                None,
                None,
                OutcomeResolutionBoundary::Submitted,
            )
            .await
        }
        GatewayPaymentStatus::Unknown => {
            resolve_recovery_unknown_outcome(pool, reservation, outcome.evidence(), None).await
        }
    }
}

/// Re-enters the same recovery application authority from durable evidence and
/// never resolves a live gateway or submits another mutation.
pub async fn apply_reconciled_subscription_recovery_gateway_outcome(
    pool: &PgPool,
    coordinator: &dyn BillingTransactionCoordinator,
    billing_scope_id: BillingScopeId,
    attempt_id: PaymentAttemptId,
    outcome: &GatewayPaymentOutcome,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    let mut transaction = pool.begin().await?;
    let attempt = crate::find_payment_attempt_by_id_in_transaction(
        &mut transaction,
        billing_scope_id,
        attempt_id,
    )
    .await?
    .ok_or(SubscriptionEnrollmentApplicationError::InvalidState(
        "subscription recovery attempt was not found",
    ))?;
    let provider_key = sqlx::query_scalar::<_, String>(
        "SELECT provider_key FROM billing_gateway_accounts WHERE billing_scope_id = $1 AND id = $2",
    )
    .bind(billing_scope_id.as_uuid())
    .bind(attempt.identity().gateway_account_id().as_uuid())
    .fetch_optional(&mut *transaction)
    .await?
    .ok_or(SubscriptionEnrollmentApplicationError::InvalidState(
        "subscription recovery gateway account was not found",
    ))?;
    transaction.commit().await?;
    let provider_key = GatewayProviderKey::new(provider_key).map_err(|_| {
        SubscriptionEnrollmentApplicationError::InvalidState(
            "subscription recovery gateway provider key is invalid",
        )
    })?;
    let reservation = SubscriptionRecoveryReservation::from_attempt(&attempt, provider_key)
        .map_err(|_| {
            SubscriptionEnrollmentApplicationError::InvalidState(
                "reconciled attempt is not a valid subscription recovery",
            )
        })?;
    apply_subscription_recovery_gateway_outcome(pool, coordinator, &reservation, outcome).await
}

/// Commits final payment-method replacement admission before exposing its
/// one-shot Customer Vault capability.
pub async fn admit_subscription_payment_method_replacement(
    pool: &PgPool,
    reservation: &SubscriptionPaymentMethodReplacement,
) -> Result<
    SubscriptionPaymentMethodReplacementAdmissionOutcome,
    SubscriptionEnrollmentApplicationError,
> {
    let mut transaction = pool.begin().await?;
    let outcome = crate::admit_subscription_payment_method_replacement_in_transaction(
        &mut transaction,
        reservation,
    )
    .await?;
    transaction.commit().await?;
    Ok(match outcome {
        SubscriptionPaymentMethodReplacementSubmissionOutcome::Admitted(attempt) => {
            SubscriptionPaymentMethodReplacementAdmissionOutcome::Admitted(Box::new(
                AdmittedSubscriptionPaymentMethodReplacement {
                    reservation: reservation.clone(),
                    attempt,
                },
            ))
        }
        SubscriptionPaymentMethodReplacementSubmissionOutcome::AlreadyAdmitted(attempt) => {
            SubscriptionPaymentMethodReplacementAdmissionOutcome::AlreadyAdmitted(attempt)
        }
        SubscriptionPaymentMethodReplacementSubmissionOutcome::Rejected { attempt, reason } => {
            SubscriptionPaymentMethodReplacementAdmissionOutcome::Rejected { attempt, reason }
        }
    })
}

/// Performs the one Customer Vault mutation authorized by committed admission.
pub async fn submit_admitted_subscription_payment_method_replacement(
    pool: &PgPool,
    coordinator: &dyn BillingTransactionCoordinator,
    admission: AdmittedSubscriptionPaymentMethodReplacement,
    command: &ReplaceSubscriptionPaymentMethod,
    gateway: &ResolvedGateway,
) -> Result<
    SubscriptionPaymentMethodReplacementProviderResult,
    SubscriptionEnrollmentApplicationError,
> {
    let identity = admission.reservation.identity();
    if admission.attempt.identity() != identity
        || admission.attempt.request() != admission.reservation.request()
        || admission.attempt.status() != PaymentAttemptStatus::Pending
        || admission
            .attempt
            .state()
            .timestamps()
            .submitted_at()
            .is_none()
        || command.billing_scope_id() != identity.billing_scope_id()
        || command.subscriber_id() != identity.subscriber_id()
        || command.plan_key() != admission.reservation.plan_key()
        || command.gateway_configuration_id() != identity.gateway_configuration_id()
        || gateway.billing_scope_id() != identity.billing_scope_id()
        || gateway.gateway_account_id() != identity.gateway_account_id()
        || gateway.gateway_configuration_id() != identity.gateway_configuration_id()
        || gateway.provider_key() != admission.reservation.provider_key()
    {
        return Err(SubscriptionEnrollmentApplicationError::SubmissionIdentityMismatch);
    }
    let request = GatewayStorePaymentMethodRequest::new(
        command.payment_token().clone(),
        admission.attempt.request().gateway_order_id().clone(),
        Some(command.billing_contact().clone()),
    );
    match gateway.store_payment_method(request).await {
        Ok(outcome) => apply_subscription_payment_method_replacement_gateway_outcome(
            pool,
            coordinator,
            &admission.reservation,
            &outcome,
        )
        .await
        .map(SubscriptionPaymentMethodReplacementProviderResult::Payment),
        Err(GatewayMutationError::NotSubmitted(error)) => {
            let evidence = mutation_error_evidence(error.detail());
            let cooldown = matches!(error, GatewayNotSubmittedError::RateLimited(_))
                .then_some(RateLimitCooldown::Account);
            let payment = resolve_payment_method_replacement_non_approved_outcome(
                pool,
                &admission.reservation,
                &evidence,
                PaymentAttemptStatus::Failed,
                Some(not_submitted_resolution_code(&error)),
                cooldown,
                OutcomeResolutionBoundary::AdmittedNotSubmitted,
            )
            .await?;
            Ok(SubscriptionPaymentMethodReplacementProviderResult::NotSubmitted { payment, error })
        }
        Err(GatewayMutationError::RateLimitedIndeterminate(detail)) => {
            resolve_payment_method_replacement_unknown_outcome(
                pool,
                &admission.reservation,
                &mutation_error_evidence(&detail),
                Some(RateLimitCooldown::Provider),
            )
            .await
            .map(SubscriptionPaymentMethodReplacementProviderResult::Payment)
        }
        Err(GatewayMutationError::Indeterminate(detail)) => {
            resolve_payment_method_replacement_unknown_outcome(
                pool,
                &admission.reservation,
                &mutation_error_evidence(&detail),
                None,
            )
            .await
            .map(SubscriptionPaymentMethodReplacementProviderResult::Payment)
        }
    }
}

pub async fn apply_subscription_payment_method_replacement_gateway_outcome(
    pool: &PgPool,
    coordinator: &dyn BillingTransactionCoordinator,
    reservation: &SubscriptionPaymentMethodReplacement,
    outcome: &GatewayPaymentOutcome,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    match outcome.status() {
        GatewayPaymentStatus::Approved => {
            if outcome.transaction_id().is_none() || outcome.payment_method_reference().is_none() {
                return park_payment_method_replacement_approved_outcome(
                    pool,
                    reservation,
                    outcome.evidence(),
                    PAYMENT_METHOD_REPLACEMENT_INCOMPLETE_APPROVAL_TEXT,
                )
                .await;
            }
            for attempt_index in 0..APPROVED_APPLICATION_ATTEMPTS {
                match apply_payment_method_replacement_approved_outcome(
                    coordinator,
                    reservation,
                    outcome.evidence(),
                )
                .await
                {
                    Ok(result) => return Ok(result),
                    Err(_) if attempt_index + 1 < APPROVED_APPLICATION_ATTEMPTS => {
                        tokio::time::sleep(APPROVED_EVIDENCE_RETRY_DELAY).await;
                    }
                    Err(_) => break,
                }
            }
            park_payment_method_replacement_approved_outcome(
                pool,
                reservation,
                outcome.evidence(),
                PAYMENT_METHOD_REPLACEMENT_STORAGE_FAILURE_TEXT,
            )
            .await
        }
        GatewayPaymentStatus::Declined => {
            resolve_payment_method_replacement_non_approved_outcome(
                pool,
                reservation,
                outcome.evidence(),
                PaymentAttemptStatus::Declined,
                None,
                None,
                OutcomeResolutionBoundary::Submitted,
            )
            .await
        }
        GatewayPaymentStatus::Failed => {
            resolve_payment_method_replacement_non_approved_outcome(
                pool,
                reservation,
                outcome.evidence(),
                PaymentAttemptStatus::Failed,
                None,
                None,
                OutcomeResolutionBoundary::Submitted,
            )
            .await
        }
        GatewayPaymentStatus::Unknown => {
            resolve_payment_method_replacement_unknown_outcome(
                pool,
                reservation,
                outcome.evidence(),
                None,
            )
            .await
        }
    }
}

pub async fn apply_reconciled_subscription_payment_method_replacement_gateway_outcome(
    pool: &PgPool,
    coordinator: &dyn BillingTransactionCoordinator,
    billing_scope_id: BillingScopeId,
    attempt_id: PaymentAttemptId,
    outcome: &GatewayPaymentOutcome,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    let mut transaction = pool.begin().await?;
    let attempt = crate::find_payment_attempt_by_id_in_transaction(
        &mut transaction,
        billing_scope_id,
        attempt_id,
    )
    .await?
    .ok_or(SubscriptionEnrollmentApplicationError::InvalidState(
        "payment method replacement attempt was not found",
    ))?;
    let outcome = reconciled_outcome_with_persisted_evidence(&attempt, outcome);
    let provider_key = sqlx::query_scalar::<_, String>(
        "SELECT provider_key FROM billing_gateway_accounts WHERE billing_scope_id = $1 AND id = $2",
    )
    .bind(billing_scope_id.as_uuid())
    .bind(attempt.identity().gateway_account_id().as_uuid())
    .fetch_optional(&mut *transaction)
    .await?
    .ok_or(SubscriptionEnrollmentApplicationError::InvalidState(
        "payment method replacement gateway account was not found",
    ))?;
    transaction.commit().await?;
    let provider_key = GatewayProviderKey::new(provider_key).map_err(|_| {
        SubscriptionEnrollmentApplicationError::InvalidState(
            "payment method replacement gateway provider key is invalid",
        )
    })?;
    let reservation = SubscriptionPaymentMethodReplacement::from_attempt(&attempt, provider_key)
        .map_err(|_| {
            SubscriptionEnrollmentApplicationError::InvalidState(
                "reconciled attempt is not a valid payment method replacement",
            )
        })?;
    apply_subscription_payment_method_replacement_gateway_outcome(
        pool,
        coordinator,
        &reservation,
        &outcome,
    )
    .await
}

fn reconciled_outcome_with_persisted_evidence(
    attempt: &PaymentAttempt,
    outcome: &GatewayPaymentOutcome,
) -> GatewayPaymentOutcome {
    let observed = outcome.evidence();
    let persisted = attempt.state().processor_evidence();
    let observed_descriptor = observed.descriptor();
    let persisted_descriptor = persisted.descriptor();
    let descriptor = GatewayPaymentDescriptor::from_provider_parts(
        observed_descriptor
            .payment_type()
            .or_else(|| persisted_descriptor.payment_type())
            .cloned(),
        observed_descriptor
            .card_brand()
            .or_else(|| persisted_descriptor.card_brand())
            .cloned(),
        observed_descriptor
            .card_last_four()
            .or_else(|| persisted_descriptor.card_last_four())
            .map(|value| value.expose()),
        observed_descriptor
            .card_exp_month()
            .or_else(|| persisted_descriptor.card_exp_month()),
        observed_descriptor
            .card_exp_year()
            .or_else(|| persisted_descriptor.card_exp_year()),
    );
    GatewayPaymentOutcome::new(
        outcome.status(),
        ProcessorEvidence::new(
            observed
                .transaction_id()
                .or_else(|| persisted.transaction_id())
                .cloned(),
            observed
                .payment_method_reference()
                .or_else(|| persisted.payment_method_reference())
                .cloned(),
            observed
                .response()
                .or_else(|| persisted.response())
                .cloned(),
            observed
                .response_code()
                .or_else(|| persisted.response_code())
                .cloned(),
            observed
                .response_text()
                .or_else(|| persisted.response_text())
                .cloned(),
            observed
                .condition()
                .or_else(|| persisted.condition())
                .cloned(),
            descriptor,
        ),
    )
}

async fn apply_payment_method_replacement_approved_outcome(
    coordinator: &dyn BillingTransactionCoordinator,
    reservation: &SubscriptionPaymentMethodReplacement,
    evidence: &ProcessorEvidence,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    let identity = reservation.identity();
    let mut transaction = coordinator
        .begin(
            BillingEventSubject::new(identity.billing_scope_id(), identity.subscriber_id()),
            BILLING_LOCK_TIMEOUT,
        )
        .await?;
    let subject_state = transaction.subject_state();
    let application = apply_payment_method_replacement_approved_on_connection(
        transaction.connection(),
        subject_state,
        reservation,
        evidence,
    )
    .await;
    let (result, event) = match application {
        Ok(application) => application,
        Err(error) => {
            let _ = transaction.rollback().await;
            return Err(error);
        }
    };
    if let Some(event) = event.as_ref()
        && let Err(error) = transaction.append_event(event).await
    {
        let _ = transaction.rollback().await;
        return Err(error.into());
    }
    transaction.commit().await?;
    Ok(result)
}

async fn apply_payment_method_replacement_approved_on_connection(
    connection: &mut PgConnection,
    subject_state: BillingTransactionSubjectState,
    reservation: &SubscriptionPaymentMethodReplacement,
    evidence: &ProcessorEvidence,
) -> Result<
    (SubscriptionEnrollmentPaymentResult, Option<BillingEvent>),
    SubscriptionEnrollmentApplicationError,
> {
    set_application_timeouts(connection).await?;
    let identity = reservation.identity();
    lock_payment_method_domain(
        connection,
        identity.subscriber_id(),
        identity.gateway_account_id().as_uuid(),
    )
    .await?;
    lock_subscription_aggregate(connection, identity.subscriber_id(), reservation.plan_key())
        .await?;
    let attempt = lock_expected_payment_method_replacement_attempt(connection, reservation).await?;
    if attempt.status() == PaymentAttemptStatus::Approved {
        let subscription = load_applied_subscription(connection, &attempt).await?;
        observe_processor_charge(connection, &attempt, evidence, ChargeProgression::Applied)
            .await?;
        return Ok((
            SubscriptionEnrollmentPaymentResult::new(attempt, subscription),
            None,
        ));
    }
    if subject_state != BillingTransactionSubjectState::LiveRecipient {
        return Err(SubscriptionEnrollmentApplicationError::InvalidState(
            "a payment method replacement event requires a live recipient",
        ));
    }
    if attempt.status().is_terminal() {
        observe_processor_charge(
            connection,
            &attempt,
            evidence,
            ChargeProgression::ReconciliationRequired,
        )
        .await?;
        return Ok((
            SubscriptionEnrollmentPaymentResult::confirmation_pending(
                attempt,
                None,
                evidence.clone(),
            ),
            None,
        ));
    }
    let observation =
        observe_processor_charge(connection, &attempt, evidence, ChargeProgression::Pending)
            .await?;
    let ObservedCharge::Owned(charge) = observation else {
        let parked = park_locked_attempt(
            connection,
            &attempt,
            evidence,
            None,
            "The approved gateway transaction is already owned by another payment attempt.",
        )
        .await?;
        return Ok((SubscriptionEnrollmentPaymentResult::new(parked, None), None));
    };
    if charge.role == ChargeRole::Additional {
        transition_charge(
            connection,
            charge.id,
            ChargeProgression::ReconciliationRequired,
            None,
        )
        .await?;
        let parked = park_locked_attempt(
            connection,
            &attempt,
            evidence,
            None,
            "An additional approved stored-method result requires manual review.",
        )
        .await?;
        return Ok((SubscriptionEnrollmentPaymentResult::new(parked, None), None));
    }
    let transaction_id =
        evidence
            .transaction_id()
            .ok_or(SubscriptionEnrollmentApplicationError::InvalidState(
                INVALID_APPLICATION_STATE,
            ))?;
    let method_id = upsert_payment_method(connection, &attempt, evidence).await?;
    let expected = reservation.expected_state();
    let row = sqlx::query(
        r#"
        SELECT status, payment_method_id, initial_transaction_id
        FROM billing_subscriptions
        WHERE id = $1 AND billing_scope_id = $2 AND subscriber_id = $3
            AND gateway_account_id = $4 AND plan_key = $5
        FOR UPDATE
        "#,
    )
    .bind(reservation.subscription_id().as_uuid())
    .bind(identity.billing_scope_id().as_uuid())
    .bind(identity.subscriber_id().as_uuid())
    .bind(identity.gateway_account_id().as_uuid())
    .bind(reservation.plan_key().as_str())
    .fetch_optional(&mut *connection)
    .await?;
    let Some(row) = row else {
        transition_charge(
            connection,
            charge.id,
            ChargeProgression::ReconciliationRequired,
            Some(PaymentResolutionCode::SubscriptionApprovedPaymentMethodUpdateSubscriptionIneligible),
        )
        .await?;
        let parked = park_locked_attempt(
            connection,
            &attempt,
            evidence,
            Some(PaymentResolutionCode::SubscriptionApprovedPaymentMethodUpdateSubscriptionIneligible),
            PAYMENT_METHOD_REPLACEMENT_STALE_STATE_TEXT,
        )
        .await?;
        return Ok((SubscriptionEnrollmentPaymentResult::new(parked, None), None));
    };
    let status: String = row.try_get("status")?;
    let current_method_id = PaymentMethodId::new(row.try_get("payment_method_id")?);
    let current_transaction_id: String = row.try_get("initial_transaction_id")?;
    let expected_state = status == "active" || status == "past_due";
    let baseline_matches = current_method_id == expected.payment_method_id()
        && current_transaction_id == expected.expected_initial_transaction_id().expose();
    let exact_replay =
        current_method_id == method_id && current_transaction_id == transaction_id.expose();
    if !expected_state || (!baseline_matches && !exact_replay) {
        let code = if expected_state {
            PaymentResolutionCode::SubscriptionApprovedPaymentMethodUpdateStaleState
        } else {
            PaymentResolutionCode::SubscriptionApprovedPaymentMethodUpdateSubscriptionIneligible
        };
        transition_charge(
            connection,
            charge.id,
            ChargeProgression::ReconciliationRequired,
            Some(code),
        )
        .await?;
        let parked = park_locked_attempt(
            connection,
            &attempt,
            evidence,
            Some(code),
            PAYMENT_METHOD_REPLACEMENT_STALE_STATE_TEXT,
        )
        .await?;
        let subscription = load_subscription(
            connection,
            identity.billing_scope_id(),
            reservation.subscription_id(),
        )
        .await?;
        return Ok((
            SubscriptionEnrollmentPaymentResult::new(parked, subscription),
            None,
        ));
    }
    if baseline_matches {
        let updated = sqlx::query(
            r#"
            UPDATE billing_subscriptions
            SET payment_method_id = $2, initial_transaction_id = $3,
                updated_at = clock_timestamp()
            WHERE id = $1 AND billing_scope_id = $4 AND subscriber_id = $5
                AND gateway_account_id = $6 AND plan_key = $7
                AND status IN ('active', 'past_due')
                AND payment_method_id = $8 AND initial_transaction_id = $9
            "#,
        )
        .bind(reservation.subscription_id().as_uuid())
        .bind(method_id.as_uuid())
        .bind(transaction_id.expose())
        .bind(identity.billing_scope_id().as_uuid())
        .bind(identity.subscriber_id().as_uuid())
        .bind(identity.gateway_account_id().as_uuid())
        .bind(reservation.plan_key().as_str())
        .bind(expected.payment_method_id().as_uuid())
        .bind(expected.expected_initial_transaction_id().expose())
        .execute(&mut *connection)
        .await?;
        if updated.rows_affected() != 1 {
            return Err(SubscriptionEnrollmentApplicationError::InvalidState(
                INVALID_APPLICATION_STATE,
            ));
        }
    }
    mark_attempt_approved(
        connection,
        &attempt,
        evidence,
        reservation.subscription_id(),
        method_id,
    )
    .await?;
    disable_payment_method_if_unreferenced(connection, expected.payment_method_id()).await?;
    transition_charge(connection, charge.id, ChargeProgression::Applied, None).await?;
    let attempt = find_payment_attempt_by_id_on_connection(
        connection,
        identity.billing_scope_id(),
        identity.attempt_id(),
    )
    .await?
    .ok_or(SubscriptionEnrollmentApplicationError::InvalidState(
        INVALID_APPLICATION_STATE,
    ))?;
    let subscription = load_subscription(
        connection,
        identity.billing_scope_id(),
        reservation.subscription_id(),
    )
    .await?
    .ok_or(SubscriptionEnrollmentApplicationError::InvalidState(
        INVALID_APPLICATION_STATE,
    ))?;
    let descriptor = evidence.descriptor();
    let card = descriptor
        .card_brand()
        .cloned()
        .zip(descriptor.card_last_four().cloned())
        .map(|(brand, last_four)| PaymentCardDisplay::new(brand, last_four));
    let event = BillingEvent::PaymentMethodChanged {
        attempt_id: identity.attempt_id(),
        subscription_id: reservation.subscription_id(),
        plan_key: reservation.plan_key().clone(),
        card,
    };
    Ok((
        SubscriptionEnrollmentPaymentResult::new(attempt, Some(subscription)),
        Some(event),
    ))
}

async fn apply_recovery_approved_outcome(
    coordinator: &dyn BillingTransactionCoordinator,
    reservation: &SubscriptionRecoveryReservation,
    evidence: &ProcessorEvidence,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    let identity = reservation.identity();
    let mut transaction = coordinator
        .begin(
            BillingEventSubject::new(identity.billing_scope_id(), identity.subscriber_id()),
            BILLING_LOCK_TIMEOUT,
        )
        .await?;
    let subject_state = transaction.subject_state();
    let application = apply_recovery_approved_on_connection(
        transaction.connection(),
        subject_state,
        reservation,
        evidence,
    )
    .await;
    let (result, event) = match application {
        Ok(application) => application,
        Err(error) => {
            let _ = transaction.rollback().await;
            return Err(error);
        }
    };
    if let Some(event) = event.as_ref()
        && let Err(error) = transaction.append_event(event).await
    {
        let _ = transaction.rollback().await;
        return Err(error.into());
    }
    transaction.commit().await?;
    Ok(result)
}

async fn apply_recovery_approved_on_connection(
    connection: &mut PgConnection,
    subject_state: BillingTransactionSubjectState,
    reservation: &SubscriptionRecoveryReservation,
    evidence: &ProcessorEvidence,
) -> Result<
    (SubscriptionEnrollmentPaymentResult, Option<BillingEvent>),
    SubscriptionEnrollmentApplicationError,
> {
    set_application_timeouts(connection).await?;
    let identity = reservation.identity();
    lock_payment_method_domain(
        connection,
        identity.subscriber_id(),
        identity.gateway_account_id().as_uuid(),
    )
    .await?;
    lock_subscription_aggregate(connection, identity.subscriber_id(), reservation.plan_key())
        .await?;
    let attempt = lock_expected_recovery_attempt(connection, reservation).await?;

    if attempt.status() == PaymentAttemptStatus::Approved {
        let subscription = load_applied_subscription(connection, &attempt)
            .await?
            .ok_or(SubscriptionEnrollmentApplicationError::InvalidState(
                INVALID_APPLICATION_STATE,
            ))?;
        observe_processor_charge(connection, &attempt, evidence, ChargeProgression::Applied)
            .await?;
        return Ok((
            SubscriptionEnrollmentPaymentResult::new(attempt, Some(subscription)),
            None,
        ));
    }
    if subject_state != BillingTransactionSubjectState::LiveRecipient {
        return Err(SubscriptionEnrollmentApplicationError::InvalidState(
            "a subscription recovery event requires a live recipient",
        ));
    }
    if attempt.status().is_terminal() {
        let observation = observe_processor_charge(
            connection,
            &attempt,
            evidence,
            ChargeProgression::ExternalReversalRequired,
        )
        .await?;
        if let ObservedCharge::Owned(charge) = observation {
            transition_charge(
                connection,
                charge.id,
                ChargeProgression::ExternalReversalRequired,
                None,
            )
            .await?;
        }
        return Ok((
            SubscriptionEnrollmentPaymentResult::confirmation_pending(
                attempt,
                None,
                evidence.clone(),
            ),
            None,
        ));
    }

    let observation =
        observe_processor_charge(connection, &attempt, evidence, ChargeProgression::Pending)
            .await?;
    let ObservedCharge::Owned(charge) = observation else {
        let parked = park_locked_attempt(
            connection,
            &attempt,
            evidence,
            None,
            "The approved gateway transaction is already owned by another payment attempt.",
        )
        .await?;
        return Ok((SubscriptionEnrollmentPaymentResult::new(parked, None), None));
    };
    if charge.role == ChargeRole::Additional {
        transition_charge(
            connection,
            charge.id,
            ChargeProgression::ExternalReversalRequired,
            None,
        )
        .await?;
        let parked = park_locked_attempt(
            connection,
            &attempt,
            evidence,
            None,
            "An additional approved charge requires manual reversal review.",
        )
        .await?;
        return Ok((SubscriptionEnrollmentPaymentResult::new(parked, None), None));
    }
    if !recovery_subscription_matches(connection, reservation).await? {
        transition_charge(
            connection,
            charge.id,
            ChargeProgression::ExternalReversalRequired,
            Some(PaymentResolutionCode::SubscriptionApprovedRecoveryStaleState),
        )
        .await?;
        let parked = park_locked_attempt(
            connection,
            &attempt,
            evidence,
            Some(PaymentResolutionCode::SubscriptionApprovedRecoveryStaleState),
            RECOVERY_STALE_STATE_TEXT,
        )
        .await?;
        return Ok((SubscriptionEnrollmentPaymentResult::new(parked, None), None));
    }

    let transaction_id =
        evidence
            .transaction_id()
            .ok_or(SubscriptionEnrollmentApplicationError::InvalidState(
                INVALID_APPLICATION_STATE,
            ))?;
    let old_method_id = reservation.expected_state().payment_method_id();
    let method_id = upsert_payment_method(connection, &attempt, evidence).await?;
    let updated = sqlx::query(
        r#"
        UPDATE billing_subscriptions
        SET status = 'active', payment_method_id = $2,
            current_period_start_at = $3, current_period_end_at = $4,
            next_renewal_at = $4, initial_transaction_id = $5,
            updated_at = clock_timestamp()
        WHERE id = $1 AND billing_scope_id = $6 AND subscriber_id = $7
            AND gateway_account_id = $8 AND plan_key = $9
            AND status = $10 AND status IN ('active', 'past_due')
            AND payment_method_id = $11 AND initial_transaction_id = $12
            AND next_renewal_at = $3
        "#,
    )
    .bind(reservation.subscription_id().as_uuid())
    .bind(method_id.as_uuid())
    .bind(reservation.period().start_at())
    .bind(reservation.period().end_at())
    .bind(transaction_id.expose())
    .bind(identity.billing_scope_id().as_uuid())
    .bind(identity.subscriber_id().as_uuid())
    .bind(identity.gateway_account_id().as_uuid())
    .bind(reservation.plan_key().as_str())
    .bind(reservation.expected_state().status().as_str())
    .bind(old_method_id.as_uuid())
    .bind(
        reservation
            .expected_state()
            .initial_transaction_id()
            .expose(),
    )
    .execute(&mut *connection)
    .await?;
    if updated.rows_affected() != 1 {
        return Err(SubscriptionEnrollmentApplicationError::InvalidState(
            INVALID_APPLICATION_STATE,
        ));
    }
    advance_subscription_discount_after_successful_charge(
        connection,
        reservation.subscription_id(),
        reservation.plan_key(),
    )
    .await?;
    disable_payment_method_if_unreferenced(connection, old_method_id).await?;
    mark_attempt_approved(
        connection,
        &attempt,
        evidence,
        reservation.subscription_id(),
        method_id,
    )
    .await?;
    transition_charge(connection, charge.id, ChargeProgression::Applied, None).await?;

    let attempt = find_payment_attempt_by_id_on_connection(
        connection,
        identity.billing_scope_id(),
        identity.attempt_id(),
    )
    .await?
    .ok_or(SubscriptionEnrollmentApplicationError::InvalidState(
        INVALID_APPLICATION_STATE,
    ))?;
    let subscription = load_subscription(
        connection,
        identity.billing_scope_id(),
        reservation.subscription_id(),
    )
    .await?
    .ok_or(SubscriptionEnrollmentApplicationError::InvalidState(
        INVALID_APPLICATION_STATE,
    ))?;
    let event = BillingEvent::SubscriptionRenewed {
        attempt_id: identity.attempt_id(),
        subscription_id: reservation.subscription_id(),
        plan_key: reservation.plan_key().clone(),
        charge: syrup_rail::ChargeAmount::try_from(attempt.request().amount()).map_err(|_| {
            SubscriptionEnrollmentApplicationError::InvalidState(INVALID_APPLICATION_STATE)
        })?,
        period: reservation.period().clone(),
    };
    Ok((
        SubscriptionEnrollmentPaymentResult::new(attempt, Some(subscription)),
        Some(event),
    ))
}

async fn lock_expected_recovery_attempt(
    connection: &mut PgConnection,
    reservation: &SubscriptionRecoveryReservation,
) -> Result<PaymentAttempt, SubscriptionEnrollmentApplicationError> {
    let identity = reservation.identity();
    let attempt = lock_payment_attempt_by_id_on_connection(
        connection,
        identity.billing_scope_id(),
        identity.attempt_id(),
    )
    .await?
    .ok_or(SubscriptionEnrollmentApplicationError::InvalidState(
        INVALID_APPLICATION_STATE,
    ))?;
    if attempt.identity() != identity
        || attempt.kind() != PaymentAttemptKind::SubscriptionRecovery
        || attempt.request() != reservation.request()
    {
        return Err(SubscriptionEnrollmentApplicationError::InvalidState(
            INVALID_APPLICATION_STATE,
        ));
    }
    Ok(attempt)
}

async fn lock_expected_payment_method_replacement_attempt(
    connection: &mut PgConnection,
    reservation: &SubscriptionPaymentMethodReplacement,
) -> Result<PaymentAttempt, SubscriptionEnrollmentApplicationError> {
    let identity = reservation.identity();
    let attempt = lock_payment_attempt_by_id_on_connection(
        connection,
        identity.billing_scope_id(),
        identity.attempt_id(),
    )
    .await?
    .ok_or(SubscriptionEnrollmentApplicationError::InvalidState(
        INVALID_APPLICATION_STATE,
    ))?;
    if attempt.identity() != identity
        || attempt.kind() != PaymentAttemptKind::SubscriptionPaymentMethodUpdate
        || attempt.request() != reservation.request()
    {
        return Err(SubscriptionEnrollmentApplicationError::InvalidState(
            INVALID_APPLICATION_STATE,
        ));
    }
    Ok(attempt)
}

async fn recovery_subscription_matches(
    connection: &mut PgConnection,
    reservation: &SubscriptionRecoveryReservation,
) -> Result<bool, SubscriptionEnrollmentApplicationError> {
    let identity = reservation.identity();
    let expected = reservation.expected_state();
    let row = sqlx::query(
        r#"
        SELECT status, payment_method_id, initial_transaction_id, next_renewal_at
        FROM billing_subscriptions
        WHERE id = $1 AND billing_scope_id = $2 AND subscriber_id = $3
            AND gateway_account_id = $4 AND plan_key = $5
        FOR UPDATE
        "#,
    )
    .bind(reservation.subscription_id().as_uuid())
    .bind(identity.billing_scope_id().as_uuid())
    .bind(identity.subscriber_id().as_uuid())
    .bind(identity.gateway_account_id().as_uuid())
    .bind(reservation.plan_key().as_str())
    .fetch_optional(&mut *connection)
    .await?;
    let Some(row) = row else {
        return Ok(false);
    };
    let status = row.try_get::<String, _>("status")?;
    let payment_method_id: Uuid = row.try_get("payment_method_id")?;
    let initial_transaction_id: String = row.try_get("initial_transaction_id")?;
    let next_renewal_at: DateTime<Utc> = row.try_get("next_renewal_at")?;
    Ok(status == expected.status().as_str()
        && matches!(status.as_str(), "active" | "past_due")
        && payment_method_id == expected.payment_method_id().into_uuid()
        && syrup_rail::canonical_gateway_transaction_ids_equal(
            &initial_transaction_id,
            expected.initial_transaction_id().expose(),
        )
        && next_renewal_at == *reservation.period().start_at())
}

async fn disable_payment_method_if_unreferenced(
    connection: &mut PgConnection,
    payment_method_id: PaymentMethodId,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        UPDATE billing_payment_methods AS methods
        SET status = 'disabled', updated_at = clock_timestamp()
        WHERE methods.id = $1 AND methods.status = 'active'
            AND NOT EXISTS (
                SELECT 1 FROM billing_subscriptions AS subscriptions
                WHERE subscriptions.payment_method_id = methods.id
                    AND subscriptions.status <> 'canceled'
            )
        "#,
    )
    .bind(payment_method_id.as_uuid())
    .execute(connection)
    .await?;
    Ok(())
}

async fn advance_subscription_discount_after_successful_charge(
    connection: &mut PgConnection,
    subscription_id: SubscriptionId,
    plan_key: &PlanKey,
) -> Result<(), SubscriptionEnrollmentApplicationError> {
    let row = sqlx::query(
        r#"
        SELECT duration, status, periods_total, periods_applied, base_amount_cents
        FROM billing_subscription_discounts
        WHERE subscription_id = $1 AND plan_key = $2
        FOR UPDATE
        "#,
    )
    .bind(subscription_id.as_uuid())
    .bind(plan_key.as_str())
    .fetch_optional(&mut *connection)
    .await?;
    let Some(row) = row else {
        return Ok(());
    };
    let duration: String = row.try_get("duration")?;
    let status: String = row.try_get("status")?;
    if duration != "limited_months" || status == "completed" {
        return Ok(());
    }
    let periods_total: i32 = row.try_get("periods_total")?;
    let periods_applied: i32 = row.try_get("periods_applied")?;
    if periods_applied >= periods_total {
        return Err(SubscriptionEnrollmentApplicationError::InvalidState(
            INVALID_APPLICATION_STATE,
        ));
    }
    let next_periods_applied = periods_applied + 1;
    let completed = next_periods_applied == periods_total;
    sqlx::query(
        r#"
        UPDATE billing_subscription_discounts
        SET periods_applied = $3,
            status = CASE WHEN $4 THEN 'completed' ELSE 'active' END,
            completed_at = CASE WHEN $4 THEN clock_timestamp() ELSE NULL END
        WHERE subscription_id = $1 AND plan_key = $2
        "#,
    )
    .bind(subscription_id.as_uuid())
    .bind(plan_key.as_str())
    .bind(next_periods_applied)
    .bind(completed)
    .execute(&mut *connection)
    .await?;
    if completed {
        let base_amount_cents: i32 = row.try_get("base_amount_cents")?;
        sqlx::query(
            "UPDATE billing_subscriptions SET amount_cents = $2, updated_at = clock_timestamp() WHERE id = $1",
        )
        .bind(subscription_id.as_uuid())
        .bind(base_amount_cents)
        .execute(&mut *connection)
        .await?;
    }
    Ok(())
}

pub(crate) async fn resolve_recovery_non_approved_outcome(
    pool: &PgPool,
    reservation: &SubscriptionRecoveryReservation,
    evidence: &ProcessorEvidence,
    status: PaymentAttemptStatus,
    resolution_code: Option<PaymentResolutionCode>,
    cooldown: Option<RateLimitCooldown>,
    boundary: OutcomeResolutionBoundary,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    let mut transaction = pool.begin().await?;
    set_application_timeouts(&mut transaction).await?;
    lock_subscription_aggregate(
        &mut transaction,
        reservation.identity().subscriber_id(),
        reservation.plan_key(),
    )
    .await?;
    let attempt = lock_expected_recovery_attempt(&mut transaction, reservation).await?;
    let may_resolve = attempt.status().is_resolvable()
        && match boundary {
            OutcomeResolutionBoundary::Prepared => {
                attempt.state().timestamps().submitted_at().is_none()
            }
            OutcomeResolutionBoundary::AdmittedNotSubmitted => {
                attempt.state().timestamps().submitted_at().is_some()
            }
            OutcomeResolutionBoundary::Submitted => true,
        };
    if may_resolve {
        update_attempt_resolution(
            &mut transaction,
            &attempt,
            evidence,
            status,
            resolution_code,
            None,
            None,
            false,
        )
        .await?;
        if boundary == OutcomeResolutionBoundary::AdmittedNotSubmitted {
            sqlx::query(
                "UPDATE billing_payment_attempts SET submitted_at = NULL, updated_at = clock_timestamp() WHERE id = $1",
            )
            .bind(attempt.identity().attempt_id().as_uuid())
            .execute(&mut *transaction)
            .await?;
        }
    }
    if let Some(cooldown) = cooldown {
        extend_recovery_rate_limit_cooldown(&mut transaction, reservation, cooldown).await?;
    }
    let attempt = find_payment_attempt_by_id_on_connection(
        &mut transaction,
        reservation.identity().billing_scope_id(),
        reservation.identity().attempt_id(),
    )
    .await?
    .ok_or(SubscriptionEnrollmentApplicationError::InvalidState(
        INVALID_APPLICATION_STATE,
    ))?;
    let result = payment_result_for_attempt(&mut transaction, attempt).await?;
    transaction.commit().await?;
    Ok(result)
}

pub(crate) async fn resolve_payment_method_replacement_non_approved_outcome(
    pool: &PgPool,
    reservation: &SubscriptionPaymentMethodReplacement,
    evidence: &ProcessorEvidence,
    status: PaymentAttemptStatus,
    resolution_code: Option<PaymentResolutionCode>,
    cooldown: Option<RateLimitCooldown>,
    boundary: OutcomeResolutionBoundary,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    let mut transaction = pool.begin().await?;
    set_application_timeouts(&mut transaction).await?;
    lock_subscription_aggregate(
        &mut transaction,
        reservation.identity().subscriber_id(),
        reservation.plan_key(),
    )
    .await?;
    let attempt =
        lock_expected_payment_method_replacement_attempt(&mut transaction, reservation).await?;
    let may_resolve = attempt.status().is_resolvable()
        && match boundary {
            OutcomeResolutionBoundary::Prepared => {
                attempt.state().timestamps().submitted_at().is_none()
            }
            OutcomeResolutionBoundary::AdmittedNotSubmitted => {
                attempt.state().timestamps().submitted_at().is_some()
            }
            OutcomeResolutionBoundary::Submitted => true,
        };
    if may_resolve {
        update_attempt_resolution(
            &mut transaction,
            &attempt,
            evidence,
            status,
            resolution_code,
            None,
            None,
            false,
        )
        .await?;
        if boundary == OutcomeResolutionBoundary::AdmittedNotSubmitted {
            sqlx::query(
                "UPDATE billing_payment_attempts SET submitted_at = NULL, updated_at = clock_timestamp() WHERE id = $1",
            )
            .bind(attempt.identity().attempt_id().as_uuid())
            .execute(&mut *transaction)
            .await?;
        }
    }
    if let Some(cooldown) = cooldown {
        extend_payment_method_replacement_rate_limit_cooldown(
            &mut transaction,
            reservation,
            cooldown,
        )
        .await?;
    }
    let attempt = find_payment_attempt_by_id_on_connection(
        &mut transaction,
        reservation.identity().billing_scope_id(),
        reservation.identity().attempt_id(),
    )
    .await?
    .ok_or(SubscriptionEnrollmentApplicationError::InvalidState(
        INVALID_APPLICATION_STATE,
    ))?;
    let result = payment_result_for_attempt(&mut transaction, attempt).await?;
    transaction.commit().await?;
    Ok(result)
}

async fn resolve_recovery_unknown_outcome(
    pool: &PgPool,
    reservation: &SubscriptionRecoveryReservation,
    evidence: &ProcessorEvidence,
    cooldown: Option<RateLimitCooldown>,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    let mut transaction = pool.begin().await?;
    set_application_timeouts(&mut transaction).await?;
    lock_subscription_aggregate(
        &mut transaction,
        reservation.identity().subscriber_id(),
        reservation.plan_key(),
    )
    .await?;
    let attempt = lock_expected_recovery_attempt(&mut transaction, reservation).await?;
    if attempt.status().is_resolvable() {
        update_attempt_resolution(
            &mut transaction,
            &attempt,
            evidence,
            PaymentAttemptStatus::Unknown,
            None,
            None,
            None,
            false,
        )
        .await?;
        if evidence_looks_approved(evidence) {
            observe_processor_charge(
                &mut transaction,
                &attempt,
                evidence,
                ChargeProgression::Pending,
            )
            .await?;
        }
    }
    if let Some(cooldown) = cooldown {
        extend_recovery_rate_limit_cooldown(&mut transaction, reservation, cooldown).await?;
    }
    let attempt = find_payment_attempt_by_id_on_connection(
        &mut transaction,
        reservation.identity().billing_scope_id(),
        reservation.identity().attempt_id(),
    )
    .await?
    .ok_or(SubscriptionEnrollmentApplicationError::InvalidState(
        INVALID_APPLICATION_STATE,
    ))?;
    let result = payment_result_for_attempt(&mut transaction, attempt).await?;
    transaction.commit().await?;
    Ok(result)
}

async fn resolve_payment_method_replacement_unknown_outcome(
    pool: &PgPool,
    reservation: &SubscriptionPaymentMethodReplacement,
    evidence: &ProcessorEvidence,
    cooldown: Option<RateLimitCooldown>,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    let mut transaction = pool.begin().await?;
    set_application_timeouts(&mut transaction).await?;
    lock_subscription_aggregate(
        &mut transaction,
        reservation.identity().subscriber_id(),
        reservation.plan_key(),
    )
    .await?;
    let attempt =
        lock_expected_payment_method_replacement_attempt(&mut transaction, reservation).await?;
    if attempt.status().is_resolvable() && attempt.status() != PaymentAttemptStatus::ReviewRequired
    {
        update_attempt_resolution(
            &mut transaction,
            &attempt,
            evidence,
            PaymentAttemptStatus::Unknown,
            None,
            None,
            None,
            false,
        )
        .await?;
        if evidence_looks_approved(evidence) {
            observe_processor_charge(
                &mut transaction,
                &attempt,
                evidence,
                ChargeProgression::Pending,
            )
            .await?;
        }
    }
    if let Some(cooldown) = cooldown {
        extend_payment_method_replacement_rate_limit_cooldown(
            &mut transaction,
            reservation,
            cooldown,
        )
        .await?;
    }
    let attempt = find_payment_attempt_by_id_on_connection(
        &mut transaction,
        reservation.identity().billing_scope_id(),
        reservation.identity().attempt_id(),
    )
    .await?
    .ok_or(SubscriptionEnrollmentApplicationError::InvalidState(
        INVALID_APPLICATION_STATE,
    ))?;
    let result = payment_result_for_attempt(&mut transaction, attempt).await?;
    transaction.commit().await?;
    Ok(result)
}

async fn park_recovery_approved_outcome(
    pool: &PgPool,
    reservation: &SubscriptionRecoveryReservation,
    evidence: &ProcessorEvidence,
    message: &'static str,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    match try_park_recovery_approved_outcome(pool, reservation, evidence, message).await {
        Ok(result) => Ok(result),
        Err(_) => {
            observe_recovery_approved_evidence_with_retry(pool, reservation, evidence).await?;
            let mut transaction = pool.begin().await?;
            let attempt = find_payment_attempt_by_id_on_connection(
                &mut transaction,
                reservation.identity().billing_scope_id(),
                reservation.identity().attempt_id(),
            )
            .await?
            .ok_or(SubscriptionEnrollmentApplicationError::InvalidState(
                INVALID_APPLICATION_STATE,
            ))?;
            let result = if attempt.status() == PaymentAttemptStatus::Approved {
                payment_result_for_attempt(&mut transaction, attempt).await?
            } else {
                SubscriptionEnrollmentPaymentResult::confirmation_pending(
                    attempt,
                    None,
                    evidence.clone(),
                )
            };
            transaction.commit().await?;
            Ok(result)
        }
    }
}

async fn park_payment_method_replacement_approved_outcome(
    pool: &PgPool,
    reservation: &SubscriptionPaymentMethodReplacement,
    evidence: &ProcessorEvidence,
    message: &'static str,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    match try_park_payment_method_replacement_approved_outcome(pool, reservation, evidence, message)
        .await
    {
        Ok(result) => Ok(result),
        Err(_) => {
            observe_payment_method_replacement_approved_evidence_with_retry(
                pool,
                reservation,
                evidence,
            )
            .await?;
            let mut transaction = pool.begin().await?;
            let attempt = find_payment_attempt_by_id_on_connection(
                &mut transaction,
                reservation.identity().billing_scope_id(),
                reservation.identity().attempt_id(),
            )
            .await?
            .ok_or(SubscriptionEnrollmentApplicationError::InvalidState(
                INVALID_APPLICATION_STATE,
            ))?;
            let result = if attempt.status() == PaymentAttemptStatus::Approved {
                payment_result_for_attempt(&mut transaction, attempt).await?
            } else {
                SubscriptionEnrollmentPaymentResult::confirmation_pending(
                    attempt,
                    None,
                    evidence.clone(),
                )
            };
            transaction.commit().await?;
            Ok(result)
        }
    }
}

async fn try_park_payment_method_replacement_approved_outcome(
    pool: &PgPool,
    reservation: &SubscriptionPaymentMethodReplacement,
    evidence: &ProcessorEvidence,
    message: &'static str,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    let mut transaction = pool.begin().await?;
    set_application_timeouts(&mut transaction).await?;
    let attempt =
        lock_expected_payment_method_replacement_attempt(&mut transaction, reservation).await?;
    let attempt = if attempt.status() == PaymentAttemptStatus::Approved {
        observe_processor_charge(
            &mut transaction,
            &attempt,
            evidence,
            ChargeProgression::Applied,
        )
        .await?;
        attempt
    } else if attempt.status().is_terminal() {
        observe_processor_charge(
            &mut transaction,
            &attempt,
            evidence,
            ChargeProgression::ReconciliationRequired,
        )
        .await?;
        attempt
    } else {
        observe_processor_charge(
            &mut transaction,
            &attempt,
            evidence,
            ChargeProgression::Pending,
        )
        .await?;
        park_locked_attempt(&mut transaction, &attempt, evidence, None, message).await?
    };
    let result = payment_result_for_attempt(&mut transaction, attempt).await?;
    transaction.commit().await?;
    Ok(result)
}

async fn observe_payment_method_replacement_approved_evidence_with_retry(
    pool: &PgPool,
    reservation: &SubscriptionPaymentMethodReplacement,
    evidence: &ProcessorEvidence,
) -> Result<(), SubscriptionEnrollmentApplicationError> {
    for attempt_index in 0..APPROVED_EVIDENCE_WRITE_ATTEMPTS {
        let result = async {
            let mut transaction = pool.begin().await?;
            set_application_timeouts(&mut transaction).await?;
            let attempt =
                lock_expected_payment_method_replacement_attempt(&mut transaction, reservation)
                    .await?;
            observe_processor_charge(
                &mut transaction,
                &attempt,
                evidence,
                ChargeProgression::Pending,
            )
            .await?;
            transaction.commit().await?;
            Ok::<(), SubscriptionEnrollmentApplicationError>(())
        }
        .await;
        match result {
            Ok(()) => return Ok(()),
            Err(error)
                if is_retryable_evidence_error(&error)
                    && attempt_index + 1 < APPROVED_EVIDENCE_WRITE_ATTEMPTS =>
            {
                tokio::time::sleep(APPROVED_EVIDENCE_RETRY_DELAY).await;
            }
            Err(error) if is_retryable_evidence_error(&error) => break,
            Err(error) => return Err(error),
        }
    }
    observe_payment_method_replacement_approved_evidence_without_attempt_lock(
        pool,
        reservation,
        evidence,
    )
    .await
}

async fn observe_payment_method_replacement_approved_evidence_without_attempt_lock(
    pool: &PgPool,
    reservation: &SubscriptionPaymentMethodReplacement,
    evidence: &ProcessorEvidence,
) -> Result<(), SubscriptionEnrollmentApplicationError> {
    let identity = reservation.identity();
    let request = reservation.request();
    let descriptor = evidence.descriptor();
    let transaction_id = evidence.transaction_id().map(GatewayTransactionId::expose);
    let mut transaction = pool.begin().await?;
    set_application_timeouts(&mut transaction).await?;
    for _ in 0..2 {
        let has_existing_charge: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM billing_processor_charges WHERE attempt_id = $1)",
        )
        .bind(identity.attempt_id().as_uuid())
        .fetch_one(&mut *transaction)
        .await?;
        let role = if has_existing_charge {
            "additional"
        } else {
            "primary"
        };
        let inserted = sqlx::query_scalar::<_, Uuid>(
            r#"
            INSERT INTO billing_processor_charges (
                id, attempt_id, billing_scope_id, gateway_account_id, gateway_order_id,
                gateway_transaction_id, gateway_payment_method_reference,
                gateway_response, gateway_response_code, gateway_response_text,
                gateway_condition, payment_type, card_brand, card_last4,
                card_exp_month, card_exp_year, charge_role, progression_state,
                attempt_kind, plan_key, host_charge_target_id, amount_cents, currency
            ) VALUES (
                $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
                $14, $15, $16, $17, 'pending',
                'subscription_payment_method_update', $18, NULL, 0, $19
            )
            ON CONFLICT DO NOTHING
            RETURNING id
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(identity.attempt_id().as_uuid())
        .bind(identity.billing_scope_id().as_uuid())
        .bind(identity.gateway_account_id().as_uuid())
        .bind(request.gateway_order_id().expose())
        .bind(transaction_id)
        .bind(
            evidence
                .payment_method_reference()
                .map(|value| value.expose()),
        )
        .bind(evidence.response().map(GatewayDiagnostic::expose))
        .bind(evidence.response_code().map(GatewayDiagnostic::expose))
        .bind(evidence.response_text().map(GatewayDiagnostic::expose))
        .bind(evidence.condition().map(GatewayDiagnostic::expose))
        .bind(descriptor.payment_type().map(GatewayDiagnostic::expose))
        .bind(descriptor.card_brand().map(GatewayDiagnostic::expose))
        .bind(descriptor.card_last_four().map(|value| value.expose()))
        .bind(descriptor.card_exp_month())
        .bind(descriptor.card_exp_year())
        .bind(role)
        .bind(reservation.plan_key().as_str())
        .bind(request.amount().currency().as_str())
        .fetch_optional(&mut *transaction)
        .await?;
        if inserted.is_some() {
            transaction.commit().await?;
            return Ok(());
        }
    }
    let evidence_matches = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT gateway_payment_method_reference IS NOT DISTINCT FROM $3
            AND gateway_response IS NOT DISTINCT FROM $4
            AND gateway_response_code IS NOT DISTINCT FROM $5
            AND gateway_response_text IS NOT DISTINCT FROM $6
            AND gateway_condition IS NOT DISTINCT FROM $7
            AND payment_type IS NOT DISTINCT FROM $8
            AND card_brand IS NOT DISTINCT FROM $9
            AND card_last4 IS NOT DISTINCT FROM $10
            AND card_exp_month IS NOT DISTINCT FROM $11
            AND card_exp_year IS NOT DISTINCT FROM $12
        FROM billing_processor_charges
        WHERE attempt_id = $1 AND gateway_transaction_id IS NOT DISTINCT FROM $2
        "#,
    )
    .bind(identity.attempt_id().as_uuid())
    .bind(transaction_id)
    .bind(
        evidence
            .payment_method_reference()
            .map(|value| value.expose()),
    )
    .bind(evidence.response().map(GatewayDiagnostic::expose))
    .bind(evidence.response_code().map(GatewayDiagnostic::expose))
    .bind(evidence.response_text().map(GatewayDiagnostic::expose))
    .bind(evidence.condition().map(GatewayDiagnostic::expose))
    .bind(descriptor.payment_type().map(GatewayDiagnostic::expose))
    .bind(descriptor.card_brand().map(GatewayDiagnostic::expose))
    .bind(descriptor.card_last_four().map(|value| value.expose()))
    .bind(descriptor.card_exp_month())
    .bind(descriptor.card_exp_year())
    .fetch_optional(&mut *transaction)
    .await?;
    if evidence_matches == Some(true) {
        transaction.commit().await?;
        return Ok(());
    }
    if let Some(transaction_id) = transaction_id {
        let owned_elsewhere: bool = sqlx::query_scalar(
            r#"
            SELECT EXISTS (
                SELECT 1 FROM billing_processor_charges
                WHERE gateway_account_id = $1 AND gateway_transaction_id = $2
                    AND attempt_id <> $3
            )
            "#,
        )
        .bind(identity.gateway_account_id().as_uuid())
        .bind(transaction_id)
        .bind(identity.attempt_id().as_uuid())
        .fetch_one(&mut *transaction)
        .await?;
        if owned_elsewhere {
            transaction.commit().await?;
            return Ok(());
        }
    }
    Err(SubscriptionEnrollmentApplicationError::ApprovedEvidenceNotDurable)
}

async fn try_park_recovery_approved_outcome(
    pool: &PgPool,
    reservation: &SubscriptionRecoveryReservation,
    evidence: &ProcessorEvidence,
    message: &'static str,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    let mut transaction = pool.begin().await?;
    set_application_timeouts(&mut transaction).await?;
    lock_subscription_aggregate(
        &mut transaction,
        reservation.identity().subscriber_id(),
        reservation.plan_key(),
    )
    .await?;
    let attempt = lock_expected_recovery_attempt(&mut transaction, reservation).await?;
    let attempt = if attempt.status() == PaymentAttemptStatus::Approved {
        observe_processor_charge(
            &mut transaction,
            &attempt,
            evidence,
            ChargeProgression::Applied,
        )
        .await?;
        attempt
    } else if attempt.status().is_terminal() {
        let progression =
            if evidence.transaction_id().is_some() && attempt.request().amount().cents() > 0 {
                ChargeProgression::ExternalReversalRequired
            } else {
                ChargeProgression::ReconciliationRequired
            };
        observe_processor_charge(&mut transaction, &attempt, evidence, progression).await?;
        attempt
    } else {
        observe_processor_charge(
            &mut transaction,
            &attempt,
            evidence,
            ChargeProgression::Pending,
        )
        .await?;
        park_locked_attempt(&mut transaction, &attempt, evidence, None, message).await?
    };
    let result = payment_result_for_attempt(&mut transaction, attempt).await?;
    transaction.commit().await?;
    Ok(result)
}

async fn observe_recovery_approved_evidence_with_retry(
    pool: &PgPool,
    reservation: &SubscriptionRecoveryReservation,
    evidence: &ProcessorEvidence,
) -> Result<(), SubscriptionEnrollmentApplicationError> {
    for attempt_index in 0..APPROVED_EVIDENCE_WRITE_ATTEMPTS {
        let result = async {
            let mut transaction = pool.begin().await?;
            set_application_timeouts(&mut transaction).await?;
            let attempt = lock_expected_recovery_attempt(&mut transaction, reservation).await?;
            observe_processor_charge(
                &mut transaction,
                &attempt,
                evidence,
                ChargeProgression::Pending,
            )
            .await?;
            transaction.commit().await?;
            Ok::<(), SubscriptionEnrollmentApplicationError>(())
        }
        .await;
        match result {
            Ok(()) => return Ok(()),
            Err(error)
                if is_retryable_evidence_error(&error)
                    && attempt_index + 1 < APPROVED_EVIDENCE_WRITE_ATTEMPTS =>
            {
                tokio::time::sleep(APPROVED_EVIDENCE_RETRY_DELAY).await;
            }
            Err(error) if is_retryable_evidence_error(&error) => break,
            Err(error) => return Err(error),
        }
    }
    observe_recovery_approved_evidence_without_attempt_lock(pool, reservation, evidence).await
}

async fn observe_recovery_approved_evidence_without_attempt_lock(
    pool: &PgPool,
    reservation: &SubscriptionRecoveryReservation,
    evidence: &ProcessorEvidence,
) -> Result<(), SubscriptionEnrollmentApplicationError> {
    let identity = reservation.identity();
    let request = reservation.request();
    let descriptor = evidence.descriptor();
    let transaction_id = evidence.transaction_id().map(GatewayTransactionId::expose);
    let mut transaction = pool.begin().await?;
    set_application_timeouts(&mut transaction).await?;
    for _ in 0..2 {
        let has_existing_charge: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM billing_processor_charges WHERE attempt_id = $1)",
        )
        .bind(identity.attempt_id().as_uuid())
        .fetch_one(&mut *transaction)
        .await?;
        let role = if has_existing_charge {
            "additional"
        } else {
            "primary"
        };
        let inserted = sqlx::query_scalar::<_, Uuid>(
            r#"
            INSERT INTO billing_processor_charges (
                id, attempt_id, billing_scope_id, gateway_account_id, gateway_order_id,
                gateway_transaction_id, gateway_payment_method_reference,
                gateway_response, gateway_response_code, gateway_response_text,
                gateway_condition, payment_type, card_brand, card_last4,
                card_exp_month, card_exp_year, charge_role, progression_state,
                attempt_kind, plan_key, host_charge_target_id, amount_cents, currency
            ) VALUES (
                $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
                $14, $15, $16, $17, 'pending', 'subscription_recovery', $18,
                NULL, $19, $20
            )
            ON CONFLICT DO NOTHING
            RETURNING id
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(identity.attempt_id().as_uuid())
        .bind(identity.billing_scope_id().as_uuid())
        .bind(identity.gateway_account_id().as_uuid())
        .bind(request.gateway_order_id().expose())
        .bind(transaction_id)
        .bind(
            evidence
                .payment_method_reference()
                .map(|value| value.expose()),
        )
        .bind(evidence.response().map(GatewayDiagnostic::expose))
        .bind(evidence.response_code().map(GatewayDiagnostic::expose))
        .bind(evidence.response_text().map(GatewayDiagnostic::expose))
        .bind(evidence.condition().map(GatewayDiagnostic::expose))
        .bind(descriptor.payment_type().map(GatewayDiagnostic::expose))
        .bind(descriptor.card_brand().map(GatewayDiagnostic::expose))
        .bind(descriptor.card_last_four().map(|value| value.expose()))
        .bind(descriptor.card_exp_month())
        .bind(descriptor.card_exp_year())
        .bind(role)
        .bind(reservation.plan_key().as_str())
        .bind(request.amount().cents())
        .bind(request.amount().currency().as_str())
        .fetch_optional(&mut *transaction)
        .await?;
        if inserted.is_some() {
            transaction.commit().await?;
            return Ok(());
        }
    }
    let evidence_matches = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT gateway_payment_method_reference IS NOT DISTINCT FROM $3
            AND gateway_response IS NOT DISTINCT FROM $4
            AND gateway_response_code IS NOT DISTINCT FROM $5
            AND gateway_response_text IS NOT DISTINCT FROM $6
            AND gateway_condition IS NOT DISTINCT FROM $7
            AND payment_type IS NOT DISTINCT FROM $8
            AND card_brand IS NOT DISTINCT FROM $9
            AND card_last4 IS NOT DISTINCT FROM $10
            AND card_exp_month IS NOT DISTINCT FROM $11
            AND card_exp_year IS NOT DISTINCT FROM $12
        FROM billing_processor_charges
        WHERE attempt_id = $1 AND gateway_transaction_id IS NOT DISTINCT FROM $2
        "#,
    )
    .bind(identity.attempt_id().as_uuid())
    .bind(transaction_id)
    .bind(
        evidence
            .payment_method_reference()
            .map(|value| value.expose()),
    )
    .bind(evidence.response().map(GatewayDiagnostic::expose))
    .bind(evidence.response_code().map(GatewayDiagnostic::expose))
    .bind(evidence.response_text().map(GatewayDiagnostic::expose))
    .bind(evidence.condition().map(GatewayDiagnostic::expose))
    .bind(descriptor.payment_type().map(GatewayDiagnostic::expose))
    .bind(descriptor.card_brand().map(GatewayDiagnostic::expose))
    .bind(descriptor.card_last_four().map(|value| value.expose()))
    .bind(descriptor.card_exp_month())
    .bind(descriptor.card_exp_year())
    .fetch_optional(&mut *transaction)
    .await?;
    if evidence_matches == Some(true) {
        transaction.commit().await?;
        return Ok(());
    }
    if let Some(transaction_id) = transaction_id {
        let owned_elsewhere: bool = sqlx::query_scalar(
            r#"
            SELECT EXISTS (
                SELECT 1 FROM billing_processor_charges
                WHERE gateway_account_id = $1 AND gateway_transaction_id = $2
                    AND attempt_id <> $3
            )
            "#,
        )
        .bind(identity.gateway_account_id().as_uuid())
        .bind(transaction_id)
        .bind(identity.attempt_id().as_uuid())
        .fetch_one(&mut *transaction)
        .await?;
        if owned_elsewhere {
            transaction.commit().await?;
            return Ok(());
        }
    }
    Err(SubscriptionEnrollmentApplicationError::ApprovedEvidenceNotDurable)
}

async fn extend_recovery_rate_limit_cooldown(
    connection: &mut PgConnection,
    reservation: &SubscriptionRecoveryReservation,
    cooldown: RateLimitCooldown,
) -> Result<(), SubscriptionEnrollmentApplicationError> {
    let identity = reservation.identity();
    let result = match cooldown {
        RateLimitCooldown::Account => {
            sqlx::query(
                r#"
                UPDATE billing_gateway_accounts
                SET mutation_rate_limited_until = GREATEST(
                        COALESCE(mutation_rate_limited_until, '-infinity'::timestamptz),
                        clock_timestamp() + make_interval(secs => $4)
                    ),
                    updated_at = clock_timestamp()
                WHERE id = $1 AND billing_scope_id = $2
                    AND gateway_configuration_id = $3
                "#,
            )
            .bind(identity.gateway_account_id().as_uuid())
            .bind(identity.billing_scope_id().as_uuid())
            .bind(identity.gateway_configuration_id().as_uuid())
            .bind(PROVIDER_RATE_LIMIT_RETRY_AFTER_SECONDS)
            .execute(&mut *connection)
            .await?
        }
        RateLimitCooldown::Provider => {
            sqlx::query(
                r#"
                UPDATE billing_gateway_provider_rate_limits
                SET rate_limited_until = GREATEST(
                        rate_limited_until,
                        clock_timestamp() + make_interval(secs => $2)
                    ),
                    updated_at = clock_timestamp()
                WHERE provider_key = $1
                "#,
            )
            .bind(reservation.provider_key().as_str())
            .bind(PROVIDER_RATE_LIMIT_RETRY_AFTER_SECONDS)
            .execute(&mut *connection)
            .await?
        }
    };
    if result.rows_affected() != 1 {
        return Err(SubscriptionEnrollmentApplicationError::InvalidState(
            INVALID_APPLICATION_STATE,
        ));
    }
    Ok(())
}

async fn extend_payment_method_replacement_rate_limit_cooldown(
    connection: &mut PgConnection,
    reservation: &SubscriptionPaymentMethodReplacement,
    cooldown: RateLimitCooldown,
) -> Result<(), SubscriptionEnrollmentApplicationError> {
    let identity = reservation.identity();
    let result = match cooldown {
        RateLimitCooldown::Account => {
            sqlx::query(
                r#"
                UPDATE billing_gateway_accounts
                SET mutation_rate_limited_until = GREATEST(
                        COALESCE(mutation_rate_limited_until, '-infinity'::timestamptz),
                        clock_timestamp() + make_interval(secs => $4)
                    ), updated_at = clock_timestamp()
                WHERE id = $1 AND billing_scope_id = $2
                    AND gateway_configuration_id = $3
                "#,
            )
            .bind(identity.gateway_account_id().as_uuid())
            .bind(identity.billing_scope_id().as_uuid())
            .bind(identity.gateway_configuration_id().as_uuid())
            .bind(PROVIDER_RATE_LIMIT_RETRY_AFTER_SECONDS)
            .execute(&mut *connection)
            .await?
        }
        RateLimitCooldown::Provider => {
            sqlx::query(
                r#"
                UPDATE billing_gateway_provider_rate_limits
                SET rate_limited_until = GREATEST(
                        rate_limited_until,
                        clock_timestamp() + make_interval(secs => $2)
                    ), updated_at = clock_timestamp()
                WHERE provider_key = $1
                "#,
            )
            .bind(reservation.provider_key().as_str())
            .bind(PROVIDER_RATE_LIMIT_RETRY_AFTER_SECONDS)
            .execute(&mut *connection)
            .await?
        }
    };
    if result.rows_affected() != 1 {
        return Err(SubscriptionEnrollmentApplicationError::InvalidState(
            INVALID_APPLICATION_STATE,
        ));
    }
    Ok(())
}

async fn apply_approved_outcome(
    coordinator: &dyn BillingTransactionCoordinator,
    reservation: &SubscriptionEnrollmentReservation,
    evidence: &ProcessorEvidence,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    let identity = reservation.identity();
    let mut transaction = coordinator
        .begin(
            BillingEventSubject::new(identity.billing_scope_id(), identity.subscriber_id()),
            BILLING_LOCK_TIMEOUT,
        )
        .await?;

    let subject_state = transaction.subject_state();
    let application = apply_approved_on_connection(
        transaction.connection(),
        subject_state,
        reservation,
        evidence,
    )
    .await;
    let (result, event) = match application {
        Ok(application) => application,
        Err(error) => {
            let _ = transaction.rollback().await;
            return Err(error);
        }
    };
    if let Some(event) = event.as_ref()
        && let Err(error) = transaction.append_event(event).await
    {
        let _ = transaction.rollback().await;
        return Err(error.into());
    }
    transaction.commit().await?;
    Ok(result)
}

async fn apply_approved_on_connection(
    connection: &mut PgConnection,
    subject_state: BillingTransactionSubjectState,
    reservation: &SubscriptionEnrollmentReservation,
    evidence: &ProcessorEvidence,
) -> Result<
    (SubscriptionEnrollmentPaymentResult, Option<BillingEvent>),
    SubscriptionEnrollmentApplicationError,
> {
    set_application_timeouts(connection).await?;
    let identity = reservation.identity();
    lock_payment_method_domain(
        connection,
        identity.subscriber_id(),
        identity.gateway_account_id().as_uuid(),
    )
    .await?;
    lock_subscription_aggregate(connection, identity.subscriber_id(), reservation.plan_key())
        .await?;
    let attempt = lock_expected_attempt(connection, reservation).await?;

    if attempt.status() == PaymentAttemptStatus::Approved {
        let subscription = load_applied_subscription(connection, &attempt).await?;
        let Some(subscription) = subscription else {
            return Err(SubscriptionEnrollmentApplicationError::InvalidState(
                INVALID_APPLICATION_STATE,
            ));
        };
        observe_processor_charge(connection, &attempt, evidence, ChargeProgression::Applied)
            .await?;
        return Ok((
            SubscriptionEnrollmentPaymentResult::new(attempt, Some(subscription)),
            None,
        ));
    }

    if subject_state != BillingTransactionSubjectState::LiveRecipient {
        return Err(SubscriptionEnrollmentApplicationError::InvalidState(
            "a new subscription enrollment event requires a live recipient",
        ));
    }

    if attempt.status().is_terminal() {
        let observation = observe_processor_charge(
            connection,
            &attempt,
            evidence,
            ChargeProgression::ExternalReversalRequired,
        )
        .await?;
        let parked = park_locked_attempt(
            connection,
            &attempt,
            evidence,
            None,
            TERMINAL_APPROVAL_RACE_TEXT,
        )
        .await?;
        if let ObservedCharge::Owned(charge) = observation {
            transition_charge(
                connection,
                charge.id,
                ChargeProgression::ExternalReversalRequired,
                None,
            )
            .await?;
        }
        return Ok((SubscriptionEnrollmentPaymentResult::new(parked, None), None));
    }

    let observation =
        observe_processor_charge(connection, &attempt, evidence, ChargeProgression::Pending)
            .await?;
    let ObservedCharge::Owned(charge) = observation else {
        let parked = park_locked_attempt(
            connection,
            &attempt,
            evidence,
            None,
            "The approved gateway transaction is already owned by another payment attempt.",
        )
        .await?;
        return Ok((SubscriptionEnrollmentPaymentResult::new(parked, None), None));
    };
    if charge.role == ChargeRole::Additional {
        transition_charge(
            connection,
            charge.id,
            ChargeProgression::ExternalReversalRequired,
            None,
        )
        .await?;
        let parked = park_locked_attempt(
            connection,
            &attempt,
            evidence,
            None,
            "An additional approved charge requires manual reversal review.",
        )
        .await?;
        return Ok((SubscriptionEnrollmentPaymentResult::new(parked, None), None));
    }

    if current_subscription_exists(connection, reservation).await? {
        transition_charge(
            connection,
            charge.id,
            ChargeProgression::ExternalReversalRequired,
            Some(PaymentResolutionCode::SubscriptionInitialCurrentSubscriptionConflict),
        )
        .await?;
        let parked = park_locked_attempt(
            connection,
            &attempt,
            evidence,
            Some(PaymentResolutionCode::SubscriptionInitialCurrentSubscriptionConflict),
            CURRENT_SUBSCRIPTION_CONFLICT_TEXT,
        )
        .await?;
        return Ok((SubscriptionEnrollmentPaymentResult::new(parked, None), None));
    }
    if active_grant_exists(connection, reservation).await? {
        transition_charge(
            connection,
            charge.id,
            ChargeProgression::ExternalReversalRequired,
            Some(PaymentResolutionCode::SubscriptionInitialCurrentGrantConflict),
        )
        .await?;
        let parked = park_locked_attempt(
            connection,
            &attempt,
            evidence,
            Some(PaymentResolutionCode::SubscriptionInitialCurrentGrantConflict),
            CURRENT_GRANT_CONFLICT_TEXT,
        )
        .await?;
        return Ok((SubscriptionEnrollmentPaymentResult::new(parked, None), None));
    }

    let transaction_id =
        evidence
            .transaction_id()
            .ok_or(SubscriptionEnrollmentApplicationError::InvalidState(
                INVALID_APPLICATION_STATE,
            ))?;
    let method_id = upsert_payment_method(connection, &attempt, evidence).await?;
    let period_start_at = attempt.state().timestamps().submitted_or_created_at();
    let period = next_monthly_billing_period(period_start_at).map_err(|_| {
        SubscriptionEnrollmentApplicationError::InvalidState(INVALID_APPLICATION_STATE)
    })?;
    let subscription_id = SubscriptionId::new(Uuid::now_v7());
    let recurring_cents = recurring_amount_after_initial(&attempt)?;
    insert_subscription(
        connection,
        &attempt,
        subscription_id,
        method_id,
        recurring_cents,
        &period,
        transaction_id,
    )
    .await?;
    apply_initial_discount(connection, &attempt, subscription_id, period_start_at).await?;
    mark_attempt_approved(connection, &attempt, evidence, subscription_id, method_id).await?;
    transition_charge(connection, charge.id, ChargeProgression::Applied, None).await?;

    let attempt = find_payment_attempt_by_id_on_connection(
        connection,
        identity.billing_scope_id(),
        identity.attempt_id(),
    )
    .await?
    .ok_or(SubscriptionEnrollmentApplicationError::InvalidState(
        INVALID_APPLICATION_STATE,
    ))?;
    let subscription = load_subscription(connection, identity.billing_scope_id(), subscription_id)
        .await?
        .ok_or(SubscriptionEnrollmentApplicationError::InvalidState(
            INVALID_APPLICATION_STATE,
        ))?;
    let event = BillingEvent::SubscriptionStarted {
        attempt_id: identity.attempt_id(),
        subscription_id,
        plan_key: reservation.plan_key().clone(),
        charge: syrup_rail::ChargeAmount::new(
            attempt.request().amount().cents(),
            attempt.request().amount().currency(),
        )
        .map_err(|_| {
            SubscriptionEnrollmentApplicationError::InvalidState(INVALID_APPLICATION_STATE)
        })?,
        period,
    };
    Ok((
        SubscriptionEnrollmentPaymentResult::new(attempt, Some(subscription)),
        Some(event),
    ))
}

pub(crate) async fn resolve_non_approved_outcome(
    pool: &PgPool,
    reservation: &SubscriptionEnrollmentReservation,
    evidence: &ProcessorEvidence,
    status: PaymentAttemptStatus,
    resolution_code: Option<PaymentResolutionCode>,
    cooldown: Option<RateLimitCooldown>,
    boundary: OutcomeResolutionBoundary,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    let mut transaction = pool.begin().await?;
    set_application_timeouts(&mut transaction).await?;
    lock_subscription_aggregate(
        &mut transaction,
        reservation.identity().subscriber_id(),
        reservation.plan_key(),
    )
    .await?;
    let attempt = lock_expected_attempt(&mut transaction, reservation).await?;
    let may_resolve = attempt.status().is_resolvable()
        && match boundary {
            OutcomeResolutionBoundary::Prepared => {
                attempt.state().timestamps().submitted_at().is_none()
            }
            OutcomeResolutionBoundary::AdmittedNotSubmitted => {
                attempt.state().timestamps().submitted_at().is_some()
            }
            OutcomeResolutionBoundary::Submitted => true,
        };
    if may_resolve {
        update_attempt_resolution(
            &mut transaction,
            &attempt,
            evidence,
            status,
            resolution_code,
            None,
            None,
            false,
        )
        .await?;
        if boundary == OutcomeResolutionBoundary::AdmittedNotSubmitted {
            sqlx::query(
                "UPDATE billing_payment_attempts SET submitted_at = NULL, updated_at = clock_timestamp() WHERE id = $1",
            )
            .bind(attempt.identity().attempt_id().as_uuid())
            .execute(&mut *transaction)
            .await?;
        }
    }
    if let Some(cooldown) = cooldown {
        extend_rate_limit_cooldown(&mut transaction, reservation, cooldown).await?;
    }
    let attempt = find_payment_attempt_by_id_on_connection(
        &mut transaction,
        reservation.identity().billing_scope_id(),
        reservation.identity().attempt_id(),
    )
    .await?
    .ok_or(SubscriptionEnrollmentApplicationError::InvalidState(
        INVALID_APPLICATION_STATE,
    ))?;
    let result = payment_result_for_attempt(&mut transaction, attempt).await?;
    transaction.commit().await?;
    Ok(result)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OutcomeResolutionBoundary {
    Prepared,
    AdmittedNotSubmitted,
    Submitted,
}

fn mutation_error_evidence(detail: &GatewayDiagnostic) -> ProcessorEvidence {
    ProcessorEvidence::new(
        None,
        None,
        None,
        None,
        Some(detail.clone()),
        None,
        syrup_rail::GatewayPaymentDescriptor::default(),
    )
}

pub(crate) const fn not_submitted_resolution_code(
    error: &GatewayNotSubmittedError,
) -> PaymentResolutionCode {
    match error {
        GatewayNotSubmittedError::RequestRejected(_) => {
            PaymentResolutionCode::GatewayRequestRejectedBeforeSubmission
        }
        GatewayNotSubmittedError::Malformed(_) => {
            PaymentResolutionCode::GatewayMalformedBeforeSubmission
        }
        GatewayNotSubmittedError::Configuration(_) => {
            PaymentResolutionCode::GatewayConfigurationBeforeSubmission
        }
        GatewayNotSubmittedError::Unavailable(_) => {
            PaymentResolutionCode::GatewayUnavailableBeforeSubmission
        }
        GatewayNotSubmittedError::RateLimited(_) => {
            PaymentResolutionCode::GatewayProviderRateLimitedBeforeSubmission
        }
    }
}

async fn resolve_unknown_outcome(
    pool: &PgPool,
    reservation: &SubscriptionEnrollmentReservation,
    evidence: &ProcessorEvidence,
    cooldown: Option<RateLimitCooldown>,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    let mut transaction = pool.begin().await?;
    set_application_timeouts(&mut transaction).await?;
    lock_subscription_aggregate(
        &mut transaction,
        reservation.identity().subscriber_id(),
        reservation.plan_key(),
    )
    .await?;
    let attempt = lock_expected_attempt(&mut transaction, reservation).await?;
    if attempt.status().is_resolvable() {
        update_attempt_resolution(
            &mut transaction,
            &attempt,
            evidence,
            PaymentAttemptStatus::Unknown,
            None,
            None,
            None,
            false,
        )
        .await?;
        if evidence_looks_approved(evidence) {
            observe_processor_charge(
                &mut transaction,
                &attempt,
                evidence,
                ChargeProgression::Pending,
            )
            .await?;
        }
    }
    if let Some(cooldown) = cooldown {
        extend_rate_limit_cooldown(&mut transaction, reservation, cooldown).await?;
    }
    let attempt = find_payment_attempt_by_id_on_connection(
        &mut transaction,
        reservation.identity().billing_scope_id(),
        reservation.identity().attempt_id(),
    )
    .await?
    .ok_or(SubscriptionEnrollmentApplicationError::InvalidState(
        INVALID_APPLICATION_STATE,
    ))?;
    let result = payment_result_for_attempt(&mut transaction, attempt).await?;
    transaction.commit().await?;
    Ok(result)
}

#[derive(Clone, Copy)]
pub(crate) enum RateLimitCooldown {
    Account,
    Provider,
}

async fn extend_rate_limit_cooldown(
    connection: &mut PgConnection,
    reservation: &SubscriptionEnrollmentReservation,
    cooldown: RateLimitCooldown,
) -> Result<(), SubscriptionEnrollmentApplicationError> {
    let result = match cooldown {
        RateLimitCooldown::Account => {
            sqlx::query(
                r#"
                UPDATE billing_gateway_accounts
                SET mutation_rate_limited_until = GREATEST(
                        COALESCE(mutation_rate_limited_until, '-infinity'::timestamptz),
                        clock_timestamp() + make_interval(secs => $4)
                    ),
                    updated_at = clock_timestamp()
                WHERE id = $1
                    AND billing_scope_id = $2
                    AND gateway_configuration_id = $3
                "#,
            )
            .bind(reservation.identity().gateway_account_id().as_uuid())
            .bind(reservation.identity().billing_scope_id().as_uuid())
            .bind(reservation.identity().gateway_configuration_id().as_uuid())
            .bind(PROVIDER_RATE_LIMIT_RETRY_AFTER_SECONDS)
            .execute(&mut *connection)
            .await?
        }
        RateLimitCooldown::Provider => {
            sqlx::query(
                r#"
                UPDATE billing_gateway_provider_rate_limits
                SET rate_limited_until = GREATEST(
                    rate_limited_until,
                    clock_timestamp() + make_interval(secs => $2)
                )
                WHERE provider_key = $1
                "#,
            )
            .bind(reservation.provider_key().as_str())
            .bind(PROVIDER_RATE_LIMIT_RETRY_AFTER_SECONDS)
            .execute(&mut *connection)
            .await?
        }
    };
    if result.rows_affected() != 1 {
        return Err(SubscriptionEnrollmentApplicationError::InvalidState(
            INVALID_APPLICATION_STATE,
        ));
    }
    Ok(())
}

async fn park_approved_outcome(
    pool: &PgPool,
    reservation: &SubscriptionEnrollmentReservation,
    evidence: &ProcessorEvidence,
    message: &'static str,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    match try_park_approved_outcome(pool, reservation, evidence, message).await {
        Ok(result) => Ok(result),
        Err(_) => {
            observe_approved_evidence_with_retry(pool, reservation, evidence).await?;
            let mut transaction = pool.begin().await?;
            let attempt = find_payment_attempt_by_id_on_connection(
                &mut transaction,
                reservation.identity().billing_scope_id(),
                reservation.identity().attempt_id(),
            )
            .await?
            .ok_or(SubscriptionEnrollmentApplicationError::InvalidState(
                INVALID_APPLICATION_STATE,
            ))?;
            let result = if attempt.status() == PaymentAttemptStatus::Approved {
                payment_result_for_attempt(&mut transaction, attempt).await?
            } else {
                SubscriptionEnrollmentPaymentResult::confirmation_pending(
                    attempt,
                    None,
                    evidence.clone(),
                )
            };
            transaction.commit().await?;
            Ok(result)
        }
    }
}

async fn try_park_approved_outcome(
    pool: &PgPool,
    reservation: &SubscriptionEnrollmentReservation,
    evidence: &ProcessorEvidence,
    message: &'static str,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    let mut transaction = pool.begin().await?;
    set_application_timeouts(&mut transaction).await?;
    lock_subscription_aggregate(
        &mut transaction,
        reservation.identity().subscriber_id(),
        reservation.plan_key(),
    )
    .await?;
    let attempt = lock_expected_attempt(&mut transaction, reservation).await?;
    let attempt = if attempt.status() == PaymentAttemptStatus::Approved {
        observe_processor_charge(
            &mut transaction,
            &attempt,
            evidence,
            ChargeProgression::Applied,
        )
        .await?;
        attempt
    } else if attempt.status().is_terminal() {
        let progression =
            if evidence.transaction_id().is_some() && attempt.request().amount().cents() > 0 {
                ChargeProgression::ExternalReversalRequired
            } else {
                ChargeProgression::ReconciliationRequired
            };
        observe_processor_charge(&mut transaction, &attempt, evidence, progression).await?;
        attempt
    } else {
        observe_processor_charge(
            &mut transaction,
            &attempt,
            evidence,
            ChargeProgression::Pending,
        )
        .await?;
        park_locked_attempt(&mut transaction, &attempt, evidence, None, message).await?
    };
    let result = payment_result_for_attempt(&mut transaction, attempt).await?;
    transaction.commit().await?;
    Ok(result)
}

async fn observe_approved_evidence_with_retry(
    pool: &PgPool,
    reservation: &SubscriptionEnrollmentReservation,
    evidence: &ProcessorEvidence,
) -> Result<(), SubscriptionEnrollmentApplicationError> {
    let mut last_error = None;
    for attempt_index in 0..APPROVED_EVIDENCE_WRITE_ATTEMPTS {
        let result = async {
            let mut transaction = pool.begin().await?;
            set_application_timeouts(&mut transaction).await?;
            let attempt = lock_expected_attempt(&mut transaction, reservation).await?;
            observe_processor_charge(
                &mut transaction,
                &attempt,
                evidence,
                ChargeProgression::Pending,
            )
            .await?;
            transaction.commit().await?;
            Ok::<(), SubscriptionEnrollmentApplicationError>(())
        }
        .await;
        match result {
            Ok(()) => return Ok(()),
            Err(error) if is_retryable_evidence_error(&error) => {
                last_error = Some(error);
                if attempt_index + 1 < APPROVED_EVIDENCE_WRITE_ATTEMPTS {
                    tokio::time::sleep(APPROVED_EVIDENCE_RETRY_DELAY).await;
                } else {
                    break;
                }
            }
            Err(error) => return Err(error),
        }
    }
    let _ = last_error;
    observe_approved_evidence_without_attempt_lock(pool, reservation, evidence).await
}

/// Last-resort immutable evidence write used when another transaction keeps
/// the attempt row locked beyond the bounded application window.
///
/// The reservation already carries every frozen charge dimension. Database
/// foreign keys and uniqueness constraints remain the authority, so this path
/// can retain processor evidence without mutating or locking the attempt.
async fn observe_approved_evidence_without_attempt_lock(
    pool: &PgPool,
    reservation: &SubscriptionEnrollmentReservation,
    evidence: &ProcessorEvidence,
) -> Result<(), SubscriptionEnrollmentApplicationError> {
    let identity = reservation.identity();
    let descriptor = evidence.descriptor();
    let transaction_id = evidence.transaction_id().map(GatewayTransactionId::expose);
    let mut transaction = pool.begin().await?;
    set_application_timeouts(&mut transaction).await?;

    for _ in 0..2 {
        let has_existing_charge: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM billing_processor_charges WHERE attempt_id = $1)",
        )
        .bind(identity.attempt_id().as_uuid())
        .fetch_one(&mut *transaction)
        .await?;
        let role = if has_existing_charge {
            "additional"
        } else {
            "primary"
        };
        let inserted = sqlx::query_scalar::<_, Uuid>(
            r#"
            INSERT INTO billing_processor_charges (
                id, attempt_id, billing_scope_id, gateway_account_id, gateway_order_id,
                gateway_transaction_id, gateway_payment_method_reference,
                gateway_response, gateway_response_code, gateway_response_text,
                gateway_condition, payment_type, card_brand, card_last4,
                card_exp_month, card_exp_year, charge_role, progression_state,
                attempt_kind, plan_key, host_charge_target_id, amount_cents, currency
            ) VALUES (
                $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
                $14, $15, $16, $17, 'pending', 'subscription_initial', $18,
                NULL, $19, $20
            )
            ON CONFLICT DO NOTHING
            RETURNING id
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(identity.attempt_id().as_uuid())
        .bind(identity.billing_scope_id().as_uuid())
        .bind(identity.gateway_account_id().as_uuid())
        .bind(reservation.gateway_order_id().expose())
        .bind(transaction_id)
        .bind(
            evidence
                .payment_method_reference()
                .map(|value| value.expose()),
        )
        .bind(evidence.response().map(GatewayDiagnostic::expose))
        .bind(evidence.response_code().map(GatewayDiagnostic::expose))
        .bind(evidence.response_text().map(GatewayDiagnostic::expose))
        .bind(evidence.condition().map(GatewayDiagnostic::expose))
        .bind(descriptor.payment_type().map(GatewayDiagnostic::expose))
        .bind(descriptor.card_brand().map(GatewayDiagnostic::expose))
        .bind(descriptor.card_last_four().map(|value| value.expose()))
        .bind(descriptor.card_exp_month())
        .bind(descriptor.card_exp_year())
        .bind(role)
        .bind(reservation.plan_key().as_str())
        .bind(reservation.expected_charge().charge().cents())
        .bind(reservation.expected_charge().charge().currency().as_str())
        .fetch_optional(&mut *transaction)
        .await?;
        if inserted.is_some() {
            transaction.commit().await?;
            return Ok(());
        }
    }

    let evidence_matches = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT gateway_payment_method_reference IS NOT DISTINCT FROM $3
            AND gateway_response IS NOT DISTINCT FROM $4
            AND gateway_response_code IS NOT DISTINCT FROM $5
            AND gateway_response_text IS NOT DISTINCT FROM $6
            AND gateway_condition IS NOT DISTINCT FROM $7
            AND payment_type IS NOT DISTINCT FROM $8
            AND card_brand IS NOT DISTINCT FROM $9
            AND card_last4 IS NOT DISTINCT FROM $10
            AND card_exp_month IS NOT DISTINCT FROM $11
            AND card_exp_year IS NOT DISTINCT FROM $12
        FROM billing_processor_charges
        WHERE attempt_id = $1
            AND gateway_transaction_id IS NOT DISTINCT FROM $2
        "#,
    )
    .bind(identity.attempt_id().as_uuid())
    .bind(transaction_id)
    .bind(
        evidence
            .payment_method_reference()
            .map(|value| value.expose()),
    )
    .bind(evidence.response().map(GatewayDiagnostic::expose))
    .bind(evidence.response_code().map(GatewayDiagnostic::expose))
    .bind(evidence.response_text().map(GatewayDiagnostic::expose))
    .bind(evidence.condition().map(GatewayDiagnostic::expose))
    .bind(descriptor.payment_type().map(GatewayDiagnostic::expose))
    .bind(descriptor.card_brand().map(GatewayDiagnostic::expose))
    .bind(descriptor.card_last_four().map(|value| value.expose()))
    .bind(descriptor.card_exp_month())
    .bind(descriptor.card_exp_year())
    .fetch_optional(&mut *transaction)
    .await?;
    if evidence_matches == Some(true) {
        transaction.commit().await?;
        return Ok(());
    }
    if let Some(transaction_id) = transaction_id {
        let owned_elsewhere: bool = sqlx::query_scalar(
            r#"
            SELECT EXISTS (
                SELECT 1 FROM billing_processor_charges
                WHERE gateway_account_id = $1 AND gateway_transaction_id = $2
                    AND attempt_id <> $3
            )
            "#,
        )
        .bind(identity.gateway_account_id().as_uuid())
        .bind(transaction_id)
        .bind(identity.attempt_id().as_uuid())
        .fetch_one(&mut *transaction)
        .await?;
        if owned_elsewhere {
            transaction.commit().await?;
            return Ok(());
        }
    }
    Err(SubscriptionEnrollmentApplicationError::ApprovedEvidenceNotDurable)
}

fn is_retryable_evidence_error(error: &SubscriptionEnrollmentApplicationError) -> bool {
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

async fn lock_expected_attempt(
    connection: &mut PgConnection,
    reservation: &SubscriptionEnrollmentReservation,
) -> Result<PaymentAttempt, SubscriptionEnrollmentApplicationError> {
    let identity = reservation.identity();
    let attempt = lock_payment_attempt_by_id_on_connection(
        connection,
        identity.billing_scope_id(),
        identity.attempt_id(),
    )
    .await?
    .ok_or(SubscriptionEnrollmentApplicationError::InvalidState(
        INVALID_APPLICATION_STATE,
    ))?;
    let current = attempt.identity();
    if current != identity
        || attempt.kind() != PaymentAttemptKind::SubscriptionInitial
        || attempt.request().target().plan_key() != Some(reservation.plan_key())
        || attempt.request().gateway_order_id() != reservation.gateway_order_id()
    {
        return Err(SubscriptionEnrollmentApplicationError::InvalidState(
            INVALID_APPLICATION_STATE,
        ));
    }
    Ok(attempt)
}

async fn set_application_timeouts(connection: &mut PgConnection) -> Result<(), sqlx::Error> {
    sqlx::query(
        "SELECT set_config('lock_timeout', $1, true), set_config('statement_timeout', $2, true)",
    )
    .bind(BILLING_ROW_LOCK_TIMEOUT)
    .bind(BILLING_OPERATION_TIMEOUT)
    .execute(connection)
    .await?;
    Ok(())
}

async fn lock_payment_method_domain(
    connection: &mut PgConnection,
    subscriber_id: SubscriberId,
    gateway_account_id: &Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtextextended($1::uuid::text || ':' || $2::uuid::text, 0))",
    )
    .bind(gateway_account_id)
    .bind(subscriber_id.as_uuid())
    .execute(connection)
    .await?;
    Ok(())
}

async fn lock_subscription_aggregate(
    connection: &mut PgConnection,
    subscriber_id: SubscriberId,
    plan_key: &PlanKey,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::uuid::text || ':' || $2, 0))")
        .bind(subscriber_id.as_uuid())
        .bind(plan_key.as_str())
        .execute(connection)
        .await?;
    Ok(())
}

async fn current_subscription_exists(
    connection: &mut PgConnection,
    reservation: &SubscriptionEnrollmentReservation,
) -> Result<bool, sqlx::Error> {
    let identity = reservation.identity();
    let rows = sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT id FROM billing_subscriptions
        WHERE billing_scope_id = $1 AND subscriber_id = $2 AND plan_key = $3
            AND (
                status IN ('active', 'past_due')
                OR (status = 'canceled' AND current_period_end_at > clock_timestamp())
            )
        ORDER BY updated_at DESC, id DESC
        FOR NO KEY UPDATE
        "#,
    )
    .bind(identity.billing_scope_id().as_uuid())
    .bind(identity.subscriber_id().as_uuid())
    .bind(reservation.plan_key().as_str())
    .fetch_all(connection)
    .await?;
    Ok(!rows.is_empty())
}

async fn active_grant_exists(
    connection: &mut PgConnection,
    reservation: &SubscriptionEnrollmentReservation,
) -> Result<bool, sqlx::Error> {
    let identity = reservation.identity();
    let rows = sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT id FROM billing_subscription_grants
        WHERE billing_scope_id = $1 AND subscriber_id = $2 AND plan_key = $3
            AND revoked_at IS NULL
            AND starts_at <= clock_timestamp() AND ends_at > clock_timestamp()
        ORDER BY ends_at DESC, id DESC
        FOR UPDATE
        "#,
    )
    .bind(identity.billing_scope_id().as_uuid())
    .bind(identity.subscriber_id().as_uuid())
    .bind(reservation.plan_key().as_str())
    .fetch_all(connection)
    .await?;
    Ok(!rows.is_empty())
}

async fn upsert_payment_method(
    connection: &mut PgConnection,
    attempt: &PaymentAttempt,
    evidence: &ProcessorEvidence,
) -> Result<PaymentMethodId, SubscriptionEnrollmentApplicationError> {
    let identity = attempt.identity();
    let reference = evidence.payment_method_reference().ok_or(
        SubscriptionEnrollmentApplicationError::InvalidState(INVALID_APPLICATION_STATE),
    )?;
    let descriptor = evidence.descriptor();
    let row_id: Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO billing_payment_methods (
            id, billing_scope_id, subscriber_id, gateway_account_id,
            gateway_payment_method_reference, status, payment_type, card_brand,
            card_last4, card_exp_month, card_exp_year, billing_name, billing_email
        ) VALUES ($1, $2, $3, $4, $5, 'active', $6, $7, $8, $9, $10, $11, $12)
        ON CONFLICT (gateway_account_id, subscriber_id, gateway_payment_method_reference)
        DO UPDATE SET status = 'active', payment_type = EXCLUDED.payment_type,
            card_brand = EXCLUDED.card_brand, card_last4 = EXCLUDED.card_last4,
            card_exp_month = EXCLUDED.card_exp_month,
            card_exp_year = EXCLUDED.card_exp_year,
            billing_name = EXCLUDED.billing_name,
            billing_email = EXCLUDED.billing_email,
            updated_at = clock_timestamp()
        RETURNING id
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(identity.billing_scope_id().as_uuid())
    .bind(identity.subscriber_id().as_uuid())
    .bind(identity.gateway_account_id().as_uuid())
    .bind(reference.expose())
    .bind(descriptor.payment_type().map(GatewayDiagnostic::expose))
    .bind(descriptor.card_brand().map(GatewayDiagnostic::expose))
    .bind(descriptor.card_last_four().map(|value| value.expose()))
    .bind(descriptor.card_exp_month())
    .bind(descriptor.card_exp_year())
    .bind(attempt.request().billing_contact().name())
    .bind(attempt.request().billing_contact().email())
    .fetch_one(connection)
    .await?;
    Ok(PaymentMethodId::new(row_id))
}

async fn insert_subscription(
    connection: &mut PgConnection,
    attempt: &PaymentAttempt,
    subscription_id: SubscriptionId,
    method_id: PaymentMethodId,
    recurring_amount_cents: i32,
    period: &BillingPeriod,
    transaction_id: &GatewayTransactionId,
) -> Result<(), sqlx::Error> {
    let identity = attempt.identity();
    let plan_key = attempt
        .request()
        .target()
        .plan_key()
        .expect("validated initial plan");
    sqlx::query(
        r#"
        INSERT INTO billing_subscriptions (
            id, billing_scope_id, subscriber_id, plan_key, status,
            gateway_account_id, payment_method_id, amount_cents, currency,
            current_period_start_at, current_period_end_at, next_renewal_at,
            initial_transaction_id
        ) VALUES ($1, $2, $3, $4, 'active', $5, $6, $7, $8, $9, $10, $10, $11)
        "#,
    )
    .bind(subscription_id.as_uuid())
    .bind(identity.billing_scope_id().as_uuid())
    .bind(identity.subscriber_id().as_uuid())
    .bind(plan_key.as_str())
    .bind(identity.gateway_account_id().as_uuid())
    .bind(method_id.as_uuid())
    .bind(recurring_amount_cents)
    .bind(attempt.request().amount().currency().as_str())
    .bind(period.start_at())
    .bind(period.end_at())
    .bind(transaction_id.expose())
    .execute(connection)
    .await?;
    Ok(())
}

fn recurring_amount_after_initial(
    attempt: &PaymentAttempt,
) -> Result<i32, SubscriptionEnrollmentApplicationError> {
    let Some(discount) = attempt.request().target().enrollment_discount() else {
        return Ok(attempt.request().amount().cents());
    };
    let snapshot = discount.snapshot();
    Ok(match snapshot.duration() {
        SubscriptionDiscountDuration::Indefinite => snapshot.discounted_charge().cents(),
        SubscriptionDiscountDuration::LimitedMonths(months) if months.get() == 1 => {
            snapshot.base_charge().cents()
        }
        SubscriptionDiscountDuration::LimitedMonths(_) => snapshot.discounted_charge().cents(),
    })
}

async fn apply_initial_discount(
    connection: &mut PgConnection,
    attempt: &PaymentAttempt,
    subscription_id: SubscriptionId,
    applied_at: DateTime<Utc>,
) -> Result<(), SubscriptionEnrollmentApplicationError> {
    let Some(discount) = attempt.request().target().enrollment_discount() else {
        return Ok(());
    };
    let identity = attempt.identity();
    let snapshot = discount.snapshot();
    let (amount_off_cents, percent_off_bps) = match snapshot.kind() {
        SubscriptionDiscountKind::AmountOffCents(value) => (Some(value.get()), None),
        SubscriptionDiscountKind::PercentOffBasisPoints(value) => {
            (None, Some(i32::from(value.get())))
        }
    };
    let (duration_months, periods_total, status, completed_at) = match snapshot.duration() {
        SubscriptionDiscountDuration::Indefinite => (None, None, "active", None),
        SubscriptionDiscountDuration::LimitedMonths(months) => {
            let months = i32::from(months.get());
            if months == 1 {
                (Some(months), Some(months), "completed", Some(applied_at))
            } else {
                (Some(months), Some(months), "active", None)
            }
        }
    };
    sqlx::query(
        r#"
        INSERT INTO billing_subscription_discounts (
            subscription_id, billing_scope_id, subscriber_id, plan_key,
            discount_claim_id, discount_code_id, code_snapshot, label_snapshot,
            discount_kind, amount_off_cents, percent_off_bps, currency, duration,
            duration_months, base_amount_cents, discounted_amount_cents,
            periods_total, periods_applied, status, applied_at, completed_at
        ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
            $14, $15, $16, $17, 1, $18, $19, $20
        )
        "#,
    )
    .bind(subscription_id.as_uuid())
    .bind(identity.billing_scope_id().as_uuid())
    .bind(identity.subscriber_id().as_uuid())
    .bind(
        attempt
            .request()
            .target()
            .plan_key()
            .expect("validated plan")
            .as_str(),
    )
    .bind(discount.claim_id().as_uuid())
    .bind(discount.code_id().as_uuid())
    .bind(snapshot.code().as_str())
    .bind(snapshot.label())
    .bind(snapshot.kind().as_str())
    .bind(amount_off_cents)
    .bind(percent_off_bps)
    .bind(snapshot.currency().as_str())
    .bind(snapshot.duration().as_str())
    .bind(duration_months)
    .bind(snapshot.base_charge().cents())
    .bind(snapshot.discounted_charge().cents())
    .bind(periods_total)
    .bind(status)
    .bind(applied_at)
    .bind(completed_at)
    .execute(&mut *connection)
    .await?;

    let updated = sqlx::query(
        r#"
        UPDATE billing_subscription_discount_claims
        SET status = 'applied', applied_subscription_id = $5,
            applied_payment_attempt_id = $6, applied_at = $7
        WHERE id = $1 AND billing_scope_id = $2 AND subscriber_id = $3
            AND plan_key = $4 AND status IN ('saved', 'expired')
            AND applied_subscription_id IS NULL
            AND applied_payment_attempt_id IS NULL
        "#,
    )
    .bind(discount.claim_id().as_uuid())
    .bind(identity.billing_scope_id().as_uuid())
    .bind(identity.subscriber_id().as_uuid())
    .bind(
        attempt
            .request()
            .target()
            .plan_key()
            .expect("validated plan")
            .as_str(),
    )
    .bind(subscription_id.as_uuid())
    .bind(identity.attempt_id().as_uuid())
    .bind(applied_at)
    .execute(connection)
    .await?;
    if updated.rows_affected() != 1 {
        return Err(SubscriptionEnrollmentApplicationError::InvalidState(
            INVALID_APPLICATION_STATE,
        ));
    }
    Ok(())
}

async fn mark_attempt_approved(
    connection: &mut PgConnection,
    attempt: &PaymentAttempt,
    evidence: &ProcessorEvidence,
    subscription_id: SubscriptionId,
    method_id: PaymentMethodId,
) -> Result<(), SubscriptionEnrollmentApplicationError> {
    update_attempt_resolution(
        connection,
        attempt,
        evidence,
        PaymentAttemptStatus::Approved,
        None,
        Some(subscription_id),
        Some(method_id),
        false,
    )
    .await
}

async fn park_locked_attempt(
    connection: &mut PgConnection,
    attempt: &PaymentAttempt,
    evidence: &ProcessorEvidence,
    resolution_code: Option<PaymentResolutionCode>,
    message: &'static str,
) -> Result<PaymentAttempt, SubscriptionEnrollmentApplicationError> {
    update_attempt_resolution(
        connection,
        attempt,
        evidence,
        PaymentAttemptStatus::ReviewRequired,
        resolution_code,
        None,
        None,
        true,
    )
    .await?;
    sqlx::query(
        r#"
        UPDATE billing_payment_attempts
        SET gateway_response_text = CASE
                WHEN gateway_response_text IS NULL THEN $2
                ELSE left($2 || ' ' || gateway_response_text, 512)
            END,
            updated_at = clock_timestamp()
        WHERE id = $1
        "#,
    )
    .bind(attempt.identity().attempt_id().as_uuid())
    .bind(message)
    .execute(&mut *connection)
    .await?;
    find_payment_attempt_by_id_on_connection(
        connection,
        attempt.identity().billing_scope_id(),
        attempt.identity().attempt_id(),
    )
    .await?
    .ok_or(SubscriptionEnrollmentApplicationError::InvalidState(
        INVALID_APPLICATION_STATE,
    ))
}

#[allow(clippy::too_many_arguments)]
async fn update_attempt_resolution(
    connection: &mut PgConnection,
    attempt: &PaymentAttempt,
    evidence: &ProcessorEvidence,
    status: PaymentAttemptStatus,
    resolution_code: Option<PaymentResolutionCode>,
    subscription_id: Option<SubscriptionId>,
    payment_method_id: Option<PaymentMethodId>,
    allow_terminal_approval_race: bool,
) -> Result<(), SubscriptionEnrollmentApplicationError> {
    let descriptor = evidence.descriptor();
    let result = sqlx::query(
        r#"
        UPDATE billing_payment_attempts
        SET status = $2, subscription_id = COALESCE($3, subscription_id),
            payment_method_id = COALESCE($4, payment_method_id),
            gateway_transaction_id = $5,
            gateway_payment_method_reference = $6,
            gateway_response = $7, gateway_response_code = $8,
            gateway_response_text = $9, gateway_condition = $10,
            payment_type = $11, card_brand = $12, card_last4 = $13,
            card_exp_month = $14, card_exp_year = $15,
            resolution_code = $16,
            resolved_at = CASE WHEN $2 IN ('approved', 'declined', 'failed')
                THEN clock_timestamp() ELSE resolved_at END,
            review_required_at = CASE WHEN $2 = 'review_required'
                THEN COALESCE(review_required_at, clock_timestamp()) ELSE review_required_at END,
            updated_at = clock_timestamp()
        WHERE id = $1
            AND (
                status IN ('pending', 'unknown', 'review_required')
                OR ($17 AND status IN ('declined', 'failed'))
            )
        "#,
    )
    .bind(attempt.identity().attempt_id().as_uuid())
    .bind(status.as_str())
    .bind(subscription_id.map(SubscriptionId::into_uuid))
    .bind(payment_method_id.map(PaymentMethodId::into_uuid))
    .bind(evidence.transaction_id().map(GatewayTransactionId::expose))
    .bind(
        evidence
            .payment_method_reference()
            .map(|value| value.expose()),
    )
    .bind(evidence.response().map(GatewayDiagnostic::expose))
    .bind(evidence.response_code().map(GatewayDiagnostic::expose))
    .bind(evidence.response_text().map(GatewayDiagnostic::expose))
    .bind(evidence.condition().map(GatewayDiagnostic::expose))
    .bind(descriptor.payment_type().map(GatewayDiagnostic::expose))
    .bind(descriptor.card_brand().map(GatewayDiagnostic::expose))
    .bind(descriptor.card_last_four().map(|value| value.expose()))
    .bind(descriptor.card_exp_month())
    .bind(descriptor.card_exp_year())
    .bind(resolution_code.map(PaymentResolutionCode::as_str))
    .bind(allow_terminal_approval_race)
    .execute(connection)
    .await?;
    if result.rows_affected() != 1 {
        return Err(SubscriptionEnrollmentApplicationError::InvalidState(
            INVALID_APPLICATION_STATE,
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ChargeRole {
    Primary,
    Additional,
}

impl ChargeRole {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Additional => "additional",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ChargeProgression {
    Pending,
    ReconciliationRequired,
    ExternalReversalRequired,
    Applied,
}

impl ChargeProgression {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::ReconciliationRequired => "reconciliation_required",
            Self::ExternalReversalRequired => "external_reversal_required",
            Self::Applied => "applied",
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct ChargeRecord {
    id: Uuid,
    role: ChargeRole,
}

#[derive(Clone, Copy, Debug)]
enum ObservedCharge {
    Owned(ChargeRecord),
    OwnedByOtherAttempt,
}

async fn observe_processor_charge(
    connection: &mut PgConnection,
    attempt: &PaymentAttempt,
    evidence: &ProcessorEvidence,
    initial_progression: ChargeProgression,
) -> Result<ObservedCharge, SubscriptionEnrollmentApplicationError> {
    let identity = attempt.identity();
    let transaction_id = evidence.transaction_id().map(GatewayTransactionId::expose);
    let has_existing_charge: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM billing_processor_charges WHERE attempt_id = $1)",
    )
    .bind(identity.attempt_id().as_uuid())
    .fetch_one(&mut *connection)
    .await?;
    if let Some(transaction_id) = transaction_id {
        if processor_charge_owned_by_other_attempt(connection, attempt, transaction_id).await? {
            return Ok(ObservedCharge::OwnedByOtherAttempt);
        }
        if let Some(charge) =
            identify_transactionless_charge(connection, attempt, evidence, transaction_id).await?
        {
            return Ok(ObservedCharge::Owned(charge));
        }
    }
    let role = if has_existing_charge {
        ChargeRole::Additional
    } else {
        ChargeRole::Primary
    };
    let descriptor = evidence.descriptor();
    let inserted = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO billing_processor_charges (
            id, attempt_id, billing_scope_id, gateway_account_id, gateway_order_id,
            gateway_transaction_id, gateway_payment_method_reference,
            gateway_response, gateway_response_code, gateway_response_text,
            gateway_condition, payment_type, card_brand, card_last4,
            card_exp_month, card_exp_year, charge_role, progression_state,
            reconciliation_required_at, external_reversal_required_at, applied_at,
            attempt_kind, plan_key, host_charge_target_id, amount_cents, currency
        ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
            $14, $15, $16, $17, $18,
            CASE WHEN $18 = 'reconciliation_required' THEN clock_timestamp() END,
            CASE WHEN $18 = 'external_reversal_required' THEN clock_timestamp() END,
            CASE WHEN $18 = 'applied' THEN clock_timestamp() END,
            $19, $20, NULL, $21, $22
        )
        ON CONFLICT DO NOTHING
        RETURNING id
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(identity.attempt_id().as_uuid())
    .bind(identity.billing_scope_id().as_uuid())
    .bind(identity.gateway_account_id().as_uuid())
    .bind(attempt.request().gateway_order_id().expose())
    .bind(transaction_id)
    .bind(
        evidence
            .payment_method_reference()
            .map(|value| value.expose()),
    )
    .bind(evidence.response().map(GatewayDiagnostic::expose))
    .bind(evidence.response_code().map(GatewayDiagnostic::expose))
    .bind(evidence.response_text().map(GatewayDiagnostic::expose))
    .bind(evidence.condition().map(GatewayDiagnostic::expose))
    .bind(descriptor.payment_type().map(GatewayDiagnostic::expose))
    .bind(descriptor.card_brand().map(GatewayDiagnostic::expose))
    .bind(descriptor.card_last_four().map(|value| value.expose()))
    .bind(descriptor.card_exp_month())
    .bind(descriptor.card_exp_year())
    .bind(role.as_str())
    .bind(initial_progression.as_str())
    .bind(attempt.kind().as_str())
    .bind(attempt.request().target().plan_key().map(PlanKey::as_str))
    .bind(attempt.request().amount().cents())
    .bind(attempt.request().amount().currency().as_str())
    .fetch_optional(&mut *connection)
    .await?;
    if let Some(id) = inserted {
        return Ok(ObservedCharge::Owned(ChargeRecord { id, role }));
    }

    let row = sqlx::query(
        r#"
        SELECT id, charge_role,
            gateway_payment_method_reference IS NOT DISTINCT FROM $3
                AND gateway_response IS NOT DISTINCT FROM $4
                AND gateway_response_code IS NOT DISTINCT FROM $5
                AND gateway_response_text IS NOT DISTINCT FROM $6
                AND gateway_condition IS NOT DISTINCT FROM $7
                AND payment_type IS NOT DISTINCT FROM $8
                AND card_brand IS NOT DISTINCT FROM $9
                AND card_last4 IS NOT DISTINCT FROM $10
                AND card_exp_month IS NOT DISTINCT FROM $11
                AND card_exp_year IS NOT DISTINCT FROM $12 AS evidence_matches
        FROM billing_processor_charges
        WHERE attempt_id = $1
            AND gateway_transaction_id IS NOT DISTINCT FROM $2
        FOR UPDATE
        "#,
    )
    .bind(identity.attempt_id().as_uuid())
    .bind(transaction_id)
    .bind(
        evidence
            .payment_method_reference()
            .map(|value| value.expose()),
    )
    .bind(evidence.response().map(GatewayDiagnostic::expose))
    .bind(evidence.response_code().map(GatewayDiagnostic::expose))
    .bind(evidence.response_text().map(GatewayDiagnostic::expose))
    .bind(evidence.condition().map(GatewayDiagnostic::expose))
    .bind(descriptor.payment_type().map(GatewayDiagnostic::expose))
    .bind(descriptor.card_brand().map(GatewayDiagnostic::expose))
    .bind(descriptor.card_last_four().map(|value| value.expose()))
    .bind(descriptor.card_exp_month())
    .bind(descriptor.card_exp_year())
    .fetch_optional(&mut *connection)
    .await?;
    if let Some(row) = row {
        if !row.try_get::<bool, _>("evidence_matches")? {
            return Err(SubscriptionEnrollmentApplicationError::InvalidState(
                "processor charge replay evidence changed",
            ));
        }
        let role = match row.try_get::<String, _>("charge_role")?.as_str() {
            "primary" => ChargeRole::Primary,
            "additional" => ChargeRole::Additional,
            _ => {
                return Err(SubscriptionEnrollmentApplicationError::InvalidState(
                    INVALID_APPLICATION_STATE,
                ));
            }
        };
        return Ok(ObservedCharge::Owned(ChargeRecord {
            id: row.try_get("id")?,
            role,
        }));
    }
    if let Some(transaction_id) = transaction_id
        && processor_charge_owned_by_other_attempt(connection, attempt, transaction_id).await?
    {
        return Ok(ObservedCharge::OwnedByOtherAttempt);
    }
    Err(SubscriptionEnrollmentApplicationError::InvalidState(
        INVALID_APPLICATION_STATE,
    ))
}

async fn processor_charge_owned_by_other_attempt(
    connection: &mut PgConnection,
    attempt: &PaymentAttempt,
    transaction_id: &str,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        r#"
        SELECT EXISTS (
            SELECT 1 FROM billing_processor_charges
            WHERE gateway_account_id = $1 AND gateway_transaction_id = $2
                AND attempt_id <> $3
        )
        "#,
    )
    .bind(attempt.identity().gateway_account_id().as_uuid())
    .bind(transaction_id)
    .bind(attempt.identity().attempt_id().as_uuid())
    .fetch_one(connection)
    .await
}

async fn identify_transactionless_charge(
    connection: &mut PgConnection,
    attempt: &PaymentAttempt,
    evidence: &ProcessorEvidence,
    transaction_id: &str,
) -> Result<Option<ChargeRecord>, SubscriptionEnrollmentApplicationError> {
    let descriptor = evidence.descriptor();
    let row = sqlx::query(
        r#"
        SELECT id, charge_role,
            gateway_payment_method_reference IS NOT DISTINCT FROM $2
                AND gateway_response IS NOT DISTINCT FROM $3
                AND gateway_response_code IS NOT DISTINCT FROM $4
                AND gateway_response_text IS NOT DISTINCT FROM $5
                AND gateway_condition IS NOT DISTINCT FROM $6
                AND payment_type IS NOT DISTINCT FROM $7
                AND card_brand IS NOT DISTINCT FROM $8
                AND card_last4 IS NOT DISTINCT FROM $9
                AND card_exp_month IS NOT DISTINCT FROM $10
                AND card_exp_year IS NOT DISTINCT FROM $11 AS evidence_matches
        FROM billing_processor_charges
        WHERE attempt_id = $1
            AND billing_canonical_gateway_transaction_id(gateway_transaction_id) IS NULL
        FOR UPDATE
        "#,
    )
    .bind(attempt.identity().attempt_id().as_uuid())
    .bind(
        evidence
            .payment_method_reference()
            .map(|value| value.expose()),
    )
    .bind(evidence.response().map(GatewayDiagnostic::expose))
    .bind(evidence.response_code().map(GatewayDiagnostic::expose))
    .bind(evidence.response_text().map(GatewayDiagnostic::expose))
    .bind(evidence.condition().map(GatewayDiagnostic::expose))
    .bind(descriptor.payment_type().map(GatewayDiagnostic::expose))
    .bind(descriptor.card_brand().map(GatewayDiagnostic::expose))
    .bind(descriptor.card_last_four().map(|value| value.expose()))
    .bind(descriptor.card_exp_month())
    .bind(descriptor.card_exp_year())
    .fetch_optional(&mut *connection)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    if !row.try_get::<bool, _>("evidence_matches")? {
        return Err(SubscriptionEnrollmentApplicationError::InvalidState(
            "processor charge identification changed immutable evidence",
        ));
    }
    let charge_id: Uuid = row.try_get("id")?;
    sqlx::query(
        "UPDATE billing_processor_charges SET gateway_transaction_id = $2, updated_at = clock_timestamp() WHERE id = $1",
    )
    .bind(charge_id)
    .bind(transaction_id)
    .execute(connection)
    .await?;
    let role = match row.try_get::<String, _>("charge_role")?.as_str() {
        "primary" => ChargeRole::Primary,
        "additional" => ChargeRole::Additional,
        _ => {
            return Err(SubscriptionEnrollmentApplicationError::InvalidState(
                INVALID_APPLICATION_STATE,
            ));
        }
    };
    Ok(Some(ChargeRecord {
        id: charge_id,
        role,
    }))
}

async fn transition_charge(
    connection: &mut PgConnection,
    charge_id: Uuid,
    progression: ChargeProgression,
    resolution_code: Option<PaymentResolutionCode>,
) -> Result<(), SubscriptionEnrollmentApplicationError> {
    let result = sqlx::query(
        r#"
        UPDATE billing_processor_charges
        SET progression_state = $2, state_code = COALESCE(state_code, $3),
            reconciliation_required_at = CASE WHEN $2 = 'reconciliation_required'
                THEN COALESCE(reconciliation_required_at, clock_timestamp())
                ELSE reconciliation_required_at END,
            external_reversal_required_at = CASE WHEN $2 = 'external_reversal_required'
                THEN COALESCE(external_reversal_required_at, clock_timestamp())
                ELSE external_reversal_required_at END,
            applied_at = CASE WHEN $2 = 'applied'
                THEN COALESCE(applied_at, clock_timestamp()) ELSE applied_at END,
            updated_at = clock_timestamp()
        WHERE id = $1
            AND progression_state IN ('pending', 'reconciliation_required',
                'external_reversal_required', 'applied')
        "#,
    )
    .bind(charge_id)
    .bind(progression.as_str())
    .bind(resolution_code.map(PaymentResolutionCode::as_str))
    .execute(connection)
    .await?;
    if result.rows_affected() != 1 {
        return Err(SubscriptionEnrollmentApplicationError::InvalidState(
            INVALID_APPLICATION_STATE,
        ));
    }
    Ok(())
}

fn evidence_looks_approved(evidence: &ProcessorEvidence) -> bool {
    evidence.transaction_id().is_some()
        && (evidence
            .response()
            .is_some_and(|value| syrup_rail::gateway_response_is_approved(Some(value.expose())))
            || evidence
                .condition()
                .is_some_and(|value| syrup_rail::gateway_state_is_approved(value.expose())))
}

pub(crate) async fn payment_result_for_attempt(
    connection: &mut PgConnection,
    attempt: PaymentAttempt,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    let subscription = match attempt.kind() {
        PaymentAttemptKind::SubscriptionRecovery
        | PaymentAttemptKind::SubscriptionPaymentMethodUpdate => {
            load_applied_subscription(connection, &attempt).await?
        }
        _ if attempt.status() == PaymentAttemptStatus::Approved => {
            load_applied_subscription(connection, &attempt).await?
        }
        _ => None,
    };
    Ok(SubscriptionEnrollmentPaymentResult::new(
        attempt,
        subscription,
    ))
}

async fn load_applied_subscription(
    connection: &mut PgConnection,
    attempt: &PaymentAttempt,
) -> Result<Option<Subscription>, SubscriptionEnrollmentApplicationError> {
    let Some(subscription_id) = attempt.request().target().subscription_id() else {
        return Ok(None);
    };
    load_subscription(
        connection,
        attempt.identity().billing_scope_id(),
        subscription_id,
    )
    .await
}

async fn load_subscription(
    connection: &mut PgConnection,
    billing_scope_id: BillingScopeId,
    subscription_id: SubscriptionId,
) -> Result<Option<Subscription>, SubscriptionEnrollmentApplicationError> {
    let row = sqlx::query(
        r#"
        SELECT id, plan_key, status, payment_method_id, amount_cents, currency,
            current_period_start_at, current_period_end_at, next_renewal_at
        FROM billing_subscriptions
        WHERE billing_scope_id = $1 AND id = $2
        "#,
    )
    .bind(billing_scope_id.as_uuid())
    .bind(subscription_id.as_uuid())
    .fetch_optional(connection)
    .await?;
    row.map(|row| subscription_from_row(&row)).transpose()
}

fn subscription_from_row(
    row: &sqlx::postgres::PgRow,
) -> Result<Subscription, SubscriptionEnrollmentApplicationError> {
    let status = row
        .try_get::<String, _>("status")?
        .parse::<SubscriptionStatus>()
        .map_err(|_| {
            SubscriptionEnrollmentApplicationError::InvalidState(INVALID_APPLICATION_STATE)
        })?;
    let currency_value = row.try_get::<String, _>("currency")?;
    let currency = syrup_rail::CurrencyCode::new(&currency_value).map_err(|_| {
        SubscriptionEnrollmentApplicationError::InvalidState(INVALID_APPLICATION_STATE)
    })?;
    let charge =
        syrup_rail::ChargeAmount::new(row.try_get("amount_cents")?, currency).map_err(|_| {
            SubscriptionEnrollmentApplicationError::InvalidState(INVALID_APPLICATION_STATE)
        })?;
    let period = BillingPeriod::new(
        row.try_get("current_period_start_at")?,
        row.try_get("current_period_end_at")?,
    )
    .map_err(|_| SubscriptionEnrollmentApplicationError::InvalidState(INVALID_APPLICATION_STATE))?;
    Ok(Subscription::new(
        SubscriptionId::new(row.try_get("id")?),
        PlanKey::new(row.try_get::<String, _>("plan_key")?).map_err(|_| {
            SubscriptionEnrollmentApplicationError::InvalidState(INVALID_APPLICATION_STATE)
        })?,
        status,
        PaymentMethodId::new(row.try_get("payment_method_id")?),
        charge,
        period,
        row.try_get("next_renewal_at")?,
    ))
}

#[cfg(test)]
mod tests {
    use std::{
        error::Error,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use async_trait::async_trait;
    use chrono::Duration as ChronoDuration;
    use sqlx::{Postgres, Transaction};
    use syrup_rail::{
        BillingContact, BillingEventKey, ChargeAmount, CurrencyCode, EndUserMutationAdmission,
        EndUserMutationAdmissionResult, EndUserMutationCommand, EnrollSubscription,
        GatewayAccountId, GatewayAccountMode, GatewayConfigurationId, GatewayError,
        GatewayLifecycleCursorKey, GatewayLifecycleQueryPolicy, GatewayMutationError,
        GatewayMutationReferenceFactory, GatewayOrderId, GatewayPaymentDescriptor,
        GatewayPaymentMethodReference, GatewayProviderKey, GatewayQueryRequest,
        GatewayResolutionError, GatewayResolver, GatewaySaleRequest,
        GatewayStorePaymentMethodRequest, GatewayTransactionReport,
        GatewayTransactionReportRequest, IdempotencyKey, PaymentAttemptId, PaymentGateway,
        PaymentToken, PercentOffBasisPoints, ResolvedGateway, SubscriptionDiscountCode,
        SubscriptionDiscountSnapshot, SubscriptionEnrollmentExpectedCharge,
        SubscriptionEnrollmentReservationOutcome,
    };
    use tokio::sync::Mutex;

    use super::*;
    use crate::{
        BillingEventWriteError, BillingTransaction, GatewayMutationCooldownScope,
        SubscriptionBillingService, SubscriptionEnrollmentServiceError, SubscriptionOfferStore,
        reserve_subscription_enrollment_in_transaction,
        test_support::{TestDatabase, create_gateway_account},
    };

    #[derive(Debug, Error)]
    #[error("injected host transaction failure")]
    struct InjectedHostError;

    struct TestReferenceFactory;

    impl GatewayMutationReferenceFactory for TestReferenceFactory {
        fn for_attempt(
            &self,
            _kind: PaymentAttemptKind,
            attempt_id: PaymentAttemptId,
        ) -> GatewayOrderId {
            GatewayOrderId::from_generated_attempt(
                format!("test_initial_{}", attempt_id.as_uuid().simple()),
                attempt_id,
            )
            .expect("valid generated test order")
        }
    }

    struct NeverCalledGateway;

    #[async_trait]
    impl PaymentGateway for NeverCalledGateway {
        async fn account_mode(&self) -> Result<GatewayAccountMode, GatewayError> {
            panic!("application fixture construction must not call the provider")
        }

        async fn sale(
            &self,
            _request: GatewaySaleRequest,
        ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
            panic!("application fixture construction must not call the provider")
        }

        async fn store_payment_method(
            &self,
            _request: GatewayStorePaymentMethodRequest,
        ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
            panic!("application fixture construction must not call the provider")
        }

        async fn query_transaction(
            &self,
            _request: GatewayQueryRequest,
        ) -> Result<Option<GatewayPaymentOutcome>, GatewayError> {
            panic!("application fixture construction must not call the provider")
        }

        async fn query_transaction_reports(
            &self,
            _request: GatewayTransactionReportRequest,
        ) -> Result<Vec<GatewayTransactionReport>, GatewayError> {
            panic!("application fixture construction must not call the provider")
        }
    }

    struct ScriptedGateway {
        sale_calls: AtomicUsize,
        sale_result: Mutex<Option<Result<GatewayPaymentOutcome, GatewayMutationError>>>,
        store_calls: AtomicUsize,
        store_result: Mutex<Option<Result<GatewayPaymentOutcome, GatewayMutationError>>>,
    }

    struct RateLimitedReadinessGateway;

    #[async_trait]
    impl PaymentGateway for RateLimitedReadinessGateway {
        async fn account_mode(&self) -> Result<GatewayAccountMode, GatewayError> {
            Err(GatewayError::RateLimited(GatewayDiagnostic::new(
                "query throttle",
            )))
        }

        async fn sale(
            &self,
            _request: GatewaySaleRequest,
        ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
            panic!("readiness throttle must prevent provider mutation")
        }

        async fn store_payment_method(
            &self,
            _request: GatewayStorePaymentMethodRequest,
        ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
            panic!("initial enrollment must not store without a sale")
        }

        async fn query_transaction(
            &self,
            _request: GatewayQueryRequest,
        ) -> Result<Option<GatewayPaymentOutcome>, GatewayError> {
            panic!("initial enrollment readiness must not query transactions")
        }

        async fn query_transaction_reports(
            &self,
            _request: GatewayTransactionReportRequest,
        ) -> Result<Vec<GatewayTransactionReport>, GatewayError> {
            panic!("initial enrollment readiness must not query reports")
        }
    }

    struct CooldownDuringReadinessGateway {
        pool: PgPool,
        account_id: Uuid,
    }

    #[async_trait]
    impl PaymentGateway for CooldownDuringReadinessGateway {
        async fn account_mode(&self) -> Result<GatewayAccountMode, GatewayError> {
            sqlx::query(
                r#"
                UPDATE billing_gateway_accounts
                SET mutation_rate_limited_until = clock_timestamp() + interval '1 minute'
                WHERE id = $1
                "#,
            )
            .bind(self.account_id)
            .execute(&self.pool)
            .await
            .expect("test readiness cooldown write");
            Ok(GatewayAccountMode::Live)
        }

        async fn sale(
            &self,
            _request: GatewaySaleRequest,
        ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
            panic!("fresh cooldown must prevent provider mutation")
        }

        async fn store_payment_method(
            &self,
            _request: GatewayStorePaymentMethodRequest,
        ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
            panic!("initial enrollment must not store without a sale")
        }

        async fn query_transaction(
            &self,
            _request: GatewayQueryRequest,
        ) -> Result<Option<GatewayPaymentOutcome>, GatewayError> {
            panic!("initial enrollment readiness must not query transactions")
        }

        async fn query_transaction_reports(
            &self,
            _request: GatewayTransactionReportRequest,
        ) -> Result<Vec<GatewayTransactionReport>, GatewayError> {
            panic!("initial enrollment readiness must not query reports")
        }
    }

    struct PermitAdmission {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl EndUserMutationAdmission for PermitAdmission {
        async fn admit(&self, _command: EndUserMutationCommand) -> EndUserMutationAdmissionResult {
            self.calls.fetch_add(1, Ordering::SeqCst);
            EndUserMutationAdmissionResult::Allowed
        }
    }

    struct StaticResolver {
        gateway: ResolvedGateway,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl GatewayResolver for StaticResolver {
        async fn resolve(
            &self,
            billing_scope_id: BillingScopeId,
            gateway_account_id: GatewayAccountId,
            gateway_configuration_id: GatewayConfigurationId,
            provider_key: GatewayProviderKey,
        ) -> Result<ResolvedGateway, GatewayResolutionError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if billing_scope_id != self.gateway.billing_scope_id()
                || gateway_account_id != self.gateway.gateway_account_id()
                || gateway_configuration_id != self.gateway.gateway_configuration_id()
                || provider_key != *self.gateway.provider_key()
            {
                return Err(GatewayResolutionError::ConfigurationChanged);
            }
            Ok(self.gateway.clone())
        }
    }

    impl ScriptedGateway {
        fn new(result: Result<GatewayPaymentOutcome, GatewayMutationError>) -> Self {
            Self {
                sale_calls: AtomicUsize::new(0),
                sale_result: Mutex::new(Some(result)),
                store_calls: AtomicUsize::new(0),
                store_result: Mutex::new(None),
            }
        }

        fn for_stored_method(result: Result<GatewayPaymentOutcome, GatewayMutationError>) -> Self {
            Self {
                sale_calls: AtomicUsize::new(0),
                sale_result: Mutex::new(None),
                store_calls: AtomicUsize::new(0),
                store_result: Mutex::new(Some(result)),
            }
        }
    }

    #[async_trait]
    impl PaymentGateway for ScriptedGateway {
        async fn account_mode(&self) -> Result<GatewayAccountMode, GatewayError> {
            Ok(GatewayAccountMode::Live)
        }

        async fn sale(
            &self,
            _request: GatewaySaleRequest,
        ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
            self.sale_calls.fetch_add(1, Ordering::SeqCst);
            self.sale_result
                .lock()
                .await
                .take()
                .expect("submission capability permits one scripted sale")
        }

        async fn store_payment_method(
            &self,
            _request: GatewayStorePaymentMethodRequest,
        ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
            self.store_calls.fetch_add(1, Ordering::SeqCst);
            self.store_result
                .lock()
                .await
                .take()
                .expect("submission capability permits one scripted stored-method mutation")
        }

        async fn query_transaction(
            &self,
            _request: GatewayQueryRequest,
        ) -> Result<Option<GatewayPaymentOutcome>, GatewayError> {
            panic!("initial submission must not query")
        }

        async fn query_transaction_reports(
            &self,
            _request: GatewayTransactionReportRequest,
        ) -> Result<Vec<GatewayTransactionReport>, GatewayError> {
            panic!("initial submission must not query reports")
        }
    }

    fn scripted_resolved_gateway<G>(
        account: crate::test_support::GatewayAccountFixture,
        gateway: Arc<G>,
    ) -> ResolvedGateway
    where
        G: PaymentGateway + 'static,
    {
        ResolvedGateway::new(
            BillingScopeId::new(account.billing_scope_id),
            GatewayAccountId::new(account.gateway_account_id),
            GatewayConfigurationId::new(account.gateway_configuration_id),
            GatewayProviderKey::new("nmi").unwrap(),
            GatewayLifecycleQueryPolicy::new(
                GatewayLifecycleCursorKey::new("test_cursor").unwrap(),
                ChronoDuration::minutes(1),
                10,
                2,
                2,
                20,
            )
            .unwrap(),
            Arc::new(TestReferenceFactory),
            gateway,
        )
    }

    struct TestOfferStore;

    #[async_trait]
    impl SubscriptionOfferStore for TestOfferStore {
        async fn lock_current_offer(
            &self,
            connection: &mut PgConnection,
            billing_scope_id: BillingScopeId,
            plan_key: &PlanKey,
        ) -> Result<Option<syrup_rail::SubscriptionOffer>, sqlx::Error> {
            let row = sqlx::query_as::<_, (i32, String)>(
                r#"
                SELECT amount_cents, currency
                FROM host_subscription_offers
                WHERE billing_scope_id = $1 AND plan_key = $2
                FOR UPDATE
                "#,
            )
            .bind(billing_scope_id.as_uuid())
            .bind(plan_key.as_str())
            .fetch_optional(connection)
            .await?;
            row.map(|(amount_cents, currency)| {
                Ok(syrup_rail::SubscriptionOffer::new(
                    plan_key.clone(),
                    ChargeAmount::new(amount_cents, CurrencyCode::new(&currency).unwrap()).unwrap(),
                ))
            })
            .transpose()
        }
    }

    #[derive(Clone)]
    struct TestCoordinator {
        pool: PgPool,
        events: Arc<Mutex<Vec<BillingEvent>>>,
        fail_event: bool,
    }

    #[async_trait]
    impl BillingTransactionCoordinator for TestCoordinator {
        async fn begin(
            &self,
            _subject: BillingEventSubject,
            _lock_timeout: Duration,
        ) -> Result<Box<dyn BillingTransaction>, BillingTransactionError> {
            Ok(Box::new(TestTransaction {
                transaction: Some(
                    self.pool
                        .begin()
                        .await
                        .map_err(BillingTransactionError::new)?,
                ),
                events: Arc::clone(&self.events),
                fail_event: self.fail_event,
            }))
        }
    }

    struct TestTransaction {
        transaction: Option<Transaction<'static, Postgres>>,
        events: Arc<Mutex<Vec<BillingEvent>>>,
        fail_event: bool,
    }

    #[async_trait]
    impl BillingTransaction for TestTransaction {
        fn connection(&mut self) -> &mut PgConnection {
            &mut *self.transaction.as_mut().expect("active test transaction")
        }

        fn subject_state(&self) -> BillingTransactionSubjectState {
            BillingTransactionSubjectState::LiveRecipient
        }

        async fn append_event(
            &mut self,
            event: &BillingEvent,
        ) -> Result<(), BillingEventWriteError> {
            if self.fail_event {
                return Err(BillingEventWriteError::new(InjectedHostError));
            }
            self.events.lock().await.push(event.clone());
            Ok(())
        }

        async fn commit(mut self: Box<Self>) -> Result<(), BillingTransactionError> {
            self.transaction
                .take()
                .expect("active test transaction")
                .commit()
                .await
                .map_err(BillingTransactionError::new)
        }

        async fn rollback(mut self: Box<Self>) -> Result<(), BillingTransactionError> {
            self.transaction
                .take()
                .expect("active test transaction")
                .rollback()
                .await
                .map_err(BillingTransactionError::new)
        }
    }

    struct ApplicationFixture {
        database: TestDatabase,
        reservation: SubscriptionEnrollmentReservation,
        coordinator: TestCoordinator,
        command: EnrollSubscription,
        gateway_account: crate::test_support::GatewayAccountFixture,
        admission: Option<Box<AdmittedSubscriptionEnrollment>>,
    }

    impl ApplicationFixture {
        async fn cleanup(self) -> Result<(), Box<dyn Error>> {
            self.database.cleanup().await
        }
    }

    async fn application_fixture(
        project: &str,
        discounted: bool,
        fail_event: bool,
    ) -> Result<ApplicationFixture, Box<dyn Error>> {
        enrollment_fixture(project, discounted, fail_event, true).await
    }

    async fn enrollment_fixture(
        project: &str,
        discounted: bool,
        fail_event: bool,
        prepare_submission: bool,
    ) -> Result<ApplicationFixture, Box<dyn Error>> {
        let database = TestDatabase::start(project).await?;
        sqlx::query(
            r#"
            CREATE TABLE host_subscription_offers (
                billing_scope_id uuid NOT NULL,
                plan_key text NOT NULL,
                amount_cents integer NOT NULL,
                currency text NOT NULL,
                PRIMARY KEY (billing_scope_id, plan_key)
            )
            "#,
        )
        .execute(&database.pool)
        .await?;
        let account = create_gateway_account(&database.pool, "nmi").await?;
        sqlx::query(
            "INSERT INTO host_subscription_offers VALUES ($1, 'base_subscription', 1000, 'USD')",
        )
        .bind(account.billing_scope_id)
        .execute(&database.pool)
        .await?;
        let subscriber_id = Uuid::now_v7();
        let expected_charge = if discounted {
            let code_id = Uuid::now_v7();
            sqlx::query(
                r#"
                INSERT INTO billing_subscription_discount_codes (
                    id, billing_scope_id, plan_key, code_normalized, display_code,
                    status, discount_kind, percent_off_bps, currency,
                    duration, duration_months
                ) VALUES (
                    $1, $2, 'base_subscription', 'SAVE20', 'SAVE20',
                    'active', 'percent_off', 2000, 'USD', 'limited_months', 3
                )
                "#,
            )
            .bind(code_id)
            .bind(account.billing_scope_id)
            .execute(&database.pool)
            .await?;
            sqlx::query(
                r#"
                INSERT INTO billing_subscription_discount_claims (
                    id, billing_scope_id, subscriber_id, plan_key,
                    discount_code_id, code_snapshot, discount_kind,
                    percent_off_bps, currency, duration, duration_months,
                    base_amount_cents, discounted_amount_cents, status
                ) VALUES (
                    $1, $2, $3, 'base_subscription', $4, 'SAVE20',
                    'percent_off', 2000, 'USD', 'limited_months', 3,
                    1000, 800, 'saved'
                )
                "#,
            )
            .bind(Uuid::now_v7())
            .bind(account.billing_scope_id)
            .bind(subscriber_id)
            .bind(code_id)
            .execute(&database.pool)
            .await?;
            SubscriptionEnrollmentExpectedCharge::discounted(
                PlanKey::new("base_subscription")?,
                SubscriptionDiscountSnapshot::new(
                    SubscriptionDiscountCode::new("SAVE20")?,
                    None,
                    SubscriptionDiscountKind::PercentOffBasisPoints(PercentOffBasisPoints::new(
                        2000,
                    )?),
                    SubscriptionDiscountDuration::LimitedMonths(
                        syrup_rail::LimitedDiscountMonths::new(3)?,
                    ),
                    ChargeAmount::new(1000, CurrencyCode::new("USD")?)?,
                    ChargeAmount::new(800, CurrencyCode::new("USD")?)?,
                )?,
            )
        } else {
            SubscriptionEnrollmentExpectedCharge::full_price(syrup_rail::SubscriptionOffer::new(
                PlanKey::new("base_subscription")?,
                ChargeAmount::new(1000, CurrencyCode::new("USD")?)?,
            ))
        };
        let attempt_id = PaymentAttemptId::new(Uuid::now_v7());
        let command = EnrollSubscription::new(
            attempt_id,
            BillingScopeId::new(account.billing_scope_id),
            SubscriberId::new(subscriber_id),
            GatewayConfigurationId::new(account.gateway_configuration_id),
            IdempotencyKey::new("application-key")?,
            PaymentToken::new("opaque-payment-token")?,
            BillingContact::new(
                Some("Ada".to_owned()),
                Some("Lovelace".to_owned()),
                Some("ada@example.test".to_owned()),
            )?,
            expected_charge,
        );
        let gateway = ResolvedGateway::new(
            BillingScopeId::new(account.billing_scope_id),
            GatewayAccountId::new(account.gateway_account_id),
            GatewayConfigurationId::new(account.gateway_configuration_id),
            GatewayProviderKey::new("nmi")?,
            GatewayLifecycleQueryPolicy::new(
                GatewayLifecycleCursorKey::new("test_cursor")?,
                ChronoDuration::minutes(1),
                10,
                2,
                2,
                20,
            )?,
            Arc::new(TestReferenceFactory),
            Arc::new(NeverCalledGateway),
        );
        let reservation = SubscriptionEnrollmentReservation::from_command(&command, &gateway)?;
        let admission = if prepare_submission {
            let mut transaction = database.pool.begin().await?;
            assert!(matches!(
                reserve_subscription_enrollment_in_transaction(
                    &mut transaction,
                    &TestOfferStore,
                    &reservation,
                )
                .await?,
                SubscriptionEnrollmentReservationOutcome::Reserved(_)
            ));
            transaction.commit().await?;
            Some(
                match admit_subscription_enrollment_submission(
                    &database.pool,
                    &TestOfferStore,
                    &reservation,
                )
                .await?
                {
                    SubscriptionEnrollmentAdmissionOutcome::Admitted(admission) => admission,
                    other => return Err(format!("unexpected final admission: {other:?}").into()),
                },
            )
        } else {
            None
        };
        let coordinator = TestCoordinator {
            pool: database.pool.clone(),
            events: Arc::new(Mutex::new(Vec::new())),
            fail_event,
        };
        Ok(ApplicationFixture {
            database,
            reservation,
            coordinator,
            command,
            gateway_account: account,
            admission,
        })
    }

    fn approved_outcome(transaction_id: &str) -> GatewayPaymentOutcome {
        approved_outcome_with_reference(Some(transaction_id), "vault_application")
    }

    fn approved_outcome_with_transaction(transaction_id: Option<&str>) -> GatewayPaymentOutcome {
        approved_outcome_with_reference(transaction_id, "vault_application")
    }

    fn approved_outcome_with_reference(
        transaction_id: Option<&str>,
        payment_method_reference: &str,
    ) -> GatewayPaymentOutcome {
        GatewayPaymentOutcome::new(
            GatewayPaymentStatus::Approved,
            ProcessorEvidence::new(
                transaction_id.map(|value| GatewayTransactionId::new(value).unwrap()),
                Some(GatewayPaymentMethodReference::new(payment_method_reference).unwrap()),
                Some(GatewayDiagnostic::new("1")),
                Some(GatewayDiagnostic::new("100")),
                Some(GatewayDiagnostic::new("Approved")),
                Some(GatewayDiagnostic::new("complete")),
                GatewayPaymentDescriptor::from_provider_parts(
                    Some(GatewayDiagnostic::new("creditcard")),
                    Some(GatewayDiagnostic::new("visa")),
                    Some("4242"),
                    Some(12),
                    Some(2031),
                ),
            ),
        )
    }

    #[tokio::test]
    async fn foreground_service_applies_once_and_replays_before_host_admission()
    -> Result<(), Box<dyn Error>> {
        let fixture = enrollment_fixture("service_enroll", false, false, false).await?;
        let gateway = Arc::new(ScriptedGateway::new(Ok(approved_outcome(
            "txn_service_enroll",
        ))));
        let resolved = scripted_resolved_gateway(fixture.gateway_account, Arc::clone(&gateway));
        let resolver = Arc::new(StaticResolver {
            gateway: resolved,
            calls: AtomicUsize::new(0),
        });
        let admission = Arc::new(PermitAdmission {
            calls: AtomicUsize::new(0),
        });
        let service = SubscriptionBillingService::new(
            fixture.database.pool.clone(),
            Arc::new(TestOfferStore),
            resolver.clone(),
            admission.clone(),
            Arc::new(fixture.coordinator.clone()),
        );

        let result = service.enroll(fixture.command.clone()).await?;
        assert_eq!(result.attempt().status(), PaymentAttemptStatus::Approved);
        assert!(result.subscription().is_some());
        assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 1);
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
        assert_eq!(admission.calls.load(Ordering::SeqCst), 1);

        let replay = service.enroll(fixture.command.clone()).await?;
        assert_eq!(replay, result);
        assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 1);
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
        assert_eq!(admission.calls.load(Ordering::SeqCst), 1);
        fixture.cleanup().await
    }

    #[tokio::test]
    async fn foreground_recovery_derives_locked_terms_applies_once_and_replays()
    -> Result<(), Box<dyn Error>> {
        let fixture = enrollment_fixture("service_recovery", true, false, false).await?;
        let initial_gateway = Arc::new(ScriptedGateway::new(Ok(approved_outcome(
            "txn_recovery_initial",
        ))));
        let initial_resolver = Arc::new(StaticResolver {
            gateway: scripted_resolved_gateway(
                fixture.gateway_account,
                Arc::clone(&initial_gateway),
            ),
            calls: AtomicUsize::new(0),
        });
        let initial_service = SubscriptionBillingService::new(
            fixture.database.pool.clone(),
            Arc::new(TestOfferStore),
            initial_resolver,
            Arc::new(PermitAdmission {
                calls: AtomicUsize::new(0),
            }),
            Arc::new(fixture.coordinator.clone()),
        );
        let initial = initial_service.enroll(fixture.command.clone()).await?;
        let subscription_id = initial
            .subscription()
            .expect("approved enrollment has a subscription")
            .id();
        let due_at = Utc::now() - ChronoDuration::days(1);
        let current_period_start_at = due_at - ChronoDuration::days(31);
        let persisted_due_at: DateTime<Utc> = sqlx::query_scalar(
            r#"
            UPDATE billing_subscriptions
            SET current_period_start_at = $2,
                current_period_end_at = $3,
                next_renewal_at = $3,
                updated_at = clock_timestamp()
            WHERE id = $1
            RETURNING next_renewal_at
            "#,
        )
        .bind(subscription_id.as_uuid())
        .bind(current_period_start_at)
        .bind(due_at)
        .fetch_one(&fixture.database.pool)
        .await?;

        let recovery_gateway = Arc::new(ScriptedGateway::new(Ok(approved_outcome_with_reference(
            Some("txn_recovery_approved"),
            "vault_recovery",
        ))));
        let resolver = Arc::new(StaticResolver {
            gateway: scripted_resolved_gateway(
                fixture.gateway_account,
                Arc::clone(&recovery_gateway),
            ),
            calls: AtomicUsize::new(0),
        });
        let admission = Arc::new(PermitAdmission {
            calls: AtomicUsize::new(0),
        });
        let service = SubscriptionBillingService::new(
            fixture.database.pool.clone(),
            Arc::new(TestOfferStore),
            resolver.clone(),
            admission.clone(),
            Arc::new(fixture.coordinator.clone()),
        );
        let command = RecoverSubscriptionPayment::new(
            PaymentAttemptId::new(Uuid::now_v7()),
            fixture.command.billing_scope_id(),
            fixture.command.subscriber_id(),
            fixture.command.plan_key().clone(),
            fixture.command.gateway_configuration_id(),
            IdempotencyKey::new("recovery-key")?,
            PaymentToken::new("opaque-recovery-token")?,
            fixture.command.billing_contact().clone(),
        );

        let result = service.recover(command.clone()).await?;
        assert_eq!(result.attempt().status(), PaymentAttemptStatus::Approved);
        assert_eq!(
            result.attempt().kind(),
            PaymentAttemptKind::SubscriptionRecovery
        );
        let recovered = result
            .subscription()
            .expect("recovery applies subscription");
        assert_eq!(recovered.id(), subscription_id);
        assert_eq!(recovered.status(), SubscriptionStatus::Active);
        assert_eq!(*recovered.current_period().start_at(), persisted_due_at);
        assert_eq!(
            recovered.payment_method_id(),
            result
                .attempt()
                .request()
                .target()
                .payment_method_id()
                .unwrap()
        );
        assert_eq!(recovery_gateway.sale_calls.load(Ordering::SeqCst), 1);
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
        assert_eq!(admission.calls.load(Ordering::SeqCst), 1);

        let discount: (i32, String, i32) = sqlx::query_as(
            r#"
            SELECT discounts.periods_applied, discounts.status, subscriptions.amount_cents
            FROM billing_subscription_discounts AS discounts
            INNER JOIN billing_subscriptions AS subscriptions
                ON subscriptions.id = discounts.subscription_id
            WHERE discounts.subscription_id = $1
            "#,
        )
        .bind(subscription_id.as_uuid())
        .fetch_one(&fixture.database.pool)
        .await?;
        assert_eq!(discount, (2, "active".to_owned(), 800));

        let replay = service.recover(command.clone()).await?;
        assert_eq!(replay, result);
        assert_eq!(recovery_gateway.sale_calls.load(Ordering::SeqCst), 1);
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
        assert_eq!(admission.calls.load(Ordering::SeqCst), 1);
        let next_due_at = Utc::now() - ChronoDuration::hours(12);
        sqlx::query(
            r#"
            UPDATE billing_subscriptions
            SET current_period_end_at = $2,
                next_renewal_at = $2,
                updated_at = clock_timestamp()
            WHERE id = $1
            "#,
        )
        .bind(subscription_id.as_uuid())
        .bind(next_due_at)
        .execute(&fixture.database.pool)
        .await?;
        assert!(matches!(
            service.recover(command).await,
            Err(SubscriptionEnrollmentServiceError::IdempotencyConflict)
        ));
        assert_eq!(recovery_gateway.sale_calls.load(Ordering::SeqCst), 1);
        let events = fixture.coordinator.events.lock().await;
        assert_eq!(events.len(), 2);
        assert!(matches!(
            events[1],
            BillingEvent::SubscriptionRenewed { .. }
        ));
        drop(events);
        fixture.cleanup().await
    }

    #[tokio::test]
    async fn foreground_payment_method_replacement_applies_once_and_replays_before_admission()
    -> Result<(), Box<dyn Error>> {
        let fixture = enrollment_fixture("replace_method", false, false, false).await?;
        let initial_gateway = Arc::new(ScriptedGateway::new(Ok(approved_outcome_with_reference(
            Some("txn_method_initial"),
            "vault_method_old",
        ))));
        let initial_service = SubscriptionBillingService::new(
            fixture.database.pool.clone(),
            Arc::new(TestOfferStore),
            Arc::new(StaticResolver {
                gateway: scripted_resolved_gateway(
                    fixture.gateway_account,
                    Arc::clone(&initial_gateway),
                ),
                calls: AtomicUsize::new(0),
            }),
            Arc::new(PermitAdmission {
                calls: AtomicUsize::new(0),
            }),
            Arc::new(fixture.coordinator.clone()),
        );
        let initial = initial_service.enroll(fixture.command.clone()).await?;
        let subscription = initial
            .subscription()
            .expect("approved enrollment has subscription");
        let subscription_id = subscription.id();
        let old_method_id = subscription.payment_method_id();

        let gateway = Arc::new(ScriptedGateway::for_stored_method(Ok(
            approved_outcome_with_reference(Some("txn_method_new"), "vault_method_new"),
        )));
        let resolver = Arc::new(StaticResolver {
            gateway: scripted_resolved_gateway(fixture.gateway_account, Arc::clone(&gateway)),
            calls: AtomicUsize::new(0),
        });
        let admission = Arc::new(PermitAdmission {
            calls: AtomicUsize::new(0),
        });
        let service = SubscriptionBillingService::new(
            fixture.database.pool.clone(),
            Arc::new(TestOfferStore),
            resolver.clone(),
            admission.clone(),
            Arc::new(fixture.coordinator.clone()),
        );
        let command = ReplaceSubscriptionPaymentMethod::new(
            PaymentAttemptId::new(Uuid::now_v7()),
            fixture.command.billing_scope_id(),
            fixture.command.subscriber_id(),
            fixture.command.plan_key().clone(),
            fixture.command.gateway_configuration_id(),
            IdempotencyKey::new("replace-method-key")?,
            PaymentToken::new("opaque-replacement-token")?,
            fixture.command.billing_contact().clone(),
        );

        let result = service.replace_payment_method(command.clone()).await?;
        assert_eq!(result.attempt().status(), PaymentAttemptStatus::Approved);
        assert_eq!(
            result.attempt().kind(),
            PaymentAttemptKind::SubscriptionPaymentMethodUpdate
        );
        let updated = result
            .subscription()
            .expect("approved replacement returns subscription");
        assert_eq!(updated.id(), subscription_id);
        assert_ne!(updated.payment_method_id(), old_method_id);
        assert_eq!(
            updated.payment_method_id(),
            result
                .attempt()
                .request()
                .target()
                .payment_method_id()
                .expect("applied attempt carries replacement method")
        );
        let old_status: String =
            sqlx::query_scalar("SELECT status FROM billing_payment_methods WHERE id = $1")
                .bind(old_method_id.as_uuid())
                .fetch_one(&fixture.database.pool)
                .await?;
        assert_eq!(old_status, "disabled");
        assert_eq!(gateway.store_calls.load(Ordering::SeqCst), 1);
        assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 0);
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
        assert_eq!(admission.calls.load(Ordering::SeqCst), 1);

        let replay = service.replace_payment_method(command).await?;
        assert_eq!(replay, result);
        assert_eq!(gateway.store_calls.load(Ordering::SeqCst), 1);
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
        assert_eq!(admission.calls.load(Ordering::SeqCst), 1);
        let events = fixture.coordinator.events.lock().await;
        assert_eq!(events.len(), 2);
        assert!(matches!(
            events[1],
            BillingEvent::PaymentMethodChanged { .. }
        ));
        drop(events);
        fixture.cleanup().await
    }

    #[tokio::test]
    async fn foreground_service_pre_reservation_cooldown_creates_no_attempt_or_provider_io()
    -> Result<(), Box<dyn Error>> {
        let fixture = enrollment_fixture("service_cooldown", false, false, false).await?;
        sqlx::query(
            r#"
            UPDATE billing_gateway_accounts
            SET mutation_rate_limited_until = clock_timestamp() + interval '1 minute'
            WHERE id = $1
            "#,
        )
        .bind(fixture.gateway_account.gateway_account_id)
        .execute(&fixture.database.pool)
        .await?;
        let gateway = Arc::new(ScriptedGateway::new(Ok(approved_outcome(
            "txn_must_not_submit",
        ))));
        let resolved = scripted_resolved_gateway(fixture.gateway_account, Arc::clone(&gateway));
        let resolver = Arc::new(StaticResolver {
            gateway: resolved,
            calls: AtomicUsize::new(0),
        });
        let admission = Arc::new(PermitAdmission {
            calls: AtomicUsize::new(0),
        });
        let service = SubscriptionBillingService::new(
            fixture.database.pool.clone(),
            Arc::new(TestOfferStore),
            resolver.clone(),
            admission.clone(),
            Arc::new(fixture.coordinator.clone()),
        );

        let error = service
            .enroll(fixture.command.clone())
            .await
            .expect_err("active local cooldown must reject before reservation");
        assert!(matches!(
            error,
            SubscriptionEnrollmentServiceError::GatewayMutationCooldown {
                scope: GatewayMutationCooldownScope::Account
            }
        ));
        assert_eq!(admission.calls.load(Ordering::SeqCst), 1);
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 0);
        assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 0);
        let attempts: i64 = sqlx::query_scalar("SELECT count(*) FROM billing_payment_attempts")
            .fetch_one(&fixture.database.pool)
            .await?;
        assert_eq!(attempts, 0);
        fixture.cleanup().await
    }

    #[tokio::test]
    async fn foreground_service_resumes_the_durable_attempt_not_the_retry_candidate_id()
    -> Result<(), Box<dyn Error>> {
        let fixture = enrollment_fixture("service_resume", false, false, false).await?;
        let original_attempt_id = fixture.command.attempt_id();
        let mut transaction = fixture.database.pool.begin().await?;
        assert!(matches!(
            reserve_subscription_enrollment_in_transaction(
                &mut transaction,
                &TestOfferStore,
                &fixture.reservation,
            )
            .await?,
            SubscriptionEnrollmentReservationOutcome::Reserved(_)
        ));
        transaction.commit().await?;

        let retry = EnrollSubscription::new(
            PaymentAttemptId::new(Uuid::now_v7()),
            fixture.command.billing_scope_id(),
            fixture.command.subscriber_id(),
            fixture.command.gateway_configuration_id(),
            fixture.command.idempotency_key().clone(),
            fixture.command.payment_token().clone(),
            fixture.command.billing_contact().clone(),
            fixture.command.expected_charge().clone(),
        );
        let gateway = Arc::new(ScriptedGateway::new(Ok(approved_outcome(
            "txn_service_resume",
        ))));
        let resolved = scripted_resolved_gateway(fixture.gateway_account, Arc::clone(&gateway));
        let resolver = Arc::new(StaticResolver {
            gateway: resolved,
            calls: AtomicUsize::new(0),
        });
        let admission = Arc::new(PermitAdmission {
            calls: AtomicUsize::new(0),
        });
        let service = SubscriptionBillingService::new(
            fixture.database.pool.clone(),
            Arc::new(TestOfferStore),
            resolver,
            admission.clone(),
            Arc::new(fixture.coordinator.clone()),
        );

        let result = service.enroll(retry).await?;
        assert_eq!(
            result.attempt().identity().attempt_id(),
            original_attempt_id
        );
        assert_eq!(result.attempt().status(), PaymentAttemptStatus::Approved);
        assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 1);
        assert_eq!(admission.calls.load(Ordering::SeqCst), 1);
        fixture.cleanup().await
    }

    #[tokio::test]
    async fn foreground_readiness_throttle_resolves_attempt_and_provider_cooldown_atomically()
    -> Result<(), Box<dyn Error>> {
        let fixture = enrollment_fixture("svc_ready_429", false, false, false).await?;
        let resolved = scripted_resolved_gateway(
            fixture.gateway_account,
            Arc::new(RateLimitedReadinessGateway),
        );
        let resolver = Arc::new(StaticResolver {
            gateway: resolved,
            calls: AtomicUsize::new(0),
        });
        let admission = Arc::new(PermitAdmission {
            calls: AtomicUsize::new(0),
        });
        let service = SubscriptionBillingService::new(
            fixture.database.pool.clone(),
            Arc::new(TestOfferStore),
            resolver.clone(),
            admission.clone(),
            Arc::new(fixture.coordinator.clone()),
        );

        let error = service
            .enroll(fixture.command.clone())
            .await
            .expect_err("provider readiness throttle must return a typed cooldown");
        assert!(matches!(
            error,
            SubscriptionEnrollmentServiceError::GatewayMutationCooldown {
                scope: GatewayMutationCooldownScope::Provider
            }
        ));
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
        assert_eq!(admission.calls.load(Ordering::SeqCst), 1);
        let state: (String, Option<String>, bool, bool) = sqlx::query_as(
            r#"
            SELECT attempt.status, attempt.resolution_code,
                COALESCE(account.mutation_rate_limited_until > clock_timestamp(), false),
                provider.rate_limited_until > clock_timestamp()
            FROM billing_payment_attempts AS attempt
            INNER JOIN billing_gateway_accounts AS account
                ON account.id = attempt.gateway_account_id
            INNER JOIN billing_gateway_provider_rate_limits AS provider
                ON provider.provider_key = account.provider_key
            WHERE attempt.id = $1
            "#,
        )
        .bind(fixture.command.attempt_id().as_uuid())
        .fetch_one(&fixture.database.pool)
        .await?;
        assert_eq!(state.0, "failed");
        assert_eq!(
            state.1.as_deref(),
            Some("gateway_provider_rate_limited_before_submission")
        );
        assert!(!state.2);
        assert!(state.3);
        fixture.cleanup().await
    }

    #[tokio::test]
    async fn foreground_fresh_cooldown_after_readiness_prevents_the_admitted_sale()
    -> Result<(), Box<dyn Error>> {
        let fixture = enrollment_fixture("svc_fresh_stop", false, false, false).await?;
        let gateway = Arc::new(CooldownDuringReadinessGateway {
            pool: fixture.database.pool.clone(),
            account_id: fixture.gateway_account.gateway_account_id,
        });
        let resolved = scripted_resolved_gateway(fixture.gateway_account, gateway);
        let resolver = Arc::new(StaticResolver {
            gateway: resolved,
            calls: AtomicUsize::new(0),
        });
        let admission = Arc::new(PermitAdmission {
            calls: AtomicUsize::new(0),
        });
        let service = SubscriptionBillingService::new(
            fixture.database.pool.clone(),
            Arc::new(TestOfferStore),
            resolver,
            admission,
            Arc::new(fixture.coordinator.clone()),
        );

        let error = service
            .enroll(fixture.command.clone())
            .await
            .expect_err("fresh cooldown must close the one-shot sale boundary");
        assert!(matches!(
            error,
            SubscriptionEnrollmentServiceError::GatewayMutationCooldown {
                scope: GatewayMutationCooldownScope::Account
            }
        ));
        let attempt: (String, Option<String>, bool) = sqlx::query_as(
            r#"
            SELECT status, resolution_code, submitted_at IS NOT NULL
            FROM billing_payment_attempts
            WHERE id = $1
            "#,
        )
        .bind(fixture.command.attempt_id().as_uuid())
        .fetch_one(&fixture.database.pool)
        .await?;
        assert_eq!(attempt.0, "failed");
        assert_eq!(
            attempt.1.as_deref(),
            Some("gateway_account_mutation_cooldown_before_submission")
        );
        assert!(!attempt.2);
        fixture.cleanup().await
    }

    #[tokio::test]
    async fn foreground_stale_prepared_replay_expires_before_admission_or_live_terms()
    -> Result<(), Box<dyn Error>> {
        let fixture = enrollment_fixture("svc_stale_replay", false, false, false).await?;
        let mut transaction = fixture.database.pool.begin().await?;
        assert!(matches!(
            reserve_subscription_enrollment_in_transaction(
                &mut transaction,
                &TestOfferStore,
                &fixture.reservation,
            )
            .await?,
            SubscriptionEnrollmentReservationOutcome::Reserved(_)
        ));
        transaction.commit().await?;
        sqlx::query(
            r#"
            UPDATE billing_payment_attempts
            SET created_at = clock_timestamp() - interval '30 minutes'
            WHERE id = $1
            "#,
        )
        .bind(fixture.command.attempt_id().as_uuid())
        .execute(&fixture.database.pool)
        .await?;
        sqlx::query("DELETE FROM host_subscription_offers")
            .execute(&fixture.database.pool)
            .await?;

        let gateway = Arc::new(ScriptedGateway::new(Ok(approved_outcome(
            "txn_stale_must_not_submit",
        ))));
        let resolved = scripted_resolved_gateway(fixture.gateway_account, Arc::clone(&gateway));
        let resolver = Arc::new(StaticResolver {
            gateway: resolved,
            calls: AtomicUsize::new(0),
        });
        let admission = Arc::new(PermitAdmission {
            calls: AtomicUsize::new(0),
        });
        let service = SubscriptionBillingService::new(
            fixture.database.pool.clone(),
            Arc::new(TestOfferStore),
            resolver.clone(),
            admission.clone(),
            Arc::new(fixture.coordinator.clone()),
        );

        let result = service.enroll(fixture.command.clone()).await?;
        assert_eq!(result.attempt().status(), PaymentAttemptStatus::Failed);
        assert_eq!(
            result.attempt().state().resolution_code(),
            Some(PaymentResolutionCode::SubscriptionInitialPreparedAttemptExpired)
        );
        assert_eq!(admission.calls.load(Ordering::SeqCst), 0);
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 0);
        assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 0);
        fixture.cleanup().await
    }

    #[tokio::test]
    async fn discounted_approval_applies_one_atomic_subscription_event_and_replays()
    -> Result<(), Box<dyn Error>> {
        let fixture = application_fixture("enroll_apply", true, false).await?;
        let outcome = approved_outcome("txn_application_approved");
        let result = apply_subscription_enrollment_gateway_outcome(
            &fixture.database.pool,
            &fixture.coordinator,
            &fixture.reservation,
            &outcome,
        )
        .await?;
        assert_eq!(result.attempt().status(), PaymentAttemptStatus::Approved);
        assert_eq!(
            result
                .subscription()
                .expect("applied subscription")
                .recurring_charge()
                .cents(),
            800
        );
        let events = fixture.coordinator.events.lock().await.clone();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].semantic_key(),
            BillingEventKey::SubscriptionStarted(
                result.subscription().expect("applied subscription").id()
            )
        );
        assert!(matches!(
            &events[0],
            BillingEvent::SubscriptionStarted { charge, .. } if charge.cents() == 800
        ));
        let rows: (i64, i64, i64, String, i32) = sqlx::query_as(
            r#"
            SELECT
                (SELECT count(*) FROM billing_payment_methods),
                (SELECT count(*) FROM billing_subscriptions),
                (SELECT count(*) FROM billing_processor_charges),
                (SELECT status FROM billing_subscription_discount_claims LIMIT 1),
                (SELECT periods_applied FROM billing_subscription_discounts LIMIT 1)
            "#,
        )
        .fetch_one(&fixture.database.pool)
        .await?;
        assert_eq!(rows, (1, 1, 1, "applied".to_owned(), 1));
        let progression: String =
            sqlx::query_scalar("SELECT progression_state FROM billing_processor_charges")
                .fetch_one(&fixture.database.pool)
                .await?;
        assert_eq!(progression, "applied");

        let replay = apply_subscription_enrollment_gateway_outcome(
            &fixture.database.pool,
            &fixture.coordinator,
            &fixture.reservation,
            &outcome,
        )
        .await?;
        assert_eq!(replay.attempt().status(), PaymentAttemptStatus::Approved);
        assert_eq!(fixture.coordinator.events.lock().await.len(), 1);
        fixture.cleanup().await
    }

    #[tokio::test]
    async fn reconciled_initial_approval_uses_durable_attempt_after_configuration_rotation()
    -> Result<(), Box<dyn Error>> {
        let fixture = application_fixture("rec_initial", false, false).await?;
        sqlx::query(
            r#"
            UPDATE billing_gateway_accounts
            SET gateway_configuration_id = $2, updated_at = clock_timestamp()
            WHERE id = $1
            "#,
        )
        .bind(fixture.gateway_account.gateway_account_id)
        .bind(Uuid::now_v7())
        .execute(&fixture.database.pool)
        .await?;
        let outcome = approved_outcome("txn_reconciled_approved");

        let result = apply_reconciled_subscription_enrollment_gateway_outcome(
            &fixture.database.pool,
            &fixture.coordinator,
            fixture.command.billing_scope_id(),
            fixture.command.attempt_id(),
            &outcome,
        )
        .await?;
        assert_eq!(result.attempt().status(), PaymentAttemptStatus::Approved);
        assert!(result.subscription().is_some());
        assert_eq!(fixture.coordinator.events.lock().await.len(), 1);

        let replay = apply_reconciled_subscription_enrollment_gateway_outcome(
            &fixture.database.pool,
            &fixture.coordinator,
            fixture.command.billing_scope_id(),
            fixture.command.attempt_id(),
            &outcome,
        )
        .await?;
        assert_eq!(replay, result);
        assert_eq!(fixture.coordinator.events.lock().await.len(), 1);
        fixture.cleanup().await
    }

    #[tokio::test]
    async fn committed_admission_capability_submits_and_applies_exactly_one_sale()
    -> Result<(), Box<dyn Error>> {
        let mut fixture = application_fixture("submit_once", false, false).await?;
        let gateway = Arc::new(ScriptedGateway::new(Ok(approved_outcome(
            "txn_submitted_once",
        ))));
        let resolved = scripted_resolved_gateway(fixture.gateway_account, Arc::clone(&gateway));
        let result = submit_admitted_subscription_enrollment(
            &fixture.database.pool,
            &fixture.coordinator,
            *fixture.admission.take().expect("committed admission"),
            &fixture.command,
            &resolved,
        )
        .await?;
        assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            result.payment().attempt().status(),
            PaymentAttemptStatus::Approved
        );
        assert!(result.payment().subscription().is_some());
        fixture.cleanup().await
    }

    #[tokio::test]
    async fn provider_not_submitted_error_resolves_the_admitted_attempt_without_resubmission()
    -> Result<(), Box<dyn Error>> {
        let mut fixture = application_fixture("not_submitted", false, false).await?;
        let gateway = Arc::new(ScriptedGateway::new(Err(
            GatewayMutationError::NotSubmitted(GatewayNotSubmittedError::Unavailable(
                GatewayDiagnostic::new("temporary provider outage"),
            )),
        )));
        let resolved = scripted_resolved_gateway(fixture.gateway_account, Arc::clone(&gateway));
        let result = submit_admitted_subscription_enrollment(
            &fixture.database.pool,
            &fixture.coordinator,
            *fixture.admission.take().expect("committed admission"),
            &fixture.command,
            &resolved,
        )
        .await?;
        assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            result.payment().attempt().status(),
            PaymentAttemptStatus::Failed
        );
        assert_eq!(
            result.payment().attempt().state().resolution_code(),
            Some(PaymentResolutionCode::GatewayUnavailableBeforeSubmission)
        );
        assert!(result.payment().subscription().is_none());
        fixture.cleanup().await
    }

    #[tokio::test]
    async fn provider_not_submitted_throttle_atomically_extends_the_account_cooldown()
    -> Result<(), Box<dyn Error>> {
        let mut fixture = application_fixture("account_throttle", false, false).await?;
        let gateway = Arc::new(ScriptedGateway::new(Err(
            GatewayMutationError::NotSubmitted(GatewayNotSubmittedError::RateLimited(
                GatewayDiagnostic::new("merchant throttle"),
            )),
        )));
        let resolved = scripted_resolved_gateway(fixture.gateway_account, Arc::clone(&gateway));
        let result = submit_admitted_subscription_enrollment(
            &fixture.database.pool,
            &fixture.coordinator,
            *fixture.admission.take().expect("committed admission"),
            &fixture.command,
            &resolved,
        )
        .await?;
        assert!(matches!(
            result,
            SubscriptionEnrollmentProviderResult::NotSubmitted {
                error: GatewayNotSubmittedError::RateLimited(_),
                ..
            }
        ));
        let deadlines: (bool, bool) = sqlx::query_as(
            r#"
            SELECT
                mutation_rate_limited_until > clock_timestamp(),
                provider.rate_limited_until > clock_timestamp()
            FROM billing_gateway_accounts AS account
            INNER JOIN billing_gateway_provider_rate_limits AS provider
                ON provider.provider_key = account.provider_key
            WHERE account.id = $1
            "#,
        )
        .bind(fixture.gateway_account.gateway_account_id)
        .fetch_one(&fixture.database.pool)
        .await?;
        assert_eq!(deadlines, (true, false));
        fixture.cleanup().await
    }

    #[tokio::test]
    async fn event_failure_rolls_back_application_and_durably_parks_approval()
    -> Result<(), Box<dyn Error>> {
        let fixture = application_fixture("event_fail", false, true).await?;
        let result = apply_subscription_enrollment_gateway_outcome(
            &fixture.database.pool,
            &fixture.coordinator,
            &fixture.reservation,
            &approved_outcome("txn_event_failure"),
        )
        .await?;
        assert_eq!(
            result.attempt().status(),
            PaymentAttemptStatus::ReviewRequired
        );
        assert!(result.subscription().is_none());
        let counts: (i64, i64, i64) = sqlx::query_as(
            r#"
            SELECT
                (SELECT count(*) FROM billing_payment_methods),
                (SELECT count(*) FROM billing_subscriptions),
                (SELECT count(*) FROM billing_processor_charges)
            "#,
        )
        .fetch_one(&fixture.database.pool)
        .await?;
        assert_eq!(counts, (0, 0, 1));
        assert!(fixture.coordinator.events.lock().await.is_empty());
        fixture.cleanup().await
    }

    #[tokio::test]
    async fn failed_attempt_parking_falls_back_to_permanent_charge_observation()
    -> Result<(), Box<dyn Error>> {
        let fixture = application_fixture("charge_fallback", false, false).await?;
        sqlx::raw_sql(
            r#"
            CREATE FUNCTION host_reject_attempt_status_update()
            RETURNS trigger LANGUAGE plpgsql AS $$
            BEGIN
                IF NEW.status IS DISTINCT FROM OLD.status THEN
                    RAISE EXCEPTION 'injected attempt status write failure';
                END IF;
                RETURN NEW;
            END
            $$;
            CREATE TRIGGER host_reject_attempt_status_update
            BEFORE UPDATE OF status ON billing_payment_attempts
            FOR EACH ROW EXECUTE FUNCTION host_reject_attempt_status_update();
            "#,
        )
        .execute(&fixture.database.pool)
        .await?;
        let result = apply_subscription_enrollment_gateway_outcome(
            &fixture.database.pool,
            &fixture.coordinator,
            &fixture.reservation,
            &approved_outcome("txn_charge_fallback"),
        )
        .await?;
        assert_eq!(result.attempt().status(), PaymentAttemptStatus::Pending);
        assert_eq!(result.status(), PaymentAttemptStatus::Unknown);
        assert!(result.is_confirmation_pending());
        assert_eq!(
            result
                .processor_evidence()
                .transaction_id()
                .map(GatewayTransactionId::expose),
            Some("txn_charge_fallback")
        );
        assert!(result.subscription().is_none());
        let durable: (i64, String, Option<String>) = sqlx::query_as(
            r#"
            SELECT count(*), min(progression_state), min(gateway_transaction_id)
            FROM billing_processor_charges
            "#,
        )
        .fetch_one(&fixture.database.pool)
        .await?;
        assert_eq!(
            durable,
            (
                1,
                "pending".to_owned(),
                Some("txn_charge_fallback".to_owned())
            )
        );
        let subscription_count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM billing_subscriptions")
                .fetch_one(&fixture.database.pool)
                .await?;
        assert_eq!(subscription_count, 0);
        fixture.cleanup().await
    }

    #[tokio::test]
    async fn later_exact_approval_identifies_a_transactionless_fallback_charge()
    -> Result<(), Box<dyn Error>> {
        let fixture = application_fixture("identify_charge", false, false).await?;
        sqlx::raw_sql(
            r#"
            CREATE FUNCTION host_reject_attempt_status_update()
            RETURNS trigger LANGUAGE plpgsql AS $$
            BEGIN
                IF NEW.status IS DISTINCT FROM OLD.status THEN
                    RAISE EXCEPTION 'injected attempt status write failure';
                END IF;
                RETURN NEW;
            END
            $$;
            CREATE TRIGGER host_reject_attempt_status_update
            BEFORE UPDATE OF status ON billing_payment_attempts
            FOR EACH ROW EXECUTE FUNCTION host_reject_attempt_status_update();
            "#,
        )
        .execute(&fixture.database.pool)
        .await?;
        let parked = apply_subscription_enrollment_gateway_outcome(
            &fixture.database.pool,
            &fixture.coordinator,
            &fixture.reservation,
            &approved_outcome_with_transaction(None),
        )
        .await?;
        assert_eq!(parked.attempt().status(), PaymentAttemptStatus::Pending);
        let before: Option<String> =
            sqlx::query_scalar("SELECT gateway_transaction_id FROM billing_processor_charges")
                .fetch_one(&fixture.database.pool)
                .await?;
        assert!(before.is_none());

        sqlx::query("DROP TRIGGER host_reject_attempt_status_update ON billing_payment_attempts")
            .execute(&fixture.database.pool)
            .await?;
        let exact = approved_outcome("txn_identified_later");
        let applied = apply_subscription_enrollment_gateway_outcome(
            &fixture.database.pool,
            &fixture.coordinator,
            &fixture.reservation,
            &exact,
        )
        .await?;
        assert_eq!(applied.attempt().status(), PaymentAttemptStatus::Approved);
        let identified: (String, String) = sqlx::query_as(
            "SELECT gateway_transaction_id, progression_state FROM billing_processor_charges",
        )
        .fetch_one(&fixture.database.pool)
        .await?;
        assert_eq!(
            identified,
            ("txn_identified_later".to_owned(), "applied".to_owned())
        );
        fixture.cleanup().await
    }

    #[tokio::test]
    async fn approval_after_terminal_failure_is_parked_with_reversal_required_charge()
    -> Result<(), Box<dyn Error>> {
        let fixture = application_fixture("terminal_race", false, false).await?;
        sqlx::query(
            r#"
            UPDATE billing_payment_attempts
            SET status = 'failed', resolved_at = clock_timestamp(),
                gateway_response_text = 'local failure', updated_at = clock_timestamp()
            WHERE id = $1
            "#,
        )
        .bind(fixture.reservation.identity().attempt_id().as_uuid())
        .execute(&fixture.database.pool)
        .await?;
        let result = apply_subscription_enrollment_gateway_outcome(
            &fixture.database.pool,
            &fixture.coordinator,
            &fixture.reservation,
            &approved_outcome("txn_terminal_race"),
        )
        .await?;
        assert_eq!(
            result.attempt().status(),
            PaymentAttemptStatus::ReviewRequired
        );
        assert!(result.subscription().is_none());
        let progression: String =
            sqlx::query_scalar("SELECT progression_state FROM billing_processor_charges")
                .fetch_one(&fixture.database.pool)
                .await?;
        assert_eq!(progression, "external_reversal_required");
        assert!(fixture.coordinator.events.lock().await.is_empty());
        fixture.cleanup().await
    }
}
