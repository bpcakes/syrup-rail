use std::{error::Error, io};

use super::fixtures::{create_v2_subscription_fixture, insert_v2_initial_attempt};
use super::storage_fixtures::insert_charge_gateway_text;
use super::*;

const ADDRESS_COLUMNS: [&str; 6] = [
    "billing_address_line1",
    "billing_address_line2",
    "billing_address_city",
    "billing_address_region",
    "billing_address_postal_code",
    "billing_address_country",
];

struct PopulatedV4 {
    account: GatewayAccountFixture,
    payment_method_id: Uuid,
    initial_attempt_id: Uuid,
}

/// Builds representative schema-v4 history through the shipped v3 fixtures and
/// the shipped v3-to-v4 cutover: a stored method with contact and card
/// display, a subscription, an initial attempt with contact, and an approved
/// host charge with immutable processor-charge evidence.
async fn populated_v4(database: &TestDatabase) -> Result<PopulatedV4, Box<dyn Error>> {
    let account = create_gateway_account(&database.pool, "nmi").await?;
    let subscriber_id = Uuid::now_v7();
    let (payment_method_id, _, _) =
        create_v2_subscription_fixture(&database.pool, account, subscriber_id).await?;
    sqlx::query(
        r#"
        UPDATE billing_payment_methods
        SET billing_name = 'Jordan Lee', billing_email = 'jordan@example.test',
            payment_type = 'card', card_brand = 'visa', card_last4 = '4242',
            card_exp_month = 12, card_exp_year = 2034
        WHERE id = $1
        "#,
    )
    .bind(payment_method_id)
    .execute(&database.pool)
    .await?;
    let initial_attempt_id =
        insert_v2_initial_attempt(&database.pool, account, subscriber_id, "populated").await?;
    sqlx::query(
        r#"
        UPDATE billing_payment_attempts
        SET billing_first_name = 'Jordan', billing_last_name = 'Lee',
            billing_email = 'jordan@example.test'
        WHERE id = $1
        "#,
    )
    .bind(initial_attempt_id)
    .execute(&database.pool)
    .await?;
    insert_charge_gateway_text(
        &database.pool,
        account,
        subscriber_id,
        "gateway_response_text",
        "Approved",
    )
    .await?;
    database.upgrade_v3_to_v4().await?;
    assert_v4_conforms(&database.pool).await?;
    Ok(PopulatedV4 {
        account,
        payment_method_id,
        initial_attempt_id,
    })
}

/// Serializes every canonical table without the schema-v5 address columns.
async fn canonical_rows_without_addresses(pool: &PgPool) -> Result<Vec<String>, sqlx::Error> {
    let address_columns = ADDRESS_COLUMNS
        .iter()
        .map(|column| (*column).to_owned())
        .collect::<Vec<_>>();
    let mut snapshots = Vec::with_capacity(REQUIRED_TABLES.len());
    for table in REQUIRED_TABLES {
        let query = format!(
            r#"
            SELECT '{table}:' || COALESCE(
                jsonb_agg(to_jsonb(rows) - $1::text[] ORDER BY to_jsonb(rows)::text),
                '[]'::jsonb
            )::text
            FROM public.{table} AS rows
            "#
        );
        snapshots.push(
            sqlx::query_scalar(&query)
                .bind(&address_columns)
                .fetch_one(pool)
                .await?,
        );
    }
    Ok(snapshots)
}

#[tokio::test]
async fn runtime_schema_v5_accepts_fresh_install_and_populated_v4_upgrade()
-> Result<(), Box<dyn Error>> {
    if V5_INSTALL_SQL.trim().is_empty() || V4_TO_V5_UPGRADE_SQL.trim().is_empty() {
        return Err(io::Error::other("schema-v5 artifacts must not be empty").into());
    }
    let fresh = TestDatabase::start("sr_fresh_v5").await?;
    let upgraded = TestDatabase::start_v3("sr_upgrade_v5").await?;
    let result = async {
        let populated = populated_v4(&upgraded).await?;
        let rows_before = canonical_rows_without_addresses(&upgraded.pool).await?;
        let method_rows: i64 = sqlx::query_scalar("SELECT count(*) FROM billing_payment_methods")
            .fetch_one(&upgraded.pool)
            .await?;
        let attempt_rows: i64 = sqlx::query_scalar("SELECT count(*) FROM billing_payment_attempts")
            .fetch_one(&upgraded.pool)
            .await?;
        assert!(
            method_rows >= 1 && attempt_rows >= 2,
            "fixture must be populated"
        );

        upgraded.upgrade_v4_to_v5().await?;
        assert_v5_conforms(&fresh.pool).await?;
        assert_v5_conforms(&upgraded.pool).await?;
        let fresh_fingerprint = canonical_catalog_fingerprint(&fresh.pool).await?;
        let upgraded_fingerprint = canonical_catalog_fingerprint(&upgraded.pool).await?;
        assert_eq!(fresh_fingerprint, upgraded_fingerprint);
        assert_eq!(fresh_fingerprint, V5_CATALOG_FINGERPRINT);

        // Financial identities, statuses, fingerprints, contacts and evidence
        // are byte-identical; only the appended address columns are new.
        assert_eq!(
            canonical_rows_without_addresses(&upgraded.pool).await?,
            rows_before
        );
        for table in ["billing_payment_methods", "billing_payment_attempts"] {
            let query = format!(
                "SELECT count(*) FROM public.{table} WHERE num_nonnulls({}) <> 0",
                ADDRESS_COLUMNS.join(", ")
            );
            let addressed: i64 = sqlx::query_scalar(&query).fetch_one(&upgraded.pool).await?;
            assert_eq!(addressed, 0, "{table} must not infer historical addresses");
        }

        // Historical addressless attempts remain readable by the v5 runtime.
        let mut transaction = upgraded.pool.begin().await?;
        let attempt = crate::find_payment_attempt_by_id_in_transaction(
            &mut transaction,
            syrup_rail::BillingScopeId::new(populated.account.billing_scope_id),
            syrup_rail::PaymentAttemptId::new(populated.initial_attempt_id),
        )
        .await?
        .ok_or_else(|| io::Error::other("historical attempt must load"))?;
        transaction.rollback().await?;
        assert_eq!(attempt.request().billing_contact().address(), None);
        assert_eq!(
            attempt.request().billing_contact().first_name(),
            Some("Jordan")
        );
        let method_address: Option<String> = sqlx::query_scalar(
            "SELECT billing_address_line1 FROM billing_payment_methods WHERE id = $1",
        )
        .bind(populated.payment_method_id)
        .fetch_one(&upgraded.pool)
        .await?;
        assert_eq!(method_address, None);
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
async fn schema_v5_and_v4_assertions_reject_each_other_and_address_drift()
-> Result<(), Box<dyn Error>> {
    let v4 = TestDatabase::start_v4("sr_v5_reject_v4").await?;
    let v5 = TestDatabase::start("sr_v5_reject_v5").await?;
    let dropped = TestDatabase::start("sr_v5_drop_check").await?;
    let unvalidated = TestDatabase::start("sr_v5_not_valid").await?;
    let result = async {
        assert!(matches!(
            crate::assert_runtime_schema_v5_compatible(&v4.pool).await,
            Err(crate::SchemaConformanceError::Contract { version: 5, .. })
        ));
        assert!(matches!(
            crate::assert_runtime_schema_v4_compatible(&v5.pool).await,
            Err(crate::SchemaConformanceError::Contract { version: 4, .. })
        ));
        crate::assert_runtime_schema_v4_compatible(&v4.pool).await?;
        crate::assert_runtime_schema_v5_compatible(&v5.pool).await?;

        sqlx::raw_sql(
            "ALTER TABLE billing_payment_methods DROP CONSTRAINT billing_payment_methods_billing_address_valid",
        )
        .execute(&dropped.pool)
        .await?;
        assert!(matches!(
            crate::assert_runtime_schema_v5_compatible(&dropped.pool).await,
            Err(crate::SchemaConformanceError::Contract { version: 5, .. })
        ));

        let definition: String = sqlx::query_scalar(
            r#"
            SELECT pg_get_constraintdef(oid)
            FROM pg_constraint
            WHERE conname = 'billing_payment_attempts_billing_address_valid'
            "#,
        )
        .fetch_one(&unvalidated.pool)
        .await?;
        sqlx::raw_sql(&format!(
            "ALTER TABLE billing_payment_attempts \
             DROP CONSTRAINT billing_payment_attempts_billing_address_valid; \
             ALTER TABLE billing_payment_attempts \
             ADD CONSTRAINT billing_payment_attempts_billing_address_valid {definition} NOT VALID"
        ))
        .execute(&unvalidated.pool)
        .await?;
        let error = crate::assert_runtime_schema_v5_compatible(&unvalidated.pool)
            .await
            .expect_err("a NOT VALID address constraint must be rejected");
        assert!(
            matches!(
                &error,
                crate::SchemaConformanceError::Contract { version: 5, detail }
                    if detail.contains("billing_payment_attempts_billing_address_valid")
            ),
            "{error}"
        );
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanups = [
        v4.cleanup().await,
        v5.cleanup().await,
        dropped.cleanup().await,
        unvalidated.cleanup().await,
    ];
    result?;
    for cleanup in cleanups {
        cleanup?;
    }
    Ok(())
}

#[tokio::test]
async fn v5_address_constraints_accept_only_whole_bounded_addresses() -> Result<(), Box<dyn Error>>
{
    let database = TestDatabase::start("sr_v5_addr_chk").await?;
    let result = async {
        let account = create_gateway_account(&database.pool, "nmi").await?;
        let subscriber_id = Uuid::now_v7();
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
        .bind(account.billing_scope_id)
        .bind(subscriber_id)
        .bind(account.gateway_account_id)
        .bind(format!("vault_{}", opaque_fixture_uuid(payment_method_id)))
        .execute(&database.pool)
        .await?;
        let attempt_id = Uuid::now_v7();
        let target_id = Uuid::now_v7();
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
        .bind(attempt_id)
        .bind(account.billing_scope_id)
        .bind(subscriber_id)
        .bind(target_id)
        .bind(format!("address-{}", opaque_fixture_uuid(attempt_id)))
        .bind(format!("host_charge:{target_id}:100:USD"))
        .bind(account.gateway_account_id)
        .bind(account.gateway_configuration_id)
        .bind(format!("address-order-{}", opaque_fixture_uuid(attempt_id)))
        .execute(&database.pool)
        .await?;
        let long = "a".repeat(256);
        let at_limit = "é".repeat(127);
        let cases: [(&str, [Option<&str>; 6], bool); 12] = [
            ("absent", [None, None, None, None, None, None], true),
            (
                "complete",
                [
                    Some("1 Main St"),
                    Some("Suite 2"),
                    Some("Boston"),
                    Some("MA"),
                    Some("02110"),
                    Some("US"),
                ],
                true,
            ),
            (
                "line and country only",
                [Some("1 Main St"), None, None, None, None, Some("GB")],
                true,
            ),
            (
                "255-octet multibyte line",
                [Some(&at_limit), None, None, None, None, Some("US")],
                true,
            ),
            (
                "missing line 1",
                [None, None, Some("Boston"), None, None, Some("US")],
                false,
            ),
            (
                "missing country",
                [Some("1 Main St"), None, None, None, None, None],
                false,
            ),
            (
                "lowercase country",
                [Some("1 Main St"), None, None, None, None, Some("us")],
                false,
            ),
            (
                "three-letter country",
                [Some("1 Main St"), None, None, None, None, Some("USA")],
                false,
            ),
            (
                "blank line 1",
                [Some("   "), None, None, None, None, Some("US")],
                false,
            ),
            (
                "blank optional field",
                [Some("1 Main St"), None, Some(" "), None, None, Some("US")],
                false,
            ),
            (
                "256-octet field",
                [Some("1 Main St"), Some(&long), None, None, None, Some("US")],
                false,
            ),
            (
                "orphan optional field",
                [None, None, None, None, Some("02110"), None],
                false,
            ),
        ];
        for (table, row_id) in [
            ("billing_payment_methods", payment_method_id),
            ("billing_payment_attempts", attempt_id),
        ] {
            let constraint = format!("{table}_billing_address_valid");
            let statement = format!(
                r#"
                UPDATE public.{table}
                SET billing_address_line1 = $2, billing_address_line2 = $3,
                    billing_address_city = $4, billing_address_region = $5,
                    billing_address_postal_code = $6, billing_address_country = $7
                WHERE id = $1
                "#
            );
            for (case, values, accepted) in &cases {
                let update = sqlx::query(&statement)
                    .bind(row_id)
                    .bind(values[0])
                    .bind(values[1])
                    .bind(values[2])
                    .bind(values[3])
                    .bind(values[4])
                    .bind(values[5])
                    .execute(&database.pool)
                    .await;
                match (accepted, update) {
                    (true, Ok(result)) if result.rows_affected() == 1 => {}
                    (false, Err(sqlx::Error::Database(error)))
                        if error.code().as_deref() == Some("23514")
                            && error.constraint() == Some(constraint.as_str()) => {}
                    (_, outcome) => {
                        return Err(io::Error::other(format!(
                            "{table} {case}: unexpected constraint outcome {outcome:?}"
                        ))
                        .into());
                    }
                }
            }
        }
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn v4_to_v5_upgrade_rolls_back_on_lock_timeout_and_refuses_a_rerun()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start_v4("sr_v5_rollback").await?;
    let result = async {
        let mut blocker = database.pool.begin().await?;
        sqlx::query("LOCK TABLE billing_payment_attempts IN ACCESS SHARE MODE")
            .execute(&mut *blocker)
            .await?;
        let error = database
            .upgrade_v4_to_v5()
            .await
            .expect_err("a held table lock must stop the upgrade at its lock timeout");
        assert!(
            error.to_string().contains("lock timeout"),
            "unexpected upgrade failure: {error}"
        );
        blocker.rollback().await?;
        assert_v4_conforms(&database.pool).await?;

        database.upgrade_v4_to_v5().await?;
        assert_v5_conforms(&database.pool).await?;
        let rerun = database
            .upgrade_v4_to_v5()
            .await
            .expect_err("the upgrade must refuse to run twice");
        assert!(
            rerun
                .to_string()
                .contains("schema-v5 upgrade is already applied"),
            "unexpected rerun failure: {rerun}"
        );
        assert_v5_conforms(&database.pool).await?;
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}
