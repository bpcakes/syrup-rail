use super::*;

#[tokio::test]
async fn schema_v1_contains_no_host_or_cutover_vocabulary() -> Result<(), Box<dyn Error>> {
    if V1_INSTALL_SQL.trim().is_empty() {
        return Err(io::Error::other("version-1 install artifact is empty").into());
    }
    for forbidden in [
        "tenant_id",
        "user_id",
        "nmi_",
        "admin_user_id",
        "granted_by_admin_user_id",
        "revoked_by_admin_user_id",
        "app_resolution_code",
        "base_subscription_",
        "order_sale",
        "acquisition_channel",
        "google_ads_",
        "billing_pending_approved_",
        "2026-",
    ] {
        if V1_INSTALL_SQL.contains(forbidden) {
            return Err(io::Error::other(format!(
                "version-1 install artifact contains forbidden vocabulary {forbidden:?}"
            ))
            .into());
        }
    }

    let database = TestDatabase::start_v1("sr_schema_v1").await?;
    let result = assert_v1_conforms(&database.pool).await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn schema_v1_gateway_order_identity_is_account_scoped() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start_v1("sr_order_v1").await?;
    let result = async {
        let provider_key = "test_gateway";
        sqlx::query("INSERT INTO billing_gateway_provider_rate_limits (provider_key) VALUES ($1)")
            .bind(provider_key)
            .execute(&database.pool)
            .await?;

        let scope_a = Uuid::now_v7();
        let scope_b = Uuid::now_v7();
        let account_a = Uuid::now_v7();
        let account_b = Uuid::now_v7();
        for (account, scope) in [(account_a, scope_a), (account_b, scope_b)] {
            sqlx::query(
                r#"
                INSERT INTO billing_gateway_accounts (
                    id,
                    billing_scope_id,
                    provider_key,
                    gateway_configuration_id
                ) VALUES ($1, $2, $3, $4)
                "#,
            )
            .bind(account)
            .bind(scope)
            .bind(provider_key)
            .bind(Uuid::now_v7())
            .execute(&database.pool)
            .await?;
        }

        let subscriber = Uuid::now_v7();
        insert_host_charge_attempt(
            &database.pool,
            scope_a,
            subscriber,
            account_a,
            "shared-order-reference",
        )
        .await?;
        insert_host_charge_attempt(
            &database.pool,
            scope_b,
            subscriber,
            account_b,
            "shared-order-reference",
        )
        .await?;

        let duplicate = insert_host_charge_attempt(
            &database.pool,
            scope_a,
            Uuid::now_v7(),
            account_a,
            "shared-order-reference",
        )
        .await;
        let error =
            duplicate.expect_err("one gateway account must reject a duplicate order reference");
        let constraint = error
            .as_database_error()
            .and_then(|error| error.constraint());
        if constraint != Some("billing_payment_attempts_gateway_order_idx") {
            return Err(
                io::Error::other(format!("unexpected duplicate-order error: {error}")).into(),
            );
        }
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn schema_v1_payment_facts_supports_skip_locked_row_locks() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start_v1("sr_view_lock_v1").await?;
    let result = async {
        let gateway = create_gateway_account(&database.pool, "test_gateway").await?;
        let attempt_id = insert_host_charge_attempt_record(
            &database.pool,
            gateway,
            Uuid::now_v7(),
            Uuid::now_v7(),
            "view-lock-order",
            "view-lock-idempotency",
        )
        .await?;

        let mut locker = database.pool.begin().await?;
        let locked = sqlx::query_scalar::<_, Uuid>(
            r#"
            SELECT attempt_id
            FROM billing_payment_facts
            WHERE attempt_id = $1
            FOR UPDATE
            "#,
        )
        .bind(attempt_id)
        .fetch_one(&mut *locker)
        .await?;
        if locked != attempt_id {
            return Err(io::Error::other("payment-facts view returned the wrong attempt").into());
        }

        let mut contender = database.pool.begin().await?;
        let skipped = sqlx::query_scalar::<_, Uuid>(
            r#"
            SELECT attempt_id
            FROM billing_payment_facts
            WHERE attempt_id = $1
            FOR UPDATE SKIP LOCKED
            "#,
        )
        .bind(attempt_id)
        .fetch_optional(&mut *contender)
        .await?;
        if skipped.is_some() {
            return Err(io::Error::other(
                "payment-facts SKIP LOCKED did not observe the underlying attempt lock",
            )
            .into());
        }

        locker.rollback().await?;
        let acquired = sqlx::query_scalar::<_, Uuid>(
            r#"
            SELECT attempt_id
            FROM billing_payment_facts
            WHERE attempt_id = $1
            FOR UPDATE SKIP LOCKED
            "#,
        )
        .bind(attempt_id)
        .fetch_one(&mut *contender)
        .await?;
        contender.rollback().await?;
        if acquired != attempt_id {
            return Err(io::Error::other(
                "payment-facts row was not lockable after the competing lock released",
            )
            .into());
        }
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn schema_v1_gateway_lifecycle_state_round_trips() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start_v1("sr_lifecycle_v1").await?;
    let result = async {
        let provider_key = "test_gateway";
        let scope = Uuid::now_v7();
        let account = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO billing_gateway_provider_rate_limits (provider_key) VALUES ($1)",
        )
        .bind(provider_key)
        .execute(&database.pool)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO billing_gateway_accounts (
                id,
                billing_scope_id,
                provider_key,
                gateway_configuration_id
            ) VALUES ($1, $2, $3, $4)
            "#,
        )
        .bind(account)
        .bind(scope)
        .bind(provider_key)
        .bind(Uuid::now_v7())
        .execute(&database.pool)
        .await?;

        let valid = [
            ("unknown", None),
            ("pending_settlement", None),
            ("settled", None),
            ("settled", Some(1)),
            ("voided", None),
            ("refunded", Some(100)),
            ("chargeback", None),
            ("chargeback", Some(100)),
        ];
        for (position, (status, refunded_amount_cents)) in valid.iter().enumerate() {
            let order = format!("valid-lifecycle-{position}");
            let row = sqlx::query_as::<_, (String, Option<i32>)>(
                r#"
                INSERT INTO billing_gateway_lifecycle_pending_updates (
                    billing_scope_id,
                    gateway_account_id,
                    gateway_order_id,
                    gateway_lifecycle_status,
                    refunded_amount_cents
                ) VALUES ($1, $2, $3, $4, $5)
                RETURNING gateway_lifecycle_status, refunded_amount_cents
                "#,
            )
            .bind(scope)
            .bind(account)
            .bind(order)
            .bind(*status)
            .bind(*refunded_amount_cents)
            .fetch_one(&database.pool)
            .await?;
            if row != (status.to_string(), *refunded_amount_cents) {
                return Err(io::Error::other(format!(
                    "lifecycle state changed during staging: expected {status:?}/{refunded_amount_cents:?}, found {row:?}"
                ))
                .into());
            }
        }

        let invalid = [
            ("unknown", Some(1)),
            ("pending_settlement", Some(1)),
            ("voided", Some(1)),
            ("settled", Some(0)),
            ("refunded", None),
            ("chargeback", Some(0)),
            ("not_a_state", None),
        ];
        for (position, (status, refunded_amount_cents)) in invalid.iter().enumerate() {
            let inserted = sqlx::query(
                r#"
                INSERT INTO billing_gateway_lifecycle_pending_updates (
                    billing_scope_id,
                    gateway_account_id,
                    gateway_order_id,
                    gateway_lifecycle_status,
                    refunded_amount_cents
                ) VALUES ($1, $2, $3, $4, $5)
                "#,
            )
            .bind(scope)
            .bind(account)
            .bind(format!("invalid-lifecycle-{position}"))
            .bind(*status)
            .bind(*refunded_amount_cents)
            .execute(&database.pool)
            .await;
            if inserted.is_ok() {
                return Err(io::Error::other(format!(
                    "invalid lifecycle state was accepted: {status:?}/{refunded_amount_cents:?}"
                ))
                .into());
            }
        }
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}
