use std::{fmt, time::Duration};

use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool, Row};
use syrup_rail::{
    BillingEvent, BillingScopeId, GatewayDiagnostic, GatewayNotSubmittedError, GatewayOrderId,
    GatewayProviderKey, PaymentAttempt, PaymentAttemptIdentity, PaymentAttemptKind,
    PaymentAttemptRequest, PaymentAttemptStatus, PaymentMethodId, PaymentResolutionCode, PlanKey,
    ProcessorChargeProgression, ProcessorEvidence, SubscriberId, Subscription,
    SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentReservation, SubscriptionId,
    SubscriptionPaymentMethodReplacement, SubscriptionRecoveryReservation,
    SubscriptionRenewalReservation,
};
use thiserror::Error;
use uuid::Uuid;

use crate::{
    BillingTransaction, BillingTransactionError,
    attempts::{
        AttemptApproval, AttemptResolutionStatus, AttemptTransition, PaymentAttemptStoreError,
        find_payment_attempt_by_id_on_connection, lock_payment_attempt_by_id_on_connection,
        persist_attempt_transition,
    },
    processor_charges::{
        LockFreeApprovedEvidenceOutcome, LockFreeApprovedEvidenceTerms, observe_processor_charge,
    },
    renewal_failure::RenewalFailureStoreError,
    subscription_persistence::{
        SubscriptionPersistenceCodecError, subscription_from_row as decode_subscription_row,
    },
};

mod initial;
mod payment_method_replacement;
mod recovery;
mod renewal;

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

async fn renewal_subscription_matches(
    connection: &mut PgConnection,
    reservation: &SubscriptionRenewalReservation,
) -> Result<bool, SubscriptionEnrollmentApplicationError> {
    let identity = reservation.identity();
    let expected = reservation.expected_state();
    let row = sqlx::query(
        r#"
        SELECT status, payment_method_id, initial_transaction_id,
            amount_cents, currency, next_renewal_at
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

#[derive(Clone, Copy)]
pub(crate) enum RateLimitCooldown {
    Account,
    Provider,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReservationOperation {
    Initial,
    Recovery,
    Renewal,
    PaymentMethodReplacement,
}

impl ReservationOperation {
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
}

/// A closed, secret-free view of the durable terms used while applying a
/// provider outcome. It keeps each operation's matching rule explicit while
/// sharing only the common locking and cooldown mechanics.
#[derive(Clone, Copy)]
enum OutcomeReservation<'a> {
    Initial(&'a SubscriptionEnrollmentReservation),
    Recovery(&'a SubscriptionRecoveryReservation),
    Renewal(&'a SubscriptionRenewalReservation),
    PaymentMethodReplacement(&'a SubscriptionPaymentMethodReplacement),
}

impl<'a> OutcomeReservation<'a> {
    const fn operation(self) -> ReservationOperation {
        match self {
            Self::Initial(_) => ReservationOperation::Initial,
            Self::Recovery(_) => ReservationOperation::Recovery,
            Self::Renewal(_) => ReservationOperation::Renewal,
            Self::PaymentMethodReplacement(_) => ReservationOperation::PaymentMethodReplacement,
        }
    }

    const fn identity(self) -> PaymentAttemptIdentity {
        match self {
            Self::Initial(reservation) => reservation.identity(),
            Self::Recovery(reservation) => reservation.identity(),
            Self::Renewal(reservation) => reservation.identity(),
            Self::PaymentMethodReplacement(reservation) => reservation.identity(),
        }
    }

    const fn plan_key(self) -> &'a PlanKey {
        match self {
            Self::Initial(reservation) => reservation.plan_key(),
            Self::Recovery(reservation) => reservation.plan_key(),
            Self::Renewal(reservation) => reservation.plan_key(),
            Self::PaymentMethodReplacement(reservation) => reservation.plan_key(),
        }
    }

    const fn provider_key(self) -> &'a GatewayProviderKey {
        match self {
            Self::Initial(reservation) => reservation.provider_key(),
            Self::Recovery(reservation) => reservation.provider_key(),
            Self::Renewal(reservation) => reservation.provider_key(),
            Self::PaymentMethodReplacement(reservation) => reservation.provider_key(),
        }
    }

    const fn expected_kind(self) -> PaymentAttemptKind {
        self.operation().expected_kind()
    }

    fn expected_attempt(self) -> ReservationAttemptExpectation<'a> {
        match self {
            Self::Initial(reservation) => ReservationAttemptExpectation::Initial {
                identity: reservation.identity(),
                plan_key: reservation.plan_key(),
                gateway_order_id: reservation.gateway_order_id(),
            },
            Self::Recovery(reservation) => ReservationAttemptExpectation::Exact {
                identity: reservation.identity(),
                kind: self.expected_kind(),
                request: reservation.request(),
            },
            Self::Renewal(reservation) => ReservationAttemptExpectation::Exact {
                identity: reservation.identity(),
                kind: self.expected_kind(),
                request: reservation.request(),
            },
            Self::PaymentMethodReplacement(reservation) => ReservationAttemptExpectation::Exact {
                identity: reservation.identity(),
                kind: self.expected_kind(),
                request: reservation.request(),
            },
        }
    }

    fn matches_attempt(self, attempt: &PaymentAttempt) -> bool {
        self.expected_attempt().matches(attempt)
    }
}

/// Initial enrollment preserves its historical application match: identity,
/// kind, plan, and gateway order. Every later operation validates its entire
/// request exactly.
enum ReservationAttemptExpectation<'a> {
    Initial {
        identity: PaymentAttemptIdentity,
        plan_key: &'a PlanKey,
        gateway_order_id: &'a GatewayOrderId,
    },
    Exact {
        identity: PaymentAttemptIdentity,
        kind: PaymentAttemptKind,
        request: &'a PaymentAttemptRequest,
    },
}

impl ReservationAttemptExpectation<'_> {
    const fn expected_kind(&self) -> PaymentAttemptKind {
        match self {
            Self::Initial { .. } => PaymentAttemptKind::SubscriptionInitial,
            Self::Exact { kind, .. } => *kind,
        }
    }

    fn matches(&self, attempt: &PaymentAttempt) -> bool {
        match self {
            Self::Initial {
                identity,
                plan_key,
                gateway_order_id,
            } => {
                attempt.identity() == *identity
                    && attempt.kind() == self.expected_kind()
                    && attempt.request().target().plan_key() == Some(*plan_key)
                    && attempt.request().gateway_order_id() == *gateway_order_id
            }
            Self::Exact {
                identity,
                kind,
                request,
            } => {
                attempt.identity() == *identity
                    && attempt.kind() == *kind
                    && attempt.request() == *request
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OutcomeResolutionKind {
    NonApproved,
    Unknown,
}

/// Typed durable resolution inputs. This separates a provider outcome's
/// status/code/cooldown/boundary from the mechanics that persist it.
#[derive(Clone, Copy)]
struct OutcomeResolutionCommand {
    kind: OutcomeResolutionKind,
    status: AttemptResolutionStatus,
    resolution_code: Option<PaymentResolutionCode>,
    cooldown: Option<RateLimitCooldown>,
    boundary: OutcomeResolutionBoundary,
}

impl OutcomeResolutionCommand {
    const fn non_approved(
        status: AttemptResolutionStatus,
        resolution_code: Option<PaymentResolutionCode>,
        cooldown: Option<RateLimitCooldown>,
        boundary: OutcomeResolutionBoundary,
    ) -> Self {
        Self {
            kind: OutcomeResolutionKind::NonApproved,
            status,
            resolution_code,
            cooldown,
            boundary,
        }
    }

    const fn unknown(cooldown: Option<RateLimitCooldown>) -> Self {
        Self {
            kind: OutcomeResolutionKind::Unknown,
            status: AttemptResolutionStatus::Unknown,
            resolution_code: None,
            cooldown,
            boundary: OutcomeResolutionBoundary::Submitted,
        }
    }

    const fn may_resolve(self, status: PaymentAttemptStatus, submitted: bool) -> bool {
        status.is_resolvable()
            && match self.boundary {
                OutcomeResolutionBoundary::Prepared => !submitted,
                OutcomeResolutionBoundary::AdmittedNotSubmitted => submitted,
                OutcomeResolutionBoundary::Submitted => true,
            }
    }

    fn resolved_status(
        self,
        operation: ReservationOperation,
        current: PaymentAttemptStatus,
    ) -> AttemptResolutionStatus {
        if self.kind == OutcomeResolutionKind::Unknown
            && operation.preserves_review_required_for_unknown()
            && current == PaymentAttemptStatus::ReviewRequired
        {
            AttemptResolutionStatus::ReviewRequired
        } else {
            self.status
        }
    }

    fn clears_submitted_at(self) -> bool {
        self.boundary == OutcomeResolutionBoundary::AdmittedNotSubmitted
    }

    fn records_pending_evidence(self, status: AttemptResolutionStatus) -> bool {
        self.kind == OutcomeResolutionKind::Unknown
            && status != AttemptResolutionStatus::ReviewRequired
    }

    fn marks_renewal_past_due(self, status: AttemptResolutionStatus) -> bool {
        self.boundary == OutcomeResolutionBoundary::Submitted
            && matches!(
                status,
                AttemptResolutionStatus::Declined | AttemptResolutionStatus::Failed
            )
    }
}

async fn resolve_pool_outcome(
    pool: &PgPool,
    reservation: OutcomeReservation<'_>,
    evidence: &ProcessorEvidence,
    resolution: OutcomeResolutionCommand,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    let identity = reservation.identity();
    let mut transaction = pool.begin().await?;
    set_application_timeouts(&mut transaction).await?;
    lock_subscription_aggregate(
        &mut transaction,
        identity.subscriber_id(),
        reservation.plan_key(),
    )
    .await?;
    let attempt = lock_expected_reservation_attempt(&mut transaction, reservation).await?;
    if resolution.may_resolve(
        attempt.status(),
        attempt.state().timestamps().submitted_at().is_some(),
    ) {
        let status = resolution.resolved_status(reservation.operation(), attempt.status());
        persist_attempt_transition(
            &mut transaction,
            &attempt,
            evidence,
            AttemptTransition::Resolved {
                status,
                resolution_code: resolution.resolution_code,
            },
        )
        .await
        .map_err(map_attempt_transition_error)?;
        if resolution.clears_submitted_at() {
            clear_attempt_submission(&mut transaction, &attempt).await?;
        }
        if resolution.records_pending_evidence(status) && evidence_looks_approved(evidence) {
            observe_processor_charge(
                &mut transaction,
                &attempt,
                evidence,
                ProcessorChargeProgression::Pending,
            )
            .await?;
        }
    }
    if let Some(cooldown) = resolution.cooldown {
        extend_rate_limit_cooldown(&mut transaction, reservation, cooldown).await?;
    }
    let result = payment_result_for_reservation_attempt(&mut transaction, reservation).await?;
    transaction.commit().await?;
    Ok(result)
}

async fn clear_attempt_submission(
    connection: &mut PgConnection,
    attempt: &PaymentAttempt,
) -> Result<(), SubscriptionEnrollmentApplicationError> {
    sqlx::query(
        "UPDATE billing_payment_attempts SET submitted_at = NULL, updated_at = clock_timestamp() WHERE id = $1",
    )
    .bind(attempt.identity().attempt_id().as_uuid())
    .execute(connection)
    .await?;
    Ok(())
}

async fn payment_result_for_reservation_attempt(
    connection: &mut PgConnection,
    reservation: OutcomeReservation<'_>,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    let identity = reservation.identity();
    let attempt = find_payment_attempt_by_id_on_connection(
        connection,
        identity.billing_scope_id(),
        identity.attempt_id(),
    )
    .await?
    .ok_or(SubscriptionEnrollmentApplicationError::InvalidState(
        INVALID_APPLICATION_STATE,
    ))?;
    payment_result_for_attempt(connection, attempt).await
}

async fn extend_rate_limit_cooldown(
    connection: &mut PgConnection,
    reservation: OutcomeReservation<'_>,
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
                    )
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

pub(crate) fn mutation_error_evidence(detail: &GatewayDiagnostic) -> ProcessorEvidence {
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

async fn persist_approved_evidence_without_attempt_lock(
    pool: &PgPool,
    terms: LockFreeApprovedEvidenceTerms<'_>,
    evidence: &ProcessorEvidence,
) -> Result<(), SubscriptionEnrollmentApplicationError> {
    match crate::processor_charges::persist_approved_evidence_without_attempt_lock(
        pool, terms, evidence,
    )
    .await?
    {
        LockFreeApprovedEvidenceOutcome::Persisted
        | LockFreeApprovedEvidenceOutcome::ExactReplay
        | LockFreeApprovedEvidenceOutcome::OwnedByOtherAttempt => Ok(()),
        LockFreeApprovedEvidenceOutcome::NotDurable => {
            Err(SubscriptionEnrollmentApplicationError::ApprovedEvidenceNotDurable)
        }
    }
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

pub(crate) async fn set_application_timeouts(
    connection: &mut PgConnection,
) -> Result<(), sqlx::Error> {
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

async fn mark_attempt_approved(
    connection: &mut PgConnection,
    attempt: &PaymentAttempt,
    evidence: &ProcessorEvidence,
    subscription_id: SubscriptionId,
    method_id: PaymentMethodId,
) -> Result<(), SubscriptionEnrollmentApplicationError> {
    persist_attempt_transition(
        connection,
        attempt,
        evidence,
        AttemptTransition::Approved(AttemptApproval::Subscription {
            subscription_id,
            payment_method_id: method_id,
        }),
    )
    .await
    .map_err(map_attempt_transition_error)
}

pub(crate) async fn park_locked_attempt(
    connection: &mut PgConnection,
    attempt: &PaymentAttempt,
    evidence: &ProcessorEvidence,
    resolution_code: Option<PaymentResolutionCode>,
    message: &'static str,
) -> Result<PaymentAttempt, SubscriptionEnrollmentApplicationError> {
    persist_attempt_transition(
        connection,
        attempt,
        evidence,
        AttemptTransition::LateApprovalReview {
            resolution_code,
            message,
        },
    )
    .await
    .map_err(map_attempt_transition_error)?;
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
    let subscription = if attempt.status() == PaymentAttemptStatus::Approved {
        load_applied_subscription(connection, &attempt).await?
    } else {
        None
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
            current_period_start_at, current_period_end_at, next_renewal_at,
            phase, recurring_period_kind, recurring_period_count,
            dunning_retry_delays_seconds, dunning_exhaustion, past_due_access,
            next_payment_attempt_at
        FROM billing_subscriptions
        WHERE billing_scope_id = $1 AND id = $2
        "#,
    )
    .bind(billing_scope_id.as_uuid())
    .bind(subscription_id.as_uuid())
    .fetch_optional(connection)
    .await?;
    row.map(|row| decode_subscription_row(&row).map_err(map_subscription_persistence_error))
        .transpose()
}

#[cfg(test)]
mod tests;
