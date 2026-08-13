use super::fixtures::*;
use super::storage_fixtures::*;
use super::*;
use std::time::Duration;

#[derive(Clone, Copy)]
struct ConcurrentReindexCase {
    target: &'static str,
    table: &'static str,
    expected_new_shadow: &'static str,
    expected_old_shadow: &'static str,
}

const NUMBERED_REINDEX_CASE: ConcurrentReindexCase = ConcurrentReindexCase {
    target: "billing_payment_attempts_gateway_order_idx",
    table: "billing_payment_attempts",
    expected_new_shadow: "billing_payment_attempts_gateway_order_idx_ccnew1",
    expected_old_shadow: "billing_payment_attempts_gateway_order_idx_ccold1",
};
const TRUNCATED_REINDEX_CASE: ConcurrentReindexCase = ConcurrentReindexCase {
    target: "billing_payment_attempts_subscription_billing_inflight_idx",
    table: "billing_payment_attempts",
    expected_new_shadow: "billing_payment_attempts_subscription_billing_inflight_id_ccnew",
    expected_old_shadow: "billing_payment_attempts_subscription_billing_inflight_id_ccold",
};
const GATEWAY_ORDER_NEW_SHADOW: &str = "billing_payment_attempts_gateway_order_idx_ccnew";

#[test]
fn runtime_schema_contract_accepts_only_postgresql_18() {
    for server_version_num in [180000, 180999] {
        assert!(require_supported_postgres_version_num(server_version_num).is_ok());
    }
    for server_version_num in [170006, 190000] {
        assert!(matches!(
            require_supported_postgres_version_num(server_version_num),
            Err(crate::SchemaConformanceError::UnsupportedPostgresVersion {
                required_major: 18,
                actual_server_version_num,
            }) if actual_server_version_num == server_version_num
        ));
    }
}

#[tokio::test]
async fn schema_conformance_retries_only_reindex_transitions_once() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let transition_attempts = AtomicUsize::new(0);
    let result = retry_reindex_transition_once(|| {
        let attempt = transition_attempts.fetch_add(1, Ordering::SeqCst);
        async move {
            if attempt == 0 {
                Err(SchemaConformanceAttemptError::RetryableReindexTransition {
                    fallback: crate::SchemaConformanceError::Contract {
                        version: 2,
                        detail: REINDEX_TRANSITION_DETAIL.to_owned(),
                    },
                })
            } else {
                Ok(())
            }
        }
    })
    .await;
    assert!(result.is_ok());
    assert_eq!(transition_attempts.load(Ordering::SeqCst), 2);

    let ordinary_attempts = AtomicUsize::new(0);
    let result = retry_reindex_transition_once(|| {
        ordinary_attempts.fetch_add(1, Ordering::SeqCst);
        async {
            Err(SchemaConformanceAttemptError::Final(
                crate::SchemaConformanceError::Contract {
                    version: 2,
                    detail: "ordinary schema drift".to_owned(),
                },
            ))
        }
    })
    .await;
    assert!(matches!(
        result,
        Err(crate::SchemaConformanceError::Contract { detail, .. })
            if detail == "ordinary schema drift"
    ));
    assert_eq!(ordinary_attempts.load(Ordering::SeqCst), 1);

    let stale_attempts = AtomicUsize::new(0);
    let result = retry_reindex_transition_once(|| {
        stale_attempts.fetch_add(1, Ordering::SeqCst);
        async {
            Err(SchemaConformanceAttemptError::RetryableReindexTransition {
                fallback: crate::SchemaConformanceError::Contract {
                    version: 2,
                    detail: "actionable stale-shadow diagnostic".to_owned(),
                },
            })
        }
    })
    .await;
    assert!(matches!(
        result,
        Err(crate::SchemaConformanceError::Contract { detail, .. })
            if detail == "actionable stale-shadow diagnostic"
    ));
    assert_eq!(stale_attempts.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn runtime_schema_v2_compatibility_accepts_a_fresh_v2_install() -> Result<(), Box<dyn Error>>
{
    if V2_INSTALL_SQL.trim().is_empty() {
        return Err(io::Error::other("version-2 install artifact is empty").into());
    }
    let database = TestDatabase::start("sr_schema_v2").await?;
    let result = crate::assert_runtime_schema_v2_compatible(&database.pool).await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn runtime_schema_v2_compatibility_accepts_host_prefixed_extensions()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("sr_catalog_v2").await?;
    let result = async {
        sqlx::raw_sql(
            r#"
            CREATE TABLE example_host_billing_scopes (
                id uuid PRIMARY KEY
            );

            ALTER TABLE billing_gateway_accounts
                ADD CONSTRAINT example_host_gateway_accounts_scope_fk
                FOREIGN KEY (billing_scope_id)
                REFERENCES example_host_billing_scopes(id)
                ON DELETE RESTRICT;

            CREATE INDEX example_host_gateway_accounts_scope_idx
            ON billing_gateway_accounts (billing_scope_id, id);

            CREATE FUNCTION example_host_gateway_account_noop()
            RETURNS trigger
            LANGUAGE plpgsql
            SET search_path = pg_catalog, public
            AS $$
            BEGIN
                RETURN NEW;
            END
            $$;

            CREATE TRIGGER example_host_gateway_account_noop
            BEFORE UPDATE ON billing_gateway_accounts
            FOR EACH ROW
            EXECUTE FUNCTION example_host_gateway_account_noop();
            "#,
        )
        .execute(&database.pool)
        .await?;
        crate::assert_runtime_schema_v2_compatible(&database.pool).await?;
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn runtime_schema_v2_compatibility_accepts_only_active_reindex_shadow_indexes()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("sr_v2_reindex").await?;
    let observer_role = format!("syrup_schema_observer_{}", Uuid::now_v7().simple());
    let result = async {
        sqlx::query(&format!("CREATE ROLE {observer_role} NOLOGIN"))
            .execute(&database.pool)
            .await?;
        sqlx::query(&format!("GRANT USAGE ON SCHEMA public TO {observer_role}"))
            .execute(&database.pool)
            .await?;
        sqlx::query(&format!(
            "GRANT SELECT ON ALL TABLES IN SCHEMA public TO {observer_role}"
        ))
        .execute(&database.pool)
        .await?;
        sqlx::raw_sql(
            r#"
            CREATE SEQUENCE billing_payment_attempts_gateway_order_idx_ccnew;
            CREATE SEQUENCE billing_payment_attempts_gateway_order_idx_ccold;
            "#,
        )
        .execute(&database.pool)
        .await?;
        exercise_active_concurrent_reindex(&database.pool, NUMBERED_REINDEX_CASE, &observer_role)
            .await?;
        sqlx::raw_sql(
            r#"
            DROP SEQUENCE billing_payment_attempts_gateway_order_idx_ccnew;
            DROP SEQUENCE billing_payment_attempts_gateway_order_idx_ccold;
            "#,
        )
        .execute(&database.pool)
        .await?;

        exercise_active_concurrent_reindex(&database.pool, TRUNCATED_REINDEX_CASE, &observer_role)
            .await
    }
    .await;
    let role_cleanup = drop_schema_observer_role(&database.pool, &observer_role).await;
    let cleanup = database.cleanup().await;
    result?;
    role_cleanup?;
    cleanup
}

#[tokio::test]
async fn runtime_schema_v2_compatibility_accepts_active_table_reindex_shadows()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("sr_v2_tbl_live").await?;
    let result = exercise_active_table_reindex(&database.pool).await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

async fn exercise_active_table_reindex(pool: &PgPool) -> Result<(), Box<dyn Error>> {
    let mut before_swap_reader = Some(pool.acquire().await?);
    let reader = before_swap_reader
        .as_mut()
        .ok_or_else(|| io::Error::other("table-reindex reader was not retained"))?;
    sqlx::query("BEGIN TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut **reader)
        .await?;
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM billing_payment_attempts")
        .fetch_one(&mut **reader)
        .await?;

    let reindex_pool = pool.clone();
    let mut reindex_task = tokio::spawn(async move {
        sqlx::query("REINDEX TABLE CONCURRENTLY billing_payment_attempts")
            .execute(&reindex_pool)
            .await
    });

    let assertion_result = async {
        wait_for_reindex_phase(
            pool,
            "billing_payment_attempts",
            "waiting for old snapshots",
        )
        .await?;
        let shadow_count = sqlx::query_scalar::<_, i64>(
            r#"
            SELECT count(*)
            FROM pg_catalog.pg_index AS catalog_index
            INNER JOIN pg_catalog.pg_class AS index_relation
                ON index_relation.oid = catalog_index.indexrelid
            INNER JOIN pg_catalog.pg_class AS table_relation
                ON table_relation.oid = catalog_index.indrelid
            WHERE table_relation.relname = 'billing_payment_attempts'
                AND index_relation.relname ~ '_ccnew[0-9]*$'
            "#,
        )
        .fetch_one(pool)
        .await?;
        if shadow_count < 2 {
            return Err(io::Error::other(format!(
                "table reindex exposed only {shadow_count} concurrent shadows"
            ))
            .into());
        }
        crate::assert_runtime_schema_v2_compatible(pool).await?;

        let reader = before_swap_reader
            .as_mut()
            .ok_or_else(|| io::Error::other("table-reindex reader was not retained"))?;
        sqlx::query("ROLLBACK").execute(&mut **reader).await?;
        before_swap_reader = None;
        Ok::<_, Box<dyn Error>>(())
    }
    .await;

    if let Some(mut reader) = before_swap_reader.take() {
        let _ = sqlx::query("ROLLBACK").execute(&mut *reader).await;
    }

    let reindex_result: Result<(), Box<dyn Error>> =
        match tokio::time::timeout(Duration::from_secs(30), &mut reindex_task).await {
            Ok(joined) => {
                joined.map_err(|error| io::Error::other(error.to_string()))??;
                Ok(())
            }
            Err(_) => {
                reindex_task.abort();
                let _ = reindex_task.await;
                Err(io::Error::other("table reindex did not finish after reader left").into())
            }
        };

    assertion_result?;
    reindex_result?;
    crate::assert_runtime_schema_v2_compatible(pool).await?;
    Ok(())
}

#[tokio::test]
async fn runtime_schema_v2_recheck_detects_a_completed_reindex_transition()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("sr_v2_transition").await?;
    let result = exercise_completed_reindex_transition(&database.pool).await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn runtime_schema_v2_retries_completion_before_the_first_catalog_load()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("sr_v2_early_tx").await?;
    let result = exercise_completion_before_the_first_catalog_load(&database.pool).await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

async fn exercise_completion_before_the_first_catalog_load(
    pool: &PgPool,
) -> Result<(), Box<dyn Error>> {
    let mut old_snapshot = Some(pool.acquire().await?);
    let reader = old_snapshot
        .as_mut()
        .ok_or_else(|| io::Error::other("early-transition reader was not retained"))?;
    sqlx::query("BEGIN TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut **reader)
        .await?;
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM billing_payment_attempts")
        .fetch_one(&mut **reader)
        .await?;

    let reindex_pool = pool.clone();
    let mut reindex_task = tokio::spawn(async move {
        sqlx::query("REINDEX INDEX CONCURRENTLY billing_payment_attempts_gateway_order_idx")
            .execute(&reindex_pool)
            .await
    });

    let snapshot_result = async {
        wait_for_reindex_phase(
            pool,
            "billing_payment_attempts",
            "waiting for old snapshots",
        )
        .await?;
        let mut validator = pool
            .begin_with("BEGIN TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .await?;
        let snapshot_has_shadow = sqlx::query_scalar::<_, bool>(
            r#"
            SELECT EXISTS (
                SELECT 1
                FROM pg_catalog.pg_class
                WHERE relname = $1
            )
            "#,
        )
        .bind(GATEWAY_ORDER_NEW_SHADOW)
        .fetch_one(&mut *validator)
        .await?;
        if !snapshot_has_shadow {
            return Err(io::Error::other(
                "validator snapshot did not retain the pre-completion shadow",
            )
            .into());
        }

        let reader = old_snapshot
            .as_mut()
            .ok_or_else(|| io::Error::other("early-transition reader was not retained"))?;
        sqlx::query("ROLLBACK").execute(&mut **reader).await?;
        old_snapshot = None;
        Ok::<_, Box<dyn Error>>(validator)
    }
    .await;

    if let Some(mut reader) = old_snapshot.take() {
        let _ = sqlx::query("ROLLBACK").execute(&mut *reader).await;
    }

    let reindex_result: Result<(), Box<dyn Error>> =
        match tokio::time::timeout(Duration::from_secs(10), &mut reindex_task).await {
            Ok(joined) => {
                joined.map_err(|error| io::Error::other(error.to_string()))??;
                Ok(())
            }
            Err(_) => {
                reindex_task.abort();
                let _ = reindex_task.await;
                Err(
                    io::Error::other("early reindex transition did not finish after reader left")
                        .into(),
                )
            }
        };

    let mut validator = snapshot_result?;
    reindex_result?;
    let transition = assert_schema_conforms(
        &mut validator,
        2,
        V2_CURRENT_SUBSCRIPTION_COLUMNS,
        V2_CATALOG_FINGERPRINT,
    )
    .await;
    validator.rollback().await?;
    match transition {
        Err(SchemaConformanceAttemptError::RetryableReindexTransition {
            fallback: crate::SchemaConformanceError::Contract { detail, .. },
        }) if detail.contains("not planner/write ready")
            && detail.contains(GATEWAY_ORDER_NEW_SHADOW) => {}
        Err(error) => {
            return Err(io::Error::other(format!(
                "expected an early reindex-transition candidate, got {error:?}"
            ))
            .into());
        }
        Ok(()) => {
            return Err(io::Error::other(
                "schema validation accepted a snapshot-only reindex shadow without a retry",
            )
            .into());
        }
    }
    crate::assert_runtime_schema_v2_compatible(pool).await?;
    Ok(())
}

async fn exercise_completed_reindex_transition(pool: &PgPool) -> Result<(), Box<dyn Error>> {
    let mut before_swap_reader = Some(pool.acquire().await?);
    let reader = before_swap_reader
        .as_mut()
        .ok_or_else(|| io::Error::other("transition reader was not retained"))?;
    sqlx::query("BEGIN TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut **reader)
        .await?;
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM billing_payment_attempts")
        .fetch_one(&mut **reader)
        .await?;

    let reindex_pool = pool.clone();
    let mut reindex_task = tokio::spawn(async move {
        sqlx::query("REINDEX INDEX CONCURRENTLY billing_payment_attempts_gateway_order_idx")
            .execute(&reindex_pool)
            .await
    });

    let snapshot_result = async {
        wait_for_reindex_phase(
            pool,
            "billing_payment_attempts",
            "waiting for old snapshots",
        )
        .await?;
        require_only_shadow_index(pool, "billing_payment_attempts", GATEWAY_ORDER_NEW_SHADOW)
            .await?;

        let mut transaction = pool
            .begin_with("BEGIN TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .await?;
        let billing_indexes = load_billing_index_catalog(&mut transaction).await?;
        if !active_reindex_shadows(&billing_indexes)
            .iter()
            .any(|(_, index_name, _)| *index_name == GATEWAY_ORDER_NEW_SHADOW)
        {
            return Err(io::Error::other(
                "validator snapshot did not classify the live reindex shadow",
            )
            .into());
        }

        let reader = before_swap_reader
            .as_mut()
            .ok_or_else(|| io::Error::other("transition reader was not retained"))?;
        sqlx::query("ROLLBACK").execute(&mut **reader).await?;
        before_swap_reader = None;
        Ok::<_, Box<dyn Error>>((transaction, billing_indexes))
    }
    .await;

    if let Some(mut reader) = before_swap_reader.take() {
        let _ = sqlx::query("ROLLBACK").execute(&mut *reader).await;
    }

    let reindex_result: Result<(), Box<dyn Error>> =
        match tokio::time::timeout(Duration::from_secs(10), &mut reindex_task).await {
            Ok(joined) => {
                joined.map_err(|error| io::Error::other(error.to_string()))??;
                Ok(())
            }
            Err(_) => {
                reindex_task.abort();
                let _ = reindex_task.await;
                Err(io::Error::other("reindex transition did not finish after reader left").into())
            }
        };

    let (mut transaction, billing_indexes) = snapshot_result?;
    reindex_result?;
    let transition =
        require_unchanged_active_reindex_shadows(&mut transaction, 2, &billing_indexes).await;
    transaction.rollback().await?;
    match transition {
        Err(SchemaConformanceAttemptError::RetryableReindexTransition {
            fallback: crate::SchemaConformanceError::Contract { detail, .. },
        }) if detail == REINDEX_TRANSITION_DETAIL => {}
        Err(error) => {
            return Err(io::Error::other(format!(
                "expected a completed-reindex transition diagnostic, got {error:?}"
            ))
            .into());
        }
        Ok(()) => {
            return Err(io::Error::other(
                "schema recheck accepted live evidence after its reindex completed",
            )
            .into());
        }
    }
    crate::assert_runtime_schema_v2_compatible(pool).await?;
    Ok(())
}

async fn drop_schema_observer_role(pool: &PgPool, role: &str) -> Result<(), sqlx::Error> {
    sqlx::query(&format!("DROP OWNED BY {role}"))
        .execute(pool)
        .await?;
    sqlx::query(&format!("DROP ROLE {role}"))
        .execute(pool)
        .await?;
    Ok(())
}

async fn exercise_active_concurrent_reindex(
    pool: &PgPool,
    case: ConcurrentReindexCase,
    observer_role: &str,
) -> Result<(), Box<dyn Error>> {
    let mut before_swap_reader = Some(pool.acquire().await?);
    let reader = before_swap_reader
        .as_mut()
        .ok_or_else(|| io::Error::other("pre-swap reader was not retained"))?;
    sqlx::query("BEGIN TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut **reader)
        .await?;
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM billing_payment_attempts")
        .fetch_one(&mut **reader)
        .await?;

    let reindex_pool = pool.clone();
    let mut reindex_task = tokio::spawn(async move {
        sqlx::query(&format!("REINDEX INDEX CONCURRENTLY {}", case.target))
            .execute(&reindex_pool)
            .await
    });
    let mut after_swap_reader = None;

    let exercise_result = async {
        wait_for_reindex_phase(pool, case.table, "waiting for old snapshots").await?;
        require_only_shadow_index(pool, case.table, case.expected_new_shadow).await?;
        crate::assert_runtime_schema_v2_compatible(pool).await?;
        require_hidden_reindex_to_fail_closed(pool, observer_role, case.expected_new_shadow)
            .await?;

        let mut reader = pool.acquire().await?;
        sqlx::query("BEGIN TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *reader)
            .await?;
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM billing_payment_attempts")
            .fetch_one(&mut *reader)
            .await?;
        after_swap_reader = Some(reader);

        let reader = before_swap_reader
            .as_mut()
            .ok_or_else(|| io::Error::other("pre-swap reader was not retained"))?;
        sqlx::query("ROLLBACK").execute(&mut **reader).await?;
        before_swap_reader = None;

        wait_for_reindex_phase(pool, case.table, "waiting for readers before marking dead").await?;
        require_only_shadow_index(pool, case.table, case.expected_old_shadow).await?;
        crate::assert_runtime_schema_v2_compatible(pool).await?;

        let reader = after_swap_reader
            .as_mut()
            .ok_or_else(|| io::Error::other("post-swap reader was not retained"))?;
        sqlx::query("ROLLBACK").execute(&mut **reader).await?;
        after_swap_reader = None;
        Ok::<_, Box<dyn Error>>(())
    }
    .await;

    if let Some(reader) = before_swap_reader.as_mut() {
        let _ = sqlx::query("ROLLBACK").execute(&mut **reader).await;
    }
    if let Some(reader) = after_swap_reader.as_mut() {
        let _ = sqlx::query("ROLLBACK").execute(&mut **reader).await;
    }

    let reindex_result: Result<(), Box<dyn Error>> =
        match tokio::time::timeout(Duration::from_secs(10), &mut reindex_task).await {
            Ok(joined) => {
                joined.map_err(|error| io::Error::other(error.to_string()))??;
                Ok(())
            }
            Err(_) => {
                reindex_task.abort();
                let _ = reindex_task.await;
                Err(io::Error::other("concurrent reindex did not finish after readers left").into())
            }
        };

    exercise_result?;
    reindex_result?;
    crate::assert_runtime_schema_v2_compatible(pool).await?;
    Ok(())
}

async fn require_hidden_reindex_to_fail_closed(
    pool: &PgPool,
    observer_role: &str,
    expected_shadow: &str,
) -> Result<(), Box<dyn Error>> {
    let mut transaction = pool
        .begin_with("BEGIN TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .await?;
    sqlx::query(&format!("SET LOCAL ROLE {observer_role}"))
        .execute(&mut *transaction)
        .await?;
    let result = assert_schema_conforms(
        &mut transaction,
        2,
        V2_CURRENT_SUBSCRIPTION_COLUMNS,
        V2_CATALOG_FINGERPRINT,
    )
    .await;
    transaction.rollback().await?;

    match result {
        Err(SchemaConformanceAttemptError::RetryableReindexTransition {
            fallback: crate::SchemaConformanceError::Contract { detail, .. },
        }) if detail.contains("not planner/write ready") && detail.contains(expected_shadow) => {
            Ok(())
        }
        Err(error) => Err(io::Error::other(format!(
            "expected hidden reindex progress to fail closed, got {error:?}"
        ))
        .into()),
        Ok(()) => Err(io::Error::other(
            "schema validation accepted a reindex whose progress details were hidden",
        )
        .into()),
    }
}

async fn wait_for_reindex_phase(
    pool: &PgPool,
    table: &str,
    expected_phase: &str,
) -> Result<(), Box<dyn Error>> {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let observed = sqlx::query_scalar::<_, bool>(
                r#"
                SELECT EXISTS (
                    SELECT 1
                    FROM pg_catalog.pg_stat_progress_create_index AS progress
                    INNER JOIN pg_catalog.pg_class AS table_relation
                        ON table_relation.oid = progress.relid
                    WHERE progress.datid = (
                            SELECT database.oid
                            FROM pg_catalog.pg_database AS database
                            WHERE database.datname = current_database()
                        )
                        AND progress.command = 'REINDEX CONCURRENTLY'
                        AND progress.phase = $1
                        AND table_relation.relname = $2
                )
                "#,
            )
            .bind(expected_phase)
            .bind(table)
            .fetch_one(pool)
            .await?;
            if observed {
                return Ok::<_, sqlx::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .map_err(|_| {
        io::Error::other(format!(
            "concurrent reindex never reached {expected_phase:?}"
        ))
    })??;
    Ok(())
}

async fn require_only_shadow_index(
    pool: &PgPool,
    table: &str,
    expected: &str,
) -> Result<(), Box<dyn Error>> {
    let shadows = sqlx::query_scalar::<_, String>(
        r#"
        SELECT index_relation.relname
        FROM pg_catalog.pg_index AS catalog_index
        INNER JOIN pg_catalog.pg_class AS index_relation
            ON index_relation.oid = catalog_index.indexrelid
        INNER JOIN pg_catalog.pg_class AS table_relation
            ON table_relation.oid = catalog_index.indrelid
        WHERE table_relation.relname = $1
            AND index_relation.relname ~ '_cc(new|old)[0-9]*$'
        ORDER BY index_relation.relname
        "#,
    )
    .bind(table)
    .fetch_all(pool)
    .await?;
    if shadows == [expected] {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "expected only concurrent-reindex shadow {expected:?}, found {shadows:?}"
        ))
        .into())
    }
}

const STALE_REINDEX_SHADOW: &str = "billing_gateway_accounts_pkey_ccnew1";

async fn create_failed_concurrent_index_with_reindex_suffix(
    pool: &PgPool,
) -> Result<(), Box<dyn Error>> {
    create_gateway_account(pool, "reindex_provider").await?;
    create_gateway_account(pool, "reindex_provider").await?;

    let failed_build = sqlx::query(
        r#"
        CREATE UNIQUE INDEX CONCURRENTLY billing_gateway_accounts_pkey_ccnew1
        ON billing_gateway_accounts (provider_key)
        "#,
    )
    .execute(pool)
    .await;
    if failed_build.is_ok() {
        Err(io::Error::other(
            "duplicate provider keys unexpectedly satisfied the unique shadow index",
        )
        .into())
    } else {
        Ok(())
    }
}

#[tokio::test]
async fn runtime_schema_v2_compatibility_rejects_failed_concurrent_index_with_reindex_suffix()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("sr_v2_stale").await?;
    let result = async {
        create_failed_concurrent_index_with_reindex_suffix(&database.pool).await?;

        match crate::assert_runtime_schema_v2_compatible(&database.pool).await {
            Err(crate::SchemaConformanceError::Contract { detail, .. })
                if detail.contains("not planner/write ready")
                    && detail.contains(STALE_REINDEX_SHADOW) =>
            {
                Ok::<_, Box<dyn Error>>(())
            }
            Err(error) => Err(io::Error::other(format!(
                "expected stale concurrent-index readiness diagnostic, got {error}"
            ))
            .into()),
            Ok(()) => Err(io::Error::other(
                "runtime schema-v2 compatibility accepted a failed concurrent index",
            )
            .into()),
        }
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn runtime_schema_v2_compatibility_rejects_stale_shadow_locked_during_table_reindex_gather()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("sr_v2_tbl_stale").await?;
    let result = exercise_stale_shadow_during_table_reindex_gather(&database.pool).await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

async fn exercise_stale_shadow_during_table_reindex_gather(
    pool: &PgPool,
) -> Result<(), Box<dyn Error>> {
    create_failed_concurrent_index_with_reindex_suffix(pool).await?;
    sqlx::query(
        "CREATE INDEX host_gateway_accounts_reindex_blocker_idx ON billing_gateway_accounts (id)",
    )
    .execute(pool)
    .await?;
    // PostgreSQL 18's RelationGetIndexList returns OID order. Creating the
    // blocker after the stale shadow makes gather lock the stale relation
    // before it waits on the blocker's AccessExclusiveLock.
    let stale_precedes_blocker = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT
            'billing_gateway_accounts_pkey_ccnew1'::regclass::oid
            < 'host_gateway_accounts_reindex_blocker_idx'::regclass::oid
        "#,
    )
    .fetch_one(pool)
    .await?;
    if !stale_precedes_blocker {
        return Err(io::Error::other(
            "stale-shadow fixture must precede its gather blocker by OID",
        )
        .into());
    }

    let mut blocker = pool.acquire().await?;
    sqlx::query("BEGIN").execute(&mut *blocker).await?;
    sqlx::query("ALTER INDEX host_gateway_accounts_reindex_blocker_idx SET (fillfactor = 90)")
        .execute(&mut *blocker)
        .await?;

    let reindex_pool = pool.clone();
    let mut reindex_task = tokio::spawn(async move {
        sqlx::query("REINDEX TABLE CONCURRENTLY billing_gateway_accounts")
            .execute(&reindex_pool)
            .await
    });

    let assertion_result = async {
        wait_for_reindex_gather_to_lock_stale_shadow(pool).await?;
        match crate::assert_runtime_schema_v2_compatible(pool).await {
            Err(crate::SchemaConformanceError::Contract { detail, .. })
                if detail.contains("not planner/write ready")
                    && detail.contains(STALE_REINDEX_SHADOW) =>
            {
                Ok::<_, Box<dyn Error>>(())
            }
            Err(error) => Err(io::Error::other(format!(
                "expected locked stale-shadow readiness diagnostic, got {error}"
            ))
            .into()),
            Ok(()) => Err(io::Error::other(
                "runtime schema-v2 compatibility accepted a stale shadow during table reindex gathering",
            )
            .into()),
        }
    }
    .await;

    let _ = sqlx::query("ROLLBACK").execute(&mut *blocker).await;
    drop(blocker);
    let reindex_result: Result<(), Box<dyn Error>> =
        match tokio::time::timeout(Duration::from_secs(20), &mut reindex_task).await {
            Ok(joined) => {
                joined.map_err(|error| io::Error::other(error.to_string()))??;
                Ok(())
            }
            Err(_) => {
                reindex_task.abort();
                let _ = reindex_task.await;
                Err(io::Error::other("table reindex did not finish after blocker left").into())
            }
        };

    assertion_result?;
    reindex_result
}

async fn wait_for_reindex_gather_to_lock_stale_shadow(pool: &PgPool) -> Result<(), Box<dyn Error>> {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let observed = sqlx::query_scalar::<_, bool>(
                r#"
                SELECT EXISTS (
                    SELECT 1
                    FROM pg_catalog.pg_class AS stale_index
                    INNER JOIN pg_catalog.pg_index AS stale_catalog_index
                        ON stale_catalog_index.indexrelid = stale_index.oid
                    INNER JOIN pg_catalog.pg_class AS table_relation
                        ON table_relation.oid = stale_catalog_index.indrelid
                    INNER JOIN pg_catalog.pg_locks AS stale_lock
                        ON stale_lock.locktype = 'relation'
                        AND stale_lock.database = (
                            SELECT database.oid
                            FROM pg_catalog.pg_database AS database
                            WHERE database.datname = current_database()
                        )
                        AND stale_lock.relation = stale_index.oid
                        AND stale_lock.mode = 'ShareUpdateExclusiveLock'
                        AND stale_lock.granted
                    INNER JOIN pg_catalog.pg_locks AS table_lock
                        ON table_lock.pid = stale_lock.pid
                        AND table_lock.locktype = 'relation'
                        AND table_lock.database = stale_lock.database
                        AND table_lock.relation = table_relation.oid
                        AND table_lock.mode = 'ShareUpdateExclusiveLock'
                        AND table_lock.granted
                    INNER JOIN pg_catalog.pg_class AS canonical_index
                        ON canonical_index.relname = 'billing_gateway_accounts_pkey'
                    INNER JOIN pg_catalog.pg_index AS canonical_catalog_index
                        ON canonical_catalog_index.indexrelid = canonical_index.oid
                        AND canonical_catalog_index.indrelid = table_relation.oid
                        AND canonical_catalog_index.indisvalid
                        AND canonical_catalog_index.indisready
                        AND canonical_catalog_index.indislive
                    INNER JOIN pg_catalog.pg_locks AS canonical_lock
                        ON canonical_lock.pid = stale_lock.pid
                        AND canonical_lock.locktype = 'relation'
                        AND canonical_lock.database = stale_lock.database
                        AND canonical_lock.relation = canonical_index.oid
                        AND canonical_lock.mode = 'ShareUpdateExclusiveLock'
                        AND canonical_lock.granted
                    WHERE stale_index.relname = $1
                        AND table_relation.relname = 'billing_gateway_accounts'
                        AND pg_catalog.starts_with(
                            canonical_index.relname,
                            pg_catalog.regexp_replace(
                                stale_index.relname,
                                '_cc(new|old)[0-9]*$',
                                ''
                            )
                        )
                        AND NOT EXISTS (
                            SELECT 1
                            FROM pg_catalog.pg_stat_progress_create_index AS progress
                            WHERE progress.pid = stale_lock.pid
                                AND progress.datid = stale_lock.database
                                AND progress.relid = table_relation.oid
                                AND progress.command = 'REINDEX CONCURRENTLY'
                                AND progress.phase <> 'initializing'
                        )
                )
                "#,
            )
            .bind(STALE_REINDEX_SHADOW)
            .fetch_one(pool)
            .await?;
            if observed {
                return Ok::<_, sqlx::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .map_err(|_| io::Error::other("table reindex never locked the stale shadow during gather"))??;
    Ok(())
}

#[tokio::test]
async fn runtime_schema_v2_compatibility_rejects_valid_index_with_reindex_suffix()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("sr_v2_named").await?;
    let result = async {
        sqlx::query(
            r#"
            CREATE UNIQUE INDEX billing_gateway_accounts_runtime_ccold2
            ON billing_gateway_accounts (provider_key)
            "#,
        )
        .execute(&database.pool)
        .await?;

        match crate::assert_runtime_schema_v2_compatible(&database.pool).await {
            Err(crate::SchemaConformanceError::Contract { detail, .. })
                if detail.contains("canonical catalog fingerprint differs") =>
            {
                Ok::<_, Box<dyn Error>>(())
            }
            Err(error) => Err(io::Error::other(format!(
                "expected suffix-bearing index drift diagnostic, got {error}"
            ))
            .into()),
            Ok(()) => Err(io::Error::other(
                "runtime schema-v2 compatibility accepted a valid suffix-bearing index",
            )
            .into()),
        }
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn runtime_schema_v2_compatibility_rejects_added_columns_on_canonical_tables()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("sr_v2_host_col").await?;
    let result = async {
        sqlx::query("ALTER TABLE billing_gateway_accounts ADD COLUMN example_host_note text")
            .execute(&database.pool)
            .await?;

        match crate::assert_runtime_schema_v2_compatible(&database.pool).await {
            Err(crate::SchemaConformanceError::Contract { version, detail })
                if version == 2 && detail.contains("canonical catalog fingerprint differs") =>
            {
                Ok::<_, Box<dyn Error>>(())
            }
            Err(error) => Err(io::Error::other(format!(
                "expected an added canonical-column diagnostic, got {error}"
            ))
            .into()),
            Ok(()) => Err(io::Error::other(
                "runtime schema-v2 compatibility accepted a host column on a canonical table",
            )
            .into()),
        }
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn runtime_schema_v2_compatibility_accepts_a_checked_in_v1_upgrade()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start_v1_then_upgrade("sr_rt_up_v2").await?;
    let result = crate::assert_runtime_schema_v2_compatible(&database.pool).await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn schema_v2_keyset_indexes_match_their_reader_identity_and_order()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("sr_v2_keysets").await?;
    let result = async {
        let mut connection = database.pool.acquire().await?;
        require_index_contract(&mut connection, 2, RENEWAL_DISPATCH_INDEX_CONTRACT).await?;
        require_index_contract(&mut connection, 2, SUBSCRIPTION_HISTORY_INDEX_CONTRACT).await?;
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[test]
fn index_contract_validation_rejects_every_planner_relevant_shape_drift() {
    let contract = SUBSCRIPTION_HISTORY_INDEX_CONTRACT;
    let canonical = catalog_shape_for_contract(contract);
    validate_index_contract(2, contract, Some(&canonical))
        .expect("the canonical fixture must satisfy its contract");

    let mut cases = Vec::new();
    let mut shape = canonical.clone();
    shape.table_name = "billing_subscriptions".to_owned();
    cases.push(("belongs to table", shape));
    let mut shape = canonical.clone();
    shape.access_method = "hash".to_owned();
    cases.push(("uses access method", shape));
    let mut shape = canonical.clone();
    shape.is_unique = true;
    cases.push(("has unique=", shape));
    let mut shape = canonical.clone();
    shape.key_expressions[3] = "resolved_at".to_owned();
    cases.push(("has key expressions", shape));
    let mut shape = canonical.clone();
    shape.key_orderings[3] = "ASC NULLS LAST".to_owned();
    cases.push(("has key ordering", shape));
    let mut shape = canonical.clone();
    shape.key_opclasses[2] = "text_pattern_ops".to_owned();
    cases.push(("has key operator classes", shape));
    let mut shape = canonical.clone();
    shape
        .included_expressions
        .push("resolution_code".to_owned());
    cases.push(("has included expressions", shape));
    let mut shape = canonical.clone();
    shape.predicate = None;
    cases.push(("has predicate", shape));
    let mut shape = canonical.clone();
    shape.is_valid = false;
    cases.push(("planner/write ready", shape));
    let mut shape = canonical.clone();
    shape.is_ready = false;
    cases.push(("planner/write ready", shape));
    let mut shape = canonical;
    shape.is_live = false;
    cases.push(("planner/write ready", shape));

    for (expected_detail, shape) in cases {
        let error = validate_index_contract(2, contract, Some(&shape))
            .expect_err("index drift must fail its complete contract");
        let crate::SchemaConformanceError::Contract { version, detail } = error else {
            panic!("expected a contract error for index drift");
        };
        assert_eq!(version, 2);
        assert!(
            detail.contains(expected_detail),
            "expected {expected_detail:?} in diagnostic {detail:?}"
        );
    }

    let missing = validate_index_contract(2, contract, None)
        .expect_err("a missing index must fail its contract");
    assert!(missing.to_string().contains("is missing"));
}

fn catalog_shape_for_contract(contract: IndexContract) -> CatalogIndexShape {
    CatalogIndexShape {
        table_name: contract.table.to_owned(),
        access_method: "btree".to_owned(),
        key_expressions: contract
            .keys
            .iter()
            .map(|key| key.expression.to_owned())
            .collect(),
        key_orderings: contract
            .keys
            .iter()
            .map(|key| key.ordering.catalog_label().to_owned())
            .collect(),
        key_opclasses: contract
            .keys
            .iter()
            .map(|key| key.opclass.to_owned())
            .collect(),
        included_expressions: contract
            .included_expressions
            .iter()
            .map(|expression| (*expression).to_owned())
            .collect(),
        predicate: contract.predicate.map(str::to_owned),
        is_unique: contract.unique,
        is_valid: true,
        is_ready: true,
        is_live: true,
    }
}

#[tokio::test]
async fn runtime_schema_v2_compatibility_rejects_wrong_keyset_index_shape()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("sr_v2_bad_idx").await?;
    let result = async {
        sqlx::raw_sql(
            r#"
            DROP INDEX billing_subscriptions_due_idx;

            CREATE INDEX billing_subscriptions_due_idx
            ON billing_subscriptions (
                next_payment_attempt_at,
                gateway_account_id,
                id
            )
            INCLUDE (billing_scope_id, next_renewal_at)
            WHERE status IN ('active', 'past_due')
                AND next_payment_attempt_at IS NOT NULL;
            "#,
        )
        .execute(&database.pool)
        .await?;

        match crate::assert_runtime_schema_v2_compatible(&database.pool).await {
            Err(crate::SchemaConformanceError::Contract { version, detail })
                if version == 2
                    && detail.contains(
                        "renewal-dispatch keyset index billing_subscriptions_due_idx has key expressions",
                    ) =>
            {
                Ok::<_, Box<dyn Error>>(())
            }
            Err(error) => Err(io::Error::other(format!(
                "expected a targeted renewal keyset-index diagnostic, got {error}"
            ))
            .into()),
            Ok(()) => Err(io::Error::other(
                "runtime schema-v2 compatibility accepted a wrong renewal keyset index",
            )
            .into()),
        }
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn runtime_schema_v2_compatibility_rejects_an_unchanged_v1_catalog()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start_v1("sr_rt_v1").await?;
    let result = match crate::assert_runtime_schema_v2_compatible(&database.pool).await {
        Err(crate::SchemaConformanceError::Contract { version, detail })
            if version == 2 && !detail.trim().is_empty() =>
        {
            Ok(())
        }
        Err(error) => Err(io::Error::other(format!(
            "expected an explicit schema-v2 catalog diagnostic, got {error}"
        ))),
        Ok(()) => Err(io::Error::other(
            "runtime schema-v2 compatibility accepted an unchanged v1 catalog",
        )),
    };
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn runtime_schema_v2_compatibility_rejects_canonical_drift() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("sr_rt_drift").await?;
    let result = async {
        sqlx::query(
            r#"
            CREATE INDEX billing_gateway_accounts_runtime_drift_idx
            ON billing_gateway_accounts (updated_at)
            "#,
        )
        .execute(&database.pool)
        .await?;

        match crate::assert_runtime_schema_v2_compatible(&database.pool).await {
            Err(crate::SchemaConformanceError::Contract { version, detail })
                if version == 2 && detail.contains("canonical catalog fingerprint differs") => {}
            Err(error) => {
                return Err(io::Error::other(format!(
                    "expected schema-v2 fingerprint drift diagnostic, got {error}"
                ))
                .into());
            }
            Ok(()) => {
                return Err(io::Error::other(
                    "runtime schema-v2 compatibility accepted canonical drift",
                )
                .into());
            }
        }

        sqlx::query("DROP INDEX billing_gateway_accounts_runtime_drift_idx")
            .execute(&database.pool)
            .await?;
        crate::assert_runtime_schema_v2_compatible(&database.pool).await?;
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn schema_v2_term_schedule_status_and_discount_shapes_are_constrained()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("sr_shapes_v2").await?;
    let result = async {
        let gateway = create_gateway_account(&database.pool, "test_gateway").await?;
        let subscriber_id = Uuid::now_v7();
        let (payment_method_id, subscription_id, initial_transaction_id) =
            create_v2_subscription_fixture(&database.pool, gateway, subscriber_id).await?;

        for assignment in [
            "recurring_period_count = 0",
            "recurring_period_count = 65536",
            "phase = 'paid_trial'",
            "trial_amount_cents = 10",
        ] {
            let rejected = sqlx::query(&format!(
                "UPDATE billing_subscriptions SET {assignment} WHERE id = $1"
            ))
            .bind(subscription_id)
            .execute(&database.pool)
            .await;
            expect_database_constraint(rejected, "billing_subscriptions_terms_check")?;
        }

        sqlx::query(
            r#"
            UPDATE billing_subscriptions
            SET dunning_retry_delays_seconds = ARRAY[]::bigint[]
            WHERE id = $1
            "#,
        )
        .bind(subscription_id)
        .execute(&database.pool)
        .await?;
        for schedule in [
            "ARRAY[1, NULL]::bigint[]",
            "ARRAY[[1, 2], [3, 4]]::bigint[]",
            "'[0:1]={1,2}'::bigint[]",
            "ARRAY[0]::bigint[]",
            "ARRAY[4294967296]::bigint[]",
            "ARRAY(SELECT generate_series(1, 17)::bigint)",
        ] {
            let rejected = sqlx::query(&format!(
                "UPDATE billing_subscriptions SET dunning_retry_delays_seconds = {schedule} WHERE id = $1"
            ))
            .bind(subscription_id)
            .execute(&database.pool)
            .await;
            expect_database_constraint(
                rejected,
                "billing_subscriptions_dunning_schedule_check",
            )?;
        }

        let missing_active_schedule = sqlx::query(
            "UPDATE billing_subscriptions SET next_payment_attempt_at = NULL WHERE id = $1",
        )
        .bind(subscription_id)
        .execute(&database.pool)
        .await;
        expect_database_constraint(
            missing_active_schedule,
            "billing_subscriptions_payment_schedule_check",
        )?;

        let unpaid_without_timestamp = sqlx::query(
            r#"
            UPDATE billing_subscriptions
            SET status = 'unpaid', next_payment_attempt_at = NULL
            WHERE id = $1
            "#,
        )
        .bind(subscription_id)
        .execute(&database.pool)
        .await;
        expect_database_constraint(
            unpaid_without_timestamp,
            "billing_subscriptions_unpaid_state_check",
        )?;

        let initial_attempt_id = insert_v2_initial_attempt(
            &database.pool,
            gateway,
            Uuid::now_v7(),
            "v2-initial-terms",
        )
        .await?;
        for assignment in [
            "subscription_initial_terms_version = 3",
            "subscription_initial_recurring_period_count = 0",
            "subscription_initial_start_kind = 'paid_trial'",
            "subscription_initial_dunning_retry_delays_seconds = '[0:1]={1,2}'::bigint[]",
        ] {
            let rejected = sqlx::query(&format!(
                "UPDATE billing_payment_attempts SET {assignment} WHERE id = $1"
            ))
            .bind(initial_attempt_id)
            .execute(&database.pool)
            .await;
            expect_database_constraint(
                rejected,
                "billing_payment_attempts_initial_terms_check",
            )?;
        }

        let renewal_attempt_id = insert_v1_terminal_subscription_attempt(
            &database.pool,
            gateway,
            subscriber_id,
            payment_method_id,
            subscription_id,
            &initial_transaction_id,
            "subscription_renewal",
            "declined",
            "v2-noninitial-terms",
            Some("2026-02-02 00:00:00+00"),
            "2026-02-02 00:01:00+00",
            None,
        )
        .await?;
        let noninitial_terms = sqlx::query(
            r#"
            UPDATE billing_payment_attempts
            SET subscription_initial_terms_version = 2
            WHERE id = $1
            "#,
        )
        .bind(renewal_attempt_id)
        .execute(&database.pool)
        .await;
        expect_database_constraint(
            noninitial_terms,
            "billing_payment_attempts_initial_terms_check",
        )?;

        insert_v2_indefinite_discount(&database.pool, gateway, subscriber_id, subscription_id, 0)
            .await?;
        sqlx::query("DELETE FROM billing_subscription_discounts WHERE subscription_id = $1")
            .bind(subscription_id)
            .execute(&database.pool)
            .await?;
        insert_v2_limited_discount(
            &database.pool,
            gateway,
            subscriber_id,
            subscription_id,
            0,
            "active",
            None,
        )
        .await?;
        sqlx::query("DELETE FROM billing_subscription_discounts WHERE subscription_id = $1")
            .bind(subscription_id)
            .execute(&database.pool)
            .await?;

        for (periods_applied, status, completed_at) in [
            (-1, "active", None),
            (3, "active", None),
            (0, "completed", Some("2026-02-01 00:00:00+00")),
        ] {
            let rejected = insert_v2_limited_discount(
                &database.pool,
                gateway,
                subscriber_id,
                subscription_id,
                periods_applied,
                status,
                completed_at,
            )
            .await;
            expect_database_constraint(
                rejected,
                "billing_subscription_discounts_duration_periods_check",
            )?;
        }

        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}
