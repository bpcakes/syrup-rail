use super::*;
use crate::schema_contract::{
    V5_TO_V6_PREFLIGHT_SQL, V5_TO_V6_UNCLASSIFIED_REVIEW_AUDIT_SQL, V5_TO_V6_UPGRADE_SQL,
    V6_CATALOG_FINGERPRINT,
};

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
    .bind(Uuid::now_v7())
    .bind(Uuid::now_v7())
    .bind(account.gateway_account_id)
    .bind(account.gateway_configuration_id)
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
    let counts: (i64, i64, i64, i64, i64) = sqlx::query_as(V5_TO_V6_PREFLIGHT_SQL)
        .fetch_one(&mut *preflight)
        .await?;
    assert_eq!(counts, (4, 1, 3, 0, 0));
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

    database.upgrade_v5_to_v6().await?;
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
