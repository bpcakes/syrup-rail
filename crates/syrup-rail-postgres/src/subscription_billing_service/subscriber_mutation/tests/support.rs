use chrono::{Duration as ChronoDuration, Utc};
use sqlx::{PgPool, Postgres, Transaction};
use syrup_rail::{BillingScopeId, PlanKey, SubscriberId};
use uuid::Uuid;

use super::ActiveSubscriptionFixture;
use crate::test_support::GatewayAccountFixture;

pub(super) async fn install_host_boundary(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::raw_sql(
        r#"
        CREATE TABLE test_billing_subjects (
            billing_scope_id uuid NOT NULL,
            subscriber_id uuid NOT NULL,
            PRIMARY KEY (billing_scope_id, subscriber_id)
        );
        CREATE TABLE test_billing_outbox (
            id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
            event_kind text NOT NULL
        );
        "#,
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub(super) async fn hold_subscription_aggregate_lock(
    pool: &PgPool,
    subscriber: SubscriberId,
    plan: &PlanKey,
) -> Result<Transaction<'static, Postgres>, sqlx::Error> {
    let mut transaction = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::uuid::text || ':' || $2, 0))")
        .bind(subscriber.as_uuid())
        .bind(plan.as_str())
        .execute(&mut *transaction)
        .await?;
    Ok(transaction)
}

pub(super) async fn insert_host_subject(
    pool: &PgPool,
    scope: BillingScopeId,
    subscriber: SubscriberId,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO test_billing_subjects (billing_scope_id, subscriber_id) VALUES ($1, $2)",
    )
    .bind(scope.as_uuid())
    .bind(subscriber.as_uuid())
    .execute(pool)
    .await?;
    Ok(())
}

pub(super) async fn insert_active_subscription(
    pool: &PgPool,
    account: GatewayAccountFixture,
    subscriber: SubscriberId,
    plan: &PlanKey,
) -> Result<ActiveSubscriptionFixture, sqlx::Error> {
    let id = Uuid::now_v7();
    let payment_method_id = Uuid::now_v7();
    let suffix = id.simple();
    let initial_transaction_id = format!("service_initial_{suffix}");
    let period_start = Utc::now() - ChronoDuration::days(1);
    let period_end = period_start + ChronoDuration::days(30);
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
    .bind(subscriber.as_uuid())
    .bind(account.gateway_account_id)
    .bind(format!("service_vault_{suffix}"))
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
            $1, $2, $3, $4, 'active', $5, $6, 5900, 'USD', $7, $8, $8, $9,
            'recurring', 'calendar_months', 1, ARRAY[]::bigint[],
            'remain_past_due', 'suspend_immediately', $8
        )
        "#,
    )
    .bind(id)
    .bind(account.billing_scope_id)
    .bind(subscriber.as_uuid())
    .bind(plan.as_str())
    .bind(account.gateway_account_id)
    .bind(payment_method_id)
    .bind(period_start)
    .bind(period_end)
    .bind(&initial_transaction_id)
    .execute(pool)
    .await?;
    Ok(ActiveSubscriptionFixture {
        id,
        payment_method_id,
        period_end,
        initial_transaction_id,
    })
}

pub(super) async fn insert_blocking_renewal(
    pool: &PgPool,
    account: GatewayAccountFixture,
    subscriber: SubscriberId,
    plan: &PlanKey,
    subscription: &ActiveSubscriptionFixture,
) -> Result<(), sqlx::Error> {
    let attempt_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO billing_payment_attempts (
            id, billing_scope_id, subscriber_id, plan_key, subscription_id,
            payment_method_id, attempt_kind, status, idempotency_key,
            request_fingerprint, amount_cents, currency, billing_period_start_at,
            billing_period_end_at, gateway_account_id, gateway_configuration_id,
            gateway_order_id, subscription_expected_payment_method_id,
            subscription_expected_initial_transaction_id, subscription_expected_status
        ) VALUES (
            $1, $2, $3, $4, $5, $6, 'subscription_renewal', 'pending',
            $7, $8, 5900, 'USD', $9, $10, $11, $12, $13, $6, $14, 'active'
        )
        "#,
    )
    .bind(attempt_id)
    .bind(account.billing_scope_id)
    .bind(subscriber.as_uuid())
    .bind(plan.as_str())
    .bind(subscription.id)
    .bind(subscription.payment_method_id)
    .bind(format!("service_renewal_{attempt_id}"))
    .bind(format!("service_renewal_fingerprint_{attempt_id}"))
    .bind(subscription.period_end)
    .bind(subscription.period_end + ChronoDuration::days(30))
    .bind(account.gateway_account_id)
    .bind(account.gateway_configuration_id)
    .bind(format!("service_renewal_order_{attempt_id}"))
    .bind(&subscription.initial_transaction_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub(super) async fn insert_stale_payment_method_update(
    pool: &PgPool,
    account: GatewayAccountFixture,
    subscriber: SubscriberId,
    plan: &PlanKey,
    subscription: &ActiveSubscriptionFixture,
) -> Result<Uuid, sqlx::Error> {
    let attempt_id = Uuid::now_v7();
    let created_at = Utc::now() - ChronoDuration::minutes(4);
    sqlx::query(
        r#"
        INSERT INTO billing_payment_attempts (
            id, billing_scope_id, subscriber_id, plan_key, subscription_id,
            payment_method_id, attempt_kind, status, idempotency_key,
            request_fingerprint, amount_cents, currency, gateway_account_id,
            gateway_configuration_id, gateway_order_id,
            payment_method_update_expected_payment_method_id,
            payment_method_update_expected_initial_transaction_id,
            created_at, updated_at
        ) VALUES (
            $1, $2, $3, $4, $5, $6,
            'subscription_payment_method_update', 'pending', $7, $8, 0, 'USD',
            $9, $10, $11, $6, $12, $13, $13
        )
        "#,
    )
    .bind(attempt_id)
    .bind(account.billing_scope_id)
    .bind(subscriber.as_uuid())
    .bind(plan.as_str())
    .bind(subscription.id)
    .bind(subscription.payment_method_id)
    .bind(format!("service_update_{attempt_id}"))
    .bind(format!("service_update_fingerprint_{attempt_id}"))
    .bind(account.gateway_account_id)
    .bind(account.gateway_configuration_id)
    .bind(format!("service_update_order_{attempt_id}"))
    .bind(&subscription.initial_transaction_id)
    .bind(created_at)
    .execute(pool)
    .await?;
    Ok(attempt_id)
}

pub(super) async fn insert_pending_initial_attempt(
    pool: &PgPool,
    account: GatewayAccountFixture,
    subscriber: SubscriberId,
    plan: &PlanKey,
) -> Result<(), sqlx::Error> {
    let attempt_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO billing_payment_attempts (
            id, billing_scope_id, subscriber_id, plan_key, attempt_kind,
            status, idempotency_key, request_fingerprint, amount_cents,
            currency, gateway_account_id, gateway_configuration_id,
            gateway_order_id, subscription_initial_terms_version,
            subscription_initial_start_kind,
            subscription_initial_recurring_base_amount_cents,
            subscription_initial_recurring_period_kind,
            subscription_initial_recurring_period_count,
            subscription_initial_dunning_retry_delays_seconds,
            subscription_initial_dunning_exhaustion,
            subscription_initial_past_due_access
        ) VALUES (
            $1, $2, $3, $4, 'subscription_initial', 'pending', $5, $6,
            100, 'USD', $7, $8, $9, 2, 'recurring_immediately', 100,
            'calendar_months', 1, ARRAY[]::bigint[],
            'remain_past_due', 'suspend_immediately'
        )
        "#,
    )
    .bind(attempt_id)
    .bind(account.billing_scope_id)
    .bind(subscriber.as_uuid())
    .bind(plan.as_str())
    .bind(format!("service_discount_{attempt_id}"))
    .bind(format!("service_discount_fingerprint_{attempt_id}"))
    .bind(account.gateway_account_id)
    .bind(account.gateway_configuration_id)
    .bind(format!("service_discount_order_{attempt_id}"))
    .execute(pool)
    .await?;
    Ok(())
}

pub(super) async fn install_cancellation_failure_trigger(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::raw_sql(
        r#"
        CREATE FUNCTION test_reject_subscription_cancellation()
        RETURNS trigger
        LANGUAGE plpgsql
        AS $$
        BEGIN
            RAISE EXCEPTION 'injected cancellation mutation failure';
        END;
        $$;
        CREATE TRIGGER test_reject_subscription_cancellation
        BEFORE UPDATE OF status ON billing_subscriptions
        FOR EACH ROW
        EXECUTE FUNCTION test_reject_subscription_cancellation();
        "#,
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub(super) async fn subscription_status(
    pool: &PgPool,
    subscription_id: Uuid,
) -> Result<String, sqlx::Error> {
    sqlx::query_scalar("SELECT status FROM billing_subscriptions WHERE id = $1")
        .bind(subscription_id)
        .fetch_one(pool)
        .await
}

pub(super) async fn payment_attempt_status(
    pool: &PgPool,
    attempt_id: Uuid,
) -> Result<String, sqlx::Error> {
    sqlx::query_scalar("SELECT status FROM billing_payment_attempts WHERE id = $1")
        .bind(attempt_id)
        .fetch_one(pool)
        .await
}

pub(super) async fn outbox_count(pool: &PgPool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT count(*) FROM test_billing_outbox")
        .fetch_one(pool)
        .await
}
