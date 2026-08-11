use super::*;

pub(super) async fn lock_offer(
    connection: &mut PgConnection,
    offers: &dyn SubscriptionOfferStore,
    billing_scope_id: BillingScopeId,
    plan_key: &PlanKey,
) -> Result<SubscriptionOffer, SubscriptionDiscountOperationError> {
    let offer = offers
        .lock_current_offer(connection, billing_scope_id, plan_key)
        .await?
        .ok_or(SubscriptionDiscountOperationError::OfferUnavailable)?;
    if offer.plan_key() != plan_key {
        return Err(SubscriptionDiscountOperationError::OfferPlanMismatch);
    }
    Ok(offer)
}

pub(super) fn validate_discount_cadence(
    duration: SubscriptionDiscountDuration,
    offer: &SubscriptionOffer,
) -> Result<(), SubscriptionDiscountOperationError> {
    if matches!(duration, SubscriptionDiscountDuration::LimitedMonths(_))
        && !offer.recurring().period().is_one_calendar_month()
    {
        return Err(SubscriptionDiscountOperationError::LimitedDiscountCadence);
    }
    Ok(())
}

pub(super) async fn set_lock_timeout(connection: &mut PgConnection) -> Result<(), sqlx::Error> {
    sqlx::query(
        "SELECT set_config('lock_timeout', $1, true), set_config('statement_timeout', $2, true)",
    )
    .bind(BILLING_ROW_LOCK_TIMEOUT)
    .bind(BILLING_OPERATION_TIMEOUT)
    .execute(connection)
    .await?;
    Ok(())
}

pub(super) async fn lock_subscription_aggregate(
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

pub(super) async fn current_subscription_exists(
    connection: &mut PgConnection,
    claim: &SubscriptionDiscountClaim,
) -> Result<bool, SubscriptionDiscountOperationError> {
    for _ in 0..2 {
        let candidate = current_subscription_id(connection, claim).await?;
        let Some(candidate) = candidate else {
            return Ok(false);
        };
        let locked: Option<Uuid> = sqlx::query_scalar(
            "SELECT id FROM billing_subscriptions WHERE id = $1 FOR NO KEY UPDATE",
        )
        .bind(candidate)
        .fetch_optional(&mut *connection)
        .await?;
        if locked.is_some() && current_subscription_id(connection, claim).await? == Some(candidate)
        {
            return Ok(true);
        }
    }
    Err(SubscriptionDiscountOperationError::InvalidState(
        "current subscription ranking did not stabilize while acquiring its row lock",
    ))
}

pub(super) async fn current_subscription_id(
    connection: &mut PgConnection,
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
    .fetch_optional(&mut *connection)
    .await
}

pub(super) async fn find_active_code(
    connection: &mut PgConnection,
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
    .fetch_optional(&mut *connection)
    .await
}

pub(super) async fn code_by_id(
    connection: &mut PgConnection,
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
    .fetch_optional(&mut *connection)
    .await
}

pub(super) async fn expire_saved_claims_for_code(
    connection: &mut PgConnection,
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
    .execute(connection)
    .await?;
    Ok(())
}

pub async fn saved_subscription_discount_claim_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    plan_key: &PlanKey,
) -> Result<Option<SubscriptionDiscountClaimRecord>, SubscriptionDiscountOperationError> {
    saved_subscription_discount_claim_on_connection(
        transaction,
        billing_scope_id,
        subscriber_id,
        plan_key,
    )
    .await
}

pub(super) async fn saved_subscription_discount_claim_on_connection(
    connection: &mut PgConnection,
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
    .fetch_optional(&mut *connection)
    .await?;
    row.as_ref().map(claim_from_row).transpose()
}

pub(super) async fn lock_initial_attempts(
    connection: &mut PgConnection,
    claim: &SubscriptionDiscountClaim,
) -> Result<(), sqlx::Error> {
    lock_initial_attempt_rows(
        connection,
        claim.billing_scope_id(),
        claim.subscriber_id(),
        claim.plan_key(),
    )
    .await
}

pub(super) async fn lock_initial_attempt_rows(
    connection: &mut PgConnection,
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
    .fetch_all(&mut *connection)
    .await?;
    Ok(())
}

pub(super) async fn blocking_initial_attempt_exists(
    connection: &mut PgConnection,
    claim: &SubscriptionDiscountClaim,
) -> Result<bool, sqlx::Error> {
    blocking_initial_attempt(
        connection,
        claim.billing_scope_id(),
        claim.subscriber_id(),
        claim.plan_key(),
    )
    .await
}

pub(super) async fn blocking_initial_attempt(
    connection: &mut PgConnection,
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
    .fetch_one(&mut *connection)
    .await
}

pub(super) fn quote_from_row(
    row: &PgRow,
    offer: &SubscriptionOffer,
) -> Result<SubscriptionDiscountCodeQuote, SubscriptionDiscountOperationError> {
    quote_for_offer(code_from_row(row)?, offer)
}

pub(super) fn quote_for_offer(
    code: SubscriptionDiscountCodeRecord,
    offer: &SubscriptionOffer,
) -> Result<SubscriptionDiscountCodeQuote, SubscriptionDiscountOperationError> {
    SubscriptionDiscountCodeQuote::new(code, offer).map_err(|error| match error {
        SubscriptionDiscountError::LimitedDiscountCadence => {
            SubscriptionDiscountOperationError::LimitedDiscountCadence
        }
        _ => SubscriptionDiscountOperationError::InvalidState(INVALID_DISCOUNT_STATE),
    })
}

pub(super) fn code_from_row(
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

pub(super) fn claim_from_row(
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

pub(super) fn discount_kind_from_row(
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

pub(super) fn discount_duration_from_row(
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

pub(super) fn parse_code_status(
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

pub(super) fn parse_claim_status(
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

pub(super) fn discount_value(kind: SubscriptionDiscountKind) -> (Option<i32>, Option<i32>) {
    match kind {
        SubscriptionDiscountKind::AmountOffCents(value) => (Some(value.get()), None),
        SubscriptionDiscountKind::PercentOffBasisPoints(value) => {
            (None, Some(i32::from(value.get())))
        }
    }
}

pub(super) fn duration_months(duration: SubscriptionDiscountDuration) -> Option<i32> {
    match duration {
        SubscriptionDiscountDuration::Indefinite => None,
        SubscriptionDiscountDuration::LimitedMonths(value) => Some(i32::from(value.get())),
    }
}
