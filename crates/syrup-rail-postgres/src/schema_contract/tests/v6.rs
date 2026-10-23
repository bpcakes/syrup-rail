use std::{error::Error, io};

use super::v5::populated_v4;
use super::*;

const V6_ATTEMPT_RESOLUTION_CODES: [&str; 2] = [
    "subscription_period_expired_before_charge",
    "subscription_approved_period_expired",
];
const V6_ATTESTATION_PRIOR_CODE: &str = "subscription_approved_period_expired";

/// Serializes every row of every canonical table.
async fn canonical_rows(pool: &PgPool) -> Result<Vec<String>, sqlx::Error> {
    let mut snapshots = Vec::with_capacity(REQUIRED_TABLES.len());
    for table in REQUIRED_TABLES {
        let query = format!(
            r#"
            SELECT '{table}:' || COALESCE(
                jsonb_agg(to_jsonb(rows) ORDER BY to_jsonb(rows)::text),
                '[]'::jsonb
            )::text
            FROM public.{table} AS rows
            "#
        );
        snapshots.push(sqlx::query_scalar(&query).fetch_one(pool).await?);
    }
    Ok(snapshots)
}

/// Inserts one pending live host-charge attempt in the current attempt shape.
async fn insert_host_charge_attempt(
    pool: &PgPool,
    gateway: GatewayAccountFixture,
    label: &str,
) -> Result<(Uuid, String), sqlx::Error> {
    let attempt_id = Uuid::now_v7();
    let target_id = Uuid::now_v7();
    let order_id = format!("{label}-order-{}", opaque_fixture_uuid(attempt_id));
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
    .bind(gateway.billing_scope_id)
    .bind(Uuid::now_v7())
    .bind(target_id)
    .bind(format!("{label}-key-{}", opaque_fixture_uuid(attempt_id)))
    .bind(format!("host_charge:{target_id}:100:USD"))
    .bind(gateway.gateway_account_id)
    .bind(gateway.gateway_configuration_id)
    .bind(&order_id)
    .execute(pool)
    .await?;
    Ok((attempt_id, order_id))
}

/// Inserts a terminal host-charge attempt with an attested external reversal
/// whose prior resolution is `prior_resolution_code`.
async fn insert_attestation_with_prior_code(
    pool: &PgPool,
    gateway: GatewayAccountFixture,
    prior_resolution_code: &str,
) -> Result<sqlx::postgres::PgQueryResult, sqlx::Error> {
    let transaction_id = format!("txn_{}", opaque_fixture_uuid(Uuid::now_v7()));
    let (attempt_id, order_id) = insert_host_charge_attempt(pool, gateway, "v6-attest").await?;
    let charge_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO billing_processor_charges (
            id, attempt_id, billing_scope_id, gateway_account_id, gateway_order_id,
            gateway_transaction_id, attempt_kind, amount_cents, currency
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
    sqlx::query(
        r#"
        INSERT INTO billing_external_reversal_attestations (
            attempt_id, processor_charge_id, actor_id, reversal_kind, reason,
            prior_resolution_code, final_resolution_code, gateway_account_id,
            gateway_configuration_id, gateway_order_id, amount_cents, currency,
            gateway_transaction_id, attested_at
        ) VALUES (
            $1, $2, $3, 'refund', 'operator confirmed full refund', $4,
            'processor_charge_externally_refunded', $5, $6, $7, 100, 'USD', $8,
            clock_timestamp()
        )
        "#,
    )
    .bind(attempt_id)
    .bind(charge_id)
    .bind(Uuid::now_v7())
    .bind(prior_resolution_code)
    .bind(gateway.gateway_account_id)
    .bind(gateway.gateway_configuration_id)
    .bind(order_id)
    .bind(transaction_id)
    .execute(pool)
    .await
}

fn is_check_violation<T: std::fmt::Debug>(
    outcome: &Result<T, sqlx::Error>,
    constraint: &str,
) -> bool {
    matches!(
        outcome,
        Err(sqlx::Error::Database(error))
            if error.code().as_deref() == Some("23514")
                && error.constraint() == Some(constraint)
    )
}

#[tokio::test]
async fn runtime_schema_v6_accepts_fresh_install_and_populated_v5_upgrade()
-> Result<(), Box<dyn Error>> {
    if V6_INSTALL_SQL.trim().is_empty() || V5_TO_V6_UPGRADE_SQL.trim().is_empty() {
        return Err(io::Error::other("schema-v6 artifacts must not be empty").into());
    }
    let fresh = TestDatabase::start("sr_fresh_v6").await?;
    let upgraded = TestDatabase::start_v3("sr_upgrade_v6").await?;
    let result = async {
        populated_v4(&upgraded).await?;
        upgraded.upgrade_v4_to_v5().await?;
        assert_v5_conforms(&upgraded.pool).await?;
        let rows_before = canonical_rows(&upgraded.pool).await?;
        let attempt_rows: i64 = sqlx::query_scalar("SELECT count(*) FROM billing_payment_attempts")
            .fetch_one(&upgraded.pool)
            .await?;
        assert!(attempt_rows >= 2, "fixture must be populated");

        upgraded.upgrade_v5_to_v6().await?;
        assert_v6_conforms(&fresh.pool).await?;
        assert_v6_conforms(&upgraded.pool).await?;
        let fresh_fingerprint = canonical_catalog_fingerprint(&fresh.pool).await?;
        let upgraded_fingerprint = canonical_catalog_fingerprint(&upgraded.pool).await?;
        assert_eq!(fresh_fingerprint, upgraded_fingerprint);
        assert_eq!(fresh_fingerprint, V6_CATALOG_FINGERPRINT);
        assert_ne!(V6_CATALOG_FINGERPRINT, V5_CATALOG_FINGERPRINT);
        for pool in [&fresh.pool, &upgraded.pool] {
            let definition: String = sqlx::query_scalar(
                r#"
                SELECT pg_get_constraintdef(oid)
                FROM pg_constraint
                WHERE conname = 'billing_payment_attempts_resolution_code_check'
                "#,
            )
            .fetch_one(pool)
            .await?;
            for code in syrup_rail::PaymentResolutionCode::ALL {
                assert!(
                    definition.contains(code.as_str()),
                    "schema-v6 resolution constraint is missing {}",
                    code.as_str()
                );
            }
        }

        // Version 6 changes only constraints; every historical row is
        // byte-identical after the upgrade.
        assert_eq!(canonical_rows(&upgraded.pool).await?, rows_before);
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
async fn schema_v6_and_v5_assertions_reject_each_other_and_constraint_drift()
-> Result<(), Box<dyn Error>> {
    let v5 = TestDatabase::start_v5("sr_v6_reject_v5").await?;
    let v6 = TestDatabase::start("sr_v6_reject_v6").await?;
    let narrowed = TestDatabase::start("sr_v6_narrowed").await?;
    let unvalidated = TestDatabase::start("sr_v6_not_valid").await?;
    let result = async {
        assert!(matches!(
            crate::assert_runtime_schema_v6_compatible(&v5.pool).await,
            Err(crate::SchemaConformanceError::Contract { version: 6, .. })
        ));
        assert!(matches!(
            crate::assert_runtime_schema_v5_compatible(&v6.pool).await,
            Err(crate::SchemaConformanceError::Contract { version: 5, .. })
        ));
        crate::assert_runtime_schema_v5_compatible(&v5.pool).await?;
        crate::assert_runtime_schema_v6_compatible(&v6.pool).await?;

        // Restoring the narrower version-5 attempt list is drift.
        let v5_definition: String = sqlx::query_scalar(
            r#"
            SELECT pg_get_constraintdef(oid)
            FROM pg_constraint
            WHERE conname = 'billing_payment_attempts_resolution_code_check'
            "#,
        )
        .fetch_one(&v5.pool)
        .await?;
        sqlx::raw_sql(&format!(
            "ALTER TABLE billing_payment_attempts \
             DROP CONSTRAINT billing_payment_attempts_resolution_code_check; \
             ALTER TABLE billing_payment_attempts \
             ADD CONSTRAINT billing_payment_attempts_resolution_code_check {v5_definition}"
        ))
        .execute(&narrowed.pool)
        .await?;
        assert!(matches!(
            crate::assert_runtime_schema_v6_compatible(&narrowed.pool).await,
            Err(crate::SchemaConformanceError::Contract { version: 6, .. })
        ));

        let definition: String = sqlx::query_scalar(
            r#"
            SELECT pg_get_constraintdef(oid)
            FROM pg_constraint
            WHERE conname = 'billing_external_reversal_attestations_resolution_check'
            "#,
        )
        .fetch_one(&unvalidated.pool)
        .await?;
        sqlx::raw_sql(&format!(
            "ALTER TABLE billing_external_reversal_attestations \
             DROP CONSTRAINT billing_external_reversal_attestations_resolution_check; \
             ALTER TABLE billing_external_reversal_attestations \
             ADD CONSTRAINT billing_external_reversal_attestations_resolution_check \
             {definition} NOT VALID"
        ))
        .execute(&unvalidated.pool)
        .await?;
        let error = crate::assert_runtime_schema_v6_compatible(&unvalidated.pool)
            .await
            .expect_err("a NOT VALID attestation constraint must be rejected");
        assert!(
            matches!(
                &error,
                crate::SchemaConformanceError::Contract { version: 6, detail }
                    if detail.contains("billing_external_reversal_attestations_resolution_check")
            ),
            "{error}"
        );
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanups = [
        v5.cleanup().await,
        v6.cleanup().await,
        narrowed.cleanup().await,
        unvalidated.cleanup().await,
    ];
    result?;
    for cleanup in cleanups {
        cleanup?;
    }
    Ok(())
}

#[tokio::test]
async fn v6_constraints_accept_expiry_dispositions_that_v5_rejects() -> Result<(), Box<dyn Error>> {
    let v5 = TestDatabase::start_v5("sr_v6_codes_v5").await?;
    let v6 = TestDatabase::start("sr_v6_codes_v6").await?;
    let result = async {
        for (database, accepted) in [(&v5, false), (&v6, true)] {
            let account = create_gateway_account(&database.pool, "nmi").await?;
            for code in V6_ATTEMPT_RESOLUTION_CODES {
                let (attempt_id, _) =
                    insert_host_charge_attempt(&database.pool, account, "v6-code").await?;
                let update = sqlx::query(
                    "UPDATE billing_payment_attempts SET resolution_code = $2 WHERE id = $1",
                )
                .bind(attempt_id)
                .bind(code)
                .execute(&database.pool)
                .await;
                if accepted {
                    assert_eq!(update?.rows_affected(), 1, "v6 must accept {code}");
                } else {
                    assert!(
                        is_check_violation(
                            &update,
                            "billing_payment_attempts_resolution_code_check"
                        ),
                        "v5 must reject {code}: {update:?}"
                    );
                }
            }
            let attestation = insert_attestation_with_prior_code(
                &database.pool,
                account,
                V6_ATTESTATION_PRIOR_CODE,
            )
            .await;
            if accepted {
                assert_eq!(attestation?.rows_affected(), 1);
            } else {
                assert!(
                    is_check_violation(
                        &attestation,
                        "billing_external_reversal_attestations_resolution_check"
                    ),
                    "v5 must reject the expiry prior code: {attestation:?}"
                );
            }
            // Unrelated values remain closed in both versions.
            let unknown = insert_attestation_with_prior_code(
                &database.pool,
                account,
                "subscription_approved_renewal_stale_state",
            )
            .await;
            assert!(is_check_violation(
                &unknown,
                "billing_external_reversal_attestations_resolution_check"
            ));
        }
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let v5_cleanup = v5.cleanup().await;
    let v6_cleanup = v6.cleanup().await;
    result?;
    v5_cleanup?;
    v6_cleanup
}

#[tokio::test]
async fn v5_to_v6_upgrade_rolls_back_on_lock_timeout_and_refuses_a_rerun_or_v4()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start_v5("sr_v6_rollback").await?;
    let v4 = TestDatabase::start_v4("sr_v6_needs_v5").await?;
    let result = async {
        let mut blocker = database.pool.begin().await?;
        sqlx::query("LOCK TABLE billing_payment_attempts IN ACCESS SHARE MODE")
            .execute(&mut *blocker)
            .await?;
        let error = database
            .upgrade_v5_to_v6()
            .await
            .expect_err("a held table lock must stop the upgrade at its lock timeout");
        assert!(
            error.to_string().contains("lock timeout"),
            "unexpected upgrade failure: {error}"
        );
        blocker.rollback().await?;
        assert_v5_conforms(&database.pool).await?;

        database.upgrade_v5_to_v6().await?;
        assert_v6_conforms(&database.pool).await?;
        let rerun = database
            .upgrade_v5_to_v6()
            .await
            .expect_err("the upgrade must refuse to run twice");
        assert!(
            rerun
                .to_string()
                .contains("schema-v6 upgrade is already applied"),
            "unexpected rerun failure: {rerun}"
        );
        assert_v6_conforms(&database.pool).await?;

        let wrong_version = v4
            .upgrade_v5_to_v6()
            .await
            .expect_err("the upgrade must refuse a schema-v4 database");
        assert!(
            wrong_version
                .to_string()
                .contains("schema-v6 upgrade requires schema v5"),
            "unexpected v4 failure: {wrong_version}"
        );
        assert_v4_conforms(&v4.pool).await?;
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    let v4_cleanup = v4.cleanup().await;
    result?;
    cleanup?;
    v4_cleanup
}
