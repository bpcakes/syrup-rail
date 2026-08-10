use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, Row, Transaction};
use syrup_rail::{
    BillingScopeId, PaymentAttemptId, PaymentResolutionCode, RenewalAttemptState, RenewalDispatch,
    SubscriptionId,
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

/// Returns the deterministic, provider-neutral renewal work due now.
///
/// The fixed bound is billing policy. Hosts format queue identities from the
/// returned facts and cannot change candidate selection by supplying a limit.
pub async fn due_renewals(pool: &PgPool) -> Result<Vec<RenewalDispatch>, RenewalStoreError> {
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
        WITH renewal_attempts AS (
            SELECT
                attempts.subscription_id,
                attempts.billing_period_start_at,
                COUNT(*) FILTER (
                    WHERE attempts.attempt_kind = 'subscription_renewal'
                        AND attempts.status = 'failed'
                        AND attempts.resolution_code = ANY($1::text[])
                        AND attempts.gateway_configuration_id IS NOT DISTINCT FROM
                            current_accounts.gateway_configuration_id
                ) AS automatic_infrastructure_attempt_count,
                MAX(attempts.resolved_at) FILTER (
                    WHERE attempts.attempt_kind = 'subscription_renewal'
                        AND attempts.status = 'failed'
                        AND attempts.resolution_code = ANY($2::text[])
                        AND attempts.gateway_configuration_id IS NOT DISTINCT FROM
                            current_accounts.gateway_configuration_id
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
            JOIN billing_subscriptions AS attempt_subscriptions
                ON attempt_subscriptions.id = attempts.subscription_id
            LEFT JOIN billing_gateway_accounts AS current_accounts
                ON current_accounts.id = attempt_subscriptions.gateway_account_id
                AND current_accounts.billing_scope_id = attempt_subscriptions.billing_scope_id
            WHERE attempts.attempt_kind IN ('subscription_renewal', 'subscription_recovery')
                AND attempts.billing_period_start_at IS NOT NULL
            GROUP BY attempts.subscription_id, attempts.billing_period_start_at
        )
        SELECT subscriptions.billing_scope_id, subscriptions.id,
            subscriptions.next_renewal_at,
            COALESCE(renewal_attempts.attempt_sequence_count, 0)::bigint
                AS attempt_sequence_count
        FROM billing_subscriptions AS subscriptions
        JOIN billing_gateway_accounts AS accounts
            ON accounts.billing_scope_id = subscriptions.billing_scope_id
            AND accounts.id = subscriptions.gateway_account_id
        JOIN billing_gateway_provider_rate_limits AS provider_limits
            ON provider_limits.provider_key = accounts.provider_key
        LEFT JOIN renewal_attempts
            ON renewal_attempts.subscription_id = subscriptions.id
            AND renewal_attempts.billing_period_start_at = subscriptions.next_renewal_at
        WHERE subscriptions.status IN ('active', 'past_due')
            AND subscriptions.next_payment_attempt_at <= clock_timestamp()
            AND provider_limits.rate_limited_until <= clock_timestamp()
            AND (
                accounts.mutation_rate_limited_until IS NULL
                OR accounts.mutation_rate_limited_until <= clock_timestamp()
            )
            AND COALESCE(renewal_attempts.has_blocking_attempt, false) = false
            AND NOT EXISTS (
                SELECT 1
                FROM billing_payment_attempts AS update_attempts
                WHERE update_attempts.subscription_id = subscriptions.id
                    AND update_attempts.attempt_kind = 'subscription_payment_method_update'
                    AND update_attempts.status IN ('pending', 'unknown', 'review_required')
                    AND NOT (
                        update_attempts.status = 'pending'
                        AND update_attempts.submitted_at IS NULL
                        AND update_attempts.created_at <= clock_timestamp()
                            - ($4::bigint * interval '1 second')
                    )
            )
            AND COALESCE(renewal_attempts.automatic_infrastructure_attempt_count, 0)
                < $5
            AND (
                renewal_attempts.last_automatic_infrastructure_failure_at IS NULL
                OR renewal_attempts.last_automatic_infrastructure_failure_at
                    <= clock_timestamp() - ($6::bigint * interval '1 second')
            )
            AND (
                renewal_attempts.last_provider_rate_limited_at IS NULL
                OR renewal_attempts.last_provider_rate_limited_at <= clock_timestamp()
                    - (
                        CASE WHEN COALESCE(
                            renewal_attempts.provider_rate_limited_attempt_count,
                            0
                        ) >= $7 THEN $8::bigint ELSE $9::bigint END
                        * interval '1 second'
                    )
            )
        ORDER BY subscriptions.next_payment_attempt_at ASC, subscriptions.id ASC
        LIMIT 100
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
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|row| {
            Ok(RenewalDispatch::new(
                BillingScopeId::new(row.try_get("billing_scope_id")?),
                SubscriptionId::new(row.try_get("id")?),
                row.try_get("next_renewal_at")?,
                row.try_get("attempt_sequence_count")?,
            ))
        })
        .collect()
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
    use std::error::Error;

    use chrono::Duration;
    use uuid::Uuid;

    use super::*;
    use crate::test_support::{GatewayAccountFixture, TestDatabase, create_gateway_account};

    async fn insert_due_subscription(
        pool: &PgPool,
        account: GatewayAccountFixture,
        plan_key: &str,
    ) -> Result<Uuid, sqlx::Error> {
        let subscriber_id = Uuid::now_v7();
        let payment_method_id = Uuid::now_v7();
        let subscription_id = Uuid::now_v7();
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
        let period_start = Utc::now() - Duration::days(32);
        let period_end = Utc::now() - Duration::days(1);
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
        .bind(period_start)
        .bind(period_end)
        .bind(format!("txn_{}", subscription_id.simple()))
        .execute(pool)
        .await?;
        Ok(subscription_id)
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
