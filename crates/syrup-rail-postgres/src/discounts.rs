use std::fmt;

use async_trait::async_trait;
use sqlx::{PgConnection, PgPool, Postgres, Row, Transaction, postgres::PgRow};
use syrup_rail::{
    BillingScopeId, ChargeAmount, CurrencyCode, DiscountClaimId, DiscountCodeId, IdempotencyKey,
    LimitedDiscountMonths, PaymentAttemptId, PercentOffBasisPoints, PlanKey, PositiveDiscountCents,
    SubscriberId, SubscriptionDiscountClaim, SubscriptionDiscountClaimOutcome,
    SubscriptionDiscountClaimRecord, SubscriptionDiscountClaimState,
    SubscriptionDiscountClaimStatus, SubscriptionDiscountClearOutcome, SubscriptionDiscountCode,
    SubscriptionDiscountCodeCreation, SubscriptionDiscountCodeQuote,
    SubscriptionDiscountCodeRecord, SubscriptionDiscountCodeStatus, SubscriptionDiscountCodeUpdate,
    SubscriptionDiscountDuration, SubscriptionDiscountError, SubscriptionDiscountKind,
    SubscriptionDiscountSnapshot, SubscriptionEnrollmentReservation, SubscriptionId,
    SubscriptionOffer,
};
use thiserror::Error;
use uuid::Uuid;

use crate::attempts::lock_subscription_aggregate;
pub use persistence::saved_subscription_discount_claim_in_transaction;
use persistence::{
    blocking_initial_attempt, blocking_initial_attempt_exists, claim_from_row, code_by_id,
    code_from_row, current_subscription_exists, discount_value, duration_months,
    expire_saved_claims_for_code, find_active_code, lock_initial_attempt_rows,
    lock_initial_attempts, lock_offer, quote_for_offer, quote_from_row,
    saved_subscription_discount_claim_on_connection, set_lock_timeout, validate_discount_cadence,
};

mod persistence;

const BILLING_ROW_LOCK_TIMEOUT: &str = "250ms";
const BILLING_OPERATION_TIMEOUT: &str = "5s";
const INVALID_DISCOUNT_STATE: &str = "canonical subscription discount state is invalid";

/// The lifecycle boundary at which enrollment terms are locked.
///
/// A single enrollment reservation crosses both stages. The stage describes
/// why the host is being consulted; it does not create a new eligibility
/// subject or attempt identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubscriptionEnrollmentOfferStage {
    /// The durable initial payment attempt is about to be reserved.
    Reservation,
    /// The prepared attempt is being revalidated immediately before provider I/O.
    SubmissionAdmission,
}

/// Stable identity and lifecycle context for locking an enrollment offer.
///
/// The PostgreSQL enrollment pipeline constructs this value from one
/// [`SubscriptionEnrollmentReservation`] and passes the same scope,
/// subscriber, plan, attempt, and idempotency identity at both stages. A host
/// implementation that derives eligibility from payment-attempt history must
/// exclude [`Self::attempt_id`] so the in-flight reservation cannot disqualify
/// itself during submission admission.
#[derive(Clone, Copy)]
pub struct SubscriptionEnrollmentOfferContext<'a> {
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    plan_key: &'a PlanKey,
    attempt_id: PaymentAttemptId,
    idempotency_key: &'a IdempotencyKey,
    stage: SubscriptionEnrollmentOfferStage,
}

impl<'a> SubscriptionEnrollmentOfferContext<'a> {
    pub(crate) const fn from_reservation(
        reservation: &'a SubscriptionEnrollmentReservation,
        stage: SubscriptionEnrollmentOfferStage,
    ) -> Self {
        let identity = reservation.identity();
        Self {
            billing_scope_id: identity.billing_scope_id(),
            subscriber_id: identity.subscriber_id(),
            plan_key: reservation.plan_key(),
            attempt_id: identity.attempt_id(),
            idempotency_key: reservation.idempotency_key(),
            stage,
        }
    }

    pub const fn billing_scope_id(self) -> BillingScopeId {
        self.billing_scope_id
    }

    pub const fn subscriber_id(self) -> SubscriberId {
        self.subscriber_id
    }

    pub const fn plan_key(self) -> &'a PlanKey {
        self.plan_key
    }

    pub const fn attempt_id(self) -> PaymentAttemptId {
        self.attempt_id
    }

    pub const fn idempotency_key(self) -> &'a IdempotencyKey {
        self.idempotency_key
    }

    pub const fn stage(self) -> SubscriptionEnrollmentOfferStage {
        self.stage
    }
}

impl fmt::Debug for SubscriptionEnrollmentOfferContext<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SubscriptionEnrollmentOfferContext")
            .field("billing_scope_id", &self.billing_scope_id)
            .field("subscriber_id", &self.subscriber_id)
            .field("plan_key", &self.plan_key)
            .field("attempt_id", &self.attempt_id)
            .field("has_idempotency_key", &true)
            .field("stage", &self.stage)
            .finish()
    }
}

#[derive(Error)]
pub enum SubscriptionDiscountOperationError {
    #[error("subscription discount storage operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("current subscription offer is unavailable")]
    OfferUnavailable,
    #[error("current subscription offer does not match the requested plan")]
    OfferPlanMismatch,
    #[error("subscription discount configuration is invalid for the current offer")]
    InvalidConfiguration,
    #[error("limited-month discounts require a one-calendar-month recurring period")]
    LimitedDiscountCadence,
    #[error("{0}")]
    InvalidState(&'static str),
}

impl SubscriptionDiscountOperationError {
    pub fn is_unique_violation(&self) -> bool {
        matches!(self, Self::Sql(sqlx::Error::Database(error)) if error.is_unique_violation())
    }

    pub fn constraint(&self) -> Option<&str> {
        match self {
            Self::Sql(sqlx::Error::Database(error)) => error.constraint(),
            _ => None,
        }
    }
}

impl fmt::Debug for SubscriptionDiscountOperationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sql(_) => formatter.write_str("SubscriptionDiscountOperationError::Sql"),
            Self::OfferUnavailable => {
                formatter.write_str("SubscriptionDiscountOperationError::OfferUnavailable")
            }
            Self::OfferPlanMismatch => {
                formatter.write_str("SubscriptionDiscountOperationError::OfferPlanMismatch")
            }
            Self::InvalidConfiguration => {
                formatter.write_str("SubscriptionDiscountOperationError::InvalidConfiguration")
            }
            Self::LimitedDiscountCadence => {
                formatter.write_str("SubscriptionDiscountOperationError::LimitedDiscountCadence")
            }
            Self::InvalidState(detail) => formatter
                .debug_tuple("SubscriptionDiscountOperationError::InvalidState")
                .field(detail)
                .finish(),
        }
    }
}

#[async_trait]
pub trait SubscriptionOfferStore: Send + Sync {
    /// Locks the host-owned row for this exact scope and plan on `connection`.
    ///
    /// Implementations must not acquire another connection. The returned row
    /// must remain protected against host price updates until the caller's
    /// transaction ends.
    async fn lock_current_offer(
        &self,
        connection: &mut PgConnection,
        billing_scope_id: BillingScopeId,
        plan_key: &PlanKey,
    ) -> Result<Option<SubscriptionOffer>, sqlx::Error>;

    /// Locks the subscriber-aware offer used for enrollment admission.
    ///
    /// Overrides must lock any host trial-eligibility state on the supplied
    /// connection and must not acquire a second connection. Enrollment calls
    /// this hook at both [`SubscriptionEnrollmentOfferStage`] values for the
    /// same reservation. Attempt-history eligibility queries must exclude
    /// [`SubscriptionEnrollmentOfferContext::attempt_id`]; the result may
    /// change only because locked host policy or eligibility state external to
    /// that in-flight attempt changed. The subscriber/plan aggregate lock
    /// serializes reservation and admission before this callback. Implementations
    /// must not acquire Syrup Rail payment-attempt ledger locks independently;
    /// doing so would invert admission's aggregate -> attempt -> offer order.
    async fn lock_enrollment_offer(
        &self,
        connection: &mut PgConnection,
        context: SubscriptionEnrollmentOfferContext<'_>,
    ) -> Result<Option<SubscriptionOffer>, sqlx::Error> {
        self.lock_current_offer(connection, context.billing_scope_id(), context.plan_key())
            .await
    }
}

pub async fn list_subscription_discount_codes(
    pool: &PgPool,
    billing_scope_id: BillingScopeId,
    plan_key: &PlanKey,
) -> Result<Vec<SubscriptionDiscountCodeRecord>, SubscriptionDiscountOperationError> {
    let mut transaction = pool.begin().await?;
    let records = list_subscription_discount_codes_in_transaction(
        &mut transaction,
        billing_scope_id,
        plan_key,
    )
    .await?;
    transaction.commit().await?;
    Ok(records)
}

/// Lists durable administrative records without reinterpreting them against
/// the current offer. Quote eligibility is intentionally owned by
/// [`validate_subscription_discount_code_in_transaction`].
pub async fn list_subscription_discount_codes_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    billing_scope_id: BillingScopeId,
    plan_key: &PlanKey,
) -> Result<Vec<SubscriptionDiscountCodeRecord>, SubscriptionDiscountOperationError> {
    set_lock_timeout(transaction).await?;
    let rows = sqlx::query(
        r#"
        SELECT id, billing_scope_id, plan_key, code_normalized, display_code,
            label, status, discount_kind, amount_off_cents, percent_off_bps,
            currency, duration, duration_months, created_at, updated_at
        FROM billing_subscription_discount_codes
        WHERE billing_scope_id = $1 AND plan_key = $2
        ORDER BY status, code_normalized, created_at DESC
        "#,
    )
    .bind(billing_scope_id.as_uuid())
    .bind(plan_key.as_str())
    .fetch_all(&mut **transaction)
    .await?;
    rows.iter().map(code_from_row).collect()
}

pub async fn validate_subscription_discount_code(
    pool: &PgPool,
    offers: &dyn SubscriptionOfferStore,
    billing_scope_id: BillingScopeId,
    plan_key: &PlanKey,
    code: &SubscriptionDiscountCode,
) -> Result<Option<SubscriptionDiscountCodeQuote>, SubscriptionDiscountOperationError> {
    let mut transaction = pool.begin().await?;
    let quote = validate_subscription_discount_code_in_transaction(
        &mut transaction,
        offers,
        billing_scope_id,
        plan_key,
        code,
    )
    .await?;
    transaction.commit().await?;
    Ok(quote)
}

pub async fn validate_subscription_discount_code_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    offers: &dyn SubscriptionOfferStore,
    billing_scope_id: BillingScopeId,
    plan_key: &PlanKey,
    code: &SubscriptionDiscountCode,
) -> Result<Option<SubscriptionDiscountCodeQuote>, SubscriptionDiscountOperationError> {
    set_lock_timeout(transaction).await?;
    let offer = lock_offer(transaction, offers, billing_scope_id, plan_key).await?;
    let row = find_active_code(transaction, billing_scope_id, plan_key, code, false).await?;
    row.as_ref()
        .map(|row| quote_from_row(row, &offer))
        .transpose()
}

/// Creates an active durable code after validating its terms against the
/// locked current offer. The returned value is the administrative record; use
/// [`validate_subscription_discount_code_in_transaction`] to obtain a quote.
pub async fn create_subscription_discount_code_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    offers: &dyn SubscriptionOfferStore,
    creation: &SubscriptionDiscountCodeCreation,
) -> Result<SubscriptionDiscountCodeRecord, SubscriptionDiscountOperationError> {
    set_lock_timeout(transaction).await?;
    let offer = lock_offer(
        transaction,
        offers,
        creation.billing_scope_id(),
        creation.plan_key(),
    )
    .await?;
    validate_discount_cadence(creation.duration(), &offer)?;
    syrup_rail::discounted_charge(
        offer.recurring().charge(),
        creation.currency(),
        creation.kind(),
    )
    .map_err(|_| SubscriptionDiscountOperationError::InvalidConfiguration)?;
    let (amount_off_cents, percent_off_bps) = discount_value(creation.kind());
    let duration_months = duration_months(creation.duration());
    sqlx::query(
        r#"
        INSERT INTO billing_subscription_discount_codes (
            id, billing_scope_id, plan_key, code_normalized, display_code,
            label, status, discount_kind, amount_off_cents, percent_off_bps,
            currency, duration, duration_months
        )
        VALUES ($1, $2, $3, $4, $4, $5, 'active', $6, $7, $8, $9, $10, $11)
        "#,
    )
    .bind(creation.id().as_uuid())
    .bind(creation.billing_scope_id().as_uuid())
    .bind(creation.plan_key().as_str())
    .bind(creation.code().as_str())
    .bind(creation.label())
    .bind(creation.kind().as_str())
    .bind(amount_off_cents)
    .bind(percent_off_bps)
    .bind(creation.currency().as_str())
    .bind(creation.duration().as_str())
    .bind(duration_months)
    .execute(&mut **transaction)
    .await?;
    let row = code_by_id(
        transaction,
        creation.billing_scope_id(),
        creation.plan_key(),
        creation.id(),
    )
    .await?
    .ok_or(SubscriptionDiscountOperationError::InvalidState(
        INVALID_DISCOUNT_STATE,
    ))?;
    code_from_row(&row)
}

pub async fn create_subscription_discount_code(
    pool: &PgPool,
    offers: &dyn SubscriptionOfferStore,
    creation: &SubscriptionDiscountCodeCreation,
) -> Result<SubscriptionDiscountCodeRecord, SubscriptionDiscountOperationError> {
    let mut transaction = pool.begin().await?;
    let record =
        create_subscription_discount_code_in_transaction(&mut transaction, offers, creation)
            .await?;
    transaction.commit().await?;
    Ok(record)
}

/// Updates a durable administrative record. Active records are validated
/// against the locked current offer. Disabled records remain administrable
/// without requiring their historical terms to be quoteable today.
pub async fn update_subscription_discount_code_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    offers: &dyn SubscriptionOfferStore,
    update: &SubscriptionDiscountCodeUpdate,
) -> Result<Option<SubscriptionDiscountCodeRecord>, SubscriptionDiscountOperationError> {
    set_lock_timeout(transaction).await?;
    if update.status() == SubscriptionDiscountCodeStatus::Active {
        let offer = lock_offer(
            transaction,
            offers,
            update.billing_scope_id(),
            update.plan_key(),
        )
        .await?;
        validate_discount_cadence(update.duration(), &offer)?;
        syrup_rail::discounted_charge(offer.recurring().charge(), update.currency(), update.kind())
            .map_err(|_| SubscriptionDiscountOperationError::InvalidConfiguration)?;
    }
    let (amount_off_cents, percent_off_bps) = discount_value(update.kind());
    let row = sqlx::query(
        r#"
        UPDATE billing_subscription_discount_codes
        SET label = $4, status = $5, discount_kind = $6,
            amount_off_cents = $7, percent_off_bps = $8, currency = $9,
            duration = $10, duration_months = $11, updated_at = now()
        WHERE id = $1 AND billing_scope_id = $2 AND plan_key = $3
        RETURNING id, billing_scope_id, plan_key, code_normalized, display_code,
            label, status, discount_kind, amount_off_cents, percent_off_bps,
            currency, duration, duration_months, created_at, updated_at
        "#,
    )
    .bind(update.id().as_uuid())
    .bind(update.billing_scope_id().as_uuid())
    .bind(update.plan_key().as_str())
    .bind(update.label())
    .bind(update.status().as_str())
    .bind(update.kind().as_str())
    .bind(amount_off_cents)
    .bind(percent_off_bps)
    .bind(update.currency().as_str())
    .bind(update.duration().as_str())
    .bind(duration_months(update.duration()))
    .fetch_optional(&mut **transaction)
    .await?;
    if row.is_some() && update.status() == SubscriptionDiscountCodeStatus::Disabled {
        expire_saved_claims_for_code(
            transaction,
            update.billing_scope_id(),
            update.plan_key(),
            update.id(),
        )
        .await?;
    }
    row.as_ref().map(code_from_row).transpose()
}

pub async fn update_subscription_discount_code(
    pool: &PgPool,
    offers: &dyn SubscriptionOfferStore,
    update: &SubscriptionDiscountCodeUpdate,
) -> Result<Option<SubscriptionDiscountCodeRecord>, SubscriptionDiscountOperationError> {
    let mut transaction = pool.begin().await?;
    let record =
        update_subscription_discount_code_in_transaction(&mut transaction, offers, update).await?;
    transaction.commit().await?;
    Ok(record)
}

/// Disables an administrative record even when its historical terms are no
/// longer quoteable against the host's current offer.
pub async fn disable_subscription_discount_code_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    billing_scope_id: BillingScopeId,
    plan_key: &PlanKey,
    discount_code_id: DiscountCodeId,
) -> Result<Option<SubscriptionDiscountCodeRecord>, SubscriptionDiscountOperationError> {
    set_lock_timeout(transaction).await?;
    let row = sqlx::query(
        r#"
        UPDATE billing_subscription_discount_codes
        SET status = 'disabled', updated_at = now()
        WHERE id = $1 AND billing_scope_id = $2 AND plan_key = $3
        RETURNING id, billing_scope_id, plan_key, code_normalized, display_code,
            label, status, discount_kind, amount_off_cents, percent_off_bps,
            currency, duration, duration_months, created_at, updated_at
        "#,
    )
    .bind(discount_code_id.as_uuid())
    .bind(billing_scope_id.as_uuid())
    .bind(plan_key.as_str())
    .fetch_optional(&mut **transaction)
    .await?;
    if row.is_some() {
        expire_saved_claims_for_code(transaction, billing_scope_id, plan_key, discount_code_id)
            .await?;
    }
    row.as_ref().map(code_from_row).transpose()
}

pub async fn disable_subscription_discount_code(
    pool: &PgPool,
    billing_scope_id: BillingScopeId,
    plan_key: &PlanKey,
    discount_code_id: DiscountCodeId,
) -> Result<Option<SubscriptionDiscountCodeRecord>, SubscriptionDiscountOperationError> {
    let mut transaction = pool.begin().await?;
    let record = disable_subscription_discount_code_in_transaction(
        &mut transaction,
        billing_scope_id,
        plan_key,
        discount_code_id,
    )
    .await?;
    transaction.commit().await?;
    Ok(record)
}

pub async fn claim_subscription_discount(
    pool: &PgPool,
    offers: &dyn SubscriptionOfferStore,
    claim: &SubscriptionDiscountClaim,
) -> Result<SubscriptionDiscountClaimOutcome, SubscriptionDiscountOperationError> {
    let mut transaction = pool.begin().await?;
    let outcome =
        claim_subscription_discount_in_transaction(&mut transaction, offers, claim).await?;
    transaction.commit().await?;
    Ok(outcome)
}

pub async fn claim_subscription_discount_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    offers: &dyn SubscriptionOfferStore,
    claim: &SubscriptionDiscountClaim,
) -> Result<SubscriptionDiscountClaimOutcome, SubscriptionDiscountOperationError> {
    claim_subscription_discount_on_connection(transaction, offers, claim).await
}

/// Executes a claim on a connection that is already inside the caller's
/// transaction.
///
/// This is crate-visible for subscriber-owned orchestration that must share a
/// host-prepared transaction. The caller owns transaction completion.
pub(crate) async fn claim_subscription_discount_on_connection(
    connection: &mut PgConnection,
    offers: &dyn SubscriptionOfferStore,
    claim: &SubscriptionDiscountClaim,
) -> Result<SubscriptionDiscountClaimOutcome, SubscriptionDiscountOperationError> {
    set_lock_timeout(connection).await?;
    lock_subscription_aggregate(connection, claim.subscriber_id(), claim.plan_key()).await?;
    let offer = lock_offer(
        connection,
        offers,
        claim.billing_scope_id(),
        claim.plan_key(),
    )
    .await?;
    if current_subscription_exists(connection, claim).await? {
        return Ok(SubscriptionDiscountClaimOutcome::BlockedBySubscription);
    }
    let Some(code_row) = find_active_code(
        connection,
        claim.billing_scope_id(),
        claim.plan_key(),
        claim.code(),
        true,
    )
    .await?
    else {
        return Ok(SubscriptionDiscountClaimOutcome::NotFound);
    };
    let code = code_from_row(&code_row)?;
    let quote = quote_for_offer(code, &offer)?;
    let existing = saved_subscription_discount_claim_on_connection(
        connection,
        claim.billing_scope_id(),
        claim.subscriber_id(),
        claim.plan_key(),
    )
    .await?;
    if let Some(existing) = existing.as_ref()
        && existing.snapshot().code() == claim.code()
    {
        return Ok(SubscriptionDiscountClaimOutcome::Existing(Box::new(
            existing.clone(),
        )));
    }
    lock_initial_attempts(connection, claim).await?;
    if blocking_initial_attempt_exists(connection, claim).await? {
        return Ok(SubscriptionDiscountClaimOutcome::BlockedByInitialAttempt);
    }
    if let Some(existing) = existing {
        let result = sqlx::query(
            "UPDATE billing_subscription_discount_claims SET status = 'superseded', superseded_at = now() WHERE id = $1 AND status = 'saved'",
        )
        .bind(existing.id().as_uuid())
        .execute(&mut *connection)
        .await?;
        if result.rows_affected() != 1 {
            return Err(SubscriptionDiscountOperationError::InvalidState(
                INVALID_DISCOUNT_STATE,
            ));
        }
    }
    let quoted_code = quote.code();
    let (amount_off_cents, percent_off_bps) = discount_value(quoted_code.kind());
    let row = sqlx::query(
        r#"
        INSERT INTO billing_subscription_discount_claims (
            id, billing_scope_id, subscriber_id, plan_key, discount_code_id,
            code_snapshot, label_snapshot, discount_kind, amount_off_cents,
            percent_off_bps, currency, duration, duration_months,
            base_amount_cents, discounted_amount_cents, status
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, 'saved')
        RETURNING id, billing_scope_id, subscriber_id, plan_key,
            discount_code_id, code_snapshot, label_snapshot, discount_kind,
            amount_off_cents, percent_off_bps, currency, duration,
            duration_months, base_amount_cents, discounted_amount_cents,
            status, claimed_at, applied_at, applied_subscription_id,
            applied_payment_attempt_id, superseded_at
        "#,
    )
    .bind(claim.id().as_uuid())
    .bind(claim.billing_scope_id().as_uuid())
    .bind(claim.subscriber_id().as_uuid())
    .bind(claim.plan_key().as_str())
    .bind(quoted_code.id().as_uuid())
    .bind(quoted_code.code().as_str())
    .bind(quoted_code.label())
    .bind(quoted_code.kind().as_str())
    .bind(amount_off_cents)
    .bind(percent_off_bps)
    .bind(quoted_code.currency().as_str())
    .bind(quoted_code.duration().as_str())
    .bind(duration_months(quoted_code.duration()))
    .bind(quote.base_charge().cents())
    .bind(quote.discounted_charge().cents())
    .fetch_one(&mut *connection)
    .await?;
    Ok(SubscriptionDiscountClaimOutcome::Saved(Box::new(
        claim_from_row(&row)?,
    )))
}

pub async fn clear_subscription_discount(
    pool: &PgPool,
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    plan_key: &PlanKey,
) -> Result<SubscriptionDiscountClearOutcome, SubscriptionDiscountOperationError> {
    let mut transaction = pool.begin().await?;
    let outcome = clear_subscription_discount_in_transaction(
        &mut transaction,
        billing_scope_id,
        subscriber_id,
        plan_key,
    )
    .await?;
    transaction.commit().await?;
    Ok(outcome)
}

pub async fn clear_subscription_discount_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    plan_key: &PlanKey,
) -> Result<SubscriptionDiscountClearOutcome, SubscriptionDiscountOperationError> {
    clear_subscription_discount_on_connection(
        transaction,
        billing_scope_id,
        subscriber_id,
        plan_key,
    )
    .await
}

/// Executes a saved-discount clear on a connection that is already inside the
/// caller's transaction. The caller owns transaction completion.
pub(crate) async fn clear_subscription_discount_on_connection(
    connection: &mut PgConnection,
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    plan_key: &PlanKey,
) -> Result<SubscriptionDiscountClearOutcome, SubscriptionDiscountOperationError> {
    set_lock_timeout(connection).await?;
    lock_subscription_aggregate(connection, subscriber_id, plan_key).await?;
    let existing = saved_subscription_discount_claim_on_connection(
        connection,
        billing_scope_id,
        subscriber_id,
        plan_key,
    )
    .await?;
    lock_initial_attempt_rows(connection, billing_scope_id, subscriber_id, plan_key).await?;
    if blocking_initial_attempt(connection, billing_scope_id, subscriber_id, plan_key).await? {
        return Ok(SubscriptionDiscountClearOutcome::BlockedByInitialAttempt);
    }
    let Some(existing) = existing else {
        return Ok(SubscriptionDiscountClearOutcome::NotFound);
    };
    let row = sqlx::query(
        r#"
        UPDATE billing_subscription_discount_claims
        SET status = 'expired'
        WHERE id = $1 AND billing_scope_id = $2 AND subscriber_id = $3
            AND plan_key = $4 AND status = 'saved'
        RETURNING id, billing_scope_id, subscriber_id, plan_key,
            discount_code_id, code_snapshot, label_snapshot, discount_kind,
            amount_off_cents, percent_off_bps, currency, duration,
            duration_months, base_amount_cents, discounted_amount_cents,
            status, claimed_at, applied_at, applied_subscription_id,
            applied_payment_attempt_id, superseded_at
        "#,
    )
    .bind(existing.id().as_uuid())
    .bind(billing_scope_id.as_uuid())
    .bind(subscriber_id.as_uuid())
    .bind(plan_key.as_str())
    .fetch_one(&mut *connection)
    .await?;
    Ok(SubscriptionDiscountClearOutcome::Cleared(Box::new(
        claim_from_row(&row)?,
    )))
}

pub async fn saved_subscription_discount_claim(
    pool: &PgPool,
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    plan_key: &PlanKey,
) -> Result<Option<SubscriptionDiscountClaimRecord>, SubscriptionDiscountOperationError> {
    let row = sqlx::query(
        r#"
        SELECT id, billing_scope_id, subscriber_id, plan_key,
            discount_code_id, code_snapshot, label_snapshot, discount_kind,
            amount_off_cents, percent_off_bps, currency, duration,
            duration_months, base_amount_cents, discounted_amount_cents,
            status, claimed_at, applied_at, applied_subscription_id,
            applied_payment_attempt_id, superseded_at
        FROM billing_subscription_discount_claims
        WHERE billing_scope_id = $1 AND subscriber_id = $2 AND plan_key = $3
            AND status = 'saved'
        ORDER BY claimed_at DESC, id DESC LIMIT 1
        "#,
    )
    .bind(billing_scope_id.as_uuid())
    .bind(subscriber_id.as_uuid())
    .bind(plan_key.as_str())
    .fetch_optional(pool)
    .await?;
    row.as_ref().map(claim_from_row).transpose()
}

pub async fn mark_subscription_discount_claim_applied_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    claim_id: DiscountClaimId,
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    plan_key: &PlanKey,
    subscription_id: SubscriptionId,
    payment_attempt_id: PaymentAttemptId,
) -> Result<SubscriptionDiscountClaimRecord, SubscriptionDiscountOperationError> {
    set_lock_timeout(transaction).await?;
    let row = sqlx::query(
        r#"
        UPDATE billing_subscription_discount_claims claims
        SET status = 'applied', applied_at = now(),
            applied_subscription_id = $5, applied_payment_attempt_id = $6
        WHERE claims.id = $1
            AND claims.billing_scope_id = $2
            AND claims.subscriber_id = $3
            AND claims.plan_key = $4
            AND claims.status IN ('saved', 'expired')
            AND EXISTS (
                SELECT 1 FROM billing_payment_attempts attempts
                WHERE attempts.id = $6
                    AND attempts.billing_scope_id = claims.billing_scope_id
                    AND attempts.subscriber_id = claims.subscriber_id
                    AND attempts.plan_key = claims.plan_key
                    AND attempts.subscription_initial_discount_claim_id = claims.id
                    AND attempts.attempt_kind = 'subscription_initial'
                    AND attempts.submitted_at IS NOT NULL
            )
        RETURNING claims.id, claims.billing_scope_id, claims.subscriber_id,
            claims.plan_key, claims.discount_code_id, claims.code_snapshot,
            claims.label_snapshot, claims.discount_kind, claims.amount_off_cents,
            claims.percent_off_bps, claims.currency, claims.duration,
            claims.duration_months, claims.base_amount_cents,
            claims.discounted_amount_cents, claims.status, claims.claimed_at,
            claims.applied_at, claims.applied_subscription_id,
            claims.applied_payment_attempt_id, claims.superseded_at
        "#,
    )
    .bind(claim_id.as_uuid())
    .bind(billing_scope_id.as_uuid())
    .bind(subscriber_id.as_uuid())
    .bind(plan_key.as_str())
    .bind(subscription_id.as_uuid())
    .bind(payment_attempt_id.as_uuid())
    .fetch_optional(&mut **transaction)
    .await?
    .ok_or(sqlx::Error::RowNotFound)?;
    claim_from_row(&row)
}

#[cfg(test)]
mod tests;
