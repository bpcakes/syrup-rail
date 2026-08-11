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
mod tests {
    use std::{collections::HashSet, error::Error, time::Duration as StdDuration};

    use chrono::Duration;
    use uuid::Uuid;

    use super::*;
    use crate::test_support::{GatewayAccountFixture, TestDatabase, create_gateway_account};

    #[derive(Clone)]
    struct DueSubscriptionFixture {
        subscription_id: Uuid,
        subscriber_id: Uuid,
        payment_method_id: Uuid,
        plan_key: String,
        initial_transaction_id: String,
    }

    async fn insert_due_subscription(
        pool: &PgPool,
        account: GatewayAccountFixture,
        plan_key: &str,
    ) -> Result<Uuid, sqlx::Error> {
        Ok(insert_due_subscription_at(
            pool,
            account,
            plan_key,
            Uuid::now_v7(),
            Utc::now() - Duration::days(1),
        )
        .await?
        .subscription_id)
    }

    async fn insert_due_subscription_at(
        pool: &PgPool,
        account: GatewayAccountFixture,
        plan_key: &str,
        subscription_id: Uuid,
        due_at: DateTime<Utc>,
    ) -> Result<DueSubscriptionFixture, sqlx::Error> {
        let subscriber_id = Uuid::now_v7();
        let payment_method_id = Uuid::now_v7();
        let initial_transaction_id = format!("txn_{}", subscription_id.simple());
        sqlx::query(
            r#"
            INSERT INTO billing_payment_methods (
                id, billing_scope_id, subscriber_id, gateway_account_id,
                gateway_payment_method_reference, status
            ) VALUES ($1, $2, $3, $4, $5, 'active')
            "#,
        )
        .bind(payment_method_id)
        .bind(account.billing_scope_id)
        .bind(subscriber_id)
        .bind(account.gateway_account_id)
        .bind(format!("vault_{}", payment_method_id.simple()))
        .execute(pool)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO billing_subscriptions (
                id, billing_scope_id, subscriber_id, plan_key, status,
                gateway_account_id, payment_method_id, amount_cents, currency,
                current_period_start_at, current_period_end_at, next_renewal_at,
                initial_transaction_id, phase, recurring_period_kind,
                recurring_period_count, dunning_retry_delays_seconds,
                dunning_exhaustion, past_due_access, next_payment_attempt_at
            ) VALUES (
                $1, $2, $3, $4, 'active', $5, $6, 1900, 'USD', $7, $8, $8, $9,
                'recurring', 'calendar_months', 1, ARRAY[]::bigint[],
                'remain_past_due', 'suspend_immediately', $8
            )
            "#,
        )
        .bind(subscription_id)
        .bind(account.billing_scope_id)
        .bind(subscriber_id)
        .bind(plan_key)
        .bind(account.gateway_account_id)
        .bind(payment_method_id)
        .bind(due_at - Duration::days(32))
        .bind(due_at)
        .bind(&initial_transaction_id)
        .execute(pool)
        .await?;
        Ok(DueSubscriptionFixture {
            subscription_id,
            subscriber_id,
            payment_method_id,
            plan_key: plan_key.to_owned(),
            initial_transaction_id,
        })
    }

    async fn update_due_at(
        pool: &PgPool,
        subscription_id: Uuid,
        due_at: DateTime<Utc>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            UPDATE billing_subscriptions
            SET current_period_end_at = $2,
                next_renewal_at = $2,
                next_payment_attempt_at = $2
            WHERE id = $1
            "#,
        )
        .bind(subscription_id)
        .bind(due_at)
        .execute(pool)
        .await?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert_renewal_attempt(
        pool: &PgPool,
        account: GatewayAccountFixture,
        subscription: &DueSubscriptionFixture,
        period_start_at: DateTime<Utc>,
        kind: &str,
        status: &str,
        resolution_code: Option<&str>,
        resolved_at: Option<DateTime<Utc>>,
    ) -> Result<(), sqlx::Error> {
        let attempt_id = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO billing_payment_attempts (
                id, billing_scope_id, subscriber_id, plan_key, subscription_id,
                payment_method_id, attempt_kind, status, idempotency_key,
                request_fingerprint, amount_cents, currency,
                billing_period_start_at, billing_period_end_at,
                gateway_account_id, gateway_configuration_id, gateway_order_id,
                resolution_code, resolved_at,
                subscription_expected_payment_method_id,
                subscription_expected_initial_transaction_id,
                subscription_expected_status
            ) VALUES (
                $1, $2, $3, $4, $5, $6, $7, $8, $9, $10,
                1900, 'USD', $11, $12, $13, $14, $15, $16, $17,
                $6, $18, 'active'
            )
            "#,
        )
        .bind(attempt_id)
        .bind(account.billing_scope_id)
        .bind(subscription.subscriber_id)
        .bind(&subscription.plan_key)
        .bind(subscription.subscription_id)
        .bind(subscription.payment_method_id)
        .bind(kind)
        .bind(status)
        .bind(format!("attempt-{}", attempt_id.simple()))
        .bind(format!("fingerprint-{}", attempt_id.simple()))
        .bind(period_start_at)
        .bind(period_start_at + Duration::days(28))
        .bind(account.gateway_account_id)
        .bind(account.gateway_configuration_id)
        .bind(format!("order_{}", attempt_id.simple()))
        .bind(resolution_code)
        .bind(resolved_at)
        .bind(&subscription.initial_transaction_id)
        .execute(pool)
        .await?;
        Ok(())
    }

    async fn insert_pending_payment_method_update(
        pool: &PgPool,
        account: GatewayAccountFixture,
        subscription: &DueSubscriptionFixture,
        created_at: DateTime<Utc>,
    ) -> Result<(), sqlx::Error> {
        let attempt_id = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO billing_payment_attempts (
                id, billing_scope_id, subscriber_id, plan_key, subscription_id,
                payment_method_id, attempt_kind, status, idempotency_key,
                request_fingerprint, amount_cents, currency,
                gateway_account_id, gateway_configuration_id, gateway_order_id,
                payment_method_update_expected_payment_method_id,
                payment_method_update_expected_initial_transaction_id,
                created_at, updated_at
            ) VALUES (
                $1, $2, $3, $4, $5, $6,
                'subscription_payment_method_update', 'pending', $7, $8,
                0, 'USD', $9, $10, $11, $6, $12, $13, $13
            )
            "#,
        )
        .bind(attempt_id)
        .bind(account.billing_scope_id)
        .bind(subscription.subscriber_id)
        .bind(&subscription.plan_key)
        .bind(subscription.subscription_id)
        .bind(subscription.payment_method_id)
        .bind(format!("method-update-{}", attempt_id.simple()))
        .bind(format!("method-update-fingerprint-{}", attempt_id.simple()))
        .bind(account.gateway_account_id)
        .bind(account.gateway_configuration_id)
        .bind(format!("method-update-order_{}", attempt_id.simple()))
        .bind(&subscription.initial_transaction_id)
        .bind(created_at)
        .execute(pool)
        .await?;
        Ok(())
    }

    fn dispatch_ids(page: &RenewalDispatchPage) -> Vec<Uuid> {
        page.dispatches()
            .iter()
            .map(|dispatch| dispatch.subscription_id().into_uuid())
            .collect()
    }

    async fn scan_dispatch_ids(pool: &PgPool) -> Result<Vec<Uuid>, RenewalStoreError> {
        let mut cursor = None;
        let mut ids = Vec::new();
        loop {
            let page = due_renewals_page(pool, cursor.as_ref()).await?;
            ids.extend(dispatch_ids(&page));
            cursor = page.next_cursor();
            if cursor.is_none() {
                return Ok(ids);
            }
        }
    }

    #[tokio::test]
    async fn due_selection_is_provider_keyed_and_has_a_fixed_shared_bound()
    -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("renew_due").await?;
        let nmi = create_gateway_account(&database.pool, "nmi").await?;
        let other = create_gateway_account(&database.pool, "other-provider").await?;
        let nmi_subscription = insert_due_subscription(&database.pool, nmi, "nmi-plan").await?;
        let other_subscription =
            insert_due_subscription(&database.pool, other, "other-plan").await?;

        sqlx::query(
            "UPDATE billing_gateway_provider_rate_limits SET rate_limited_until = clock_timestamp() + interval '1 hour' WHERE provider_key = 'nmi'",
        )
        .execute(&database.pool)
        .await?;
        let due = due_renewals(&database.pool).await?;
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].subscription_id().into_uuid(), other_subscription);
        assert_ne!(due[0].subscription_id().into_uuid(), nmi_subscription);
        assert_eq!(syrup_rail::RENEWAL_DISPATCH_LIMIT, 100);

        database.cleanup().await
    }

    #[tokio::test]
    async fn pages_drain_205_tied_due_subscriptions_once_and_legacy_wrapper_keeps_first_page()
    -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("renew_pages_205").await?;
        let account = create_gateway_account(&database.pool, "nmi").await?;
        let due_at = sqlx::query_scalar::<_, DateTime<Utc>>(
            "SELECT clock_timestamp() - interval '5 minutes'",
        )
        .fetch_one(&database.pool)
        .await?;
        let mut expected_ids = Vec::new();
        for value in 1..=205_u128 {
            let subscription_id = Uuid::from_u128(value);
            insert_due_subscription_at(
                &database.pool,
                account,
                "page-plan",
                subscription_id,
                due_at,
            )
            .await?;
            expected_ids.push(subscription_id);
        }
        expected_ids.sort_unstable();

        let first = due_renewals_page(&database.pool, None).await?;
        assert_eq!(first.dispatches().len(), 100);
        assert!(
            first
                .dispatches()
                .iter()
                .all(|dispatch| dispatch.attempt_sequence_count() == 0)
        );
        assert!(
            first
                .dispatches()
                .iter()
                .all(|dispatch| dispatch.period_start_at() == &due_at)
        );
        let first_cursor = first
            .next_cursor()
            .expect("the first 205-row page has another page");

        let second = due_renewals_page(&database.pool, Some(&first_cursor)).await?;
        assert_eq!(second.dispatches().len(), 100);
        let second_cursor = second
            .next_cursor()
            .expect("the second 205-row page has another page");
        assert_eq!(second_cursor.observed_at(), first_cursor.observed_at());

        let third = due_renewals_page(&database.pool, Some(&second_cursor)).await?;
        assert_eq!(third.dispatches().len(), 5);
        assert_eq!(third.next_cursor(), None);

        let mut dispatched_ids = dispatch_ids(&first);
        dispatched_ids.extend(dispatch_ids(&second));
        dispatched_ids.extend(dispatch_ids(&third));
        assert_eq!(dispatched_ids, expected_ids);
        assert_eq!(
            dispatched_ids.iter().copied().collect::<HashSet<_>>().len(),
            205
        );

        let legacy = due_renewals(&database.pool).await?;
        assert_eq!(legacy.len(), 100);
        let legacy_ids = legacy
            .iter()
            .map(|dispatch| dispatch.subscription_id().into_uuid())
            .collect::<Vec<_>>();
        assert_eq!(legacy_ids, expected_ids[..100]);

        database.cleanup().await
    }

    #[tokio::test]
    async fn continuation_excludes_rows_that_become_due_after_its_observed_time()
    -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("renew_pg_observe").await?;
        let account = create_gateway_account(&database.pool, "nmi").await?;
        let due_at = Utc::now() - Duration::minutes(5);
        for value in 1..=101_u128 {
            insert_due_subscription_at(
                &database.pool,
                account,
                "observed-plan",
                Uuid::from_u128(value),
                due_at,
            )
            .await?;
        }
        let later = insert_due_subscription_at(
            &database.pool,
            account,
            "later-plan",
            Uuid::from_u128(1_000),
            Utc::now() + Duration::hours(1),
        )
        .await?;

        let first = due_renewals_page(&database.pool, None).await?;
        assert_eq!(first.dispatches().len(), 100);
        let cursor = first
            .next_cursor()
            .expect("one original due row remains after the first page");
        let newly_due_at = cursor.observed_at() + Duration::milliseconds(1);
        update_due_at(&database.pool, later.subscription_id, newly_due_at).await?;
        tokio::time::sleep(StdDuration::from_millis(10)).await;

        let continuation = due_renewals_page(&database.pool, Some(&cursor)).await?;
        assert_eq!(dispatch_ids(&continuation), vec![Uuid::from_u128(101)]);
        assert_eq!(continuation.next_cursor(), None);

        let next_scan_ids = scan_dispatch_ids(&database.pool).await?;
        assert!(next_scan_ids.contains(&later.subscription_id));
        assert_eq!(next_scan_ids.len(), 102);

        database.cleanup().await
    }

    #[tokio::test]
    async fn continuation_rechecks_current_eligibility_before_dispatching()
    -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("renew_pg_recheck").await?;
        let account = create_gateway_account(&database.pool, "nmi").await?;
        let due_at = Utc::now() - Duration::minutes(5);
        for value in 1..=100_u128 {
            insert_due_subscription_at(
                &database.pool,
                account,
                "recheck-plan",
                Uuid::from_u128(value),
                due_at,
            )
            .await?;
        }
        let blocked = insert_due_subscription_at(
            &database.pool,
            account,
            "recheck-plan",
            Uuid::from_u128(1_000),
            due_at,
        )
        .await?;

        let first = due_renewals_page(&database.pool, None).await?;
        assert_eq!(first.dispatches().len(), 100);
        let cursor = first
            .next_cursor()
            .expect("the last eligible row is on the continuation");
        insert_renewal_attempt(
            &database.pool,
            account,
            &blocked,
            due_at,
            "subscription_renewal",
            "pending",
            None,
            None,
        )
        .await?;

        let continuation = due_renewals_page(&database.pool, Some(&cursor)).await?;
        assert!(continuation.dispatches().is_empty());
        assert_eq!(continuation.next_cursor(), None);

        database.cleanup().await
    }

    #[tokio::test]
    async fn all_existing_due_renewal_eligibility_gates_remain_effective()
    -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("renew_pg_gates").await?;
        let account = create_gateway_account(&database.pool, "nmi").await?;
        let due_at = Utc::now() - Duration::minutes(10);
        let included = insert_due_subscription_at(
            &database.pool,
            account,
            "gate-plan",
            Uuid::from_u128(1),
            due_at,
        )
        .await?;
        insert_renewal_attempt(
            &database.pool,
            account,
            &included,
            due_at - Duration::days(32),
            "subscription_renewal",
            "failed",
            None,
            Some(due_at - Duration::days(31)),
        )
        .await?;
        insert_renewal_attempt(
            &database.pool,
            account,
            &included,
            due_at,
            "subscription_renewal",
            "failed",
            None,
            Some(due_at - Duration::minutes(5)),
        )
        .await?;
        let stale_update = insert_due_subscription_at(
            &database.pool,
            account,
            "stale-update-plan",
            Uuid::from_u128(2),
            due_at,
        )
        .await?;
        insert_pending_payment_method_update(
            &database.pool,
            account,
            &stale_update,
            Utc::now()
                - Duration::seconds(PAYMENT_METHOD_UPDATE_UNSUBMITTED_STALE_AFTER_SECONDS + 1),
        )
        .await?;
        let past_due = insert_due_subscription_at(
            &database.pool,
            account,
            "past-due-plan",
            Uuid::from_u128(3),
            due_at,
        )
        .await?;
        sqlx::query("UPDATE billing_subscriptions SET status = 'past_due' WHERE id = $1")
            .bind(past_due.subscription_id)
            .execute(&database.pool)
            .await?;

        let future = insert_due_subscription_at(
            &database.pool,
            account,
            "future-plan",
            Uuid::from_u128(4),
            Utc::now() + Duration::hours(1),
        )
        .await?;
        let canceled = insert_due_subscription_at(
            &database.pool,
            account,
            "canceled-plan",
            Uuid::from_u128(5),
            due_at,
        )
        .await?;
        sqlx::query(
            "UPDATE billing_subscriptions SET status = 'canceled', canceled_at = clock_timestamp(), next_payment_attempt_at = NULL WHERE id = $1",
        )
        .bind(canceled.subscription_id)
        .execute(&database.pool)
        .await?;
        let unpaid = insert_due_subscription_at(
            &database.pool,
            account,
            "unpaid-plan",
            Uuid::from_u128(6),
            due_at,
        )
        .await?;
        sqlx::query(
            "UPDATE billing_subscriptions SET status = 'unpaid', unpaid_at = clock_timestamp(), next_payment_attempt_at = NULL WHERE id = $1",
        )
        .bind(unpaid.subscription_id)
        .execute(&database.pool)
        .await?;

        let provider_cooldown_account =
            create_gateway_account(&database.pool, "provider-cooldown").await?;
        sqlx::query(
            "UPDATE billing_gateway_provider_rate_limits SET rate_limited_until = clock_timestamp() + interval '1 hour' WHERE provider_key = 'provider-cooldown'",
        )
        .execute(&database.pool)
        .await?;
        let provider_cooldown = insert_due_subscription_at(
            &database.pool,
            provider_cooldown_account,
            "provider-cooldown-plan",
            Uuid::from_u128(7),
            due_at,
        )
        .await?;

        let account_cooldown_account =
            create_gateway_account(&database.pool, "account-cooldown").await?;
        sqlx::query(
            "UPDATE billing_gateway_accounts SET mutation_rate_limited_until = clock_timestamp() + interval '1 hour' WHERE id = $1",
        )
        .bind(account_cooldown_account.gateway_account_id)
        .execute(&database.pool)
        .await?;
        let account_cooldown = insert_due_subscription_at(
            &database.pool,
            account_cooldown_account,
            "account-cooldown-plan",
            Uuid::from_u128(8),
            due_at,
        )
        .await?;

        let blocking_attempt = insert_due_subscription_at(
            &database.pool,
            account,
            "blocking-attempt-plan",
            Uuid::from_u128(9),
            due_at,
        )
        .await?;
        insert_renewal_attempt(
            &database.pool,
            account,
            &blocking_attempt,
            due_at,
            "subscription_renewal",
            "pending",
            None,
            None,
        )
        .await?;

        let payment_method_update = insert_due_subscription_at(
            &database.pool,
            account,
            "payment-update-plan",
            Uuid::from_u128(10),
            due_at,
        )
        .await?;
        insert_pending_payment_method_update(
            &database.pool,
            account,
            &payment_method_update,
            Utc::now(),
        )
        .await?;

        let infrastructure_limit = insert_due_subscription_at(
            &database.pool,
            account,
            "infrastructure-limit-plan",
            Uuid::from_u128(11),
            due_at,
        )
        .await?;
        for _ in 0..syrup_rail::MAX_RENEWAL_INFRASTRUCTURE_ATTEMPTS_PER_PERIOD_CONFIGURATION {
            insert_renewal_attempt(
                &database.pool,
                account,
                &infrastructure_limit,
                due_at,
                "subscription_renewal",
                "failed",
                Some(
                    PaymentResolutionCode::GatewayAccountMutationCooldownBeforeSubmission.as_str(),
                ),
                Some(Utc::now()),
            )
            .await?;
        }

        let infrastructure_pacing = insert_due_subscription_at(
            &database.pool,
            account,
            "infrastructure-pacing-plan",
            Uuid::from_u128(12),
            due_at,
        )
        .await?;
        insert_renewal_attempt(
            &database.pool,
            account,
            &infrastructure_pacing,
            due_at,
            "subscription_renewal",
            "failed",
            Some(PaymentResolutionCode::GatewayLiveReadinessFailedBeforeSubmission.as_str()),
            Some(Utc::now()),
        )
        .await?;

        let provider_rate_pacing = insert_due_subscription_at(
            &database.pool,
            account,
            "provider-rate-pacing-plan",
            Uuid::from_u128(13),
            due_at,
        )
        .await?;
        insert_renewal_attempt(
            &database.pool,
            account,
            &provider_rate_pacing,
            due_at,
            "subscription_renewal",
            "failed",
            Some(PaymentResolutionCode::GatewayProviderRateLimitedBeforeSubmission.as_str()),
            Some(Utc::now()),
        )
        .await?;

        let page = due_renewals_page(&database.pool, None).await?;
        let included_dispatch = page
            .dispatches()
            .iter()
            .find(|dispatch| dispatch.subscription_id().into_uuid() == included.subscription_id)
            .expect("eligible subscription is dispatched");
        assert_eq!(included_dispatch.attempt_sequence_count(), 1);
        let actual = dispatch_ids(&page).into_iter().collect::<HashSet<_>>();
        let expected = [
            included.subscription_id,
            stale_update.subscription_id,
            past_due.subscription_id,
        ]
        .into_iter()
        .collect::<HashSet<_>>();
        assert_eq!(actual, expected);
        assert_eq!(page.next_cursor(), None);

        for excluded in [
            future.subscription_id,
            canceled.subscription_id,
            unpaid.subscription_id,
            provider_cooldown.subscription_id,
            account_cooldown.subscription_id,
            blocking_attempt.subscription_id,
            payment_method_update.subscription_id,
            infrastructure_limit.subscription_id,
            infrastructure_pacing.subscription_id,
            provider_rate_pacing.subscription_id,
        ] {
            assert!(!actual.contains(&excluded));
        }

        database.cleanup().await
    }

    #[tokio::test]
    async fn continuation_keeps_every_clock_dependent_gate_at_the_first_page_time()
    -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("renew_pg_frozen").await?;
        let account = create_gateway_account(&database.pool, "nmi").await?;
        let baseline_due_at = Utc::now() - Duration::minutes(5);
        for value in 1..=101_u128 {
            insert_due_subscription_at(
                &database.pool,
                account,
                "frozen-baseline-plan",
                Uuid::from_u128(value),
                baseline_due_at,
            )
            .await?;
        }
        let first = due_renewals_page(&database.pool, None).await?;
        assert_eq!(first.dispatches().len(), 100);
        let cursor = first
            .next_cursor()
            .expect("one baseline row remains for the continuation");
        let observed_at = cursor.observed_at();
        let window_opens_at = observed_at + Duration::seconds(1);

        insert_due_subscription_at(
            &database.pool,
            account,
            "frozen-due-plan",
            Uuid::from_u128(1_001),
            window_opens_at,
        )
        .await?;

        let provider_cooldown_account =
            create_gateway_account(&database.pool, "frozen-provider").await?;
        sqlx::query(
            "UPDATE billing_gateway_provider_rate_limits SET rate_limited_until = $1 WHERE provider_key = 'frozen-provider'",
        )
        .bind(window_opens_at)
        .execute(&database.pool)
        .await?;
        insert_due_subscription_at(
            &database.pool,
            provider_cooldown_account,
            "frozen-provider-plan",
            Uuid::from_u128(1_002),
            observed_at,
        )
        .await?;

        let account_cooldown_account =
            create_gateway_account(&database.pool, "frozen-account").await?;
        sqlx::query(
            "UPDATE billing_gateway_accounts SET mutation_rate_limited_until = $2 WHERE id = $1",
        )
        .bind(account_cooldown_account.gateway_account_id)
        .bind(window_opens_at)
        .execute(&database.pool)
        .await?;
        insert_due_subscription_at(
            &database.pool,
            account_cooldown_account,
            "frozen-account-plan",
            Uuid::from_u128(1_003),
            observed_at,
        )
        .await?;

        let stale_update = insert_due_subscription_at(
            &database.pool,
            account,
            "frozen-update-plan",
            Uuid::from_u128(1_004),
            observed_at,
        )
        .await?;
        insert_pending_payment_method_update(
            &database.pool,
            account,
            &stale_update,
            observed_at
                - Duration::seconds(PAYMENT_METHOD_UPDATE_UNSUBMITTED_STALE_AFTER_SECONDS - 1),
        )
        .await?;

        let infrastructure_retry = insert_due_subscription_at(
            &database.pool,
            account,
            "frozen-infrastructure-plan",
            Uuid::from_u128(1_005),
            observed_at,
        )
        .await?;
        insert_renewal_attempt(
            &database.pool,
            account,
            &infrastructure_retry,
            observed_at,
            "subscription_renewal",
            "failed",
            Some(PaymentResolutionCode::GatewayLiveReadinessFailedBeforeSubmission.as_str()),
            Some(
                observed_at
                    - Duration::seconds(syrup_rail::RENEWAL_INFRASTRUCTURE_RETRY_AFTER_SECONDS - 1),
            ),
        )
        .await?;

        let provider_rate_retry = insert_due_subscription_at(
            &database.pool,
            account,
            "frozen-provider-rate-plan",
            Uuid::from_u128(1_006),
            observed_at,
        )
        .await?;
        insert_renewal_attempt(
            &database.pool,
            account,
            &provider_rate_retry,
            observed_at,
            "subscription_renewal",
            "failed",
            Some(PaymentResolutionCode::GatewayProviderRateLimitedBeforeSubmission.as_str()),
            Some(
                observed_at
                    - Duration::seconds(
                        syrup_rail::RENEWAL_PROVIDER_RATE_LIMIT_RETRY_AFTER_SECONDS - 1,
                    ),
            ),
        )
        .await?;

        tokio::time::sleep(StdDuration::from_secs(2)).await;
        let now = sqlx::query_scalar::<_, DateTime<Utc>>("SELECT clock_timestamp()")
            .fetch_one(&database.pool)
            .await?;
        assert!(now >= window_opens_at);

        let continuation = due_renewals_page(&database.pool, Some(&cursor)).await?;
        assert_eq!(dispatch_ids(&continuation), vec![Uuid::from_u128(101)]);
        assert_eq!(continuation.next_cursor(), None);

        database.cleanup().await
    }

    #[tokio::test]
    async fn period_state_counts_both_kinds_but_renewal_only_infrastructure()
    -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("renew_state").await?;
        let account = create_gateway_account(&database.pool, "nmi").await?;
        let subscription_id = insert_due_subscription(&database.pool, account, "base-plan").await?;
        let row = sqlx::query(
            "SELECT subscriber_id, payment_method_id, next_renewal_at, initial_transaction_id, status FROM billing_subscriptions WHERE id = $1",
        )
        .bind(subscription_id)
        .fetch_one(&database.pool)
        .await?;
        let subscriber_id: Uuid = row.try_get("subscriber_id")?;
        let payment_method_id: Uuid = row.try_get("payment_method_id")?;
        let period_start_at: DateTime<Utc> = row.try_get("next_renewal_at")?;
        let initial_transaction_id: String = row.try_get("initial_transaction_id")?;
        let subscription_status: String = row.try_get("status")?;
        for (kind, resolution) in [
            (
                "subscription_renewal",
                PaymentResolutionCode::GatewayLiveReadinessFailedBeforeSubmission.as_str(),
            ),
            (
                "subscription_recovery",
                PaymentResolutionCode::GatewayLiveReadinessFailedBeforeSubmission.as_str(),
            ),
        ] {
            let attempt_id = Uuid::now_v7();
            sqlx::query(
                r#"
                INSERT INTO billing_payment_attempts (
                    id, billing_scope_id, subscriber_id, plan_key, subscription_id,
                    payment_method_id, attempt_kind, status, idempotency_key,
                    request_fingerprint, amount_cents, currency,
                    billing_period_start_at, billing_period_end_at,
                    gateway_account_id, gateway_configuration_id, gateway_order_id,
                    resolution_code, resolved_at,
                    subscription_expected_payment_method_id,
                    subscription_expected_initial_transaction_id,
                    subscription_expected_status
                ) VALUES (
                    $1, $2, $3, 'base-plan', $4, $5, $6, 'failed', $7, $8,
                    1900, 'USD', $9, $10, $11, $12, $13, $14, clock_timestamp(),
                    $5, $15, $16
                )
                "#,
            )
            .bind(attempt_id)
            .bind(account.billing_scope_id)
            .bind(subscriber_id)
            .bind(subscription_id)
            .bind(payment_method_id)
            .bind(kind)
            .bind(format!("attempt-{}", attempt_id.simple()))
            .bind(format!("fingerprint-{}", attempt_id.simple()))
            .bind(period_start_at)
            .bind(period_start_at + Duration::days(28))
            .bind(account.gateway_account_id)
            .bind(account.gateway_configuration_id)
            .bind(format!("order_{}", attempt_id.simple()))
            .bind(resolution)
            .bind(&initial_transaction_id)
            .bind(&subscription_status)
            .execute(&database.pool)
            .await?;
        }
        let mut transaction = database.pool.begin().await?;
        let state = renewal_attempt_state(
            &mut transaction,
            SubscriptionId::new(subscription_id),
            period_start_at,
            None,
        )
        .await?;
        transaction.rollback().await?;
        assert_eq!(state.attempt_sequence_count, 2);
        assert_eq!(state.automatic_infrastructure_attempt_count, 1);

        database.cleanup().await
    }
}
