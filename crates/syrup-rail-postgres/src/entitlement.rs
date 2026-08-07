use sqlx::{PgPool, Row, postgres::PgRow};
use syrup_rail::{
    ActorId, AppliedSubscriptionDiscount, BillingPeriod, ChargeAmount, CurrencyCode,
    DiscountClaimId, Entitlement, EntitlementQuery, LimitedDiscountMonths,
    MissingSubscriptionAction, PastDueAction, PaymentMethodId, PercentOffBasisPoints,
    PositiveDiscountCents, SavedSubscriptionDiscount, Subscription, SubscriptionDiscountCode,
    SubscriptionDiscountDuration, SubscriptionDiscountKind, SubscriptionDiscountSnapshot,
    SubscriptionGrant, SubscriptionGrantId, SubscriptionGrantKind, SubscriptionId,
    SubscriptionStatus,
};
use thiserror::Error;
use uuid::Uuid;

const INVALID_ENTITLEMENT_STATE: &str =
    "canonical subscription state cannot be represented as one entitlement";

#[derive(Debug, Error)]
pub enum EntitlementQueryError {
    #[error("subscription entitlement query failed")]
    Sql(#[from] sqlx::Error),
    #[error("{0}")]
    InvalidState(&'static str),
}

/// Loads one exact scope/subscriber/plan entitlement from a single database snapshot.
///
/// Gateway availability and host authentication are intentionally outside this query.
pub async fn entitlement(
    pool: &PgPool,
    query: &EntitlementQuery,
) -> Result<Entitlement, EntitlementQueryError> {
    let row = sqlx::query(
        r#"
        WITH clock AS MATERIALIZED (
            SELECT clock_timestamp() AS observed_at
        ),
        paid_candidates AS MATERIALIZED (
            SELECT subscriptions.*
            FROM billing_subscriptions subscriptions
            CROSS JOIN clock
            WHERE subscriptions.billing_scope_id = $1
                AND subscriptions.subscriber_id = $2
                AND subscriptions.plan_key = $3
                AND (
                    subscriptions.status IN ('active', 'past_due')
                    OR (
                        subscriptions.status = 'canceled'
                        AND subscriptions.current_period_end_at > clock.observed_at
                    )
                )
        ),
        active_grants AS MATERIALIZED (
            SELECT grants.*
            FROM billing_subscription_grants grants
            CROSS JOIN clock
            WHERE grants.billing_scope_id = $1
                AND grants.subscriber_id = $2
                AND grants.plan_key = $3
                AND grants.starts_at <= clock.observed_at
                AND grants.ends_at > clock.observed_at
                AND grants.revoked_at IS NULL
        ),
        paid AS MATERIALIZED (
            SELECT *
            FROM paid_candidates
            ORDER BY
                CASE status WHEN 'active' THEN 0 WHEN 'past_due' THEN 1 ELSE 2 END,
                updated_at DESC,
                id DESC
            LIMIT 1
        ),
        active_grant AS MATERIALIZED (
            SELECT *
            FROM active_grants
            ORDER BY ends_at DESC, created_at DESC, id DESC
            LIMIT 1
        )
        SELECT
            (SELECT count(*) FROM paid_candidates) AS paid_count,
            (SELECT count(*) FROM active_grants) AS grant_count,
            paid.id AS paid_id,
            paid.plan_key AS paid_plan_key,
            paid.status AS paid_status,
            paid.payment_method_id AS paid_payment_method_id,
            paid.amount_cents AS paid_amount_cents,
            paid.currency AS paid_currency,
            paid.current_period_start_at AS paid_period_start_at,
            paid.current_period_end_at AS paid_period_end_at,
            paid.next_renewal_at AS paid_next_renewal_at,
            active_grant.id AS grant_id,
            active_grant.plan_key AS grant_plan_key,
            active_grant.grant_kind AS grant_kind,
            active_grant.starts_at AS grant_starts_at,
            active_grant.ends_at AS grant_ends_at,
            active_grant.granted_by_actor_id AS grant_actor_id,
            EXISTS (
                SELECT 1
                FROM billing_payment_attempts attempts
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
                        SELECT 1
                        FROM billing_subscriptions later_subscription
                        WHERE later_subscription.billing_scope_id = attempts.billing_scope_id
                            AND later_subscription.subscriber_id = attempts.subscriber_id
                            AND later_subscription.plan_key = attempts.plan_key
                            AND later_subscription.created_at >= attempts.created_at
                    )
            ) AS blocking_initial_attempt,
            EXISTS (
                SELECT 1
                FROM billing_payment_attempts attempts
                WHERE attempts.subscription_id = paid.id
                    AND attempts.attempt_kind IN (
                        'subscription_renewal',
                        'subscription_recovery'
                    )
                    AND attempts.status IN ('pending', 'unknown', 'review_required')
            ) AS pending_recovery_confirmation,
            saved.id AS saved_claim_id,
            saved.code_snapshot AS saved_code,
            saved.label_snapshot AS saved_label,
            saved.discount_kind AS saved_kind,
            saved.amount_off_cents AS saved_amount_off_cents,
            saved.percent_off_bps AS saved_percent_off_bps,
            saved.currency AS saved_currency,
            saved.duration AS saved_duration,
            saved.duration_months AS saved_duration_months,
            saved.base_amount_cents AS saved_base_amount_cents,
            saved.discounted_amount_cents AS saved_discounted_amount_cents,
            applied.discount_claim_id AS applied_claim_id,
            applied.code_snapshot AS applied_code,
            applied.label_snapshot AS applied_label,
            applied.discount_kind AS applied_kind,
            applied.amount_off_cents AS applied_amount_off_cents,
            applied.percent_off_bps AS applied_percent_off_bps,
            applied.currency AS applied_currency,
            applied.duration AS applied_duration,
            applied.duration_months AS applied_duration_months,
            applied.base_amount_cents AS applied_base_amount_cents,
            applied.discounted_amount_cents AS applied_discounted_amount_cents,
            applied.periods_total AS applied_periods_total,
            applied.periods_applied AS applied_periods_applied
        FROM (SELECT 1) seed
        LEFT JOIN paid ON true
        LEFT JOIN active_grant ON true
        LEFT JOIN LATERAL (
            SELECT claims.*
            FROM billing_subscription_discount_claims claims
            WHERE claims.billing_scope_id = $1
                AND claims.subscriber_id = $2
                AND claims.plan_key = $3
                AND claims.status = 'saved'
            ORDER BY claims.claimed_at DESC, claims.id DESC
            LIMIT 1
        ) saved ON true
        LEFT JOIN LATERAL (
            SELECT discounts.*
            FROM billing_subscription_discounts discounts
            WHERE discounts.subscription_id = paid.id
                AND discounts.billing_scope_id = $1
                AND discounts.subscriber_id = $2
                AND discounts.plan_key = $3
                AND discounts.status = 'active'
            LIMIT 1
        ) applied ON true
        "#,
    )
    .bind(query.billing_scope_id().as_uuid())
    .bind(query.subscriber_id().as_uuid())
    .bind(query.plan_key().as_str())
    .fetch_one(pool)
    .await?;

    entitlement_from_row(&row)
}

fn entitlement_from_row(row: &PgRow) -> Result<Entitlement, EntitlementQueryError> {
    let paid_count: i64 = row.try_get("paid_count")?;
    let grant_count: i64 = row.try_get("grant_count")?;
    if paid_count > 1 || grant_count > 1 || (paid_count > 0 && grant_count > 0) {
        return Err(EntitlementQueryError::InvalidState(
            INVALID_ENTITLEMENT_STATE,
        ));
    }

    if grant_count == 1 {
        return Ok(Entitlement::Granted {
            grant: grant_from_row(row)?,
        });
    }

    let Some(subscription_id) = row.try_get::<Option<Uuid>, _>("paid_id")? else {
        let next_action = if row.try_get("blocking_initial_attempt")? {
            MissingSubscriptionAction::ConfirmInitialPayment
        } else {
            MissingSubscriptionAction::StartSubscription
        };
        return Ok(Entitlement::Missing {
            next_action,
            saved_discount: saved_discount_from_row(row)?,
        });
    };

    let status = row
        .try_get::<String, _>("paid_status")?
        .parse::<SubscriptionStatus>()
        .map_err(|_| EntitlementQueryError::InvalidState(INVALID_ENTITLEMENT_STATE))?;
    let subscription = Subscription::new(
        SubscriptionId::new(subscription_id),
        row.try_get::<String, _>("paid_plan_key")?
            .parse()
            .map_err(|_| EntitlementQueryError::InvalidState(INVALID_ENTITLEMENT_STATE))?,
        status,
        PaymentMethodId::new(row.try_get("paid_payment_method_id")?),
        ChargeAmount::new(
            row.try_get("paid_amount_cents")?,
            CurrencyCode::new(&row.try_get::<String, _>("paid_currency")?)
                .map_err(|_| EntitlementQueryError::InvalidState(INVALID_ENTITLEMENT_STATE))?,
        )
        .map_err(|_| EntitlementQueryError::InvalidState(INVALID_ENTITLEMENT_STATE))?,
        BillingPeriod::new(
            row.try_get("paid_period_start_at")?,
            row.try_get("paid_period_end_at")?,
        )
        .map_err(|_| EntitlementQueryError::InvalidState(INVALID_ENTITLEMENT_STATE))?,
        row.try_get("paid_next_renewal_at")?,
    );
    let applied_discount = applied_discount_from_row(row)?;

    Ok(match status {
        SubscriptionStatus::Active => Entitlement::PaidActive {
            subscription,
            applied_discount,
        },
        SubscriptionStatus::PastDue => Entitlement::PastDue {
            subscription,
            next_action: if row.try_get("pending_recovery_confirmation")? {
                PastDueAction::ConfirmRecoveryPayment
            } else {
                PastDueAction::RecoverPayment
            },
            applied_discount,
        },
        SubscriptionStatus::Canceled => Entitlement::PaidThroughCancellation {
            subscription,
            applied_discount,
        },
    })
}

fn grant_from_row(row: &PgRow) -> Result<SubscriptionGrant, EntitlementQueryError> {
    SubscriptionGrant::new(
        SubscriptionGrantId::new(required(row, "grant_id")?),
        required::<String>(row, "grant_plan_key")?
            .parse()
            .map_err(|_| EntitlementQueryError::InvalidState(INVALID_ENTITLEMENT_STATE))?,
        required::<String>(row, "grant_kind")?
            .parse::<SubscriptionGrantKind>()
            .map_err(|_| EntitlementQueryError::InvalidState(INVALID_ENTITLEMENT_STATE))?,
        required(row, "grant_starts_at")?,
        required(row, "grant_ends_at")?,
        ActorId::new(required(row, "grant_actor_id")?),
    )
    .map_err(|_| EntitlementQueryError::InvalidState(INVALID_ENTITLEMENT_STATE))
}

fn saved_discount_from_row(
    row: &PgRow,
) -> Result<Option<SavedSubscriptionDiscount>, EntitlementQueryError> {
    let Some(claim_id) = row.try_get::<Option<Uuid>, _>("saved_claim_id")? else {
        return Ok(None);
    };
    Ok(Some(SavedSubscriptionDiscount::new(
        DiscountClaimId::new(claim_id),
        discount_snapshot(row, "saved")?,
    )))
}

fn applied_discount_from_row(
    row: &PgRow,
) -> Result<Option<AppliedSubscriptionDiscount>, EntitlementQueryError> {
    let Some(code) = row.try_get::<Option<String>, _>("applied_code")? else {
        return Ok(None);
    };
    let duration = discount_duration(
        &required::<String>(row, "applied_duration")?,
        row.try_get("applied_duration_months")?,
    )?;
    let periods_remaining = match duration {
        SubscriptionDiscountDuration::Indefinite => None,
        SubscriptionDiscountDuration::LimitedMonths(_) => {
            let total: i32 = required(row, "applied_periods_total")?;
            let applied: i32 = required(row, "applied_periods_applied")?;
            Some(
                u8::try_from(total - applied)
                    .map_err(|_| EntitlementQueryError::InvalidState(INVALID_ENTITLEMENT_STATE))?,
            )
        }
    };
    let snapshot = discount_snapshot_from_values(row, "applied", code, duration)?;
    AppliedSubscriptionDiscount::new(
        row.try_get::<Option<Uuid>, _>("applied_claim_id")?
            .map(DiscountClaimId::new),
        snapshot,
        periods_remaining,
    )
    .map(Some)
    .map_err(|_| EntitlementQueryError::InvalidState(INVALID_ENTITLEMENT_STATE))
}

fn discount_snapshot(
    row: &PgRow,
    prefix: &str,
) -> Result<SubscriptionDiscountSnapshot, EntitlementQueryError> {
    let code = required::<String>(row, &format!("{prefix}_code"))?;
    let duration = discount_duration(
        &required::<String>(row, &format!("{prefix}_duration"))?,
        row.try_get(format!("{prefix}_duration_months").as_str())?,
    )?;
    discount_snapshot_from_values(row, prefix, code, duration)
}

fn discount_snapshot_from_values(
    row: &PgRow,
    prefix: &str,
    code: String,
    duration: SubscriptionDiscountDuration,
) -> Result<SubscriptionDiscountSnapshot, EntitlementQueryError> {
    let kind = match required::<String>(row, &format!("{prefix}_kind"))?.as_str() {
        "amount_off" => SubscriptionDiscountKind::AmountOffCents(
            PositiveDiscountCents::new(required(row, &format!("{prefix}_amount_off_cents"))?)
                .map_err(|_| EntitlementQueryError::InvalidState(INVALID_ENTITLEMENT_STATE))?,
        ),
        "percent_off" => SubscriptionDiscountKind::PercentOffBasisPoints(
            PercentOffBasisPoints::new(
                u16::try_from(required::<i32>(row, &format!("{prefix}_percent_off_bps"))?)
                    .map_err(|_| EntitlementQueryError::InvalidState(INVALID_ENTITLEMENT_STATE))?,
            )
            .map_err(|_| EntitlementQueryError::InvalidState(INVALID_ENTITLEMENT_STATE))?,
        ),
        _ => {
            return Err(EntitlementQueryError::InvalidState(
                INVALID_ENTITLEMENT_STATE,
            ));
        }
    };
    let currency = CurrencyCode::new(&required::<String>(row, &format!("{prefix}_currency"))?)
        .map_err(|_| EntitlementQueryError::InvalidState(INVALID_ENTITLEMENT_STATE))?;
    SubscriptionDiscountSnapshot::new(
        SubscriptionDiscountCode::new(&code)
            .map_err(|_| EntitlementQueryError::InvalidState(INVALID_ENTITLEMENT_STATE))?,
        row.try_get(format!("{prefix}_label").as_str())?,
        kind,
        duration,
        ChargeAmount::new(
            required(row, &format!("{prefix}_base_amount_cents"))?,
            currency,
        )
        .map_err(|_| EntitlementQueryError::InvalidState(INVALID_ENTITLEMENT_STATE))?,
        ChargeAmount::new(
            required(row, &format!("{prefix}_discounted_amount_cents"))?,
            currency,
        )
        .map_err(|_| EntitlementQueryError::InvalidState(INVALID_ENTITLEMENT_STATE))?,
    )
    .map_err(|_| EntitlementQueryError::InvalidState(INVALID_ENTITLEMENT_STATE))
}

fn discount_duration(
    duration: &str,
    duration_months: Option<i32>,
) -> Result<SubscriptionDiscountDuration, EntitlementQueryError> {
    match (duration, duration_months) {
        ("indefinite", None) => Ok(SubscriptionDiscountDuration::Indefinite),
        ("limited_months", Some(months)) => Ok(SubscriptionDiscountDuration::LimitedMonths(
            LimitedDiscountMonths::new(
                u8::try_from(months)
                    .map_err(|_| EntitlementQueryError::InvalidState(INVALID_ENTITLEMENT_STATE))?,
            )
            .map_err(|_| EntitlementQueryError::InvalidState(INVALID_ENTITLEMENT_STATE))?,
        )),
        _ => Err(EntitlementQueryError::InvalidState(
            INVALID_ENTITLEMENT_STATE,
        )),
    }
}

fn required<T>(row: &PgRow, column: &str) -> Result<T, EntitlementQueryError>
where
    for<'r> T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    row.try_get::<Option<T>, _>(column)?
        .ok_or(EntitlementQueryError::InvalidState(
            INVALID_ENTITLEMENT_STATE,
        ))
}

#[cfg(test)]
mod tests {
    use std::{error::Error, io};

    use syrup_rail::{BillingScopeId, Entitlement, EntitlementQuery, PlanKey, SubscriberId};
    use uuid::Uuid;

    use super::{EntitlementQueryError, entitlement};
    use crate::test_support::TestDatabase;

    #[tokio::test]
    async fn entitlement_is_exact_lossless_and_rejects_overlapping_owners()
    -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("sr_entitle_v1").await?;
        let result = async {
            let scope = Uuid::now_v7();
            let subscriber = Uuid::now_v7();
            let plan = PlanKey::new("base_subscription")?;
            let query = EntitlementQuery::new(
                BillingScopeId::new(scope),
                SubscriberId::new(subscriber),
                plan,
            );

            if !matches!(
                entitlement(&database.pool, &query).await?,
                Entitlement::Missing {
                    next_action: syrup_rail::MissingSubscriptionAction::StartSubscription,
                    saved_discount: None,
                }
            ) {
                return Err(io::Error::other("empty aggregate did not require enrollment").into());
            }

            let discount_code = Uuid::now_v7();
            let discount_claim = Uuid::now_v7();
            sqlx::query(
                r#"
                INSERT INTO billing_subscription_discount_codes (
                    id, billing_scope_id, plan_key, code_normalized,
                    display_code, label, status, discount_kind,
                    amount_off_cents, currency, duration
                ) VALUES (
                    $1, $2, 'base_subscription', 'WELCOME10', 'WELCOME10',
                    'Welcome discount', 'active', 'amount_off', 10, 'USD',
                    'indefinite'
                )
                "#,
            )
            .bind(discount_code)
            .bind(scope)
            .execute(&database.pool)
            .await?;
            sqlx::query(
                r#"
                INSERT INTO billing_subscription_discount_claims (
                    id, billing_scope_id, subscriber_id, plan_key,
                    discount_code_id, code_snapshot, label_snapshot,
                    discount_kind, amount_off_cents, currency, duration,
                    base_amount_cents, discounted_amount_cents, status
                ) VALUES (
                    $1, $2, $3, 'base_subscription', $4, 'WELCOME10',
                    'Welcome discount', 'amount_off', 10, 'USD',
                    'indefinite', 100, 90, 'saved'
                )
                "#,
            )
            .bind(discount_claim)
            .bind(scope)
            .bind(subscriber)
            .bind(discount_code)
            .execute(&database.pool)
            .await?;
            match entitlement(&database.pool, &query).await? {
                Entitlement::Missing {
                    saved_discount: Some(discount),
                    ..
                } if discount.snapshot().label() == Some("Welcome discount") => {}
                _ => {
                    return Err(io::Error::other(
                        "saved discount snapshot was not projected losslessly",
                    )
                    .into());
                }
            }

            let grant_id = Uuid::now_v7();
            sqlx::query(
                r#"
                INSERT INTO billing_subscription_grants (
                    id, billing_scope_id, subscriber_id, plan_key, grant_kind,
                    reason, starts_at, ends_at, granted_by_actor_id
                ) VALUES (
                    $1, $2, $3, 'base_subscription', 'promotion', 'launch',
                    clock_timestamp() - interval '1 minute',
                    clock_timestamp() + interval '1 day', $4
                )
                "#,
            )
            .bind(grant_id)
            .bind(scope)
            .bind(subscriber)
            .bind(Uuid::now_v7())
            .execute(&database.pool)
            .await?;
            if !matches!(
                entitlement(&database.pool, &query).await?,
                Entitlement::Granted { grant } if grant.id().into_uuid() == grant_id
            ) {
                return Err(io::Error::other("active grant did not own entitlement").into());
            }
            sqlx::query("DELETE FROM billing_subscription_grants WHERE id = $1")
                .bind(grant_id)
                .execute(&database.pool)
                .await?;

            let provider = "test_gateway";
            let account = Uuid::now_v7();
            let configuration = Uuid::now_v7();
            sqlx::query(
                "INSERT INTO billing_gateway_provider_rate_limits (provider_key) VALUES ($1)",
            )
            .bind(provider)
            .execute(&database.pool)
            .await?;
            sqlx::query(
                r#"
                INSERT INTO billing_gateway_accounts (
                    id, billing_scope_id, provider_key, gateway_configuration_id
                ) VALUES ($1, $2, $3, $4)
                "#,
            )
            .bind(account)
            .bind(scope)
            .bind(provider)
            .bind(configuration)
            .execute(&database.pool)
            .await?;
            let method = Uuid::now_v7();
            sqlx::query(
                r#"
                INSERT INTO billing_payment_methods (
                    id, billing_scope_id, subscriber_id, gateway_account_id,
                    gateway_payment_method_reference, status
                ) VALUES ($1, $2, $3, $4, $5, 'active')
                "#,
            )
            .bind(method)
            .bind(scope)
            .bind(subscriber)
            .bind(account)
            .bind(format!("vault_{}", method.simple()))
            .execute(&database.pool)
            .await?;
            let subscription = Uuid::now_v7();
            sqlx::query(
                r#"
                INSERT INTO billing_subscriptions (
                    id, billing_scope_id, subscriber_id, plan_key, status,
                    gateway_account_id, payment_method_id, amount_cents,
                    currency, current_period_start_at, current_period_end_at,
                    next_renewal_at, initial_transaction_id
                ) VALUES (
                    $1, $2, $3, 'base_subscription', 'active', $4, $5, 100,
                    'USD', clock_timestamp() - interval '1 day',
                    clock_timestamp() + interval '1 day',
                    clock_timestamp() + interval '1 day', $6
                )
                "#,
            )
            .bind(subscription)
            .bind(scope)
            .bind(subscriber)
            .bind(account)
            .bind(method)
            .bind(format!("txn_{}", subscription.simple()))
            .execute(&database.pool)
            .await?;
            sqlx::query(
                r#"
                INSERT INTO billing_subscription_discounts (
                    subscription_id, billing_scope_id, subscriber_id,
                    plan_key, code_snapshot, label_snapshot, discount_kind,
                    amount_off_cents, currency, duration, base_amount_cents,
                    discounted_amount_cents, periods_applied, status
                ) VALUES (
                    $1, $2, $3, 'base_subscription', 'WELCOME10',
                    'Welcome discount', 'amount_off', 10, 'USD', 'indefinite',
                    100, 90, 1, 'active'
                )
                "#,
            )
            .bind(subscription)
            .bind(scope)
            .bind(subscriber)
            .execute(&database.pool)
            .await?;
            match entitlement(&database.pool, &query).await? {
                Entitlement::PaidActive {
                    subscription: paid,
                    applied_discount: Some(discount),
                } if paid.id().into_uuid() == subscription
                    && discount.snapshot().label() == Some("Welcome discount") => {}
                _ => {
                    return Err(io::Error::other(
                        "paid entitlement or applied discount was not projected",
                    )
                    .into());
                }
            }

            let overlapping_grant = Uuid::now_v7();
            sqlx::query(
                r#"
                INSERT INTO billing_subscription_grants (
                    id, billing_scope_id, subscriber_id, plan_key, grant_kind,
                    reason, starts_at, ends_at, granted_by_actor_id
                ) VALUES (
                    $1, $2, $3, 'base_subscription', 'testing', 'overlap',
                    clock_timestamp() - interval '1 minute',
                    clock_timestamp() + interval '1 day', $4
                )
                "#,
            )
            .bind(overlapping_grant)
            .bind(scope)
            .bind(subscriber)
            .bind(Uuid::now_v7())
            .execute(&database.pool)
            .await?;
            if !matches!(
                entitlement(&database.pool, &query).await,
                Err(EntitlementQueryError::InvalidState(_))
            ) {
                return Err(io::Error::other("paid/grant overlap did not fail closed").into());
            }
            Ok::<_, Box<dyn Error>>(())
        }
        .await;
        let cleanup = database.cleanup().await;
        result?;
        cleanup
    }
}
