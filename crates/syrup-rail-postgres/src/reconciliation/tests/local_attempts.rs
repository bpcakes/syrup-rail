use chrono::{DateTime, Duration, Utc};

use super::*;

#[tokio::test]
async fn stale_local_subscription_charges_fail_without_exact_query() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("recon_charge").await?;
    let result = async {
        let account = create_gateway_account(&database.pool, "test_gateway").await?;
        let sibling = create_gateway_account(&database.pool, "test_gateway").await?;
        let stale_at = Utc::now() - Duration::minutes(31);
        let renewal = insert_subscription_charge_attempt(
            &database.pool,
            account,
            "subscription_renewal",
            "pending",
            stale_at,
        )
        .await?;
        let recovery = insert_subscription_charge_attempt(
            &database.pool,
            account,
            "subscription_recovery",
            "review_required",
            stale_at,
        )
        .await?;
        let fresh = insert_subscription_charge_attempt(
            &database.pool,
            account,
            "subscription_renewal",
            "pending",
            Utc::now(),
        )
        .await?;
        let sibling_attempt = insert_subscription_charge_attempt(
            &database.pool,
            sibling,
            "subscription_recovery",
            "pending",
            stale_at,
        )
        .await?;

        assert!(
            claim_exact_reconciliation_attempts(
                &database.pool,
                GatewayAccountId::new(account.gateway_account_id),
            )
            .await?
            .is_empty()
        );
        assert_eq!(
            fail_stale_unsubmitted_subscription_charges(
                &database.pool,
                GatewayAccountId::new(account.gateway_account_id),
            )
            .await?,
            2
        );
        assert_eq!(attempt_status(&database.pool, renewal).await?, "failed");
        assert_eq!(attempt_status(&database.pool, recovery).await?, "failed");
        assert_eq!(attempt_status(&database.pool, fresh).await?, "pending");
        assert_eq!(
            attempt_status(&database.pool, sibling_attempt).await?,
            "pending"
        );
        let messages: Vec<String> = sqlx::query_scalar(
            r#"
            SELECT gateway_response_text
            FROM billing_payment_attempts
            WHERE id IN ($1, $2)
            ORDER BY attempt_kind
            "#,
        )
        .bind(renewal)
        .bind(recovery)
        .fetch_all(&database.pool)
        .await?;
        assert_eq!(
            messages,
            vec![
                "Subscription recovery was abandoned before gateway submission.".to_owned(),
                "Subscription renewal was abandoned before gateway submission.".to_owned(),
            ]
        );
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

async fn insert_subscription_charge_attempt(
    pool: &sqlx::PgPool,
    account: crate::test_support::GatewayAccountFixture,
    kind: &str,
    status: &str,
    created_at: DateTime<Utc>,
) -> Result<Uuid, sqlx::Error> {
    let subscriber_id = Uuid::now_v7();
    let payment_method_id = Uuid::now_v7();
    let subscription_id = Uuid::now_v7();
    let attempt_id = Uuid::now_v7();
    let transaction_id = format!("initial_{}", attempt_id.simple());
    let period_start_at = Utc::now() - Duration::days(1);
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
    .bind(format!("method_{}", attempt_id.simple()))
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
            $1, $2, $3, 'test_plan', 'active', $4, $5, 100, 'USD',
            $6 - interval '1 month', $6, $6, $7, 'recurring',
            'calendar_months', 1, ARRAY[]::bigint[], 'remain_past_due',
            'suspend_immediately', $6
        )
        "#,
    )
    .bind(subscription_id)
    .bind(account.billing_scope_id)
    .bind(subscriber_id)
    .bind(account.gateway_account_id)
    .bind(payment_method_id)
    .bind(period_start_at)
    .bind(&transaction_id)
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
        INSERT INTO billing_payment_attempts (
            id, billing_scope_id, subscriber_id, plan_key, subscription_id,
            payment_method_id, attempt_kind, status, idempotency_key,
            request_fingerprint, amount_cents, currency,
            billing_period_start_at, billing_period_end_at,
            gateway_account_id, gateway_configuration_id, gateway_order_id,
            subscription_expected_payment_method_id,
            subscription_expected_initial_transaction_id,
            subscription_expected_status, created_at, updated_at
        ) VALUES (
            $1, $2, $3, 'test_plan', $4, $5, $6, $7, $8, $9, 100, 'USD',
            $10, $10 + interval '1 month', $11, $12, $13,
            $5, $14, 'active', $15, $15
        )
        "#,
    )
    .bind(attempt_id)
    .bind(account.billing_scope_id)
    .bind(subscriber_id)
    .bind(subscription_id)
    .bind(payment_method_id)
    .bind(kind)
    .bind(status)
    .bind(format!("idem_{}", attempt_id.simple()))
    .bind(format!("fingerprint_{}", attempt_id.simple()))
    .bind(period_start_at)
    .bind(account.gateway_account_id)
    .bind(account.gateway_configuration_id)
    .bind(format!("order_{}", attempt_id.simple()))
    .bind(transaction_id)
    .bind(created_at)
    .execute(pool)
    .await?;
    Ok(attempt_id)
}
