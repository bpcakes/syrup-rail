use std::fmt;

use async_trait::async_trait;
use sqlx::{PgConnection, PgPool, Postgres, Row, Transaction, postgres::PgRow};
use syrup_rail::{
    BillingScopeId, ChargeAmount, CurrencyCode, DiscountClaimId, DiscountCodeId,
    LimitedDiscountMonths, PaymentAttemptId, PercentOffBasisPoints, PlanKey, PositiveDiscountCents,
    SubscriberId, SubscriptionDiscountClaim, SubscriptionDiscountClaimOutcome,
    SubscriptionDiscountClaimRecord, SubscriptionDiscountClaimStatus,
    SubscriptionDiscountClearOutcome, SubscriptionDiscountCode, SubscriptionDiscountCodeCreation,
    SubscriptionDiscountCodeQuote, SubscriptionDiscountCodeRecord, SubscriptionDiscountCodeStatus,
    SubscriptionDiscountCodeUpdate, SubscriptionDiscountDuration, SubscriptionDiscountKind,
    SubscriptionDiscountSnapshot, SubscriptionId, SubscriptionOffer,
};
use thiserror::Error;
use uuid::Uuid;

const BILLING_ROW_LOCK_TIMEOUT: &str = "250ms";
const BILLING_OPERATION_TIMEOUT: &str = "5s";
const INVALID_DISCOUNT_STATE: &str = "canonical subscription discount state is invalid";

#[derive(Error)]
pub enum SubscriptionDiscountOperationError {
    #[error("subscription discount storage operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("current subscription offer is unavailable")]
    OfferUnavailable,
    #[error("current subscription offer does not match the requested plan")]
    OfferPlanMismatch,
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
}

pub async fn list_subscription_discount_codes(
    pool: &PgPool,
    offers: &dyn SubscriptionOfferStore,
    billing_scope_id: BillingScopeId,
    plan_key: &PlanKey,
) -> Result<Vec<SubscriptionDiscountCodeQuote>, SubscriptionDiscountOperationError> {
    let mut transaction = pool.begin().await?;
    let quotes = list_subscription_discount_codes_in_transaction(
        &mut transaction,
        offers,
        billing_scope_id,
        plan_key,
    )
    .await?;
    transaction.commit().await?;
    Ok(quotes)
}

pub async fn list_subscription_discount_codes_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    offers: &dyn SubscriptionOfferStore,
    billing_scope_id: BillingScopeId,
    plan_key: &PlanKey,
) -> Result<Vec<SubscriptionDiscountCodeQuote>, SubscriptionDiscountOperationError> {
    set_lock_timeout(transaction).await?;
    let offer = lock_offer(transaction, offers, billing_scope_id, plan_key).await?;
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
    rows.iter().map(|row| quote_from_row(row, &offer)).collect()
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

pub async fn create_subscription_discount_code_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    offers: &dyn SubscriptionOfferStore,
    creation: &SubscriptionDiscountCodeCreation,
) -> Result<SubscriptionDiscountCodeQuote, SubscriptionDiscountOperationError> {
    set_lock_timeout(transaction).await?;
    let offer = lock_offer(
        transaction,
        offers,
        creation.billing_scope_id(),
        creation.plan_key(),
    )
    .await?;
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
    quote_from_row(&row, &offer)
}

pub async fn create_subscription_discount_code(
    pool: &PgPool,
    offers: &dyn SubscriptionOfferStore,
    creation: &SubscriptionDiscountCodeCreation,
) -> Result<SubscriptionDiscountCodeQuote, SubscriptionDiscountOperationError> {
    let mut transaction = pool.begin().await?;
    let quote =
        create_subscription_discount_code_in_transaction(&mut transaction, offers, creation)
            .await?;
    transaction.commit().await?;
    Ok(quote)
}

pub async fn update_subscription_discount_code_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    offers: &dyn SubscriptionOfferStore,
    update: &SubscriptionDiscountCodeUpdate,
) -> Result<Option<SubscriptionDiscountCodeQuote>, SubscriptionDiscountOperationError> {
    set_lock_timeout(transaction).await?;
    let offer = lock_offer(
        transaction,
        offers,
        update.billing_scope_id(),
        update.plan_key(),
    )
    .await?;
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
    row.as_ref()
        .map(|row| quote_from_row(row, &offer))
        .transpose()
}

pub async fn update_subscription_discount_code(
    pool: &PgPool,
    offers: &dyn SubscriptionOfferStore,
    update: &SubscriptionDiscountCodeUpdate,
) -> Result<Option<SubscriptionDiscountCodeQuote>, SubscriptionDiscountOperationError> {
    let mut transaction = pool.begin().await?;
    let quote =
        update_subscription_discount_code_in_transaction(&mut transaction, offers, update).await?;
    transaction.commit().await?;
    Ok(quote)
}

pub async fn disable_subscription_discount_code_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    offers: &dyn SubscriptionOfferStore,
    billing_scope_id: BillingScopeId,
    plan_key: &PlanKey,
    discount_code_id: DiscountCodeId,
) -> Result<Option<SubscriptionDiscountCodeQuote>, SubscriptionDiscountOperationError> {
    set_lock_timeout(transaction).await?;
    let offer = lock_offer(transaction, offers, billing_scope_id, plan_key).await?;
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
    row.as_ref()
        .map(|row| quote_from_row(row, &offer))
        .transpose()
}

pub async fn disable_subscription_discount_code(
    pool: &PgPool,
    offers: &dyn SubscriptionOfferStore,
    billing_scope_id: BillingScopeId,
    plan_key: &PlanKey,
    discount_code_id: DiscountCodeId,
) -> Result<Option<SubscriptionDiscountCodeQuote>, SubscriptionDiscountOperationError> {
    let mut transaction = pool.begin().await?;
    let quote = disable_subscription_discount_code_in_transaction(
        &mut transaction,
        offers,
        billing_scope_id,
        plan_key,
        discount_code_id,
    )
    .await?;
    transaction.commit().await?;
    Ok(quote)
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
    set_lock_timeout(transaction).await?;
    lock_subscription_aggregate(transaction, claim.subscriber_id(), claim.plan_key()).await?;
    let offer = lock_offer(
        transaction,
        offers,
        claim.billing_scope_id(),
        claim.plan_key(),
    )
    .await?;
    if current_subscription_exists(transaction, claim).await? {
        return Ok(SubscriptionDiscountClaimOutcome::BlockedBySubscription);
    }
    let Some(code_row) = find_active_code(
        transaction,
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
    let quote = SubscriptionDiscountCodeQuote::new(code, &offer)
        .map_err(|_| SubscriptionDiscountOperationError::InvalidState(INVALID_DISCOUNT_STATE))?;
    let existing = saved_claim_for_update(
        transaction,
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
    lock_initial_attempts(transaction, claim).await?;
    if blocking_initial_attempt_exists(transaction, claim).await? {
        return Ok(SubscriptionDiscountClaimOutcome::BlockedByInitialAttempt);
    }
    if let Some(existing) = existing {
        let result = sqlx::query(
            "UPDATE billing_subscription_discount_claims SET status = 'superseded', superseded_at = now() WHERE id = $1 AND status = 'saved'",
        )
        .bind(existing.id().as_uuid())
        .execute(&mut **transaction)
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
    .fetch_one(&mut **transaction)
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
    set_lock_timeout(transaction).await?;
    lock_subscription_aggregate(transaction, subscriber_id, plan_key).await?;
    let existing =
        saved_claim_for_update(transaction, billing_scope_id, subscriber_id, plan_key).await?;
    lock_initial_attempt_rows(transaction, billing_scope_id, subscriber_id, plan_key).await?;
    if blocking_initial_attempt(transaction, billing_scope_id, subscriber_id, plan_key).await? {
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
    .fetch_one(&mut **transaction)
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

async fn lock_offer(
    transaction: &mut Transaction<'_, Postgres>,
    offers: &dyn SubscriptionOfferStore,
    billing_scope_id: BillingScopeId,
    plan_key: &PlanKey,
) -> Result<SubscriptionOffer, SubscriptionDiscountOperationError> {
    let offer = offers
        .lock_current_offer(transaction, billing_scope_id, plan_key)
        .await?
        .ok_or(SubscriptionDiscountOperationError::OfferUnavailable)?;
    if offer.plan_key() != plan_key {
        return Err(SubscriptionDiscountOperationError::OfferPlanMismatch);
    }
    Ok(offer)
}

async fn set_lock_timeout(transaction: &mut Transaction<'_, Postgres>) -> Result<(), sqlx::Error> {
    sqlx::query(
        "SELECT set_config('lock_timeout', $1, true), set_config('statement_timeout', $2, true)",
    )
    .bind(BILLING_ROW_LOCK_TIMEOUT)
    .bind(BILLING_OPERATION_TIMEOUT)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn lock_subscription_aggregate(
    transaction: &mut Transaction<'_, Postgres>,
    subscriber_id: SubscriberId,
    plan_key: &PlanKey,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::uuid::text || ':' || $2, 0))")
        .bind(subscriber_id.as_uuid())
        .bind(plan_key.as_str())
        .execute(&mut **transaction)
        .await?;
    Ok(())
}

async fn current_subscription_exists(
    transaction: &mut Transaction<'_, Postgres>,
    claim: &SubscriptionDiscountClaim,
) -> Result<bool, SubscriptionDiscountOperationError> {
    for _ in 0..2 {
        let candidate = current_subscription_id(transaction, claim).await?;
        let Some(candidate) = candidate else {
            return Ok(false);
        };
        let locked: Option<Uuid> = sqlx::query_scalar(
            "SELECT id FROM billing_subscriptions WHERE id = $1 FOR NO KEY UPDATE",
        )
        .bind(candidate)
        .fetch_optional(&mut **transaction)
        .await?;
        if locked.is_some() && current_subscription_id(transaction, claim).await? == Some(candidate)
        {
            return Ok(true);
        }
    }
    Err(SubscriptionDiscountOperationError::InvalidState(
        "current subscription ranking did not stabilize while acquiring its row lock",
    ))
}

async fn current_subscription_id(
    transaction: &mut Transaction<'_, Postgres>,
    claim: &SubscriptionDiscountClaim,
) -> Result<Option<Uuid>, sqlx::Error> {
    sqlx::query_scalar(
        r#"
        SELECT id FROM billing_current_subscriptions
        WHERE billing_scope_id = $1 AND subscriber_id = $2 AND plan_key = $3
        ORDER BY current_subscription_rank, updated_at DESC, id DESC LIMIT 1
        "#,
    )
    .bind(claim.billing_scope_id().as_uuid())
    .bind(claim.subscriber_id().as_uuid())
    .bind(claim.plan_key().as_str())
    .fetch_optional(&mut **transaction)
    .await
}

async fn find_active_code(
    transaction: &mut Transaction<'_, Postgres>,
    billing_scope_id: BillingScopeId,
    plan_key: &PlanKey,
    code: &SubscriptionDiscountCode,
    for_update: bool,
) -> Result<Option<PgRow>, sqlx::Error> {
    let lock = if for_update { "FOR UPDATE" } else { "" };
    sqlx::query(&format!(
        r#"
        SELECT id, billing_scope_id, plan_key, code_normalized, display_code,
            label, status, discount_kind, amount_off_cents, percent_off_bps,
            currency, duration, duration_months, created_at, updated_at
        FROM billing_subscription_discount_codes
        WHERE billing_scope_id = $1 AND plan_key = $2
            AND code_normalized = $3 AND status = 'active'
        {lock}
        "#,
    ))
    .bind(billing_scope_id.as_uuid())
    .bind(plan_key.as_str())
    .bind(code.as_str())
    .fetch_optional(&mut **transaction)
    .await
}

async fn code_by_id(
    transaction: &mut Transaction<'_, Postgres>,
    billing_scope_id: BillingScopeId,
    plan_key: &PlanKey,
    id: DiscountCodeId,
) -> Result<Option<PgRow>, sqlx::Error> {
    sqlx::query(
        r#"
        SELECT id, billing_scope_id, plan_key, code_normalized, display_code,
            label, status, discount_kind, amount_off_cents, percent_off_bps,
            currency, duration, duration_months, created_at, updated_at
        FROM billing_subscription_discount_codes
        WHERE id = $1 AND billing_scope_id = $2 AND plan_key = $3
        "#,
    )
    .bind(id.as_uuid())
    .bind(billing_scope_id.as_uuid())
    .bind(plan_key.as_str())
    .fetch_optional(&mut **transaction)
    .await
}

async fn expire_saved_claims_for_code(
    transaction: &mut Transaction<'_, Postgres>,
    billing_scope_id: BillingScopeId,
    plan_key: &PlanKey,
    discount_code_id: DiscountCodeId,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        UPDATE billing_subscription_discount_claims SET status = 'expired'
        WHERE billing_scope_id = $1 AND plan_key = $2
            AND discount_code_id = $3 AND status = 'saved'
        "#,
    )
    .bind(billing_scope_id.as_uuid())
    .bind(plan_key.as_str())
    .bind(discount_code_id.as_uuid())
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn saved_claim_for_update(
    transaction: &mut Transaction<'_, Postgres>,
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
        ORDER BY claimed_at DESC, id DESC LIMIT 1 FOR UPDATE
        "#,
    )
    .bind(billing_scope_id.as_uuid())
    .bind(subscriber_id.as_uuid())
    .bind(plan_key.as_str())
    .fetch_optional(&mut **transaction)
    .await?;
    row.as_ref().map(claim_from_row).transpose()
}

async fn lock_initial_attempts(
    transaction: &mut Transaction<'_, Postgres>,
    claim: &SubscriptionDiscountClaim,
) -> Result<(), sqlx::Error> {
    lock_initial_attempt_rows(
        transaction,
        claim.billing_scope_id(),
        claim.subscriber_id(),
        claim.plan_key(),
    )
    .await
}

async fn lock_initial_attempt_rows(
    transaction: &mut Transaction<'_, Postgres>,
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    plan_key: &PlanKey,
) -> Result<(), sqlx::Error> {
    sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT id FROM billing_payment_attempts
        WHERE billing_scope_id = $1 AND subscriber_id = $2 AND plan_key = $3
            AND attempt_kind = 'subscription_initial'
        ORDER BY created_at, id FOR UPDATE
        "#,
    )
    .bind(billing_scope_id.as_uuid())
    .bind(subscriber_id.as_uuid())
    .bind(plan_key.as_str())
    .fetch_all(&mut **transaction)
    .await?;
    Ok(())
}

async fn blocking_initial_attempt_exists(
    transaction: &mut Transaction<'_, Postgres>,
    claim: &SubscriptionDiscountClaim,
) -> Result<bool, sqlx::Error> {
    blocking_initial_attempt(
        transaction,
        claim.billing_scope_id(),
        claim.subscriber_id(),
        claim.plan_key(),
    )
    .await
}

async fn blocking_initial_attempt(
    transaction: &mut Transaction<'_, Postgres>,
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    plan_key: &PlanKey,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        r#"
        SELECT EXISTS (
            SELECT 1 FROM billing_payment_attempts attempts
            WHERE attempts.billing_scope_id = $1
                AND attempts.subscriber_id = $2
                AND attempts.plan_key = $3
                AND attempts.attempt_kind = 'subscription_initial'
                AND (
                    attempts.status IN ('pending', 'unknown')
                    OR (
                        attempts.status = 'review_required'
                        AND attempts.resolution_code IS DISTINCT FROM
                            'subscription_initial_current_subscription_conflict'
                    )
                )
                AND NOT EXISTS (
                    SELECT 1 FROM billing_subscriptions subscriptions
                    WHERE subscriptions.billing_scope_id = attempts.billing_scope_id
                        AND subscriptions.subscriber_id = attempts.subscriber_id
                        AND subscriptions.plan_key = attempts.plan_key
                        AND subscriptions.created_at >= attempts.created_at
                )
        )
        "#,
    )
    .bind(billing_scope_id.as_uuid())
    .bind(subscriber_id.as_uuid())
    .bind(plan_key.as_str())
    .fetch_one(&mut **transaction)
    .await
}

fn quote_from_row(
    row: &PgRow,
    offer: &SubscriptionOffer,
) -> Result<SubscriptionDiscountCodeQuote, SubscriptionDiscountOperationError> {
    SubscriptionDiscountCodeQuote::new(code_from_row(row)?, offer)
        .map_err(|_| SubscriptionDiscountOperationError::InvalidState(INVALID_DISCOUNT_STATE))
}

fn code_from_row(
    row: &PgRow,
) -> Result<SubscriptionDiscountCodeRecord, SubscriptionDiscountOperationError> {
    let kind = discount_kind_from_row(row)?;
    let duration = discount_duration_from_row(row)?;
    SubscriptionDiscountCodeRecord::new(
        DiscountCodeId::new(row.try_get("id")?),
        BillingScopeId::new(row.try_get("billing_scope_id")?),
        PlanKey::new(row.try_get::<String, _>("plan_key")?).map_err(|_| {
            SubscriptionDiscountOperationError::InvalidState(INVALID_DISCOUNT_STATE)
        })?,
        SubscriptionDiscountCode::new(&row.try_get::<String, _>("code_normalized")?).map_err(
            |_| SubscriptionDiscountOperationError::InvalidState(INVALID_DISCOUNT_STATE),
        )?,
        row.try_get("display_code")?,
        row.try_get("label")?,
        parse_code_status(&row.try_get::<String, _>("status")?)?,
        kind,
        CurrencyCode::new(&row.try_get::<String, _>("currency")?).map_err(|_| {
            SubscriptionDiscountOperationError::InvalidState(INVALID_DISCOUNT_STATE)
        })?,
        duration,
        row.try_get("created_at")?,
        row.try_get("updated_at")?,
    )
    .map_err(|_| SubscriptionDiscountOperationError::InvalidState(INVALID_DISCOUNT_STATE))
}

fn claim_from_row(
    row: &PgRow,
) -> Result<SubscriptionDiscountClaimRecord, SubscriptionDiscountOperationError> {
    let code = SubscriptionDiscountCode::new(&row.try_get::<String, _>("code_snapshot")?)
        .map_err(|_| SubscriptionDiscountOperationError::InvalidState(INVALID_DISCOUNT_STATE))?;
    let kind = discount_kind_from_row(row)?;
    let duration = discount_duration_from_row(row)?;
    let currency = CurrencyCode::new(&row.try_get::<String, _>("currency")?)
        .map_err(|_| SubscriptionDiscountOperationError::InvalidState(INVALID_DISCOUNT_STATE))?;
    let snapshot = SubscriptionDiscountSnapshot::new(
        code,
        row.try_get("label_snapshot")?,
        kind,
        duration,
        ChargeAmount::new(row.try_get("base_amount_cents")?, currency).map_err(|_| {
            SubscriptionDiscountOperationError::InvalidState(INVALID_DISCOUNT_STATE)
        })?,
        ChargeAmount::new(row.try_get("discounted_amount_cents")?, currency).map_err(|_| {
            SubscriptionDiscountOperationError::InvalidState(INVALID_DISCOUNT_STATE)
        })?,
    )
    .map_err(|_| SubscriptionDiscountOperationError::InvalidState(INVALID_DISCOUNT_STATE))?;
    SubscriptionDiscountClaimRecord::new(
        DiscountClaimId::new(row.try_get("id")?),
        BillingScopeId::new(row.try_get("billing_scope_id")?),
        SubscriberId::new(row.try_get("subscriber_id")?),
        PlanKey::new(row.try_get::<String, _>("plan_key")?).map_err(|_| {
            SubscriptionDiscountOperationError::InvalidState(INVALID_DISCOUNT_STATE)
        })?,
        DiscountCodeId::new(row.try_get("discount_code_id")?),
        snapshot,
        parse_claim_status(&row.try_get::<String, _>("status")?)?,
        row.try_get("claimed_at")?,
        row.try_get("applied_at")?,
        row.try_get::<Option<Uuid>, _>("applied_subscription_id")?
            .map(SubscriptionId::new),
        row.try_get::<Option<Uuid>, _>("applied_payment_attempt_id")?
            .map(PaymentAttemptId::new),
        row.try_get("superseded_at")?,
    )
    .map_err(|_| SubscriptionDiscountOperationError::InvalidState(INVALID_DISCOUNT_STATE))
}

fn discount_kind_from_row(
    row: &PgRow,
) -> Result<SubscriptionDiscountKind, SubscriptionDiscountOperationError> {
    match row.try_get::<String, _>("discount_kind")?.as_str() {
        "amount_off" => Ok(SubscriptionDiscountKind::AmountOffCents(
            PositiveDiscountCents::new(row.try_get::<Option<i32>, _>("amount_off_cents")?.ok_or(
                SubscriptionDiscountOperationError::InvalidState(INVALID_DISCOUNT_STATE),
            )?)
            .map_err(|_| {
                SubscriptionDiscountOperationError::InvalidState(INVALID_DISCOUNT_STATE)
            })?,
        )),
        "percent_off" => {
            let value = row.try_get::<Option<i32>, _>("percent_off_bps")?.ok_or(
                SubscriptionDiscountOperationError::InvalidState(INVALID_DISCOUNT_STATE),
            )?;
            Ok(SubscriptionDiscountKind::PercentOffBasisPoints(
                PercentOffBasisPoints::new(u16::try_from(value).map_err(|_| {
                    SubscriptionDiscountOperationError::InvalidState(INVALID_DISCOUNT_STATE)
                })?)
                .map_err(|_| {
                    SubscriptionDiscountOperationError::InvalidState(INVALID_DISCOUNT_STATE)
                })?,
            ))
        }
        _ => Err(SubscriptionDiscountOperationError::InvalidState(
            INVALID_DISCOUNT_STATE,
        )),
    }
}

fn discount_duration_from_row(
    row: &PgRow,
) -> Result<SubscriptionDiscountDuration, SubscriptionDiscountOperationError> {
    match row.try_get::<String, _>("duration")?.as_str() {
        "indefinite" if row.try_get::<Option<i32>, _>("duration_months")?.is_none() => {
            Ok(SubscriptionDiscountDuration::Indefinite)
        }
        "limited_months" => {
            let value = row.try_get::<Option<i32>, _>("duration_months")?.ok_or(
                SubscriptionDiscountOperationError::InvalidState(INVALID_DISCOUNT_STATE),
            )?;
            Ok(SubscriptionDiscountDuration::LimitedMonths(
                LimitedDiscountMonths::new(u8::try_from(value).map_err(|_| {
                    SubscriptionDiscountOperationError::InvalidState(INVALID_DISCOUNT_STATE)
                })?)
                .map_err(|_| {
                    SubscriptionDiscountOperationError::InvalidState(INVALID_DISCOUNT_STATE)
                })?,
            ))
        }
        _ => Err(SubscriptionDiscountOperationError::InvalidState(
            INVALID_DISCOUNT_STATE,
        )),
    }
}

fn parse_code_status(
    value: &str,
) -> Result<SubscriptionDiscountCodeStatus, SubscriptionDiscountOperationError> {
    match value {
        "active" => Ok(SubscriptionDiscountCodeStatus::Active),
        "disabled" => Ok(SubscriptionDiscountCodeStatus::Disabled),
        _ => Err(SubscriptionDiscountOperationError::InvalidState(
            INVALID_DISCOUNT_STATE,
        )),
    }
}

fn parse_claim_status(
    value: &str,
) -> Result<SubscriptionDiscountClaimStatus, SubscriptionDiscountOperationError> {
    match value {
        "saved" => Ok(SubscriptionDiscountClaimStatus::Saved),
        "applied" => Ok(SubscriptionDiscountClaimStatus::Applied),
        "superseded" => Ok(SubscriptionDiscountClaimStatus::Superseded),
        "expired" => Ok(SubscriptionDiscountClaimStatus::Expired),
        _ => Err(SubscriptionDiscountOperationError::InvalidState(
            INVALID_DISCOUNT_STATE,
        )),
    }
}

fn discount_value(kind: SubscriptionDiscountKind) -> (Option<i32>, Option<i32>) {
    match kind {
        SubscriptionDiscountKind::AmountOffCents(value) => (Some(value.get()), None),
        SubscriptionDiscountKind::PercentOffBasisPoints(value) => {
            (None, Some(i32::from(value.get())))
        }
    }
}

fn duration_months(duration: SubscriptionDiscountDuration) -> Option<i32> {
    match duration {
        SubscriptionDiscountDuration::Indefinite => None,
        SubscriptionDiscountDuration::LimitedMonths(value) => Some(i32::from(value.get())),
    }
}

#[cfg(test)]
mod tests {
    use std::{error::Error, io};

    use async_trait::async_trait;
    use sqlx::{PgConnection, Row};
    use syrup_rail::{
        BillingScopeId, ChargeAmount, CurrencyCode, DiscountClaimId, DiscountCodeId,
        PercentOffBasisPoints, PlanKey, PositiveDiscountCents, SubscriberId,
        SubscriptionDiscountClaim, SubscriptionDiscountClaimOutcome, SubscriptionDiscountCode,
        SubscriptionDiscountCodeCreation, SubscriptionDiscountCodeStatus,
        SubscriptionDiscountCodeUpdate, SubscriptionDiscountDuration, SubscriptionDiscountKind,
        SubscriptionOffer,
    };
    use tokio::time::{Duration, timeout};
    use uuid::Uuid;

    use super::*;
    use crate::test_support::{TestDatabase, create_gateway_account};

    struct TestOfferStore;

    #[async_trait]
    impl SubscriptionOfferStore for TestOfferStore {
        async fn lock_current_offer(
            &self,
            connection: &mut PgConnection,
            billing_scope_id: BillingScopeId,
            plan_key: &PlanKey,
        ) -> Result<Option<SubscriptionOffer>, sqlx::Error> {
            let row = sqlx::query(
                r#"
                SELECT amount_cents, currency
                FROM test_subscription_offers
                WHERE billing_scope_id = $1 AND plan_key = $2
                FOR NO KEY UPDATE
                "#,
            )
            .bind(billing_scope_id.as_uuid())
            .bind(plan_key.as_str())
            .fetch_optional(connection)
            .await?;
            row.map(|row| {
                let currency = CurrencyCode::new(row.try_get::<String, _>("currency")?.as_str())
                    .map_err(|error| sqlx::Error::Decode(Box::new(error)))?;
                let charge = ChargeAmount::new(row.try_get("amount_cents")?, currency)
                    .map_err(|error| sqlx::Error::Decode(Box::new(error)))?;
                Ok(SubscriptionOffer::new(plan_key.clone(), charge))
            })
            .transpose()
        }
    }

    #[tokio::test]
    async fn offer_lock_uses_the_callers_connection_and_blocks_price_updates()
    -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("sr_disc_lock").await?;
        let result = async {
            create_offer_table(&database.pool).await?;
            let scope = BillingScopeId::new(Uuid::now_v7());
            let plan = PlanKey::new("plan_a")?;
            insert_offer(&database.pool, scope, &plan, 5_900).await?;

            let mut transaction = database.pool.begin().await?;
            let listed = list_subscription_discount_codes_in_transaction(
                &mut transaction,
                &TestOfferStore,
                scope,
                &plan,
            )
            .await?;
            if !listed.is_empty() {
                return Err(io::Error::other("new offer unexpectedly had discount codes").into());
            }

            let pool = database.pool.clone();
            let plan_for_update = plan.clone();
            let mut update = tokio::spawn(async move {
                sqlx::query(
                    "UPDATE test_subscription_offers SET amount_cents = 6900 WHERE billing_scope_id = $1 AND plan_key = $2",
                )
                .bind(scope.as_uuid())
                .bind(plan_for_update.as_str())
                .execute(&pool)
                .await
            });
            if timeout(Duration::from_millis(100), &mut update).await.is_ok() {
                return Err(io::Error::other("price update escaped the offer row lock").into());
            }
            transaction.commit().await?;
            update.await??;

            let amount: i32 = sqlx::query_scalar(
                "SELECT amount_cents FROM test_subscription_offers WHERE billing_scope_id = $1 AND plan_key = $2",
            )
            .bind(scope.as_uuid())
            .bind(plan.as_str())
            .fetch_one(&database.pool)
            .await?;
            if amount != 6_900 {
                return Err(io::Error::other("blocked price update did not resume").into());
            }
            Ok::<_, Box<dyn Error>>(())
        }
        .await;
        let cleanup = database.cleanup().await;
        result?;
        cleanup
    }

    #[tokio::test]
    async fn discount_code_and_claim_policy_is_exact_plan_scoped_and_snapshot_preserving()
    -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("sr_disc_life").await?;
        let result = async {
            create_offer_table(&database.pool).await?;
            let scope = BillingScopeId::new(Uuid::now_v7());
            let subscriber = SubscriberId::new(Uuid::now_v7());
            let plan_a = PlanKey::new("plan_a")?;
            let plan_b = PlanKey::new("plan_b")?;
            insert_offer(&database.pool, scope, &plan_a, 5_900).await?;
            insert_offer(&database.pool, scope, &plan_b, 9_900).await?;
            let usd = CurrencyCode::new("USD")?;

            let code_a_id = DiscountCodeId::new(Uuid::now_v7());
            let code = SubscriptionDiscountCode::new("SAVE25")?;
            let creation = SubscriptionDiscountCodeCreation::new(
                code_a_id,
                scope,
                plan_a.clone(),
                code.clone(),
                Some("  Launch offer  ".into()),
                SubscriptionDiscountKind::PercentOffBasisPoints(PercentOffBasisPoints::new(2_500)?),
                usd,
                SubscriptionDiscountDuration::Indefinite,
            )?;
            let created =
                create_subscription_discount_code(&database.pool, &TestOfferStore, &creation)
                    .await?;
            if created.base_charge().cents() != 5_900
                || created.discounted_charge().cents() != 4_425
                || created.code().label() != Some("Launch offer")
            {
                return Err(io::Error::other("created quote lost canonical terms").into());
            }

            let first_claim_id = DiscountClaimId::new(Uuid::now_v7());
            let first_claim = SubscriptionDiscountClaim::new(
                first_claim_id,
                scope,
                subscriber,
                plan_a.clone(),
                code.clone(),
            );
            let first =
                claim_subscription_discount(&database.pool, &TestOfferStore, &first_claim).await?;
            let SubscriptionDiscountClaimOutcome::Saved(first) = first else {
                return Err(io::Error::other("first claim was not saved").into());
            };
            if first.snapshot().discounted_charge().cents() != 4_425 {
                return Err(io::Error::other("claim did not snapshot locked offer").into());
            }

            let update = SubscriptionDiscountCodeUpdate::new(
                code_a_id,
                scope,
                plan_a.clone(),
                Some("Changed terms".into()),
                SubscriptionDiscountCodeStatus::Active,
                SubscriptionDiscountKind::AmountOffCents(PositiveDiscountCents::new(900)?),
                usd,
                SubscriptionDiscountDuration::Indefinite,
            )?;
            update_subscription_discount_code(&database.pool, &TestOfferStore, &update)
                .await?
                .ok_or_else(|| io::Error::other("updated code disappeared"))?;

            let replay = SubscriptionDiscountClaim::new(
                DiscountClaimId::new(Uuid::now_v7()),
                scope,
                subscriber,
                plan_a.clone(),
                code.clone(),
            );
            let replay =
                claim_subscription_discount(&database.pool, &TestOfferStore, &replay).await?;
            let SubscriptionDiscountClaimOutcome::Existing(replay) = replay else {
                return Err(io::Error::other("same canonical code did not replay").into());
            };
            if replay.id() != first_claim_id
                || replay.snapshot().discounted_charge().cents() != 4_425
                || replay.snapshot().label() != Some("Launch offer")
            {
                return Err(io::Error::other("same-code replay reinterpreted the snapshot").into());
            }

            let plan_b_creation = SubscriptionDiscountCodeCreation::new(
                DiscountCodeId::new(Uuid::now_v7()),
                scope,
                plan_b.clone(),
                code.clone(),
                None,
                SubscriptionDiscountKind::AmountOffCents(PositiveDiscountCents::new(900)?),
                usd,
                SubscriptionDiscountDuration::Indefinite,
            )?;
            create_subscription_discount_code(&database.pool, &TestOfferStore, &plan_b_creation)
                .await?;
            let plan_b_claim = SubscriptionDiscountClaim::new(
                DiscountClaimId::new(Uuid::now_v7()),
                scope,
                subscriber,
                plan_b.clone(),
                code,
            );
            let plan_b_saved =
                claim_subscription_discount(&database.pool, &TestOfferStore, &plan_b_claim).await?;
            if !matches!(plan_b_saved, SubscriptionDiscountClaimOutcome::Saved(_)) {
                return Err(
                    io::Error::other("another plan did not own an independent claim").into(),
                );
            }

            disable_subscription_discount_code(
                &database.pool,
                &TestOfferStore,
                scope,
                &plan_a,
                code_a_id,
            )
            .await?
            .ok_or_else(|| io::Error::other("disabled code disappeared"))?;
            if saved_subscription_discount_claim(&database.pool, scope, subscriber, &plan_a)
                .await?
                .is_some()
                || saved_subscription_discount_claim(&database.pool, scope, subscriber, &plan_b)
                    .await?
                    .is_none()
            {
                return Err(io::Error::other("disable crossed the exact plan boundary").into());
            }
            Ok::<_, Box<dyn Error>>(())
        }
        .await;
        let cleanup = database.cleanup().await;
        result?;
        cleanup
    }

    #[tokio::test]
    async fn clear_is_blocked_by_an_exact_plan_initial_attempt() -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("sr_disc_clear").await?;
        let result = async {
            create_offer_table(&database.pool).await?;
            let gateway = create_gateway_account(&database.pool, "test_gateway").await?;
            let scope = BillingScopeId::new(gateway.billing_scope_id);
            let subscriber = SubscriberId::new(Uuid::now_v7());
            let plan = PlanKey::new("plan_a")?;
            insert_offer(&database.pool, scope, &plan, 5_900).await?;
            let creation = SubscriptionDiscountCodeCreation::new(
                DiscountCodeId::new(Uuid::now_v7()),
                scope,
                plan.clone(),
                SubscriptionDiscountCode::new("CLEAR10")?,
                None,
                SubscriptionDiscountKind::AmountOffCents(PositiveDiscountCents::new(500)?),
                CurrencyCode::new("USD")?,
                SubscriptionDiscountDuration::Indefinite,
            )?;
            create_subscription_discount_code(&database.pool, &TestOfferStore, &creation).await?;
            let claim = SubscriptionDiscountClaim::new(
                DiscountClaimId::new(Uuid::now_v7()),
                scope,
                subscriber,
                plan.clone(),
                SubscriptionDiscountCode::new("CLEAR10")?,
            );
            claim_subscription_discount(&database.pool, &TestOfferStore, &claim).await?;
            insert_pending_initial_attempt(
                &database.pool,
                gateway.billing_scope_id,
                subscriber.into_uuid(),
                plan.as_str(),
                gateway.gateway_account_id,
                gateway.gateway_configuration_id,
            )
            .await?;

            let blocked =
                clear_subscription_discount(&database.pool, scope, subscriber, &plan).await?;
            if blocked != SubscriptionDiscountClearOutcome::BlockedByInitialAttempt {
                return Err(io::Error::other("initial checkout did not block clear").into());
            }
            if saved_subscription_discount_claim(&database.pool, scope, subscriber, &plan)
                .await?
                .is_none()
            {
                return Err(io::Error::other("blocked clear expired the saved claim").into());
            }
            Ok::<_, Box<dyn Error>>(())
        }
        .await;
        let cleanup = database.cleanup().await;
        result?;
        cleanup
    }

    async fn create_offer_table(pool: &PgPool) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            CREATE TABLE test_subscription_offers (
                billing_scope_id uuid NOT NULL,
                plan_key text NOT NULL,
                amount_cents integer NOT NULL,
                currency text NOT NULL,
                PRIMARY KEY (billing_scope_id, plan_key)
            )
            "#,
        )
        .execute(pool)
        .await?;
        Ok(())
    }

    async fn insert_offer(
        pool: &PgPool,
        scope: BillingScopeId,
        plan: &PlanKey,
        amount_cents: i32,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO test_subscription_offers (billing_scope_id, plan_key, amount_cents, currency) VALUES ($1, $2, $3, 'USD')",
        )
        .bind(scope.as_uuid())
        .bind(plan.as_str())
        .bind(amount_cents)
        .execute(pool)
        .await?;
        Ok(())
    }

    async fn insert_pending_initial_attempt(
        pool: &PgPool,
        scope: Uuid,
        subscriber: Uuid,
        plan: &str,
        gateway_account: Uuid,
        gateway_configuration: Uuid,
    ) -> Result<(), sqlx::Error> {
        let attempt = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO billing_payment_attempts (
                id, billing_scope_id, subscriber_id, plan_key, attempt_kind,
                status, idempotency_key, request_fingerprint, amount_cents,
                currency, gateway_account_id, gateway_configuration_id,
                gateway_order_id
            ) VALUES (
                $1, $2, $3, $4, 'subscription_initial', 'pending', $5, $6,
                100, 'USD', $7, $8, $9
            )
            "#,
        )
        .bind(attempt)
        .bind(scope)
        .bind(subscriber)
        .bind(plan)
        .bind(format!("discount-{}", attempt.simple()))
        .bind(format!("initial:{plan}:100:USD"))
        .bind(gateway_account)
        .bind(gateway_configuration)
        .bind(format!("discount-order-{}", attempt.simple()))
        .execute(pool)
        .await?;
        Ok(())
    }
}
