use super::*;

pub(super) async fn create_subscription_fixture(
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
            initial_transaction_id
        ) VALUES (
            $1, $2, $3, 'base_subscription', 'active', $4, $5, 100, 'USD',
            '2026-01-01 00:00:00+00', '2026-02-01 00:00:00+00',
            '2026-02-01 00:00:00+00', $6
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

pub(super) async fn insert_payment_method_gateway_text(
    pool: &PgPool,
    gateway: GatewayAccountFixture,
    subscriber_id: Uuid,
    field: &str,
    value: &str,
) -> Result<sqlx::postgres::PgQueryResult, sqlx::Error> {
    debug_assert!(["payment_type", "card_brand"].contains(&field));
    let payment_method_id = Uuid::now_v7();
    let query = format!(
        r#"
        INSERT INTO billing_payment_methods (
            id,
            billing_scope_id,
            subscriber_id,
            gateway_account_id,
            gateway_payment_method_reference,
            status,
            {field}
        ) VALUES ($1, $2, $3, $4, $5, 'active', $6)
        "#
    );
    sqlx::query(&query)
        .bind(payment_method_id)
        .bind(gateway.billing_scope_id)
        .bind(subscriber_id)
        .bind(gateway.gateway_account_id)
        .bind(format!("vault_{}", opaque_fixture_uuid(payment_method_id)))
        .bind(value)
        .execute(pool)
        .await
}

pub(super) async fn insert_attempt_gateway_text(
    pool: &PgPool,
    gateway: GatewayAccountFixture,
    subscriber_id: Uuid,
    field: &str,
    value: &str,
) -> Result<sqlx::postgres::PgQueryResult, sqlx::Error> {
    debug_assert!(
        [
            "gateway_response",
            "gateway_response_code",
            "gateway_response_text",
            "gateway_condition",
            "payment_type",
            "card_brand",
            "gateway_lifecycle_action",
        ]
        .contains(&field)
    );
    let attempt_id = Uuid::now_v7();
    let target_id = Uuid::now_v7();
    let query = format!(
        r#"
        INSERT INTO billing_payment_attempts (
            id,
            billing_scope_id,
            subscriber_id,
            host_charge_target_id,
            attempt_kind,
            status,
            idempotency_key,
            request_fingerprint,
            amount_cents,
            currency,
            gateway_account_id,
            gateway_configuration_id,
            gateway_order_id,
            {field}
        ) VALUES (
            $1, $2, $3, $4, 'host_charge', 'pending', $5, $6,
            100, 'USD', $7, $8, $9, $10
        )
        "#
    );
    sqlx::query(&query)
        .bind(attempt_id)
        .bind(gateway.billing_scope_id)
        .bind(subscriber_id)
        .bind(target_id)
        .bind(format!("diagnostic-{}", opaque_fixture_uuid(attempt_id)))
        .bind(format!("host_charge:{target_id}:100:USD"))
        .bind(gateway.gateway_account_id)
        .bind(gateway.gateway_configuration_id)
        .bind(format!(
            "diagnostic-order-{}",
            opaque_fixture_uuid(attempt_id)
        ))
        .bind(value)
        .execute(pool)
        .await
}

pub(super) async fn insert_charge_gateway_text(
    pool: &PgPool,
    gateway: GatewayAccountFixture,
    subscriber_id: Uuid,
    field: &str,
    value: &str,
) -> Result<sqlx::postgres::PgQueryResult, sqlx::Error> {
    debug_assert!(
        [
            "gateway_response",
            "gateway_response_code",
            "gateway_response_text",
            "gateway_condition",
            "payment_type",
            "card_brand",
        ]
        .contains(&field)
    );
    let target_id = Uuid::now_v7();
    let order_id = format!("charge-text-order-{}", opaque_fixture_uuid(Uuid::now_v7()));
    let attempt_id = insert_host_charge_attempt_record(
        pool,
        gateway,
        subscriber_id,
        target_id,
        &order_id,
        &format!("charge-text-{}", opaque_fixture_uuid(Uuid::now_v7())),
    )
    .await?;
    let query = format!(
        r#"
        INSERT INTO billing_processor_charges (
            attempt_id,
            billing_scope_id,
            gateway_account_id,
            gateway_order_id,
            attempt_kind,
            amount_cents,
            currency,
            {field}
        ) VALUES ($1, $2, $3, $4, 'host_charge', 100, 'USD', $5)
        "#
    );
    sqlx::query(&query)
        .bind(attempt_id)
        .bind(gateway.billing_scope_id)
        .bind(gateway.gateway_account_id)
        .bind(order_id)
        .bind(value)
        .execute(pool)
        .await
}

pub(super) async fn insert_attestation_gateway_text(
    pool: &PgPool,
    gateway: GatewayAccountFixture,
    subscriber_id: Uuid,
    field: &str,
    value: &str,
) -> Result<sqlx::postgres::PgQueryResult, sqlx::Error> {
    debug_assert!(
        [
            "gateway_response",
            "gateway_response_code",
            "gateway_response_text",
            "gateway_condition",
            "payment_type",
            "card_brand",
        ]
        .contains(&field)
    );
    let target_id = Uuid::now_v7();
    let order_id = format!("attestation-order-{}", opaque_fixture_uuid(Uuid::now_v7()));
    let transaction_id = format!("txn_{}", opaque_fixture_uuid(Uuid::now_v7()));
    let attempt_id = insert_host_charge_attempt_record(
        pool,
        gateway,
        subscriber_id,
        target_id,
        &order_id,
        &format!("attestation-{}", opaque_fixture_uuid(Uuid::now_v7())),
    )
    .await?;
    let charge_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO billing_processor_charges (
            id,
            attempt_id,
            billing_scope_id,
            gateway_account_id,
            gateway_order_id,
            gateway_transaction_id,
            attempt_kind,
            amount_cents,
            currency
        ) VALUES ($1, $2, $3, $4, $5, $6, 'host_charge', 100, 'USD')
        "#,
    )
    .bind(charge_id)
    .bind(attempt_id)
    .bind(gateway.billing_scope_id)
    .bind(gateway.gateway_account_id)
    .bind(&order_id)
    .bind(&transaction_id)
    .execute(pool)
    .await?;

    let query = format!(
        r#"
        INSERT INTO billing_external_reversal_attestations (
            attempt_id,
            processor_charge_id,
            actor_id,
            reversal_kind,
            reason,
            prior_resolution_code,
            final_resolution_code,
            gateway_account_id,
            gateway_configuration_id,
            gateway_order_id,
            amount_cents,
            currency,
            gateway_transaction_id,
            attested_at,
            {field}
        ) VALUES (
            $1, $2, $3, 'refund', 'operator confirmed refund',
            'processor_charge_external_reversal_required',
            'processor_charge_externally_refunded',
            $4, $5, $6, 100, 'USD', $7, clock_timestamp(), $8
        )
        "#
    );
    sqlx::query(&query)
        .bind(attempt_id)
        .bind(charge_id)
        .bind(Uuid::now_v7())
        .bind(gateway.gateway_account_id)
        .bind(gateway.gateway_configuration_id)
        .bind(order_id)
        .bind(transaction_id)
        .bind(value)
        .execute(pool)
        .await
}

pub(super) async fn insert_lifecycle_gateway_text(
    pool: &PgPool,
    gateway: GatewayAccountFixture,
    field: &str,
    value: &str,
) -> Result<sqlx::postgres::PgQueryResult, sqlx::Error> {
    debug_assert!(["gateway_condition", "gateway_lifecycle_action"].contains(&field));
    let query = format!(
        r#"
        INSERT INTO billing_gateway_lifecycle_pending_updates (
            billing_scope_id,
            gateway_account_id,
            gateway_order_id,
            gateway_lifecycle_status,
            {field}
        ) VALUES ($1, $2, $3, 'unknown', $4)
        "#
    );
    sqlx::query(&query)
        .bind(gateway.billing_scope_id)
        .bind(gateway.gateway_account_id)
        .bind(format!(
            "lifecycle-text-{}",
            opaque_fixture_uuid(Uuid::now_v7())
        ))
        .bind(value)
        .execute(pool)
        .await
}

pub(super) async fn expect_conformance_rejection(
    pool: &PgPool,
    context: &str,
) -> Result<(), Box<dyn Error>> {
    if assert_v1_conforms(pool).await.is_ok() {
        Err(io::Error::other(format!("catalog conformance accepted {context}")).into())
    } else {
        Ok(())
    }
}

pub(super) async fn assert_admission(
    pool: &PgPool,
    gateway: GatewayAccountFixture,
    subscriber_id: Uuid,
    target_id: Uuid,
    expectation: (&str, Option<&str>, Option<Uuid>, &str),
) -> Result<(), Box<dyn Error>> {
    let (mode, idempotency_key, attempt_id, expected) = expectation;
    let actual = host_charge_admission(
        pool,
        gateway,
        subscriber_id,
        target_id,
        mode,
        idempotency_key,
        attempt_id,
    )
    .await?;
    if actual == expected {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "host charge admission {mode:?} returned {actual:?}, expected {expected:?}"
        ))
        .into())
    }
}

pub(super) async fn host_charge_admission(
    pool: &PgPool,
    gateway: GatewayAccountFixture,
    subscriber_id: Uuid,
    target_id: Uuid,
    mode: &str,
    idempotency_key: Option<&str>,
    attempt_id: Option<Uuid>,
) -> Result<String, sqlx::Error> {
    sqlx::query_scalar(
        r#"
        SELECT billing_host_charge_ledger_admission(
            $1, $2, $3, $4, $5, $6
        )
        "#,
    )
    .bind(gateway.billing_scope_id)
    .bind(subscriber_id)
    .bind(target_id)
    .bind(mode)
    .bind(idempotency_key)
    .bind(attempt_id)
    .fetch_one(pool)
    .await
}

pub(super) fn expect_database_rejection(
    result: Result<sqlx::postgres::PgQueryResult, sqlx::Error>,
    context: &str,
) -> Result<(), Box<dyn Error>> {
    if result.is_ok() {
        Err(io::Error::other(format!("database accepted {context}")).into())
    } else {
        Ok(())
    }
}

pub(super) fn expect_database_constraint(
    result: Result<sqlx::postgres::PgQueryResult, sqlx::Error>,
    expected_constraint: &str,
) -> Result<(), Box<dyn Error>> {
    let error = result.expect_err("database operation should violate a constraint");
    let actual = error
        .as_database_error()
        .and_then(|database_error| database_error.constraint());
    if actual == Some(expected_constraint) {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "expected constraint {expected_constraint:?}, found {actual:?}: {error}"
        ))
        .into())
    }
}

pub(super) async fn insert_host_charge_attempt(
    pool: &PgPool,
    billing_scope_id: Uuid,
    subscriber_id: Uuid,
    gateway_account_id: Uuid,
    gateway_order_id: &str,
) -> Result<Uuid, sqlx::Error> {
    insert_host_charge_attempt_record(
        pool,
        GatewayAccountFixture {
            billing_scope_id,
            gateway_account_id,
            gateway_configuration_id: Uuid::now_v7(),
        },
        subscriber_id,
        Uuid::now_v7(),
        gateway_order_id,
        &format!("idempotency-{}", Uuid::now_v7()),
    )
    .await
}

pub(super) async fn insert_host_charge_attempt_record(
    pool: &PgPool,
    gateway: GatewayAccountFixture,
    subscriber_id: Uuid,
    host_charge_target_id: Uuid,
    gateway_order_id: &str,
    idempotency_key: &str,
) -> Result<Uuid, sqlx::Error> {
    let attempt_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO billing_payment_attempts (
            id,
            billing_scope_id,
            subscriber_id,
            host_charge_target_id,
            attempt_kind,
            status,
            idempotency_key,
            request_fingerprint,
            amount_cents,
            currency,
            gateway_account_id,
            gateway_configuration_id,
            gateway_order_id
        ) VALUES (
            $1,
            $2,
            $3,
            $4,
            'host_charge',
            'pending',
            $5,
            $6,
            100,
            'USD',
            $7,
            $8,
            $9
        )
        "#,
    )
    .bind(attempt_id)
    .bind(gateway.billing_scope_id)
    .bind(subscriber_id)
    .bind(host_charge_target_id)
    .bind(idempotency_key)
    .bind(format!("host_charge:{host_charge_target_id}:100:USD"))
    .bind(gateway.gateway_account_id)
    .bind(gateway.gateway_configuration_id)
    .bind(gateway_order_id)
    .execute(pool)
    .await?;
    Ok(attempt_id)
}
