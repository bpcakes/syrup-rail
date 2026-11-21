use std::{error::Error, io};

use super::storage_fixtures::assert_admission;
use super::v5::populated_v4;
use super::*;

const V7_COLUMN: &str = "gateway_avs_response";

/// Serializes every row of every canonical table without the version-7
/// column, so a v6 snapshot compares with the same rows after the upgrade.
async fn canonical_rows_without_v7_column(pool: &PgPool) -> Result<Vec<String>, sqlx::Error> {
    let mut snapshots = Vec::with_capacity(REQUIRED_TABLES.len());
    for table in REQUIRED_TABLES {
        let query = format!(
            r#"
            SELECT '{table}:' || COALESCE(
                jsonb_agg(
                    to_jsonb(rows) - '{V7_COLUMN}'
                    ORDER BY (to_jsonb(rows) - '{V7_COLUMN}')::text
                ),
                '[]'::jsonb
            )::text
            FROM public.{table} AS rows
            "#
        );
        snapshots.push(
            sqlx::query_scalar(sqlx::AssertSqlSafe(query.as_str()))
                .fetch_one(pool)
                .await?,
        );
    }
    Ok(snapshots)
}

async fn v7_column_shape(
    pool: &PgPool,
) -> Result<Option<(String, String, i16, Option<i32>)>, sqlx::Error> {
    sqlx::query_as(
        r#"
        SELECT columns.data_type, columns.is_nullable, attributes.attnum,
            columns.character_maximum_length::integer
        FROM information_schema.columns AS columns
        JOIN pg_catalog.pg_attribute AS attributes
            ON attributes.attrelid = 'public.billing_payment_attempts'::regclass
            AND attributes.attname = columns.column_name
        WHERE columns.table_schema = 'public'
            AND columns.table_name = 'billing_payment_attempts'
            AND columns.column_name = $1
        "#,
    )
    .bind(V7_COLUMN)
    .fetch_optional(pool)
    .await
}

/// The attempt table's storage file, which a table rewrite replaces.
async fn attempt_table_storage(pool: &PgPool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT relfilenode::bigint FROM pg_catalog.pg_class \
         WHERE oid = 'public.billing_payment_attempts'::regclass",
    )
    .fetch_one(pool)
    .await
}

/// Writes a 64-character AVS value to an existing attempt and rejects a
/// 65-character one, then rolls both writes back. An explicit cast would
/// truncate silently; only a real write shows the column enforces its bound.
async fn assert_avs_length_bound(pool: &PgPool) -> Result<(), Box<dyn Error>> {
    let mut transaction = pool.begin().await?;
    let attempt_id: Uuid =
        sqlx::query_scalar("SELECT id FROM billing_payment_attempts ORDER BY id LIMIT 1")
            .fetch_one(&mut *transaction)
            .await?;
    let write = "UPDATE billing_payment_attempts SET gateway_avs_response = $1 WHERE id = $2";
    sqlx::query(write)
        .bind("A".repeat(64))
        .bind(attempt_id)
        .execute(&mut *transaction)
        .await?;
    let error = sqlx::query(write)
        .bind("A".repeat(65))
        .bind(attempt_id)
        .execute(&mut *transaction)
        .await
        .expect_err("a 65-character AVS value must be rejected");
    assert_eq!(
        error
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("22001"),
        "{error}"
    );
    transaction.rollback().await?;
    Ok(())
}

#[tokio::test]
async fn runtime_schema_v7_accepts_fresh_install_and_populated_v6_upgrade()
-> Result<(), Box<dyn Error>> {
    if V7_INSTALL_SQL.trim().is_empty() || V6_TO_V7_UPGRADE_SQL.trim().is_empty() {
        return Err(io::Error::other("schema-v7 artifacts must not be empty").into());
    }
    let fresh = TestDatabase::start("sr_fresh_v7").await?;
    let upgraded = TestDatabase::start_v3("sr_upgrade_v7").await?;
    let result = async {
        populated_v4(&upgraded).await?;
        upgraded.upgrade_v4_to_v5().await?;
        upgraded.upgrade_v5_to_v6().await?;
        assert_v6_conforms(&upgraded.pool).await?;
        let rows_before = canonical_rows_without_v7_column(&upgraded.pool).await?;
        let attempt_rows: i64 = sqlx::query_scalar("SELECT count(*) FROM billing_payment_attempts")
            .fetch_one(&upgraded.pool)
            .await?;
        assert!(attempt_rows >= 2, "fixture must be populated");
        let storage_before = attempt_table_storage(&upgraded.pool).await?;

        upgraded.upgrade_v6_to_v7().await?;
        // The bounded column is added without rewriting the populated table.
        assert_eq!(attempt_table_storage(&upgraded.pool).await?, storage_before);
        assert_v7_conforms(&fresh.pool).await?;
        assert_v7_conforms(&upgraded.pool).await?;
        let fresh_fingerprint = canonical_catalog_fingerprint(&fresh.pool).await?;
        let upgraded_fingerprint = canonical_catalog_fingerprint(&upgraded.pool).await?;
        assert_eq!(fresh_fingerprint, upgraded_fingerprint);
        assert_eq!(fresh_fingerprint, V7_CATALOG_FINGERPRINT);
        assert_ne!(V7_CATALOG_FINGERPRINT, V6_CATALOG_FINGERPRINT);

        // The column is the attempt table's last, nullable varchar(64) column
        // in both paths, so ADD COLUMN and the fresh install agree.
        let fresh_shape = v7_column_shape(&fresh.pool).await?;
        assert_eq!(fresh_shape, v7_column_shape(&upgraded.pool).await?);
        let (data_type, nullable, _, max_length) = fresh_shape.expect("v7 adds the AVS column");
        // Every code the domain observation retains fits the column.
        assert_eq!(
            max_length,
            Some(i32::try_from(
                syrup_rail::MAX_GATEWAY_DIAGNOSTIC_CODE_BYTES
            )?)
        );
        assert_eq!(
            (data_type.as_str(), nullable.as_str(), max_length),
            ("character varying", "YES", Some(64))
        );

        // The type bounds writes. Only the upgraded database has attempt rows;
        // the shape check above shows the fresh install has the same type.
        assert_avs_length_bound(&upgraded.pool).await?;

        // Historical rows are unchanged and infer no AVS result.
        assert_eq!(
            canonical_rows_without_v7_column(&upgraded.pool).await?,
            rows_before
        );
        let populated: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM billing_payment_attempts WHERE gateway_avs_response IS NOT NULL",
        )
        .fetch_one(&upgraded.pool)
        .await?;
        assert_eq!(populated, 0);
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let fresh_cleanup = fresh.cleanup().await;
    let upgraded_cleanup = upgraded.cleanup().await;
    result?;
    fresh_cleanup?;
    upgraded_cleanup
}

#[tokio::test]
async fn bridge_assertion_accepts_exactly_v6_or_v7() -> Result<(), Box<dyn Error>> {
    let v5 = TestDatabase::start_v5("sr_v7_br_v5").await?;
    let v6 = TestDatabase::start_v6("sr_v7_br_v6").await?;
    let v7 = TestDatabase::start("sr_v7_br_v7").await?;
    let retyped = TestDatabase::start("sr_v7_br_type").await?;
    let narrowed = TestDatabase::start("sr_v7_br_len").await?;
    let unbounded = TestDatabase::start("sr_v7_br_unb").await?;
    let constrained = TestDatabase::start("sr_v7_br_check").await?;
    let result = async {
        crate::assert_runtime_schema_v6_or_v7_compatible(&v6.pool).await?;
        crate::assert_runtime_schema_v6_or_v7_compatible(&v7.pool).await?;
        crate::assert_runtime_schema_v6_compatible(&v6.pool).await?;
        crate::assert_runtime_schema_v7_compatible(&v7.pool).await?;

        // The exact assertions still tell the versions apart. A v6-exact
        // runtime refuses a v7 catalog, which is why the bridge exists.
        assert!(matches!(
            crate::assert_runtime_schema_v6_compatible(&v7.pool).await,
            Err(crate::SchemaConformanceError::Contract { version: 6, .. })
        ));
        assert!(matches!(
            crate::assert_runtime_schema_v7_compatible(&v6.pool).await,
            Err(crate::SchemaConformanceError::Contract { version: 7, .. })
        ));
        assert!(matches!(
            crate::assert_runtime_schema_v6_or_v7_compatible(&v5.pool).await,
            Err(crate::SchemaConformanceError::Contract { version: 6, .. })
        ));

        // A v7-like catalog that is not exactly v7 is drift, not acceptance.
        sqlx::raw_sql(
            "ALTER TABLE billing_payment_attempts \
             ALTER COLUMN gateway_avs_response TYPE text",
        )
        .execute(&retyped.pool)
        .await?;
        // A change to the length bound alone is drift too.
        sqlx::raw_sql(
            "ALTER TABLE billing_payment_attempts \
             ALTER COLUMN gateway_avs_response TYPE varchar(8)",
        )
        .execute(&narrowed.pool)
        .await?;
        sqlx::raw_sql(
            "ALTER TABLE billing_payment_attempts \
             ALTER COLUMN gateway_avs_response TYPE varchar",
        )
        .execute(&unbounded.pool)
        .await?;
        sqlx::raw_sql(
            "ALTER TABLE billing_payment_attempts \
             ADD CONSTRAINT billing_payment_attempts_gateway_avs_response_check \
             CHECK (octet_length(gateway_avs_response) <= 8)",
        )
        .execute(&constrained.pool)
        .await?;
        for drifted in [&retyped, &narrowed, &unbounded, &constrained] {
            let error = crate::assert_runtime_schema_v6_or_v7_compatible(&drifted.pool)
                .await
                .expect_err("a drifted v7 catalog must be rejected");
            assert!(
                matches!(
                    &error,
                    crate::SchemaConformanceError::Contract { version: 6, detail }
                        if detail.contains("fingerprint")
                            && detail.contains(&format!("{V6_CATALOG_FINGERPRINT:#018x}"))
                            && detail.contains(&format!("{V7_CATALOG_FINGERPRINT:#018x}"))
                ),
                "{error}"
            );
            assert!(
                crate::assert_runtime_schema_v7_compatible(&drifted.pool)
                    .await
                    .is_err()
            );
        }
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanups = [
        v5.cleanup().await,
        v6.cleanup().await,
        v7.cleanup().await,
        retyped.cleanup().await,
        narrowed.cleanup().await,
        unbounded.cleanup().await,
        constrained.cleanup().await,
    ];
    result?;
    for cleanup in cleanups {
        cleanup?;
    }
    Ok(())
}

async fn assert_submit_rejects_avs_evidence(pool: &PgPool) -> Result<(), Box<dyn Error>> {
    let gateway = create_gateway_account(pool, "test_gateway").await?;
    let subscriber_id = Uuid::now_v7();
    let target_id = Uuid::now_v7();
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
            gateway_order_id,
            required_gateway_account_mode
        ) VALUES (
            $1, $2, $3, $4, 'host_charge', 'pending', 'avs-key',
            'host_charge:avs', 100, 'USD', $5, $6, 'avs-order', 'test'
        )
        "#,
    )
    .bind(attempt_id)
    .bind(gateway.billing_scope_id)
    .bind(subscriber_id)
    .bind(target_id)
    .bind(gateway.gateway_account_id)
    .bind(gateway.gateway_configuration_id)
    .execute(pool)
    .await?;
    assert_admission(
        pool,
        gateway,
        subscriber_id,
        target_id,
        ("submit", None, Some(attempt_id), "safe"),
    )
    .await?;

    sqlx::query("UPDATE billing_payment_attempts SET gateway_avs_response = 'Y' WHERE id = $1")
        .bind(attempt_id)
        .execute(pool)
        .await?;
    assert_admission(
        pool,
        gateway,
        subscriber_id,
        target_id,
        ("submit", None, Some(attempt_id), "unsafe"),
    )
    .await
}

#[tokio::test]
async fn host_charge_submit_rejects_a_pending_attempt_with_an_avs_result()
-> Result<(), Box<dyn Error>> {
    let fresh = TestDatabase::start("sr_v7_avs_new").await?;
    let upgraded = TestDatabase::start_v6("sr_v7_avs_up").await?;
    let result = async {
        upgraded.upgrade_v6_to_v7().await?;
        assert_submit_rejects_avs_evidence(&fresh.pool).await?;
        assert_submit_rejects_avs_evidence(&upgraded.pool).await
    }
    .await;
    let fresh_cleanup = fresh.cleanup().await;
    let upgraded_cleanup = upgraded.cleanup().await;
    result?;
    fresh_cleanup?;
    upgraded_cleanup
}

#[tokio::test]
async fn v6_to_v7_upgrade_rolls_back_on_lock_timeout_and_refuses_a_rerun_or_v5()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start_v6("sr_v7_rollback").await?;
    let v5 = TestDatabase::start_v5("sr_v7_needs_v6").await?;
    let result = async {
        // A concurrent writer's open transaction holds a lock that conflicts
        // with ALTER TABLE; the upgrade gives up at its lock timeout instead
        // of queueing other statements behind it indefinitely.
        let mut blocker = database.pool.begin().await?;
        sqlx::query("LOCK TABLE billing_payment_attempts IN ROW EXCLUSIVE MODE")
            .execute(&mut *blocker)
            .await?;
        let error = database
            .upgrade_v6_to_v7()
            .await
            .expect_err("a held table lock must stop the upgrade at its lock timeout");
        assert!(
            error.to_string().contains("lock timeout"),
            "unexpected upgrade failure: {error}"
        );
        blocker.rollback().await?;
        assert_v6_conforms(&database.pool).await?;
        assert_eq!(v7_column_shape(&database.pool).await?, None);

        database.upgrade_v6_to_v7().await?;
        assert_v7_conforms(&database.pool).await?;
        let rerun = database
            .upgrade_v6_to_v7()
            .await
            .expect_err("the upgrade must refuse to run twice");
        assert!(
            rerun
                .to_string()
                .contains("schema-v7 upgrade is already applied"),
            "unexpected rerun failure: {rerun}"
        );
        assert_v7_conforms(&database.pool).await?;

        let wrong_version = v5
            .upgrade_v6_to_v7()
            .await
            .expect_err("the upgrade must refuse a schema-v5 database");
        assert!(
            wrong_version
                .to_string()
                .contains("schema-v7 upgrade requires schema v6"),
            "unexpected v5 failure: {wrong_version}"
        );
        assert_v5_conforms(&v5.pool).await?;
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    let v5_cleanup = v5.cleanup().await;
    result?;
    cleanup?;
    v5_cleanup
}

/// Runs the runtime's attempt reads and PL/pgSQL functions on `pool`, so each
/// caches its prepared statement and function plan on that connection.
async fn read_attempts_on(
    pool: &PgPool,
    scope_id: Uuid,
    attempt_id: Uuid,
    gateway_account_id: Uuid,
    label: &str,
) -> Result<(), Box<dyn Error>> {
    let mut transaction = pool.begin().await?;
    let attempt = crate::attempts::find_payment_attempt_by_id_in_transaction(
        &mut transaction,
        syrup_rail::BillingScopeId::new(scope_id),
        syrup_rail::PaymentAttemptId::new(attempt_id),
    )
    .await?
    .expect("fixture attempt must be readable");
    transaction.commit().await?;
    crate::claim_exact_reconciliation_attempts(
        pool,
        syrup_rail::GatewayAccountId::new(gateway_account_id),
    )
    .await?;

    // Also exercise PL/pgSQL functions (`billing_host_charge_ledger_admission`
    // and the `%ROWTYPE` trigger `billing_set_processor_charge_attempt_dimensions`)
    // on the same pooled connection before and after the online upgrade.
    let target_id = Uuid::now_v7();
    let charge_attempt_id = Uuid::now_v7();
    let order_id = format!("cached-order-{label}");
    sqlx::query(
        r#"
        INSERT INTO billing_payment_attempts (
            id, billing_scope_id, subscriber_id, host_charge_target_id,
            attempt_kind, status, idempotency_key, request_fingerprint,
            amount_cents, currency, gateway_account_id,
            gateway_configuration_id, gateway_order_id,
            required_gateway_account_mode
        ) VALUES (
            $1, $2, $3, $4, 'host_charge', 'pending', $5, $6,
            100, 'USD', $7, $8, $9, 'live'
        )
        "#,
    )
    .bind(charge_attempt_id)
    .bind(scope_id)
    .bind(attempt.identity().subscriber_id().as_uuid())
    .bind(target_id)
    .bind(format!("cached-key-{label}"))
    .bind(format!("host_charge:{target_id}:100:USD"))
    .bind(gateway_account_id)
    .bind(attempt.identity().gateway_configuration_id().as_uuid())
    .bind(&order_id)
    .execute(pool)
    .await?;
    let admission: String = sqlx::query_scalar(
        "SELECT billing_host_charge_ledger_admission($1, $2, $3, 'submit', NULL, $4)",
    )
    .bind(scope_id)
    .bind(attempt.identity().subscriber_id().as_uuid())
    .bind(target_id)
    .bind(charge_attempt_id)
    .fetch_one(pool)
    .await?;
    assert_eq!(admission, "safe");
    let dimensions: (String, i32) = sqlx::query_as(
        r#"
        INSERT INTO billing_processor_charges (
            attempt_id, billing_scope_id, gateway_account_id,
            gateway_order_id, gateway_transaction_id
        ) VALUES ($1, $2, $3, $4, $5)
        RETURNING attempt_kind, amount_cents
        "#,
    )
    .bind(charge_attempt_id)
    .bind(scope_id)
    .bind(gateway_account_id)
    .bind(&order_id)
    .bind(format!("txn_{}", opaque_fixture_uuid(charge_attempt_id)))
    .fetch_one(pool)
    .await?;
    assert_eq!(dimensions, ("host_charge".to_owned(), 100));
    Ok(())
}

#[tokio::test]
async fn runtime_attempt_reads_cached_on_v6_survive_the_v7_upgrade() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start_v3("sr_cached_v7").await?;
    let result = async {
        populated_v4(&database).await?;
        database.upgrade_v4_to_v5().await?;
        database.upgrade_v5_to_v6().await?;
        let (scope_id, attempt_id, gateway_account_id): (Uuid, Uuid, Uuid) = sqlx::query_as(
            "SELECT billing_scope_id, id, gateway_account_id FROM billing_payment_attempts \
             ORDER BY created_at, id LIMIT 1",
        )
        .fetch_one(&database.pool)
        .await?;
        // One connection, so the second round reuses the statements and
        // PL/pgSQL plans the first round cached against the v6 catalog.
        let serving = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_with((*database.pool.connect_options()).clone())
            .await?;
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&serving)
            .await?;
        let rounds = async {
            read_attempts_on(&serving, scope_id, attempt_id, gateway_account_id, "v6").await?;
            database.upgrade_v6_to_v7().await?;
            assert_v7_conforms(&database.pool).await?;
            read_attempts_on(&serving, scope_id, attempt_id, gateway_account_id, "v7").await?;
            let pid_after: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                .fetch_one(&serving)
                .await?;
            assert_eq!(pid_after, pid);
            Ok::<(), Box<dyn Error>>(())
        }
        .await;
        serving.close().await;
        rounds
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn catalog_snapshot_spanning_cutover_keeps_the_v6_fingerprint() -> Result<(), Box<dyn Error>>
{
    let database = TestDatabase::start_v6("sr_v7_snapshot").await?;
    let result = async {
        let mut snapshot = database
            .pool
            .begin_with("BEGIN TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .await?;
        // Pin the repeatable-read snapshot before the online upgrade commits,
        // matching a startup assertion that races the migration.
        sqlx::query("SELECT current_setting('server_version_num')::integer")
            .execute(&mut *snapshot)
            .await?;
        database.upgrade_v6_to_v7().await?;
        assert_schema_conforms(
            &mut snapshot,
            6,
            super::super::V6_CURRENT_SUBSCRIPTION_COLUMNS,
            &[V6_CATALOG_FINGERPRINT],
        )
        .await
        .map_err(|error| io::Error::other(format!("{error:?}")))?;
        snapshot.commit().await?;
        crate::assert_runtime_schema_v7_compatible(&database.pool).await?;
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}
