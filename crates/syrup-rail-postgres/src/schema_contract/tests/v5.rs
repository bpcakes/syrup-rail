use super::*;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Column, Executor};

#[tokio::test]
async fn runtime_schema_v5_accepts_fresh_install_and_v4_upgrade() -> Result<(), Box<dyn Error>> {
    if V5_INSTALL_SQL.trim().is_empty()
        || V4_TO_V5_UPGRADE_SQL.trim().is_empty()
        || V4_TO_V5_INCOMPATIBLE_ATTESTATION_AUDIT_SQL
            .trim()
            .is_empty()
    {
        return Err(io::Error::other("schema-v5 artifacts must not be empty").into());
    }
    let fresh = TestDatabase::start_v5("sr_fresh_v5").await?;
    let upgraded = TestDatabase::start_v4("sr_upgrade_v5").await?;
    let result = async {
        upgraded.upgrade_v4_to_v5().await?;
        assert_v5_conforms(&fresh.pool).await?;
        assert_v5_conforms(&upgraded.pool).await?;

        let fresh_fingerprint = canonical_catalog_fingerprint(&fresh.pool).await?;
        let upgraded_fingerprint = canonical_catalog_fingerprint(&upgraded.pool).await?;
        assert_eq!(fresh_fingerprint, upgraded_fingerprint);
        assert_eq!(fresh_fingerprint, V5_CATALOG_FINGERPRINT);
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
async fn runtime_schema_v5_rejects_v4_and_canonical_column_drift() -> Result<(), Box<dyn Error>> {
    assert_ne!(V4_CATALOG_FINGERPRINT, V5_CATALOG_FINGERPRINT);
    let v4 = TestDatabase::start_v4("sr_v5_reject_v4").await?;
    let drifted = TestDatabase::start_v5("sr_v5_drift").await?;
    let result = async {
        assert!(matches!(
            crate::assert_runtime_schema_v5_compatible(&v4.pool).await,
            Err(crate::SchemaConformanceError::Contract { version: 5, .. })
        ));

        sqlx::query("ALTER TABLE billing_payment_attempts ADD COLUMN host_extra text")
            .execute(&drifted.pool)
            .await?;
        assert!(matches!(
            crate::assert_runtime_schema_v5_compatible(&drifted.pool).await,
            Err(crate::SchemaConformanceError::Contract { version: 5, .. })
        ));
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let v4_cleanup = v4.cleanup().await;
    let drifted_cleanup = drifted.cleanup().await;
    result?;
    v4_cleanup?;
    drifted_cleanup
}

#[tokio::test]
async fn v5_preflight_and_constraint_cover_complete_v4_tuple_matrix() -> Result<(), Box<dyn Error>>
{
    let v4 = TestDatabase::start_v4("sr_v5_matrix_v4").await?;
    let v5 = TestDatabase::start_v5("sr_v5_matrix_v5").await?;
    let result = async {
        let v4_account = create_gateway_account(&v4.pool, "nmi").await?;
        let v5_account = create_gateway_account(&v5.pool, "nmi").await?;
        let mut incompatible_v4_attempts = Vec::new();

        for (position, tuple) in EXTERNAL_REVERSAL_TUPLE_CASES.iter().enumerate() {
            let v4_attempt = insert_external_reversal_attestation(
                &v4.pool,
                v4_account,
                &format!("matrix-v4-{position}"),
                tuple.reversal_kind,
                tuple.prior_resolution_code,
                tuple.final_resolution_code,
            )
            .await?;
            if !tuple.allowed_in_v5 {
                incompatible_v4_attempts.push(v4_attempt);
            }

            let v5_insert = insert_external_reversal_attestation(
                &v5.pool,
                v5_account,
                &format!("matrix-v5-{position}"),
                tuple.reversal_kind,
                tuple.prior_resolution_code,
                tuple.final_resolution_code,
            )
            .await;
            if tuple.allowed_in_v5 {
                v5_insert?;
            } else {
                assert_resolution_constraint_error(v5_insert);
            }
        }

        let preflight = run_v4_to_v5_preflight_read_only(&v4.pool).await?;
        assert_eq!(preflight, (8, 2));
        let audit = run_v4_to_v5_incompatible_attestation_audit_read_only(&v4.pool).await?;
        let mut audited_attempts = audit.iter().map(|row| row.attempt_id).collect::<Vec<_>>();
        audited_attempts.sort_unstable();
        incompatible_v4_attempts.sort_unstable();
        assert_eq!(audited_attempts, incompatible_v4_attempts);
        assert!(
            audit
                .iter()
                .all(IncompatibleAttestationAuditRow::is_forbidden_v5_tuple)
        );

        for attempt_id in incompatible_v4_attempts {
            sqlx::query("DELETE FROM billing_external_reversal_attestations WHERE attempt_id = $1")
                .bind(attempt_id)
                .execute(&v4.pool)
                .await?;
        }
        v4.upgrade_v4_to_v5().await?;
        assert_v5_conforms(&v4.pool).await?;
        assert_v5_conforms(&v5.pool).await?;
        let retained = sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM billing_external_reversal_attestations",
        )
        .fetch_one(&v4.pool)
        .await?;
        assert_eq!(retained, 6);
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let v4_cleanup = v4.cleanup().await;
    let v5_cleanup = v5.cleanup().await;
    result?;
    v4_cleanup?;
    v5_cleanup
}

#[tokio::test]
async fn v4_to_v5_upgrade_rejects_incompatible_tuple_then_succeeds_after_remediation()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start_v4("sr_v5_tuple").await?;
    let result = async {
        let account = create_gateway_account(&database.pool, "nmi").await?;
        let compatible_attempt = insert_external_reversal_attestation(
            &database.pool,
            account,
            "compatible",
            "refund",
            "subscription_initial_current_grant_conflict",
            "subscription_initial_externally_refunded",
        )
        .await?;
        let incompatible_refund_attempt = insert_external_reversal_attestation(
            &database.pool,
            account,
            "incompatible-refund",
            "refund",
            "subscription_initial_current_grant_conflict",
            "processor_charge_externally_refunded",
        )
        .await?;
        let incompatible_void_attempt = insert_external_reversal_attestation(
            &database.pool,
            account,
            "incompatible-void",
            "void",
            "subscription_initial_current_grant_conflict",
            "processor_charge_externally_voided",
        )
        .await?;

        let preflight = run_v4_to_v5_preflight_read_only(&database.pool).await?;
        assert_eq!(preflight, (3, 2));
        let audit = run_v4_to_v5_incompatible_attestation_audit_read_only(&database.pool).await?;
        assert_eq!(audit.len(), 2);
        assert!(audit.iter().any(|row| {
            row.attempt_id == incompatible_refund_attempt
                && row.reversal_kind == "refund"
                && row.final_resolution_code == "processor_charge_externally_refunded"
        }));
        assert!(audit.iter().any(|row| {
            row.attempt_id == incompatible_void_attempt
                && row.reversal_kind == "void"
                && row.final_resolution_code == "processor_charge_externally_voided"
        }));

        let mut transaction = database.pool.begin().await?;
        let error = sqlx::raw_sql(V4_TO_V5_UPGRADE_SQL)
            .execute(&mut *transaction)
            .await
            .expect_err("incompatible retained evidence must abort the v5 cutover");
        assert!(error.as_database_error().is_some_and(|error| {
            error.constraint()
                == Some("billing_external_reversal_attestations_resolution_check")
        }));
        transaction.rollback().await?;

        let constraint_definition = sqlx::query_scalar::<_, String>(
            r#"
            SELECT pg_get_constraintdef(c.oid, true)
            FROM pg_constraint AS c
            WHERE c.conname =
                'billing_external_reversal_attestations_resolution_check'
            "#,
        )
        .fetch_one(&database.pool)
        .await?;
        assert!(constraint_definition.contains("prior_resolution_code = ANY"));

        // This is a disposable migration fixture standing in for the audited
        // host-owned remediation required for real financial evidence.
        sqlx::query(
            "DELETE FROM billing_external_reversal_attestations WHERE attempt_id = $1",
        )
        .bind(incompatible_refund_attempt)
        .execute(&database.pool)
        .await?;

        let preflight = run_v4_to_v5_preflight_read_only(&database.pool).await?;
        assert_eq!(preflight, (2, 1));
        let audit = run_v4_to_v5_incompatible_attestation_audit_read_only(&database.pool).await?;
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].attempt_id, incompatible_void_attempt);

        let mut transaction = database.pool.begin().await?;
        let error = sqlx::raw_sql(V4_TO_V5_UPGRADE_SQL)
            .execute(&mut *transaction)
            .await
            .expect_err("the independently forbidden void tuple must still abort the cutover");
        assert!(error.as_database_error().is_some_and(|error| {
            error.constraint()
                == Some("billing_external_reversal_attestations_resolution_check")
        }));
        transaction.rollback().await?;

        sqlx::query(
            "DELETE FROM billing_external_reversal_attestations WHERE attempt_id = $1",
        )
        .bind(incompatible_void_attempt)
        .execute(&database.pool)
        .await?;

        let preflight = run_v4_to_v5_preflight_read_only(&database.pool).await?;
        assert_eq!(preflight, (1, 0));
        assert!(
            run_v4_to_v5_incompatible_attestation_audit_read_only(&database.pool)
                .await?
                .is_empty()
        );

        database.upgrade_v4_to_v5().await?;
        assert_v5_conforms(&database.pool).await?;
        let compatible_row_survived = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM billing_external_reversal_attestations WHERE attempt_id = $1)",
        )
        .bind(compatible_attempt)
        .fetch_one(&database.pool)
        .await?;
        assert!(compatible_row_survived);
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

async fn run_v4_to_v5_preflight_read_only(pool: &PgPool) -> Result<(i64, i64), sqlx::Error> {
    let mut transaction = pool
        .begin_with("BEGIN TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .await?;
    let counts = sqlx::query_as::<_, (i64, i64)>(V4_TO_V5_PREFLIGHT_SQL)
        .fetch_one(&mut *transaction)
        .await?;
    transaction.rollback().await?;
    Ok(counts)
}

#[derive(Debug, sqlx::FromRow)]
struct IncompatibleAttestationAuditRow {
    attempt_id: Uuid,
    processor_charge_id: Uuid,
    reversal_kind: String,
    prior_resolution_code: String,
    final_resolution_code: String,
}

impl IncompatibleAttestationAuditRow {
    fn is_forbidden_v5_tuple(&self) -> bool {
        self.processor_charge_id != Uuid::nil()
            && self.prior_resolution_code == "subscription_initial_current_grant_conflict"
            && matches!(
                (
                    self.reversal_kind.as_str(),
                    self.final_resolution_code.as_str()
                ),
                ("refund", "processor_charge_externally_refunded")
                    | ("void", "processor_charge_externally_voided")
            )
    }
}

async fn run_v4_to_v5_incompatible_attestation_audit_read_only(
    pool: &PgPool,
) -> Result<Vec<IncompatibleAttestationAuditRow>, sqlx::Error> {
    let mut transaction = pool
        .begin_with("BEGIN TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .await?;
    let description = (&mut *transaction)
        .describe(V4_TO_V5_INCOMPATIBLE_ATTESTATION_AUDIT_SQL)
        .await?;
    let column_names = description
        .columns()
        .iter()
        .map(Column::name)
        .collect::<Vec<_>>();
    assert_eq!(
        column_names,
        [
            "attempt_id",
            "processor_charge_id",
            "reversal_kind",
            "prior_resolution_code",
            "final_resolution_code",
        ]
    );
    let rows = sqlx::query_as::<_, IncompatibleAttestationAuditRow>(
        V4_TO_V5_INCOMPATIBLE_ATTESTATION_AUDIT_SQL,
    )
    .fetch_all(&mut *transaction)
    .await?;
    transaction.rollback().await?;
    Ok(rows)
}

#[tokio::test]
async fn runtime_schema_v5_does_not_read_retained_attestations() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start_v5("sr_v5_no_scan").await?;
    let role = format!("schema_v5_validator_{}", Uuid::now_v7().simple());
    let result = async {
        sqlx::query(&format!("CREATE ROLE {role} NOLOGIN"))
            .execute(&database.pool)
            .await?;
        sqlx::query(&format!(
            "GRANT SELECT ON ALL TABLES IN SCHEMA public TO {role}"
        ))
        .execute(&database.pool)
        .await?;
        sqlx::query(&format!(
            "REVOKE SELECT ON billing_external_reversal_attestations FROM {role}"
        ))
        .execute(&database.pool)
        .await?;
        sqlx::query(&format!(
            "GRANT REFERENCES ON billing_external_reversal_attestations TO {role}"
        ))
        .execute(&database.pool)
        .await?;
        let can_read_attestations = sqlx::query_scalar::<_, bool>(
            "SELECT has_table_privilege($1, 'billing_external_reversal_attestations', 'SELECT')",
        )
        .bind(&role)
        .fetch_one(&database.pool)
        .await?;
        assert!(!can_read_attestations);
        let set_role = format!("SET ROLE {role}");
        let validator_pool = PgPoolOptions::new()
            .max_connections(1)
            .after_connect(move |connection, _| {
                let set_role = set_role.clone();
                Box::pin(async move {
                    sqlx::query(&set_role).execute(connection).await?;
                    Ok(())
                })
            })
            .connect_with(database.database_url().parse::<PgConnectOptions>()?)
            .await?;

        let conformance = crate::assert_runtime_schema_v5_compatible(&validator_pool).await;
        validator_pool.close().await;
        conformance?;
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let role_cleanup = drop_schema_validator_role(&database.pool, &role).await;
    let cleanup = database.cleanup().await;
    result?;
    role_cleanup?;
    cleanup
}

#[derive(Clone, Copy)]
struct ExternalReversalTupleCase {
    reversal_kind: &'static str,
    prior_resolution_code: &'static str,
    final_resolution_code: &'static str,
    allowed_in_v5: bool,
}

const EXTERNAL_REVERSAL_TUPLE_CASES: [ExternalReversalTupleCase; 8] = [
    ExternalReversalTupleCase {
        reversal_kind: "refund",
        prior_resolution_code: "subscription_initial_current_grant_conflict",
        final_resolution_code: "subscription_initial_externally_refunded",
        allowed_in_v5: true,
    },
    ExternalReversalTupleCase {
        reversal_kind: "refund",
        prior_resolution_code: "subscription_initial_current_grant_conflict",
        final_resolution_code: "processor_charge_externally_refunded",
        allowed_in_v5: false,
    },
    ExternalReversalTupleCase {
        reversal_kind: "void",
        prior_resolution_code: "subscription_initial_current_grant_conflict",
        final_resolution_code: "subscription_initial_externally_voided",
        allowed_in_v5: true,
    },
    ExternalReversalTupleCase {
        reversal_kind: "void",
        prior_resolution_code: "subscription_initial_current_grant_conflict",
        final_resolution_code: "processor_charge_externally_voided",
        allowed_in_v5: false,
    },
    ExternalReversalTupleCase {
        reversal_kind: "refund",
        prior_resolution_code: "processor_charge_external_reversal_required",
        final_resolution_code: "subscription_initial_externally_refunded",
        allowed_in_v5: true,
    },
    ExternalReversalTupleCase {
        reversal_kind: "refund",
        prior_resolution_code: "processor_charge_external_reversal_required",
        final_resolution_code: "processor_charge_externally_refunded",
        allowed_in_v5: true,
    },
    ExternalReversalTupleCase {
        reversal_kind: "void",
        prior_resolution_code: "processor_charge_external_reversal_required",
        final_resolution_code: "subscription_initial_externally_voided",
        allowed_in_v5: true,
    },
    ExternalReversalTupleCase {
        reversal_kind: "void",
        prior_resolution_code: "processor_charge_external_reversal_required",
        final_resolution_code: "processor_charge_externally_voided",
        allowed_in_v5: true,
    },
];

fn assert_resolution_constraint_error(result: Result<Uuid, sqlx::Error>) {
    let error = result.expect_err("incompatible tuple must violate the v5 constraint");
    assert!(error.as_database_error().is_some_and(|error| {
        error.constraint() == Some("billing_external_reversal_attestations_resolution_check")
    }));
}

async fn drop_schema_validator_role(pool: &PgPool, role: &str) -> Result<(), sqlx::Error> {
    sqlx::query(&format!("DROP OWNED BY {role}"))
        .execute(pool)
        .await?;
    sqlx::query(&format!("DROP ROLE {role}"))
        .execute(pool)
        .await?;
    Ok(())
}

async fn insert_external_reversal_attestation(
    pool: &PgPool,
    account: GatewayAccountFixture,
    label: &str,
    reversal_kind: &str,
    prior_resolution_code: &str,
    final_resolution_code: &str,
) -> Result<Uuid, sqlx::Error> {
    let attempt_id = Uuid::now_v7();
    let processor_charge_id = Uuid::now_v7();
    let gateway_order_id = format!("v5-{label}-{}", attempt_id.simple());
    let gateway_transaction_id = format!("v5-{label}-txn-{}", attempt_id.simple());
    sqlx::query(
        r#"
        INSERT INTO billing_payment_attempts (
            id, billing_scope_id, subscriber_id, host_charge_target_id,
            attempt_kind, status, idempotency_key, request_fingerprint,
            amount_cents, currency, gateway_account_id,
            gateway_configuration_id, gateway_order_id,
            required_gateway_account_mode, review_required_at
        ) VALUES (
            $1, $2, $3, $4, 'host_charge', 'review_required', $5, $6,
            500, 'USD', $7, $8, $9, 'live', clock_timestamp()
        )
        "#,
    )
    .bind(attempt_id)
    .bind(account.billing_scope_id)
    .bind(Uuid::now_v7())
    .bind(Uuid::now_v7())
    .bind(format!("v5-{label}-idem-{attempt_id}"))
    .bind(format!("v5-{label}-fingerprint-{attempt_id}"))
    .bind(account.gateway_account_id)
    .bind(account.gateway_configuration_id)
    .bind(&gateway_order_id)
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
        INSERT INTO billing_processor_charges (
            id, attempt_id, billing_scope_id, gateway_account_id,
            gateway_order_id, gateway_transaction_id, attempt_kind,
            amount_cents, currency
        ) VALUES ($1, $2, $3, $4, $5, $6, 'host_charge', 500, 'USD')
        "#,
    )
    .bind(processor_charge_id)
    .bind(attempt_id)
    .bind(account.billing_scope_id)
    .bind(account.gateway_account_id)
    .bind(&gateway_order_id)
    .bind(&gateway_transaction_id)
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
        INSERT INTO billing_external_reversal_attestations (
            attempt_id, processor_charge_id, actor_id, reversal_kind, reason,
            prior_resolution_code, final_resolution_code,
            gateway_account_id, gateway_configuration_id, gateway_order_id,
            amount_cents, currency, gateway_transaction_id, attested_at
        ) VALUES (
            $1, $2, $3, $4, 'migration fixture', $5, $6,
            $7, $8, $9, 500, 'USD', $10, clock_timestamp()
        )
        "#,
    )
    .bind(attempt_id)
    .bind(processor_charge_id)
    .bind(Uuid::now_v7())
    .bind(reversal_kind)
    .bind(prior_resolution_code)
    .bind(final_resolution_code)
    .bind(account.gateway_account_id)
    .bind(account.gateway_configuration_id)
    .bind(gateway_order_id)
    .bind(gateway_transaction_id)
    .execute(pool)
    .await?;
    Ok(attempt_id)
}
