use super::*;

#[tokio::test]
async fn runtime_schema_v3_accepts_fresh_install_and_v2_upgrade() -> Result<(), Box<dyn Error>> {
    if V3_INSTALL_SQL.trim().is_empty() || V2_TO_V3_UPGRADE_SQL.trim().is_empty() {
        return Err(io::Error::other("schema-v3 artifacts must not be empty").into());
    }
    let fresh = TestDatabase::start("sr_fresh_v3").await?;
    let upgraded = TestDatabase::start_v2_then_upgrade("sr_upgrade_v3").await?;
    let upgraded_from_v1 = TestDatabase::start_v1_then_upgrade("sr_v1_to_v3").await?;
    let result = async {
        assert_v3_conforms(&fresh.pool).await?;
        assert_v3_conforms(&upgraded.pool).await?;
        assert_v3_conforms(&upgraded_from_v1.pool).await?;

        let fresh_fingerprint = canonical_catalog_fingerprint(&fresh.pool).await?;
        let upgraded_fingerprint = canonical_catalog_fingerprint(&upgraded.pool).await?;
        let upgraded_from_v1_fingerprint =
            canonical_catalog_fingerprint(&upgraded_from_v1.pool).await?;
        assert_eq!(fresh_fingerprint, upgraded_fingerprint);
        assert_eq!(fresh_fingerprint, upgraded_from_v1_fingerprint);
        assert_eq!(fresh_fingerprint, V3_CATALOG_FINGERPRINT);
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let fresh_cleanup = fresh.cleanup().await;
    let upgraded_cleanup = upgraded.cleanup().await;
    let upgraded_from_v1_cleanup = upgraded_from_v1.cleanup().await;
    result?;
    fresh_cleanup?;
    upgraded_cleanup?;
    upgraded_from_v1_cleanup
}

#[tokio::test]
async fn v2_upgrade_canonicalizes_legacy_combined_name_without_losing_display()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start_v2("sr_v3_contact").await?;
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
                amount_cents, currency, gateway_account_id,
                gateway_configuration_id, gateway_order_id,
                billing_name, billing_email
            ) VALUES (
                $1, $2, $3, $4, 'host_charge', 'pending', $5, $6,
                100, 'USD', $7, $8, $9, 'Mary Ann Smith',
                'mary@example.test'
            )
            "#,
        )
        .bind(attempt_id)
        .bind(account.billing_scope_id)
        .bind(subscriber_id)
        .bind(target_id)
        .bind(format!("legacy-contact-{attempt_id}"))
        .bind(format!("host_charge:{target_id}:100:USD"))
        .bind(account.gateway_account_id)
        .bind(account.gateway_configuration_id)
        .bind(format!("legacy-contact-order-{attempt_id}"))
        .execute(&database.pool)
        .await?;

        let mut transaction = database.pool.begin().await?;
        sqlx::raw_sql(V2_TO_V3_UPGRADE_SQL)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;

        let contact = sqlx::query_as::<_, (Option<String>, Option<String>, Option<String>)>(
            r#"
            SELECT billing_first_name, billing_last_name, billing_email
            FROM billing_payment_attempts
            WHERE id = $1
            "#,
        )
        .bind(attempt_id)
        .fetch_one(&database.pool)
        .await?;
        assert_eq!(contact.0.as_deref(), Some("Mary Ann Smith"));
        assert_eq!(contact.1, None);
        assert_eq!(contact.2.as_deref(), Some("mary@example.test"));
        assert_v3_conforms(&database.pool).await?;
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn runtime_schema_v3_rejects_v2_and_canonical_column_drift() -> Result<(), Box<dyn Error>> {
    let v2 = TestDatabase::start_v2("sr_v3_reject_v2").await?;
    let drifted = TestDatabase::start("sr_v3_drift").await?;
    let result = async {
        assert!(matches!(
            crate::assert_runtime_schema_v3_compatible(&v2.pool).await,
            Err(crate::SchemaConformanceError::Contract { version: 3, .. })
        ));
        sqlx::query("ALTER TABLE billing_payment_attempts ADD COLUMN host_extra text")
            .execute(&drifted.pool)
            .await?;
        assert!(matches!(
            crate::assert_runtime_schema_v3_compatible(&drifted.pool).await,
            Err(crate::SchemaConformanceError::Contract { version: 3, .. })
        ));
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let v2_cleanup = v2.cleanup().await;
    let drifted_cleanup = drifted.cleanup().await;
    result?;
    v2_cleanup?;
    drifted_cleanup
}
