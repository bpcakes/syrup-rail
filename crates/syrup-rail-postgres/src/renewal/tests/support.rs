use chrono::{DateTime, Duration, Utc};
use sqlx::PgPool;
use syrup_rail::RenewalDispatchPage;
use uuid::Uuid;

use super::super::{RenewalStoreError, due_renewals_page};
use crate::test_support::GatewayAccountFixture;

#[derive(Clone)]
pub(super) struct DueSubscriptionFixture {
    pub(super) subscription_id: Uuid,
    pub(super) subscriber_id: Uuid,
    pub(super) payment_method_id: Uuid,
    pub(super) plan_key: String,
    pub(super) initial_transaction_id: String,
}

pub(super) async fn insert_due_subscription(
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

pub(super) async fn insert_due_subscription_at(
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

pub(super) async fn insert_due_subscription_population(
    pool: &PgPool,
    account: GatewayAccountFixture,
    due_at: DateTime<Utc>,
    population: i32,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO billing_payment_methods (
            id, billing_scope_id, subscriber_id, gateway_account_id,
            gateway_payment_method_reference, status
        )
        SELECT
            md5('renewal-plan-method-' || value)::uuid,
            $1,
            md5('renewal-plan-subscriber-' || value)::uuid,
            $2,
            'vault_renewal_plan_' || value,
            'active'
        FROM generate_series(1, $3::integer) AS fixture(value)
        "#,
    )
    .bind(account.billing_scope_id)
    .bind(account.gateway_account_id)
    .bind(population)
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
        )
        SELECT
            md5('renewal-plan-subscription-' || value)::uuid,
            $1,
            md5('renewal-plan-subscriber-' || value)::uuid,
            'plan-shape',
            'active',
            $2,
            md5('renewal-plan-method-' || value)::uuid,
            1900,
            'USD',
            scheduled_at - interval '32 days',
            scheduled_at,
            scheduled_at,
            'renewal-plan-transaction-' || value,
            'recurring',
            'calendar_months',
            1,
            ARRAY[]::bigint[],
            'remain_past_due',
            'suspend_immediately',
            scheduled_at
        FROM generate_series(1, $4::integer) AS fixture(value)
        CROSS JOIN LATERAL (
            SELECT $3::timestamptz
                - ((value - 1) / 4) * interval '1 second' AS scheduled_at
        ) AS schedule
        "#,
    )
    .bind(account.billing_scope_id)
    .bind(account.gateway_account_id)
    .bind(due_at)
    .bind(population)
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
            subscription_expected_status, resolved_at, created_at, updated_at
        )
        SELECT
            md5('renewal-plan-attempt-' || value)::uuid,
            $1,
            md5('renewal-plan-subscriber-' || value)::uuid,
            'plan-shape',
            md5('renewal-plan-subscription-' || value)::uuid,
            md5('renewal-plan-method-' || value)::uuid,
            'subscription_recovery',
            'failed',
            'renewal-plan-idempotency-' || value,
            'renewal-plan-fingerprint-' || value,
            1900,
            'USD',
            scheduled_at,
            scheduled_at + interval '1 month',
            $2,
            $3,
            'renewal-plan-order-' || value,
            md5('renewal-plan-method-' || value)::uuid,
            'renewal-plan-transaction-' || value,
            'active',
            scheduled_at - interval '1 second',
            scheduled_at - interval '2 seconds',
            scheduled_at - interval '1 second'
        FROM generate_series(1, $5::integer) AS fixture(value)
        CROSS JOIN LATERAL (
            SELECT $4::timestamptz
                - ((value - 1) / 4) * interval '1 second' AS scheduled_at
        ) AS schedule
        "#,
    )
    .bind(account.billing_scope_id)
    .bind(account.gateway_account_id)
    .bind(account.gateway_configuration_id)
    .bind(due_at)
    .bind(population)
    .execute(pool)
    .await?;
    Ok(())
}

pub(super) async fn update_due_at(
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
pub(super) async fn insert_renewal_attempt(
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

pub(super) async fn insert_pending_payment_method_update(
    pool: &PgPool,
    account: GatewayAccountFixture,
    subscription: &DueSubscriptionFixture,
    created_at: DateTime<Utc>,
) -> Result<Uuid, sqlx::Error> {
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
    Ok(attempt_id)
}

pub(super) fn dispatch_ids(page: &RenewalDispatchPage) -> Vec<Uuid> {
    page.dispatches()
        .iter()
        .map(|dispatch| dispatch.subscription_id().into_uuid())
        .collect()
}

pub(super) async fn scan_dispatch_ids(pool: &PgPool) -> Result<Vec<Uuid>, RenewalStoreError> {
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
