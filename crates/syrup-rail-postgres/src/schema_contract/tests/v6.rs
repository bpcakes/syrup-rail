use super::*;
use crate::schema_contract::{
    V5_TO_V6_PREFLIGHT_SQL, V5_TO_V6_UNCLASSIFIED_REVIEW_AUDIT_SQL, V5_TO_V6_UPGRADE_SQL,
    V6_CATALOG_FINGERPRINT,
};

#[tokio::test]
async fn fresh_v6_defaults_fail_closed_by_evidence_owner() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("sr_v6_defaults").await?;
    let result = async {
        let defaults: Vec<(String, Option<String>)> = sqlx::query_as(
            r#"
            SELECT table_name, column_default
            FROM information_schema.columns
            WHERE table_schema = 'public'
              AND column_name = 'gateway_approval_evidence'
              AND table_name IN (
                  'billing_payment_attempts',
                  'billing_processor_charges',
                  'billing_external_reversal_attestations'
              )
            ORDER BY table_name
            "#,
        )
        .fetch_all(&database.pool)
        .await?;
        assert_eq!(
            defaults,
            vec![
                (
                    "billing_external_reversal_attestations".to_owned(),
                    Some("'unclassified'::text".to_owned()),
                ),
                (
                    "billing_payment_attempts".to_owned(),
                    Some("'absent'::text".to_owned()),
                ),
                (
                    "billing_processor_charges".to_owned(),
                    Some("'unclassified'::text".to_owned()),
                ),
            ]
        );
        Ok::<(), Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn runtime_schema_v6_matches_fresh_install_and_v5_upgrade() -> Result<(), Box<dyn Error>> {
    let fresh = TestDatabase::start("sr_fresh_v6").await?;
    let upgraded = TestDatabase::start_v5("sr_upgrade_v6").await?;
    let result = async {
        assert!(
            crate::assert_runtime_schema_v6_compatible(&upgraded.pool)
                .await
                .is_err()
        );
        let mut transaction = upgraded.pool.begin().await?;
        sqlx::raw_sql(V5_TO_V6_UPGRADE_SQL)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        let actual = canonical_catalog_fingerprint(&fresh.pool).await?;
        assert_eq!(actual, canonical_catalog_fingerprint(&upgraded.pool).await?);
        assert_eq!(
            actual, V6_CATALOG_FINGERPRINT,
            "v6 fingerprint: {actual:#018x}"
        );
        crate::assert_runtime_schema_v6_compatible(&fresh.pool).await?;
        crate::assert_runtime_schema_v6_compatible(&upgraded.pool).await?;
        Ok::<(), Box<dyn Error>>(())
    }
    .await;
    let a = fresh.cleanup().await;
    let b = upgraded.cleanup().await;
    result?;
    a?;
    b
}

#[tokio::test]
async fn v5_upgrade_preserves_unclassified_evidence_and_rejects_invalid_labels()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start_v5("sr_v6_legacy").await?;
    let result = async {
    let account = create_gateway_account(&database.pool, "nmi").await?;
    let attempt_id = Uuid::now_v7();
    let subscriber_id = Uuid::now_v7();
    let target_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO billing_payment_attempts (
            id, billing_scope_id, subscriber_id, host_charge_target_id,
            attempt_kind, status, idempotency_key, request_fingerprint,
            amount_cents, gateway_account_id, gateway_configuration_id,
            gateway_order_id, required_gateway_account_mode, gateway_response_code
        ) VALUES ($1, $2, $3, $4, 'host_charge', 'review_required',
            'legacy-signal-key', 'legacy-signal-fingerprint', 100, $5, $6,
            'legacy-signal-order', 'live', '0100')
    "#,
    )
    .bind(attempt_id)
    .bind(account.billing_scope_id)
    .bind(subscriber_id)
    .bind(target_id)
    .bind(account.gateway_account_id)
    .bind(account.gateway_configuration_id)
    .execute(&database.pool)
    .await?;
    let charge_id = Uuid::now_v7();
    let charge_transaction_id = "txn_legacy_structured_charge";
    sqlx::query(
        r#"
        INSERT INTO billing_processor_charges (
            id, attempt_id, billing_scope_id, gateway_account_id,
            gateway_order_id, gateway_transaction_id, gateway_response_code,
            attempt_kind, amount_cents, currency
        ) VALUES ($1, $2, $3, $4, 'legacy-signal-order', $5, '0100',
            'host_charge', 100, 'USD')
        "#,
    )
    .bind(charge_id)
    .bind(attempt_id)
    .bind(account.billing_scope_id)
    .bind(account.gateway_account_id)
    .bind(charge_transaction_id)
    .execute(&database.pool)
    .await?;
    sqlx::query(
        r#"
        INSERT INTO billing_external_reversal_attestations (
            attempt_id, processor_charge_id, actor_id, reversal_kind, reason,
            prior_resolution_code, final_resolution_code, gateway_account_id,
            gateway_configuration_id, gateway_order_id, amount_cents, currency,
            gateway_transaction_id, attested_at
        ) VALUES ($1, $2, $3, 'refund', 'legacy reversal confirmation',
            'processor_charge_external_reversal_required',
            'processor_charge_externally_refunded', $4, $5,
            'legacy-signal-order', 100, 'USD', $6, clock_timestamp())
        "#,
    )
    .bind(attempt_id)
    .bind(charge_id)
    .bind(Uuid::now_v7())
    .bind(account.gateway_account_id)
    .bind(account.gateway_configuration_id)
    .bind(charge_transaction_id)
    .execute(&database.pool)
    .await?;
    let empty_id = Uuid::now_v7();
    let local_id = Uuid::now_v7();
    let error_id = Uuid::now_v7();
    for (id, detail) in [
        (empty_id, None),
        (
            local_id,
            Some(
                "Payment processor did not return a transaction before the reconciliation deadline.",
            ),
        ),
        (error_id, Some("Unrecognized processor detail")),
    ] {
        sqlx::query(r#"INSERT INTO billing_payment_attempts (
            id, billing_scope_id, subscriber_id, host_charge_target_id, attempt_kind,
            status, idempotency_key, request_fingerprint, amount_cents, gateway_account_id,
            gateway_configuration_id, gateway_order_id, required_gateway_account_mode, gateway_response_text
        ) SELECT $2, billing_scope_id, subscriber_id, $2, attempt_kind,
            status, $2::text, request_fingerprint, amount_cents, gateway_account_id,
            gateway_configuration_id, 'legacy_' || replace($2::text, '-', ''), required_gateway_account_mode, $3
          FROM billing_payment_attempts WHERE id = $1"#)
            .bind(attempt_id).bind(id).bind(detail).execute(&database.pool).await?;
    }
    let mut preflight = database.pool.begin().await?;
    sqlx::query("SET TRANSACTION READ ONLY")
        .execute(&mut *preflight)
        .await?;
    let counts: (i64, i64, i64, i64, i64, i64) = sqlx::query_as(V5_TO_V6_PREFLIGHT_SQL)
        .fetch_one(&mut *preflight)
        .await?;
    assert_eq!(counts, (4, 1, 3, 0, 1, 1));
    let audited = sqlx::query(V5_TO_V6_UNCLASSIFIED_REVIEW_AUDIT_SQL)
        .fetch_all(&mut *preflight)
        .await?;
    let mut audited_ids = audited
        .iter()
        .map(|row| row.try_get::<Uuid, _>("attempt_id"))
        .collect::<Result<Vec<_>, _>>()?;
    audited_ids.sort_unstable();
    let mut expected_ids = vec![attempt_id, local_id, error_id];
    expected_ids.sort_unstable();
    assert_eq!(audited_ids, expected_ids);
    preflight.rollback().await?;

    let retained_tuples_sql = "SELECT tableoid::regclass::text, xmin::text, ctid::text
        FROM billing_processor_charges
        UNION ALL
        SELECT tableoid::regclass::text, xmin::text, ctid::text
        FROM billing_external_reversal_attestations
        ORDER BY 1, 3";
    let retained_tuples: Vec<(String, String, String)> = sqlx::query_as(retained_tuples_sql)
        .fetch_all(&database.pool).await?;
    database.upgrade_v5_to_v6().await?;
    let upgraded_tuples: Vec<(String, String, String)> = sqlx::query_as(retained_tuples_sql)
        .fetch_all(&database.pool).await?;
    assert_eq!(retained_tuples, upgraded_tuples, "charge and attestation backfill must not update retained tuples");
    let charge_classification: String = sqlx::query_scalar(
        "SELECT gateway_approval_evidence FROM billing_processor_charges WHERE id = $1",
    )
    .bind(charge_id)
    .fetch_one(&database.pool)
    .await?;
    assert_eq!(charge_classification, "structured");
    let attestation_classification: String = sqlx::query_scalar(
        "SELECT gateway_approval_evidence FROM billing_external_reversal_attestations WHERE processor_charge_id = $1",
    )
    .bind(charge_id)
    .fetch_one(&database.pool)
    .await?;
    assert_eq!(attestation_classification, "structured");
    sqlx::query(
        r#"
        UPDATE billing_processor_charges
        SET progression_state = 'externally_reversed',
            state_code = 'processor_charge_external_reversal_required',
            externally_reversed_at = clock_timestamp()
        WHERE id = $1
        "#,
    )
    .bind(charge_id)
    .execute(&database.pool)
    .await?;
    sqlx::query(
        "UPDATE billing_payment_attempts SET status = 'failed', resolved_at = clock_timestamp() WHERE id = $1",
    )
    .bind(attempt_id)
    .execute(&database.pool)
    .await?;
    sqlx::query(
        "UPDATE billing_external_reversal_attestations SET gateway_response_code = '0100' WHERE processor_charge_id = $1",
    )
    .bind(charge_id)
    .execute(&database.pool)
    .await?;
    let admission = || {
        sqlx::query_scalar::<_, String>(
            "SELECT billing_host_charge_ledger_admission($1, $2, $3, 'release', NULL, NULL)",
        )
        .bind(account.billing_scope_id)
        .bind(subscriber_id)
        .bind(target_id)
        .fetch_one(&database.pool)
    };
    assert_eq!(admission().await?, "safe");
    sqlx::query(
        "UPDATE billing_external_reversal_attestations SET gateway_approval_evidence = 'unclassified' WHERE processor_charge_id = $1",
    )
    .bind(charge_id)
    .execute(&database.pool)
    .await?;
    assert_eq!(admission().await?, "unsafe");
    sqlx::query(
        "UPDATE billing_external_reversal_attestations SET gateway_approval_evidence = 'structured' WHERE processor_charge_id = $1",
    )
    .bind(charge_id)
    .execute(&database.pool)
    .await?;
    let replay_evidence = syrup_rail::ProcessorEvidence::new(
        syrup_rail::ProcessorApprovalEvidence::Structured,
        Some(syrup_rail::GatewayTransactionId::new(charge_transaction_id)?),
        None,
        None,
        Some(syrup_rail::GatewayDiagnostic::new("0100")),
        None,
        None,
        syrup_rail::GatewayPaymentDescriptor::default(),
    );
    let mut replay_transaction = database.pool.begin().await?;
    let replay = crate::observe_processor_charge_in_transaction(
        &mut replay_transaction,
        syrup_rail::PaymentAttemptId::new(attempt_id),
        &syrup_rail::GatewayOrderId::from_correlation("legacy-signal-order")?,
        &replay_evidence,
        syrup_rail::ProcessorChargeProgression::Pending,
    )
    .await?;
    assert!(matches!(
        replay,
        crate::ProcessorChargeObservationOutcome::ExactReplay(_)
    ));
    replay_transaction.rollback().await?;
    for (id, expected) in [
        (empty_id, "absent"),
        (local_id, "unclassified"),
        (error_id, "unclassified"),
    ] {
        let classification: String = sqlx::query_scalar(
            "SELECT gateway_approval_evidence FROM billing_payment_attempts WHERE id = $1",
        )
        .bind(id)
        .fetch_one(&database.pool)
        .await?;
        assert_eq!(classification, expected);
        let mut transaction = database.pool.begin().await?;
        let attempt = crate::find_payment_attempt_by_id_in_transaction(
            &mut transaction,
            syrup_rail::BillingScopeId::new(account.billing_scope_id),
            syrup_rail::PaymentAttemptId::new(id),
        )
        .await?
        .unwrap();
        assert_eq!(
            syrup_rail::review_required_attempt_can_be_manually_failed(&attempt),
            expected == "absent"
        );
        transaction.rollback().await?;
    }
    let row: (String, String) = sqlx::query_as("SELECT gateway_response_code, gateway_approval_evidence FROM billing_payment_attempts WHERE id = $1")
        .bind(attempt_id).fetch_one(&database.pool).await?;
    assert_eq!(row, ("0100".to_owned(), "unclassified".to_owned()));
    let mut transaction = database.pool.begin().await?;
    let attempt = crate::find_payment_attempt_by_id_in_transaction(
        &mut transaction,
        syrup_rail::BillingScopeId::new(account.billing_scope_id),
        syrup_rail::PaymentAttemptId::new(attempt_id),
    )
    .await?
    .unwrap();
    assert!(attempt.state().processor_evidence().may_indicate_approval());
    assert!(!syrup_rail::review_required_attempt_can_be_manually_failed(
        &attempt
    ));
    transaction.rollback().await?;
    let error = sqlx::query("UPDATE billing_payment_attempts SET gateway_approval_evidence = 'unknown-label' WHERE id = $1")
        .bind(attempt_id).execute(&database.pool).await.unwrap_err();
    assert_eq!(
        error.as_database_error().and_then(|e| e.constraint()),
        Some("billing_payment_attempts_approval_evidence_check")
    );
    crate::assert_runtime_schema_v6_compatible(&database.pool).await?;
        Ok::<(), Box<dyn Error>>(())
    }.await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn v5_preflight_inventories_terminal_host_targets() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start_v5("sr_v6_terminal").await?;
    let result = async {
        let account = create_gateway_account(&database.pool, "nmi").await?;
        let subscriber = Uuid::now_v7();
        let declined = Uuid::now_v7();
        let unsubmitted = Uuid::now_v7();
        for (id, status, submitted, response, detail) in [
            (
                declined,
                "declined",
                true,
                Some("2"),
                "Declined by processor",
            ),
            (
                unsubmitted,
                "failed",
                false,
                None,
                "Invalid gateway configuration",
            ),
        ] {
            sqlx::query(
                r#"
                INSERT INTO billing_payment_attempts (
                    id, billing_scope_id, subscriber_id, host_charge_target_id,
                    attempt_kind, status, idempotency_key, request_fingerprint,
                    amount_cents, gateway_account_id, gateway_configuration_id,
                    gateway_order_id, required_gateway_account_mode,
                    submitted_at, resolved_at, gateway_response, gateway_response_text
                ) VALUES ($1, $2, $3, $1, 'host_charge', $4, $1::text,
                    'legacy-terminal-fingerprint', 100, $5, $6,
                    'legacy_' || replace($1::text, '-', ''), 'live',
                    CASE WHEN $7 THEN clock_timestamp() END, clock_timestamp(), $8, $9)
            "#,
            )
            .bind(id)
            .bind(account.billing_scope_id)
            .bind(subscriber)
            .bind(status)
            .bind(account.gateway_account_id)
            .bind(account.gateway_configuration_id)
            .bind(submitted)
            .bind(response)
            .bind(detail)
            .execute(&database.pool)
            .await?;
        }
        let count: i64 = sqlx::query(V5_TO_V6_PREFLIGHT_SQL)
            .fetch_one(&database.pool)
            .await?
            .try_get("terminal_host_attempts_with_unclassified_evidence_count")?;
        assert_eq!(count, 2);
        let audited = sqlx::query(V5_TO_V6_UNCLASSIFIED_REVIEW_AUDIT_SQL)
            .fetch_all(&database.pool)
            .await?;
        let mut ids = audited
            .iter()
            .map(|row| row.try_get::<Uuid, _>("attempt_id"))
            .collect::<Result<Vec<_>, _>>()?;
        ids.sort_unstable();
        let mut expected = vec![declined, unsubmitted];
        expected.sort_unstable();
        assert_eq!(ids, expected);
        Ok::<(), Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}
