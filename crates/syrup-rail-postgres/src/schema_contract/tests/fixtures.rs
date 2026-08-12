use super::storage_fixtures::create_subscription_fixture;
use super::*;

use chrono::{DateTime, Utc};

pub(super) async fn create_v2_subscription_fixture(
    pool: &PgPool,
    gateway: GatewayAccountFixture,
    subscriber_id: Uuid,
) -> Result<(Uuid, Uuid, String), sqlx::Error> {
    let payment_method_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO billing_payment_methods (
            id, billing_scope_id, subscriber_id, gateway_account_id,
            gateway_payment_method_reference, status
        ) VALUES ($1, $2, $3, $4, $5, 'active')
        "#,
    )
    .bind(payment_method_id)
    .bind(gateway.billing_scope_id)
    .bind(subscriber_id)
    .bind(gateway.gateway_account_id)
    .bind(format!("vault_{}", opaque_fixture_uuid(payment_method_id)))
    .execute(pool)
    .await?;

    let subscription_id = Uuid::now_v7();
    let initial_transaction_id = format!("txn_{}", opaque_fixture_uuid(subscription_id));
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
            $1, $2, $3, 'base_subscription', 'active', $4, $5, 100, 'USD',
            '2026-01-01 00:00:00+00', '2026-02-01 00:00:00+00',
            '2026-02-01 00:00:00+00', $6, 'recurring', 'calendar_months',
            1, ARRAY[86400, 86400, 86400, 86400]::bigint[],
            'remain_past_due', 'suspend_immediately',
            '2026-02-01 00:00:00+00'
        )
        "#,
    )
    .bind(subscription_id)
    .bind(gateway.billing_scope_id)
    .bind(subscriber_id)
    .bind(gateway.gateway_account_id)
    .bind(payment_method_id)
    .bind(&initial_transaction_id)
    .execute(pool)
    .await?;
    Ok((payment_method_id, subscription_id, initial_transaction_id))
}

pub(super) async fn insert_v2_initial_attempt(
    pool: &PgPool,
    gateway: GatewayAccountFixture,
    subscriber_id: Uuid,
    identity: &str,
) -> Result<Uuid, sqlx::Error> {
    let attempt_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO billing_payment_attempts (
            id, billing_scope_id, subscriber_id, plan_key,
            attempt_kind, status, idempotency_key, request_fingerprint,
            amount_cents, currency, gateway_account_id,
            gateway_configuration_id, gateway_order_id,
            subscription_initial_terms_version,
            subscription_initial_start_kind,
            subscription_initial_recurring_base_amount_cents,
            subscription_initial_recurring_period_kind,
            subscription_initial_recurring_period_count,
            subscription_initial_dunning_retry_delays_seconds,
            subscription_initial_dunning_exhaustion,
            subscription_initial_past_due_access
        ) VALUES (
            $1, $2, $3, 'base_subscription', 'subscription_initial',
            'pending', $4, $5, 100, 'USD', $6, $7, $8,
            2, 'recurring_immediately', 100, 'calendar_months', 1,
            ARRAY[86400, 86400, 86400, 86400]::bigint[],
            'remain_past_due', 'suspend_immediately'
        )
        "#,
    )
    .bind(attempt_id)
    .bind(gateway.billing_scope_id)
    .bind(subscriber_id)
    .bind(format!("{identity}-{}", opaque_fixture_uuid(attempt_id)))
    .bind(format!("{identity}:base_subscription:100:USD"))
    .bind(gateway.gateway_account_id)
    .bind(gateway.gateway_configuration_id)
    .bind(format!(
        "{identity}-order-{}",
        opaque_fixture_uuid(attempt_id)
    ))
    .execute(pool)
    .await?;
    Ok(attempt_id)
}

pub(super) async fn insert_v2_indefinite_discount(
    pool: &PgPool,
    gateway: GatewayAccountFixture,
    subscriber_id: Uuid,
    subscription_id: Uuid,
    periods_applied: i32,
) -> Result<sqlx::postgres::PgQueryResult, sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO billing_subscription_discounts (
            subscription_id, billing_scope_id, subscriber_id, plan_key,
            code_snapshot, discount_kind, amount_off_cents, currency,
            duration, base_amount_cents, discounted_amount_cents,
            periods_applied, status
        ) VALUES (
            $1, $2, $3, 'base_subscription', 'SCHEMATEST',
            'amount_off', 10, 'USD', 'indefinite', 100, 90, $4, 'active'
        )
        "#,
    )
    .bind(subscription_id)
    .bind(gateway.billing_scope_id)
    .bind(subscriber_id)
    .bind(periods_applied)
    .execute(pool)
    .await
}

pub(super) async fn insert_v2_limited_discount(
    pool: &PgPool,
    gateway: GatewayAccountFixture,
    subscriber_id: Uuid,
    subscription_id: Uuid,
    periods_applied: i32,
    status: &str,
    completed_at: Option<&str>,
) -> Result<sqlx::postgres::PgQueryResult, sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO billing_subscription_discounts (
            subscription_id, billing_scope_id, subscriber_id, plan_key,
            code_snapshot, discount_kind, amount_off_cents, currency,
            duration, duration_months, base_amount_cents,
            discounted_amount_cents, periods_total, periods_applied,
            status, completed_at
        ) VALUES (
            $1, $2, $3, 'base_subscription', 'SCHEMATEST',
            'amount_off', 10, 'USD', 'limited_months', 3, 100, 90,
            3, $4, $5, $6::timestamptz
        )
        "#,
    )
    .bind(subscription_id)
    .bind(gateway.billing_scope_id)
    .bind(subscriber_id)
    .bind(periods_applied)
    .bind(status)
    .bind(completed_at)
    .execute(pool)
    .await
}

pub(super) async fn create_v1_subscription_with_status(
    pool: &PgPool,
    gateway: GatewayAccountFixture,
    subscriber_id: Uuid,
    status: &str,
) -> Result<(Uuid, Uuid, String), sqlx::Error> {
    let fixture = create_subscription_fixture(pool, gateway, subscriber_id).await?;
    match status {
        "active" => {}
        "past_due" => {
            sqlx::query("UPDATE billing_subscriptions SET status = 'past_due' WHERE id = $1")
                .bind(fixture.1)
                .execute(pool)
                .await?;
        }
        "canceled" => {
            sqlx::query(
                r#"
                UPDATE billing_subscriptions
                SET status = 'canceled',
                    canceled_at = '2026-01-15 00:00:00+00'
                WHERE id = $1
                "#,
            )
            .bind(fixture.1)
            .execute(pool)
            .await?;
        }
        _ => panic!("unsupported version-1 fixture status {status}"),
    }
    Ok(fixture)
}

/// Executes the shipped version-1 operator transition that could fail an
/// active-snapshot recovery and move the subscription to `past_due`.
pub(super) async fn apply_v1_manual_active_recovery_failure(
    pool: &PgPool,
    gateway: GatewayAccountFixture,
    subscriber_id: Uuid,
    payment_method_id: Uuid,
    subscription_id: Uuid,
    initial_transaction_id: &str,
) -> Result<(Uuid, DateTime<Utc>), sqlx::Error> {
    let attempt_id = Uuid::now_v7();
    let mut transaction = pool.begin().await?;
    sqlx::query(
        r#"
        INSERT INTO billing_payment_attempts (
            id, billing_scope_id, subscriber_id, plan_key,
            subscription_id, payment_method_id, attempt_kind, status,
            idempotency_key, request_fingerprint, amount_cents, currency,
            billing_period_start_at, billing_period_end_at,
            gateway_account_id, gateway_configuration_id, gateway_order_id,
            submitted_at, subscription_expected_payment_method_id,
            subscription_expected_initial_transaction_id,
            subscription_expected_status
        ) VALUES (
            $1, $2, $3, 'base_subscription', $4, $5,
            'subscription_recovery', 'review_required', $6, $7, 100, 'USD',
            '2026-02-01 00:00:00+00', '2026-03-01 00:00:00+00',
            $8, $9, $10, '2026-02-02 00:30:00+00', $5, $11, 'active'
        )
        "#,
    )
    .bind(attempt_id)
    .bind(gateway.billing_scope_id)
    .bind(subscriber_id)
    .bind(subscription_id)
    .bind(payment_method_id)
    .bind(format!(
        "legacy-active-recovery-{}",
        opaque_fixture_uuid(attempt_id)
    ))
    .bind(format!(
        "legacy-active-recovery:{}",
        opaque_fixture_uuid(attempt_id)
    ))
    .bind(gateway.gateway_account_id)
    .bind(gateway.gateway_configuration_id)
    .bind(format!(
        "legacy-active-recovery-order-{}",
        opaque_fixture_uuid(attempt_id)
    ))
    .bind(initial_transaction_id)
    .execute(&mut *transaction)
    .await?;

    let resolved_at = sqlx::query_scalar::<_, DateTime<Utc>>(
        r#"
        UPDATE billing_payment_attempts
        SET status = 'failed',
            gateway_response_text =
                'Operator marked the unresolved payment attempt as failed.',
            gateway_condition = 'failed',
            resolved_at = '2026-02-02 00:31:00+00',
            updated_at = clock_timestamp()
        WHERE id = $1 AND status = 'review_required'
        RETURNING resolved_at
        "#,
    )
    .bind(attempt_id)
    .fetch_one(&mut *transaction)
    .await?;
    sqlx::query(
        r#"
        UPDATE billing_subscriptions
        SET status = 'past_due', updated_at = clock_timestamp()
        WHERE id = $1 AND status = 'active'
        "#,
    )
    .bind(subscription_id)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok((attempt_id, resolved_at))
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn insert_v1_terminal_subscription_attempt(
    pool: &PgPool,
    gateway: GatewayAccountFixture,
    subscriber_id: Uuid,
    payment_method_id: Uuid,
    subscription_id: Uuid,
    initial_transaction_id: &str,
    attempt_kind: &str,
    status: &str,
    identity: &str,
    submitted_at: Option<&str>,
    resolved_at: &str,
    resolution_code: Option<&str>,
) -> Result<Uuid, sqlx::Error> {
    let attempt_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO billing_payment_attempts (
            id, billing_scope_id, subscriber_id, plan_key,
            subscription_id, payment_method_id, attempt_kind, status,
            idempotency_key, request_fingerprint, amount_cents, currency,
            billing_period_start_at, billing_period_end_at,
            gateway_account_id, gateway_configuration_id, gateway_order_id,
            submitted_at, resolved_at, resolution_code,
            subscription_expected_payment_method_id,
            subscription_expected_initial_transaction_id,
            subscription_expected_status
        ) VALUES (
            $1, $2, $3, 'base_subscription', $4, $5, $6, $7,
            $8, $9, 100, 'USD',
            '2026-02-01 00:00:00+00', '2026-03-01 00:00:00+00',
            $10, $11, $12, $13::timestamptz, $14::timestamptz, $15,
            $5, $16, 'past_due'
        )
        "#,
    )
    .bind(attempt_id)
    .bind(gateway.billing_scope_id)
    .bind(subscriber_id)
    .bind(subscription_id)
    .bind(payment_method_id)
    .bind(attempt_kind)
    .bind(status)
    .bind(format!("schema-upgrade-{identity}"))
    .bind(format!("schema-upgrade:{attempt_kind}:{identity}"))
    .bind(gateway.gateway_account_id)
    .bind(gateway.gateway_configuration_id)
    .bind(format!("schema-upgrade-order-{identity}"))
    .bind(submitted_at)
    .bind(resolved_at)
    .bind(resolution_code)
    .bind(initial_transaction_id)
    .execute(pool)
    .await?;
    Ok(attempt_id)
}

pub(super) async fn insert_mixed_v1_failure_history(
    pool: &PgPool,
    gateway: GatewayAccountFixture,
    subscriber_id: Uuid,
    payment_method_id: Uuid,
    subscription_id: Uuid,
    initial_transaction_id: &str,
) -> Result<Uuid, sqlx::Error> {
    let customer_attempt_id = insert_v1_terminal_subscription_attempt(
        pool,
        gateway,
        subscriber_id,
        payment_method_id,
        subscription_id,
        initial_transaction_id,
        "subscription_renewal",
        "declined",
        &format!("mixed-customer-{}", opaque_fixture_uuid(subscription_id)),
        Some("2026-02-02 00:30:00+00"),
        "2026-02-02 00:31:00+00",
        None,
    )
    .await?;
    insert_v1_terminal_subscription_attempt(
        pool,
        gateway,
        subscriber_id,
        payment_method_id,
        subscription_id,
        initial_transaction_id,
        "subscription_renewal",
        "failed",
        &format!(
            "mixed-infrastructure-{}",
            opaque_fixture_uuid(subscription_id)
        ),
        None,
        "2026-02-03 00:31:00+00",
        Some("gateway_unavailable_before_submission"),
    )
    .await?;
    insert_v1_terminal_subscription_attempt(
        pool,
        gateway,
        subscriber_id,
        payment_method_id,
        subscription_id,
        initial_transaction_id,
        "subscription_renewal",
        "failed",
        &format!("mixed-throttle-{}", opaque_fixture_uuid(subscription_id)),
        None,
        "2026-02-04 00:31:00+00",
        Some("gateway_provider_rate_limited_before_submission"),
    )
    .await?;
    insert_v1_terminal_subscription_attempt(
        pool,
        gateway,
        subscriber_id,
        payment_method_id,
        subscription_id,
        initial_transaction_id,
        "subscription_recovery",
        "declined",
        &format!("mixed-recovery-{}", opaque_fixture_uuid(subscription_id)),
        Some("2026-02-05 00:30:00+00"),
        "2026-02-05 00:31:00+00",
        None,
    )
    .await?;
    Ok(customer_attempt_id)
}

pub(super) async fn insert_v1_reclassified_exhausted_history(
    pool: &PgPool,
    gateway: GatewayAccountFixture,
    subscriber_id: Uuid,
    payment_method_id: Uuid,
    subscription_id: Uuid,
    initial_transaction_id: &str,
) -> Result<(), sqlx::Error> {
    for sequence in 0..3 {
        let day = sequence + 2;
        let identity = format!(
            "reclassified-renewal-{sequence}-{}",
            opaque_fixture_uuid(subscription_id)
        );
        let submitted_at = format!("2026-02-{day:02} 00:00:00+00");
        let resolved_at = format!("2026-02-{day:02} 00:01:00+00");
        insert_v1_terminal_subscription_attempt(
            pool,
            gateway,
            subscriber_id,
            payment_method_id,
            subscription_id,
            initial_transaction_id,
            "subscription_renewal",
            "declined",
            &identity,
            Some(&submitted_at),
            &resolved_at,
            None,
        )
        .await?;
    }
    for sequence in 0..2 {
        let day = sequence + 5;
        let identity = format!(
            "reclassified-recovery-{sequence}-{}",
            opaque_fixture_uuid(subscription_id)
        );
        let submitted_at = format!("2026-02-{day:02} 00:00:00+00");
        let resolved_at = format!("2026-02-{day:02} 00:01:00+00");
        insert_v1_terminal_subscription_attempt(
            pool,
            gateway,
            subscriber_id,
            payment_method_id,
            subscription_id,
            initial_transaction_id,
            "subscription_recovery",
            "declined",
            &identity,
            Some(&submitted_at),
            &resolved_at,
            None,
        )
        .await?;
    }
    Ok(())
}

pub(super) async fn insert_v1_initial_attempt(
    pool: &PgPool,
    gateway: GatewayAccountFixture,
    subscriber_id: Uuid,
    amount_cents: i32,
    request_fingerprint: &str,
    status: Option<&str>,
) -> Result<Uuid, sqlx::Error> {
    let attempt_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO billing_payment_attempts (
            id, billing_scope_id, subscriber_id, plan_key,
            attempt_kind, status, idempotency_key, request_fingerprint,
            amount_cents, currency, gateway_account_id,
            gateway_configuration_id, gateway_order_id
        ) VALUES (
            $1, $2, $3, 'base_subscription', 'subscription_initial',
            coalesce($4, 'pending'), $5, $6, $7, 'USD', $8, $9, $10
        )
        "#,
    )
    .bind(attempt_id)
    .bind(gateway.billing_scope_id)
    .bind(subscriber_id)
    .bind(status)
    .bind(format!(
        "legacy-initial-{}",
        opaque_fixture_uuid(attempt_id)
    ))
    .bind(request_fingerprint)
    .bind(amount_cents)
    .bind(gateway.gateway_account_id)
    .bind(gateway.gateway_configuration_id)
    .bind(format!(
        "legacy-initial-order-{}",
        opaque_fixture_uuid(attempt_id)
    ))
    .execute(pool)
    .await?;
    Ok(attempt_id)
}

pub(super) async fn insert_v1_discounted_initial_attempt(
    pool: &PgPool,
    gateway: GatewayAccountFixture,
    subscriber_id: Uuid,
    request_fingerprint: &str,
) -> Result<Uuid, sqlx::Error> {
    let discount_code_id = Uuid::now_v7();
    let code = format!("SAVE{}", opaque_fixture_uuid(discount_code_id));
    sqlx::query(
        r#"
        INSERT INTO billing_subscription_discount_codes (
            id, billing_scope_id, plan_key, code_normalized, display_code,
            status, discount_kind, amount_off_cents, currency, duration
        ) VALUES (
            $1, $2, 'base_subscription', $3, $3, 'active',
            'amount_off', 10, 'USD', 'indefinite'
        )
        "#,
    )
    .bind(discount_code_id)
    .bind(gateway.billing_scope_id)
    .bind(&code)
    .execute(pool)
    .await?;

    let discount_claim_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO billing_subscription_discount_claims (
            id, billing_scope_id, subscriber_id, plan_key,
            discount_code_id, code_snapshot, discount_kind,
            amount_off_cents, currency, duration, base_amount_cents,
            discounted_amount_cents, status
        ) VALUES (
            $1, $2, $3, 'base_subscription', $4, $5, 'amount_off',
            10, 'USD', 'indefinite', 100, 90, 'saved'
        )
        "#,
    )
    .bind(discount_claim_id)
    .bind(gateway.billing_scope_id)
    .bind(subscriber_id)
    .bind(discount_code_id)
    .bind(&code)
    .execute(pool)
    .await?;

    let attempt_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO billing_payment_attempts (
            id, billing_scope_id, subscriber_id, plan_key,
            attempt_kind, status, idempotency_key, request_fingerprint,
            amount_cents, currency, gateway_account_id,
            gateway_configuration_id, gateway_order_id,
            subscription_initial_discount_claim_id,
            subscription_initial_discount_code_id,
            subscription_initial_discount_code_snapshot,
            subscription_initial_discount_kind,
            subscription_initial_discount_amount_off_cents,
            subscription_initial_discount_currency,
            subscription_initial_discount_duration,
            subscription_initial_discount_base_amount_cents,
            subscription_initial_discount_discounted_amount_cents
        ) VALUES (
            $1, $2, $3, 'base_subscription', 'subscription_initial',
            'pending', $4, $5, 90, 'USD', $6, $7, $8,
            $9, $10, $11, 'amount_off', 10, 'USD', 'indefinite', 100, 90
        )
        "#,
    )
    .bind(attempt_id)
    .bind(gateway.billing_scope_id)
    .bind(subscriber_id)
    .bind(format!(
        "legacy-discounted-{}",
        opaque_fixture_uuid(attempt_id)
    ))
    .bind(request_fingerprint)
    .bind(gateway.gateway_account_id)
    .bind(gateway.gateway_configuration_id)
    .bind(format!(
        "legacy-discounted-order-{}",
        opaque_fixture_uuid(attempt_id)
    ))
    .bind(discount_claim_id)
    .bind(discount_code_id)
    .bind(code)
    .execute(pool)
    .await?;
    Ok(attempt_id)
}
