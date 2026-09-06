use std::{fmt, time::Duration};

use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool, Row};
use syrup_rail::{
    ApprovedProcessorEvidence, BillingEvent, BillingScopeId, GatewayAccountMode, GatewayDiagnostic,
    GatewayError, GatewayNotSubmittedError, GatewayOrderId, GatewayPaymentDiagnostic,
    GatewayProviderKey, PaymentAttempt, PaymentAttemptIdentity, PaymentAttemptKind,
    PaymentAttemptRequest, PaymentAttemptStatus, PaymentMethodId, PaymentResolutionCode, PlanKey,
    ProcessorChargeProgression, ProcessorEvidence, Subscription,
    SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentPaymentResultBuildError,
    SubscriptionEnrollmentReservation, SubscriptionId, SubscriptionPaymentMethodReplacement,
    SubscriptionRecoveryReservation, SubscriptionRenewalReservation,
};
use thiserror::Error;
use uuid::Uuid;

use crate::{
    BillingTransaction, BillingTransactionCoordinator, BillingTransactionError,
    advisory_locks::lock_payment_method_domain,
    attempts::{
        AttemptApproval, AttemptResolutionStatus, AttemptTransition, PaymentAttemptStoreError,
        find_payment_attempt_by_id_on_connection, lock_payment_attempt_by_id_on_connection,
        lock_subscription_aggregate, persist_attempt_transition,
    },
    processor_charges::{
        LockFreeApprovedEvidenceOutcome, LockFreeApprovedEvidenceTerms, observe_processor_charge,
        promote_conflicting_charge_to_external_reversal,
    },
    renewal_failure::RenewalFailureStoreError,
    subscription_persistence::{
        SubscriptionPersistenceCodecError, subscription_from_row as decode_subscription_row,
    },
};

mod initial;
mod outcome_support;
mod payment_method_replacement;
mod recovery;
mod renewal;

pub(crate) use outcome_support::*;

pub(crate) use initial::resolve_non_approved_outcome;
pub use initial::{
    AdmittedSubscriptionEnrollment, SubscriptionEnrollmentAdmissionOutcome,
    SubscriptionEnrollmentProviderResult, admit_subscription_enrollment_submission,
    apply_reconciled_subscription_enrollment_gateway_outcome,
    apply_subscription_enrollment_gateway_outcome, submit_admitted_subscription_enrollment,
};
pub(crate) use payment_method_replacement::resolve_payment_method_replacement_non_approved_outcome;
pub use payment_method_replacement::{
    AdmittedSubscriptionPaymentMethodReplacement,
    SubscriptionPaymentMethodReplacementAdmissionOutcome,
    SubscriptionPaymentMethodReplacementProviderResult,
    admit_subscription_payment_method_replacement,
    apply_reconciled_subscription_payment_method_replacement_gateway_outcome,
    apply_subscription_payment_method_replacement_gateway_outcome,
    submit_admitted_subscription_payment_method_replacement,
};
pub(crate) use recovery::resolve_recovery_non_approved_outcome;
pub use recovery::{
    AdmittedSubscriptionRecovery, SubscriptionRecoveryAdmissionOutcome,
    SubscriptionRecoveryProviderResult, admit_subscription_recovery_submission,
    apply_reconciled_subscription_recovery_gateway_outcome,
    apply_subscription_recovery_gateway_outcome, submit_admitted_subscription_recovery,
};
pub(crate) use renewal::resolve_renewal_non_approved_outcome;
pub use renewal::{
    AdmittedSubscriptionRenewal, SubscriptionRenewalAdmissionOutcome,
    SubscriptionRenewalProviderResult, admit_subscription_renewal_submission,
    apply_reconciled_subscription_renewal_gateway_outcome,
    apply_subscription_renewal_gateway_outcome, submit_admitted_subscription_renewal,
};

const BILLING_LOCK_TIMEOUT: Duration = Duration::from_millis(250);
const BILLING_ROW_LOCK_TIMEOUT: &str = "250ms";
const BILLING_OPERATION_TIMEOUT: &str = "5s";
const APPROVED_EVIDENCE_WRITE_ATTEMPTS: usize = 3;
const APPROVED_EVIDENCE_RETRY_DELAY: Duration = Duration::from_millis(50);
const APPROVED_APPLICATION_ATTEMPTS: usize = 3;
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
const RENEWAL_INCOMPLETE_APPROVAL_TEXT: &str =
    "Approved subscription renewal is missing required processor identity.";
const RENEWAL_APPROVED_STORAGE_FAILURE_TEXT: &str =
    "Approved subscription renewal could not be applied; manual review is required.";
const RENEWAL_STALE_STATE_TEXT: &str = "Approved subscription renewal could not update billing state because the subscription changed.";
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

impl From<crate::processor_charges::ProcessorChargeStoreError>
    for SubscriptionEnrollmentApplicationError
{
    fn from(error: crate::processor_charges::ProcessorChargeStoreError) -> Self {
        match error {
            crate::processor_charges::ProcessorChargeStoreError::Sql(error) => Self::Sql(error),
            crate::processor_charges::ProcessorChargeStoreError::Attempt(error) => {
                Self::Attempt(error)
            }
            crate::processor_charges::ProcessorChargeStoreError::InvalidState(message) => {
                Self::InvalidState(message)
            }
        }
    }
}

impl From<RenewalFailureStoreError> for SubscriptionEnrollmentApplicationError {
    fn from(error: RenewalFailureStoreError) -> Self {
        match error {
            RenewalFailureStoreError::Sql(error) => Self::Sql(error),
            RenewalFailureStoreError::Attempt(error) => Self::Attempt(error),
            RenewalFailureStoreError::InvalidState(message) => Self::InvalidState(message),
        }
    }
}

impl From<SubscriptionEnrollmentPaymentResultBuildError>
    for SubscriptionEnrollmentApplicationError
{
    fn from(_: SubscriptionEnrollmentPaymentResultBuildError) -> Self {
        Self::InvalidState(INVALID_APPLICATION_STATE)
    }
}

fn map_subscription_persistence_error(
    error: SubscriptionPersistenceCodecError,
) -> SubscriptionEnrollmentApplicationError {
    match error {
        SubscriptionPersistenceCodecError::RowRead(error) => {
            SubscriptionEnrollmentApplicationError::Sql(error)
        }
        SubscriptionPersistenceCodecError::InvalidState => {
            SubscriptionEnrollmentApplicationError::InvalidState(INVALID_APPLICATION_STATE)
        }
    }
}

pub(crate) fn map_attempt_transition_error(
    error: PaymentAttemptStoreError,
) -> SubscriptionEnrollmentApplicationError {
    match error {
        PaymentAttemptStoreError::Sql(error) => SubscriptionEnrollmentApplicationError::Sql(error),
        PaymentAttemptStoreError::InvalidState(_) => {
            SubscriptionEnrollmentApplicationError::InvalidState(INVALID_APPLICATION_STATE)
        }
    }
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

async fn finalize_approved_application(
    mut transaction: Box<dyn BillingTransaction>,
    application: Result<
        (SubscriptionEnrollmentPaymentResult, Option<BillingEvent>),
        SubscriptionEnrollmentApplicationError,
    >,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
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

async fn lock_expected_reservation_attempt(
    connection: &mut PgConnection,
    reservation: OutcomeReservation<'_>,
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
    if !reservation.matches_attempt(&attempt) {
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
        SELECT status, payment_method_id, initial_transaction_id, next_renewal_at,
            required_gateway_account_mode
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
        && row.try_get::<String, _>("required_gateway_account_mode")?
            == identity.required_gateway_account_mode().as_str()
        && next_renewal_at == *reservation.period().start_at())
}

async fn renewal_subscription_matches(
    connection: &mut PgConnection,
    reservation: &SubscriptionRenewalReservation,
) -> Result<bool, SubscriptionEnrollmentApplicationError> {
    let identity = reservation.identity();
    let expected = reservation.expected_state();
    let row = sqlx::query(
        r#"
        SELECT status, payment_method_id, initial_transaction_id,
            amount_cents, currency, next_renewal_at, required_gateway_account_mode
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
    let initial_transaction_id: String = row.try_get("initial_transaction_id")?;
    Ok(status == expected.status().as_str()
        && matches!(status.as_str(), "active" | "past_due")
        && row.try_get::<Uuid, _>("payment_method_id")? == expected.payment_method_id().into_uuid()
        && syrup_rail::canonical_gateway_transaction_ids_equal(
            &initial_transaction_id,
            expected.initial_transaction_id().expose(),
        )
        && row.try_get::<i32, _>("amount_cents")? == reservation.request().amount().cents()
        && row.try_get::<String, _>("currency")?
            == reservation.request().amount().currency().as_str()
        && row.try_get::<String, _>("required_gateway_account_mode")?
            == identity.required_gateway_account_mode().as_str()
        && row.try_get::<DateTime<Utc>, _>("next_renewal_at")? == *reservation.period().start_at())
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
                    AND subscriptions.status IN ('active', 'past_due')
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
    if status == "completed" {
        return Ok(());
    }
    let periods_applied: i32 = row.try_get("periods_applied")?;
    if duration == "indefinite" {
        match periods_applied {
            0 => {
                sqlx::query(
                    r#"
                    UPDATE billing_subscription_discounts
                    SET periods_applied = 1
                    WHERE subscription_id = $1 AND plan_key = $2
                        AND status = 'active' AND periods_applied = 0
                    "#,
                )
                .bind(subscription_id.as_uuid())
                .bind(plan_key.as_str())
                .execute(&mut *connection)
                .await?;
                return Ok(());
            }
            1 => return Ok(()),
            _ => {
                return Err(SubscriptionEnrollmentApplicationError::InvalidState(
                    INVALID_APPLICATION_STATE,
                ));
            }
        }
    }
    if duration != "limited_months" {
        return Err(SubscriptionEnrollmentApplicationError::InvalidState(
            INVALID_APPLICATION_STATE,
        ));
    }
    let periods_total: i32 = row.try_get("periods_total")?;
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OutcomeResolutionBoundary {
    Prepared,
    AdmittedNotSubmitted,
    Submitted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RateLimitCooldown {
    Account,
    Provider,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RateLimitCooldownPersistence {
    Applied,
    IdentityChanged,
    MissingProviderCooldown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RateLimitCooldownCommitDisposition {
    Applied,
    IdentityNotDurable,
    IdentityChanged,
    MissingProviderCooldown,
}

#[derive(Clone, Copy)]
pub(crate) enum RateLimitCooldownOperation {
    Subscription,
    HostCharge,
}

impl RateLimitCooldownOperation {
    const fn identity_not_durable_message(self) -> &'static str {
        match self {
            Self::Subscription => {
                "skipped cooldown for a gateway identity that is no longer authoritative"
            }
            Self::HostCharge => {
                "skipped host-charge cooldown for a gateway identity that is no longer authoritative"
            }
        }
    }

    const fn identity_changed_message(self) -> &'static str {
        match self {
            Self::Subscription => "skipped cooldown after the gateway account identity changed",
            Self::HostCharge => {
                "skipped host-charge cooldown after the gateway account identity changed"
            }
        }
    }

    const fn missing_provider_message(self) -> &'static str {
        match self {
            Self::Subscription => {
                "provider-scoped cooldown storage is missing; leaving the canonical attempt unresolved"
            }
            Self::HostCharge => {
                "provider-scoped cooldown storage is missing; leaving the canonical host-charge attempt unresolved"
            }
        }
    }
}

pub(crate) enum RateLimitCooldownCommitError {
    Sql(sqlx::Error),
    MissingProviderCooldown,
}

impl From<sqlx::Error> for RateLimitCooldownCommitError {
    fn from(error: sqlx::Error) -> Self {
        Self::Sql(error)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReservationOperation {
    Initial,
    Recovery,
    Renewal,
    PaymentMethodReplacement,
}

impl ReservationOperation {
    const fn from_kind(kind: PaymentAttemptKind) -> Option<Self> {
        match kind {
            PaymentAttemptKind::SubscriptionInitial => Some(Self::Initial),
            PaymentAttemptKind::SubscriptionRecovery => Some(Self::Recovery),
            PaymentAttemptKind::SubscriptionRenewal => Some(Self::Renewal),
            PaymentAttemptKind::SubscriptionPaymentMethodUpdate => {
                Some(Self::PaymentMethodReplacement)
            }
            PaymentAttemptKind::HostCharge => None,
        }
    }

    const fn expected_kind(self) -> PaymentAttemptKind {
        match self {
            Self::Initial => PaymentAttemptKind::SubscriptionInitial,
            Self::Recovery => PaymentAttemptKind::SubscriptionRecovery,
            Self::Renewal => PaymentAttemptKind::SubscriptionRenewal,
            Self::PaymentMethodReplacement => PaymentAttemptKind::SubscriptionPaymentMethodUpdate,
        }
    }

    const fn preserves_review_required_for_unknown(self) -> bool {
        matches!(self, Self::PaymentMethodReplacement)
    }

    const fn attempt_not_found_message(self) -> &'static str {
        match self {
            Self::Initial => "subscription enrollment attempt was not found",
            Self::Recovery => "subscription recovery attempt was not found",
            Self::Renewal => "subscription renewal attempt was not found",
            Self::PaymentMethodReplacement => "payment method replacement attempt was not found",
        }
    }

    const fn gateway_account_not_found_message(self) -> &'static str {
        match self {
            Self::Initial => "subscription enrollment gateway account was not found",
            Self::Recovery => "subscription recovery gateway account was not found",
            Self::Renewal => "subscription renewal gateway account was not found",
            Self::PaymentMethodReplacement => {
                "payment method replacement gateway account was not found"
            }
        }
    }

    const fn invalid_provider_message(self) -> &'static str {
        match self {
            Self::Initial => "subscription enrollment gateway provider key is invalid",
            Self::Recovery => "subscription recovery gateway provider key is invalid",
            Self::Renewal => "subscription renewal gateway provider key is invalid",
            Self::PaymentMethodReplacement => {
                "payment method replacement gateway provider key is invalid"
            }
        }
    }

    const fn invalid_attempt_message(self) -> &'static str {
        match self {
            Self::Initial => "reconciled attempt is not a valid subscription enrollment",
            Self::Recovery => "reconciled attempt is not a valid subscription recovery",
            Self::Renewal => "reconciled attempt is not a valid subscription renewal",
            Self::PaymentMethodReplacement => {
                "reconciled attempt is not a valid payment method replacement"
            }
        }
    }
}

#[derive(Clone, Copy)]
enum ReconciledApplicationEntry {
    SubscriptionBillingService,
    Exact(ReservationOperation),
}

pub(crate) const RECONCILED_SUBSCRIPTION_PAYMENT_ATTEMPT_NOT_FOUND: &str =
    "reconciled subscription payment attempt was not found";

impl ReconciledApplicationEntry {
    const fn attempt_not_found_message(self) -> &'static str {
        match self {
            Self::SubscriptionBillingService => RECONCILED_SUBSCRIPTION_PAYMENT_ATTEMPT_NOT_FOUND,
            Self::Exact(operation) => operation.attempt_not_found_message(),
        }
    }

    const fn operation_for_attempt(self, kind: PaymentAttemptKind) -> Option<ReservationOperation> {
        match self {
            Self::SubscriptionBillingService => ReservationOperation::from_kind(kind),
            Self::Exact(operation) => Some(operation),
        }
    }
}

include!("enrollment_application/outcome_pipeline.rs");
include!("enrollment_application/outcome_persistence.rs");
