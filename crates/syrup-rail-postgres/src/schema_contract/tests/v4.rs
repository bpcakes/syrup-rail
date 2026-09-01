use std::{error::Error, io};

use super::*;
use crate::schema_contract::tests::fixtures::create_v2_subscription_fixture;

#[tokio::test]
async fn runtime_schema_v4_accepts_fresh_install_and_v3_upgrade() -> Result<(), Box<dyn Error>> {
    if V4_INSTALL_SQL.trim().is_empty()
        || V3_TO_V4_PREPARE_SQL.trim().is_empty()
        || V3_TO_V4_VALIDATE_SQL.trim().is_empty()
        || V3_TO_V4_INDEX_SQL.trim().is_empty()
        || V3_TO_V4_UPGRADE_SQL.trim().is_empty()
    {
        return Err(io::Error::other("schema-v4 artifacts must not be empty").into());
    }
    let fresh = TestDatabase::start("sr_fresh_v4").await?;
    let upgraded = TestDatabase::start_v3("sr_upgrade_v4").await?;
    upgraded.upgrade_v3_to_v4().await?;
    let result = async {
        assert_v4_conforms(&fresh.pool).await?;
        assert_v4_conforms(&upgraded.pool).await?;

        let fresh_fingerprint = canonical_catalog_fingerprint(&fresh.pool).await?;
        let upgraded_fingerprint = canonical_catalog_fingerprint(&upgraded.pool).await?;
        assert_eq!(fresh_fingerprint, upgraded_fingerprint);
        assert_eq!(fresh_fingerprint, V4_CATALOG_FINGERPRINT);
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
                    "schema-v4 resolution constraint is missing {}",
                    code.as_str()
                );
            }
        }
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
async fn v3_upgrade_stages_validation_and_removes_the_compatibility_defaults()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start_v3("sr_v4_stages").await?;
    let result = async {
        database.prepare_v3_to_v4().await?;
        let prepared: (bool, bool, bool) = sqlx::query_as(
            r#"
            SELECT
                column_default = '''live''::text',
                NOT constraints.convalidated,
                (
                    SELECT NOT convalidated
                    FROM pg_constraint
                    WHERE conname =
                        'billing_payment_attempts_resolution_code_check'
                )
            FROM information_schema.columns AS columns
            JOIN pg_constraint AS constraints
                ON constraints.conname =
                    'billing_payment_attempts_required_gateway_account_mode_check'
            WHERE columns.table_schema = 'public'
                AND columns.table_name = 'billing_payment_attempts'
                AND columns.column_name = 'required_gateway_account_mode'
            "#,
        )
        .fetch_one(&database.pool)
        .await?;
        assert_eq!(prepared, (true, true, true));
        let prepared_subscription: (bool, bool) = sqlx::query_as(
            r#"
            SELECT
                column_default = '''live''::text',
                NOT constraints.convalidated
            FROM information_schema.columns AS columns
            JOIN pg_constraint AS constraints
                ON constraints.conname =
                    'billing_subscriptions_required_gateway_account_mode_check'
            WHERE columns.table_schema = 'public'
                AND columns.table_name = 'billing_subscriptions'
                AND columns.column_name = 'required_gateway_account_mode'
            "#,
        )
        .fetch_one(&database.pool)
        .await?;
        assert_eq!(prepared_subscription, (true, true));

        database.validate_v3_to_v4().await?;
        let validated: bool = sqlx::query_scalar(
            r#"
            SELECT bool_and(convalidated)
            FROM pg_constraint
            WHERE conname IN (
                'billing_payment_attempts_required_gateway_account_mode_check',
                'billing_subscriptions_required_gateway_account_mode_check',
                'billing_payment_attempts_resolution_code_check'
            )
            "#,
        )
        .fetch_one(&database.pool)
        .await?;
        assert!(validated);
        let indexes_before_build: (bool, bool) = sqlx::query_as(
            r#"
            SELECT
                to_regclass('public.billing_subscriptions_due_mode_idx') IS NOT NULL,
                to_regclass('public.billing_subscriptions_due_v4_idx') IS NOT NULL
            "#,
        )
        .fetch_one(&database.pool)
        .await?;
        assert_eq!(indexes_before_build, (false, false));

        database.index_v3_to_v4().await?;
        let indexes_before_finalization: (bool, bool) = sqlx::query_as(
            r#"
            SELECT
                (
                    SELECT indisvalid
                    FROM pg_index
                    WHERE indexrelid =
                        'public.billing_subscriptions_due_mode_idx'::regclass
                ),
                (
                    SELECT indisvalid
                    FROM pg_index
                    WHERE indexrelid =
                        'public.billing_subscriptions_due_v4_idx'::regclass
                )
            "#,
        )
        .fetch_one(&database.pool)
        .await?;
        assert_eq!(indexes_before_finalization, (true, true));

        sqlx::query("DROP INDEX public.billing_subscriptions_due_v4_idx")
            .execute(&database.pool)
            .await?;
        let missing_index = database
            .finalize_v3_to_v4()
            .await
            .expect_err("finalization must reject a missing concurrent index");
        assert!(
            missing_index
                .to_string()
                .contains("all-mode renewal index was not built successfully")
        );
        let old_index_preserved: bool = sqlx::query_scalar(
            "SELECT to_regclass('public.billing_subscriptions_due_idx') IS NOT NULL",
        )
        .fetch_one(&database.pool)
        .await?;
        assert!(old_index_preserved);
        database.index_v3_to_v4().await?;

        database.finalize_v3_to_v4().await?;
        let already_finalized = database
            .finalize_v3_to_v4()
            .await
            .expect_err("finalization must diagnose an already-applied artifact");
        assert!(
            already_finalized
                .to_string()
                .contains("schema-v4 finalization is already applied")
        );
        let indexes_after_finalization: (bool, bool, bool) = sqlx::query_as(
            r#"
            SELECT
                to_regclass('public.billing_subscriptions_due_mode_idx') IS NOT NULL,
                to_regclass('public.billing_subscriptions_due_idx') IS NOT NULL,
                to_regclass('public.billing_subscriptions_due_v4_idx') IS NULL
            "#,
        )
        .fetch_one(&database.pool)
        .await?;
        assert_eq!(indexes_after_finalization, (true, true, true));
        let default: Option<String> = sqlx::query_scalar(
            "SELECT column_default FROM information_schema.columns WHERE table_schema = 'public' AND table_name = 'billing_payment_attempts' AND column_name = 'required_gateway_account_mode'",
        )
        .fetch_one(&database.pool)
        .await?;
        assert_eq!(default, None);
        let subscription_default: Option<String> = sqlx::query_scalar(
            "SELECT column_default FROM information_schema.columns WHERE table_schema = 'public' AND table_name = 'billing_subscriptions' AND column_name = 'required_gateway_account_mode'",
        )
        .fetch_one(&database.pool)
        .await?;
        assert_eq!(subscription_default, None);

        let account = create_gateway_account(&database.pool, "nmi").await?;
        let omitted_mode = sqlx::query(
            r#"
            INSERT INTO billing_payment_attempts (
                id, billing_scope_id, subscriber_id, host_charge_target_id,
                attempt_kind, status, idempotency_key, request_fingerprint,
                amount_cents, currency, gateway_account_id,
                gateway_configuration_id, gateway_order_id
            ) VALUES (
                $1, $2, $3, $4, 'host_charge', 'pending', $5, $6,
                100, 'USD', $7, $8, $9
            )
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(account.billing_scope_id)
        .bind(Uuid::now_v7())
        .bind(Uuid::now_v7())
        .bind(format!("omitted-mode-{}", Uuid::now_v7()))
        .bind(format!("host-charge:omitted-mode:{}", Uuid::now_v7()))
        .bind(account.gateway_account_id)
        .bind(account.gateway_configuration_id)
        .bind(format!("omitted-mode-order-{}", Uuid::now_v7()))
        .execute(&database.pool)
        .await;
        assert!(matches!(
            omitted_mode,
            Err(sqlx::Error::Database(error))
                if error.code().as_deref() == Some("23502")
                    && error.message().contains("required_gateway_account_mode")
        ));
        let omitted_subscription =
            create_v2_subscription_fixture(&database.pool, account, Uuid::now_v7()).await;
        assert!(matches!(
            omitted_subscription,
            Err(sqlx::Error::Database(error))
                if error.code().as_deref() == Some("23502")
                    && error.message().contains("required_gateway_account_mode")
        ));
        assert_v4_conforms(&database.pool).await?;
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn v3_upgrade_backfills_historical_attempts_and_subscriptions_as_live()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start_v3("sr_v4_backfill").await?;
    let result = async {
        let account = create_gateway_account(&database.pool, "nmi").await?;
        let subscriber_id = Uuid::now_v7();
        let (_, subscription_id, _) =
            create_v2_subscription_fixture(&database.pool, account, subscriber_id).await?;
        let attempt_id = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO billing_payment_attempts (
                id, billing_scope_id, subscriber_id, host_charge_target_id,
                attempt_kind, status, idempotency_key, request_fingerprint,
                amount_cents, currency, gateway_account_id,
                gateway_configuration_id, gateway_order_id
            ) VALUES (
                $1, $2, $3, $4, 'host_charge', 'pending', $5, $6,
                100, 'USD', $7, $8, $9
            )
            "#,
        )
        .bind(attempt_id)
        .bind(account.billing_scope_id)
        .bind(Uuid::now_v7())
        .bind(Uuid::now_v7())
        .bind(format!("historical-{attempt_id}"))
        .bind(format!("initial:historical:{attempt_id}"))
        .bind(account.gateway_account_id)
        .bind(account.gateway_configuration_id)
        .bind(format!("historical-order-{attempt_id}"))
        .execute(&database.pool)
        .await?;

        database.upgrade_v3_to_v4().await?;
        let required_mode: String = sqlx::query_scalar(
            "SELECT required_gateway_account_mode FROM billing_payment_attempts WHERE id = $1",
        )
        .bind(attempt_id)
        .fetch_one(&database.pool)
        .await?;
        assert_eq!(required_mode, "live");
        let subscription_mode: String = sqlx::query_scalar(
            "SELECT required_gateway_account_mode FROM billing_subscriptions WHERE id = $1",
        )
        .bind(subscription_id)
        .fetch_one(&database.pool)
        .await?;
        assert_eq!(subscription_mode, "live");
        assert_v4_conforms(&database.pool).await?;
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn runtime_schema_v4_rejects_v3_and_canonical_column_drift() -> Result<(), Box<dyn Error>> {
    let v3 = TestDatabase::start_v3("sr_v4_reject_v3").await?;
    let drifted = TestDatabase::start("sr_v4_drift").await?;
    let result = async {
        assert!(matches!(
            crate::assert_runtime_schema_v4_compatible(&v3.pool).await,
            Err(crate::SchemaConformanceError::Contract { version: 4, .. })
        ));
        sqlx::query("ALTER TABLE billing_payment_attempts ADD COLUMN host_extra text")
            .execute(&drifted.pool)
            .await?;
        assert!(matches!(
            crate::assert_runtime_schema_v4_compatible(&drifted.pool).await,
            Err(crate::SchemaConformanceError::Contract { version: 4, .. })
        ));
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let v3_cleanup = v3.cleanup().await;
    let drifted_cleanup = drifted.cleanup().await;
    result?;
    v3_cleanup?;
    drifted_cleanup
}
