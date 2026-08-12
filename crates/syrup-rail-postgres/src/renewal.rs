use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, Row, Transaction};
use syrup_rail::{
    BillingScopeId, PaymentAttemptId, PaymentResolutionCode, RenewalAttemptState, RenewalDispatch,
    RenewalDispatchPage, RenewalDispatchPageCursor, SubscriptionId,
};
use thiserror::Error;

const PAYMENT_METHOD_UPDATE_UNSUBMITTED_STALE_AFTER_SECONDS: i64 = 3 * 60;

#[derive(Debug, Error)]
pub enum RenewalStoreError {
    #[error("renewal storage operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("a gateway provider has no canonical cooldown row")]
    MissingProviderCooldown,
}

/// Returns the first deterministic, provider-neutral renewal dispatch page.
///
/// This compatibility wrapper keeps the historical fixed one-hundred-item
/// first-page limit and ascending order. Hosts that need to drain one stable
/// observed scan should use [`due_renewals_page`] and retain its cursor.
pub async fn due_renewals(pool: &PgPool) -> Result<Vec<RenewalDispatch>, RenewalStoreError> {
    Ok(due_renewals_page(pool, None).await?.into_dispatches())
}

/// Returns one deterministic page of renewal work due in a stable scan.
///
/// On the first page PostgreSQL supplies one observed timestamp. Every
/// continuation preserves it for every time-dependent eligibility gate, while
/// a strict ascending `(next_payment_attempt_at, subscription_id)` key avoids
/// offset and timestamp-tie gaps or repeats for unchanged candidates. This is
/// not a cross-page MVCC snapshot: concurrently inserted, retimed, or
/// unblocked candidates behind the continuation key wait for a fresh scan.
/// Nor is it a dispatch lease: hosts own queue/outbox persistence and the
/// eventual renewal operation revalidates mutable state.
pub async fn due_renewals_page(
    pool: &PgPool,
    cursor: Option<&RenewalDispatchPageCursor>,
) -> Result<RenewalDispatchPage, RenewalStoreError> {
    let observed_at = match cursor {
        Some(cursor) => cursor.observed_at(),
        None => {
            sqlx::query_scalar::<_, DateTime<Utc>>("SELECT clock_timestamp()")
                .fetch_one(pool)
                .await?
        }
    };
    let missing_provider_cooldown = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM billing_gateway_accounts AS accounts
            LEFT JOIN billing_gateway_provider_rate_limits AS provider_limits
                ON provider_limits.provider_key = accounts.provider_key
            WHERE provider_limits.provider_key IS NULL
        )
        "#,
    )
    .fetch_one(pool)
    .await?;
    if missing_provider_cooldown {
        return Err(RenewalStoreError::MissingProviderCooldown);
    }

    let infrastructure_retry_codes =
        resolution_strings(PaymentResolutionCode::RENEWAL_INFRASTRUCTURE_RETRY_CODES);
    let infrastructure_pacing_codes =
        resolution_strings(PaymentResolutionCode::RENEWAL_INFRASTRUCTURE_PACING_CODES);
    let rows = sqlx::query(
        r#"
        WITH eligible_subscriptions AS MATERIALIZED (
            SELECT
                subscriptions.billing_scope_id,
                subscriptions.id,
                subscriptions.next_renewal_at,
                subscriptions.next_payment_attempt_at,
                accounts.gateway_configuration_id
            FROM billing_subscriptions AS subscriptions
            JOIN billing_gateway_accounts AS accounts
                ON accounts.billing_scope_id = subscriptions.billing_scope_id
                AND accounts.id = subscriptions.gateway_account_id
            JOIN billing_gateway_provider_rate_limits AS provider_limits
                ON provider_limits.provider_key = accounts.provider_key
            WHERE subscriptions.status IN ('active', 'past_due')
                AND subscriptions.next_payment_attempt_at <= $10::timestamptz
                AND provider_limits.rate_limited_until <= $10::timestamptz
                AND (
                    accounts.mutation_rate_limited_until IS NULL
                    OR accounts.mutation_rate_limited_until <= $10::timestamptz
                )
                AND NOT EXISTS (
                    SELECT 1
                    FROM billing_payment_attempts AS update_attempts
                    WHERE update_attempts.subscription_id = subscriptions.id
                        AND update_attempts.attempt_kind = 'subscription_payment_method_update'
                        AND update_attempts.status IN ('pending', 'unknown', 'review_required')
                        AND NOT (
                            update_attempts.status = 'pending'
                            AND update_attempts.submitted_at IS NULL
                            AND update_attempts.created_at <= $10::timestamptz
                                - ($4::bigint * interval '1 second')
                        )
                )
                AND (
                    $11::timestamptz IS NULL
                    OR (subscriptions.next_payment_attempt_at, subscriptions.id)
                        > ($11::timestamptz, $12::uuid)
                )
        )
        SELECT subscriptions.billing_scope_id, subscriptions.id,
            subscriptions.next_renewal_at,
            subscriptions.next_payment_attempt_at,
            COALESCE(renewal_attempts.attempt_sequence_count, 0)::bigint
                AS attempt_sequence_count
        FROM eligible_subscriptions AS subscriptions
        LEFT JOIN LATERAL (
            SELECT
                COUNT(*) FILTER (
                    WHERE attempts.attempt_kind = 'subscription_renewal'
                        AND attempts.status = 'failed'
                        AND attempts.resolution_code = ANY($1::text[])
                        AND attempts.gateway_configuration_id IS NOT DISTINCT FROM
                            subscriptions.gateway_configuration_id
                ) AS automatic_infrastructure_attempt_count,
                MAX(attempts.resolved_at) FILTER (
                    WHERE attempts.attempt_kind = 'subscription_renewal'
                        AND attempts.status = 'failed'
                        AND attempts.resolution_code = ANY($2::text[])
                        AND attempts.gateway_configuration_id IS NOT DISTINCT FROM
                            subscriptions.gateway_configuration_id
                ) AS last_automatic_infrastructure_failure_at,
                COUNT(*) FILTER (
                    WHERE attempts.attempt_kind = 'subscription_renewal'
                        AND attempts.status = 'failed'
                        AND attempts.resolution_code = $3
                ) AS provider_rate_limited_attempt_count,
                MAX(attempts.resolved_at) FILTER (
                    WHERE attempts.attempt_kind = 'subscription_renewal'
                        AND attempts.status = 'failed'
                        AND attempts.resolution_code = $3
                ) AS last_provider_rate_limited_at,
                COUNT(*) AS attempt_sequence_count,
                BOOL_OR(attempts.status IN ('pending', 'unknown', 'review_required', 'approved'))
                    AS has_blocking_attempt
            FROM billing_payment_attempts AS attempts
            WHERE attempts.subscription_id = subscriptions.id
                AND attempts.billing_period_start_at = subscriptions.next_renewal_at
                AND attempts.attempt_kind IN ('subscription_renewal', 'subscription_recovery')
        ) AS renewal_attempts ON true
        WHERE COALESCE(renewal_attempts.has_blocking_attempt, false) = false
            AND COALESCE(renewal_attempts.automatic_infrastructure_attempt_count, 0)
                < $5
            AND (
                renewal_attempts.last_automatic_infrastructure_failure_at IS NULL
                OR renewal_attempts.last_automatic_infrastructure_failure_at
                    <= $10::timestamptz - ($6::bigint * interval '1 second')
            )
            AND (
                renewal_attempts.last_provider_rate_limited_at IS NULL
                OR renewal_attempts.last_provider_rate_limited_at <= $10::timestamptz
                    - (
                        CASE WHEN COALESCE(
                            renewal_attempts.provider_rate_limited_attempt_count,
                            0
                        ) >= $7 THEN $8::bigint ELSE $9::bigint END
                        * interval '1 second'
                    )
            )
        ORDER BY subscriptions.next_payment_attempt_at ASC, subscriptions.id ASC
        LIMIT $13
        "#,
    )
    .bind(&infrastructure_retry_codes)
    .bind(&infrastructure_pacing_codes)
    .bind(PaymentResolutionCode::GatewayProviderRateLimitedBeforeSubmission.as_str())
    .bind(PAYMENT_METHOD_UPDATE_UNSUBMITTED_STALE_AFTER_SECONDS)
    .bind(syrup_rail::MAX_RENEWAL_INFRASTRUCTURE_ATTEMPTS_PER_PERIOD_CONFIGURATION)
    .bind(syrup_rail::RENEWAL_INFRASTRUCTURE_RETRY_AFTER_SECONDS)
    .bind(syrup_rail::RENEWAL_PROVIDER_RATE_LIMIT_FAST_RETRY_ATTEMPTS)
    .bind(syrup_rail::RENEWAL_PROVIDER_RATE_LIMIT_SLOW_RETRY_AFTER_SECONDS)
    .bind(syrup_rail::RENEWAL_PROVIDER_RATE_LIMIT_RETRY_AFTER_SECONDS)
    .bind(observed_at)
    .bind(cursor.map(|value| value.next_payment_attempt_at()))
    .bind(cursor.map(|value| value.subscription_id().into_uuid()))
    .bind(syrup_rail::RENEWAL_DISPATCH_LIMIT + 1)
    .fetch_all(pool)
    .await?;
    let mut candidates = rows
        .iter()
        .map(|row| {
            let subscription_id = SubscriptionId::new(row.try_get("id")?);
            Ok(DueRenewalCandidate {
                dispatch: RenewalDispatch::new(
                    BillingScopeId::new(row.try_get("billing_scope_id")?),
                    subscription_id,
                    row.try_get("next_renewal_at")?,
                    row.try_get("attempt_sequence_count")?,
                ),
                next_payment_attempt_at: row.try_get("next_payment_attempt_at")?,
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()?;
    let has_more = candidates.len() > syrup_rail::RENEWAL_DISPATCH_LIMIT as usize;
    if has_more {
        candidates.pop();
    }
    let next_cursor = has_more.then(|| {
        let last = candidates
            .last()
            .expect("a renewal page with an extra row always retains one item");
        RenewalDispatchPageCursor::new(
            observed_at,
            last.next_payment_attempt_at,
            last.dispatch.subscription_id(),
        )
    });
    Ok(RenewalDispatchPage::new(
        candidates
            .into_iter()
            .map(|candidate| candidate.dispatch)
            .collect(),
        next_cursor,
    ))
}

struct DueRenewalCandidate {
    dispatch: RenewalDispatch,
    next_payment_attempt_at: DateTime<Utc>,
}

/// Computes the one shared renewal/recovery period ledger state.
pub async fn renewal_attempt_state(
    transaction: &mut Transaction<'_, Postgres>,
    subscription_id: SubscriptionId,
    period_start_at: DateTime<Utc>,
    excluded_attempt_id: Option<PaymentAttemptId>,
) -> Result<RenewalAttemptState, RenewalStoreError> {
    let infrastructure_retry_codes =
        resolution_strings(PaymentResolutionCode::RENEWAL_INFRASTRUCTURE_RETRY_CODES);
    let infrastructure_pacing_codes =
        resolution_strings(PaymentResolutionCode::RENEWAL_INFRASTRUCTURE_PACING_CODES);
    let row = sqlx::query(
        r#"
        SELECT
            COUNT(*) AS attempt_sequence_count,
            COUNT(*) FILTER (
                WHERE attempt_kind = 'subscription_renewal'
                    AND status = 'failed'
                    AND resolution_code = ANY($4::text[])
                    AND gateway_configuration_id = (
                        SELECT accounts.gateway_configuration_id
                        FROM billing_subscriptions AS subscriptions
                        JOIN billing_gateway_accounts AS accounts
                            ON accounts.id = subscriptions.gateway_account_id
                            AND accounts.billing_scope_id = subscriptions.billing_scope_id
                        WHERE subscriptions.id = $1
                    )
            ) AS automatic_infrastructure_attempt_count,
            MAX(resolved_at) FILTER (
                WHERE attempt_kind = 'subscription_renewal'
                    AND status = 'failed'
                    AND resolution_code = ANY($5::text[])
                    AND gateway_configuration_id = (
                        SELECT accounts.gateway_configuration_id
                        FROM billing_subscriptions AS subscriptions
                        JOIN billing_gateway_accounts AS accounts
                            ON accounts.id = subscriptions.gateway_account_id
                            AND accounts.billing_scope_id = subscriptions.billing_scope_id
                        WHERE subscriptions.id = $1
                    )
            ) AS last_automatic_infrastructure_failure_at,
            COUNT(*) FILTER (
                WHERE attempt_kind = 'subscription_renewal'
                    AND status = 'failed'
                    AND resolution_code = $6
            ) AS provider_rate_limited_attempt_count,
            MAX(resolved_at) FILTER (
                WHERE attempt_kind = 'subscription_renewal'
                    AND status = 'failed'
                    AND resolution_code = $6
            ) AS last_provider_rate_limited_at,
            COALESCE(
                BOOL_OR(status IN ('pending', 'unknown', 'review_required', 'approved')),
                false
            ) AS has_blocking_attempt
        FROM billing_payment_attempts
        WHERE attempt_kind IN ('subscription_renewal', 'subscription_recovery')
            AND subscription_id = $1
            AND billing_period_start_at = $2
            AND ($3::uuid IS NULL OR id <> $3)
        "#,
    )
    .bind(subscription_id.as_uuid())
    .bind(period_start_at)
    .bind(excluded_attempt_id.map(PaymentAttemptId::into_uuid))
    .bind(&infrastructure_retry_codes)
    .bind(&infrastructure_pacing_codes)
    .bind(PaymentResolutionCode::GatewayProviderRateLimitedBeforeSubmission.as_str())
    .fetch_one(&mut **transaction)
    .await?;
    Ok(RenewalAttemptState {
        attempt_sequence_count: row.try_get("attempt_sequence_count")?,
        automatic_infrastructure_attempt_count: row
            .try_get("automatic_infrastructure_attempt_count")?,
        last_automatic_infrastructure_failure_at: row
            .try_get("last_automatic_infrastructure_failure_at")?,
        provider_rate_limited_attempt_count: row.try_get("provider_rate_limited_attempt_count")?,
        last_provider_rate_limited_at: row.try_get("last_provider_rate_limited_at")?,
        has_blocking_attempt: row.try_get("has_blocking_attempt")?,
    })
}

fn resolution_strings(codes: &[PaymentResolutionCode]) -> Vec<&'static str> {
    codes.iter().map(|code| code.as_str()).collect()
}

#[cfg(test)]
mod tests;
