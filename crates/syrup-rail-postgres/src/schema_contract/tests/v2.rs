use super::fixtures::*;
use super::storage_fixtures::*;
use super::*;

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
