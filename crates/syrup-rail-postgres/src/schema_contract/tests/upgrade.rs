use super::fixtures::*;
use super::*;

use syrup_rail::{
    BillingEvent, BillingScopeId, CancelSubscription, CancelSubscriptionOutcome, PaymentAttemptId,
    PlanKey, RenewalFailureDisposition, SubscriberId,
};

mod retry_reclassification;

#[tokio::test]
async fn schema_v1_upgrade_matches_fresh_v2() -> Result<(), Box<dyn Error>> {
    let upgraded = TestDatabase::start_v1_then_upgrade_to_v2("sr_upgrade_v2").await?;
    let fresh = TestDatabase::start_v2("sr_fresh_v2").await?;
    let result = async {
        assert_v2_conforms(&upgraded.pool).await?;
        assert_v2_conforms(&fresh.pool).await?;
        let upgraded_fingerprint = canonical_catalog_fingerprint(&upgraded.pool).await?;
        let fresh_fingerprint = canonical_catalog_fingerprint(&fresh.pool).await?;
        if upgraded_fingerprint != fresh_fingerprint {
            return Err(io::Error::other(format!(
                "fresh/upgrade catalog mismatch: fresh {fresh_fingerprint:#018x}, upgraded {upgraded_fingerprint:#018x}"
            ))
            .into());
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
async fn schema_v1_upgrade_preflight_reports_only_blockers() -> Result<(), Box<dyn Error>> {
    let normalized = V1_TO_V2_PREFLIGHT_SQL.trim().to_ascii_uppercase();
    if !normalized.starts_with("-- READ-ONLY PREFLIGHT")
        || normalized.matches(';').count() != 1
        || !normalized.ends_with(';')
    {
        return Err(
            io::Error::other("version-1 preflight must be one checked-in result query").into(),
        );
    }
    for forbidden in [
        "\nINSERT ",
        "\nUPDATE ",
        "\nDELETE ",
        "\nALTER ",
        "\nCREATE ",
        "\nDROP ",
        "\nDO ",
        "\nBEGIN",
        "\nCOMMIT",
    ] {
        if normalized.contains(forbidden) {
            return Err(io::Error::other(format!(
                "version-1 preflight contains mutation or transaction control {forbidden:?}"
            ))
            .into());
        }
    }

    let database = TestDatabase::start_v1("sr_preflight_v1").await?;
    let result = async {
        let gateway = create_gateway_account(&database.pool, "test_gateway").await?;

        let valid_subscriber_id = Uuid::now_v7();
        let (valid_method_id, valid_subscription_id, valid_initial_transaction_id) =
            create_v1_subscription_with_status(
                &database.pool,
                gateway,
                valid_subscriber_id,
                "past_due",
            )
            .await?;
        insert_v1_terminal_subscription_attempt(
            &database.pool,
            gateway,
            valid_subscriber_id,
            valid_method_id,
            valid_subscription_id,
            &valid_initial_transaction_id,
            "subscription_renewal",
            "declined",
            "valid-customer-failure",
            Some("2026-02-02 00:30:00+00"),
            "2026-02-02 00:31:00+00",
            None,
        )
        .await?;

        let mixed_subscriber_id = Uuid::now_v7();
        let (mixed_method_id, mixed_subscription_id, mixed_initial_transaction_id) =
            create_v1_subscription_with_status(
                &database.pool,
                gateway,
                mixed_subscriber_id,
                "past_due",
            )
            .await?;
        insert_mixed_v1_failure_history(
            &database.pool,
            gateway,
            mixed_subscriber_id,
            mixed_method_id,
            mixed_subscription_id,
            &mixed_initial_transaction_id,
        )
        .await?;

        let recovery_subscriber_id = Uuid::now_v7();
        let (recovery_method_id, recovery_subscription_id, recovery_initial_transaction_id) =
            create_v1_subscription_with_status(
                &database.pool,
                gateway,
                recovery_subscriber_id,
                "active",
            )
            .await?;
        apply_v1_manual_active_recovery_failure(
            &database.pool,
            gateway,
            recovery_subscriber_id,
            recovery_method_id,
            recovery_subscription_id,
            &recovery_initial_transaction_id,
        )
        .await?;

        let unreviewed_recovery_subscriber_id = Uuid::now_v7();
        let (
            unreviewed_recovery_method_id,
            unreviewed_recovery_subscription_id,
            unreviewed_recovery_initial_transaction_id,
        ) = create_v1_subscription_with_status(
            &database.pool,
            gateway,
            unreviewed_recovery_subscriber_id,
            "past_due",
        )
        .await?;
        let unreviewed_recovery_attempt_id = insert_v1_terminal_subscription_attempt(
            &database.pool,
            gateway,
            unreviewed_recovery_subscriber_id,
            unreviewed_recovery_method_id,
            unreviewed_recovery_subscription_id,
            &unreviewed_recovery_initial_transaction_id,
            "subscription_recovery",
            "declined",
            "unreviewed-active-recovery",
            Some("2026-02-02 00:30:00+00"),
            "2026-02-02 00:31:00+00",
            None,
        )
        .await?;
        // Match the active optimistic snapshot without the review marker that
        // proves the shipped v1 operator transition. Similar-looking recovery
        // evidence must not legitimize an otherwise unattributed status.
        sqlx::query(
            "UPDATE billing_payment_attempts SET subscription_expected_status = 'active' WHERE id = $1",
        )
        .bind(unreviewed_recovery_attempt_id)
        .execute(&database.pool)
        .await?;

        let anomaly_subscriber_id = Uuid::now_v7();
        let (_, anomaly_subscription_id, _) = create_v1_subscription_with_status(
            &database.pool,
            gateway,
            anomaly_subscriber_id,
            "past_due",
        )
        .await?;

        let rows = sqlx::query(V1_TO_V2_PREFLIGHT_SQL)
            .fetch_all(&database.pool)
            .await?;
        if rows.len() != 2 {
            return Err(io::Error::other(format!(
                "preflight returned {} blockers instead of two",
                rows.len()
            ))
            .into());
        }
        let blockers = rows
            .iter()
            .map(|row| {
                Ok::<_, sqlx::Error>((
                    row.try_get::<Uuid, _>("subscription_id")?,
                    row.try_get::<Uuid, _>("subscriber_id")?,
                    row.try_get::<i64, _>("qualifying_failure_count")?,
                ))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let expected_blockers = [
            (
                unreviewed_recovery_subscription_id,
                unreviewed_recovery_subscriber_id,
                0,
            ),
            (anomaly_subscription_id, anomaly_subscriber_id, 0),
        ];
        if !expected_blockers
            .iter()
            .all(|expected| blockers.contains(expected))
        {
            return Err(io::Error::other(format!(
                "preflight returned the wrong blockers: {blockers:?}"
            ))
            .into());
        }

        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn schema_v1_upgrade_anomaly_rolls_back_with_named_diagnostic() -> Result<(), Box<dyn Error>>
{
    let database = TestDatabase::start_v1("sr_up_rollback").await?;
    let result = async {
        let gateway = create_gateway_account(&database.pool, "test_gateway").await?;
        create_v1_subscription_with_status(&database.pool, gateway, Uuid::now_v7(), "past_due")
            .await?;

        let mut transaction = database.pool.begin().await?;
        let failure = sqlx::raw_sql(V1_TO_V2_UPGRADE_SQL)
            .execute(&mut *transaction)
            .await
            .expect_err("an anomalous past-due subscription must abort the upgrade");
        if !failure
            .to_string()
            .contains("billing_v1_to_v2_missing_past_due_failure_history")
        {
            return Err(io::Error::other(format!(
                "upgrade returned the wrong anomaly diagnostic: {failure}"
            ))
            .into());
        }
        transaction.rollback().await?;

        let phase_column_exists = sqlx::query_scalar::<_, bool>(
            r#"
            SELECT EXISTS (
                SELECT 1
                FROM information_schema.columns
                WHERE table_schema = 'public'
                    AND table_name = 'billing_subscriptions'
                    AND column_name = 'phase'
            )
            "#,
        )
        .fetch_one(&database.pool)
        .await?;
        if phase_column_exists {
            return Err(io::Error::other(
                "failed upgrade left a version-2 subscription column behind",
            )
            .into());
        }
        assert_v1_conforms(&database.pool).await?;
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn schema_v1_upgrade_backfills_legacy_lifecycle_attempt_terms_and_mode()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start_v1("sr_up_data").await?;
    let result = async {
        let gateway = create_gateway_account(&database.pool, "test_gateway").await?;

        let active_subscriber_id = Uuid::now_v7();
        let (_, active_subscription_id, _) = create_v1_subscription_with_status(
            &database.pool,
            gateway,
            active_subscriber_id,
            "active",
        )
        .await?;

        let past_due_subscriber_id = Uuid::now_v7();
        let (past_due_method_id, past_due_subscription_id, past_due_initial_transaction_id) =
            create_v1_subscription_with_status(
                &database.pool,
                gateway,
                past_due_subscriber_id,
                "past_due",
            )
            .await?;
        let customer_attempt_id = insert_mixed_v1_failure_history(
            &database.pool,
            gateway,
            past_due_subscriber_id,
            past_due_method_id,
            past_due_subscription_id,
            &past_due_initial_transaction_id,
        )
        .await?;

        let exhausted_subscriber_id = Uuid::now_v7();
        let (exhausted_method_id, exhausted_subscription_id, exhausted_transaction_id) =
            create_v1_subscription_with_status(
                &database.pool,
                gateway,
                exhausted_subscriber_id,
                "past_due",
            )
            .await?;
        for sequence in 0..5 {
            insert_v1_terminal_subscription_attempt(
                &database.pool,
                gateway,
                exhausted_subscriber_id,
                exhausted_method_id,
                exhausted_subscription_id,
                &exhausted_transaction_id,
                "subscription_renewal",
                "declined",
                &format!("exhausted-customer-failure-{sequence}"),
                Some(&format!("2026-02-0{} 00:00:00+00", sequence + 2)),
                &format!("2026-02-0{} 00:01:00+00", sequence + 2),
                None,
            )
            .await?;
        }

        let reclassified_subscriber_id = Uuid::now_v7();
        let (
            reclassified_method_id,
            reclassified_subscription_id,
            reclassified_transaction_id,
        ) = create_v1_subscription_with_status(
            &database.pool,
            gateway,
            reclassified_subscriber_id,
            "past_due",
        )
        .await?;
        insert_v1_reclassified_exhausted_history(
            &database.pool,
            gateway,
            reclassified_subscriber_id,
            reclassified_method_id,
            reclassified_subscription_id,
            &reclassified_transaction_id,
        )
        .await?;
        let legacy_count: i64 = sqlx::query_scalar(
            r#"
            SELECT count(*)
            FROM billing_payment_attempts
            WHERE subscription_id = $1
                AND attempt_kind IN ('subscription_renewal', 'subscription_recovery')
                AND billing_period_start_at = '2026-02-01 00:00:00+00'
                AND status IN ('declined', 'failed')
                AND resolution_code IS NULL
            "#,
        )
        .bind(reclassified_subscription_id)
        .fetch_one(&database.pool)
        .await?;
        if legacy_count != 5 {
            return Err(io::Error::other(
                "reclassified fixture did not reach the version-1 terminal count",
            )
            .into());
        }

        let recovery_subscriber_id = Uuid::now_v7();
        let (recovery_method_id, recovery_subscription_id, recovery_initial_transaction_id) =
            create_v1_subscription_with_status(
                &database.pool,
                gateway,
                recovery_subscriber_id,
                "active",
            )
            .await?;
        let (recovery_attempt_id, recovery_failed_at) =
            apply_v1_manual_active_recovery_failure(
                &database.pool,
                gateway,
                recovery_subscriber_id,
                recovery_method_id,
                recovery_subscription_id,
                &recovery_initial_transaction_id,
            )
            .await?;

        let canceled_subscriber_id = Uuid::now_v7();
        let (_, canceled_subscription_id, _) = create_v1_subscription_with_status(
            &database.pool,
            gateway,
            canceled_subscriber_id,
            "canceled",
        )
        .await?;

        let full_price_fingerprint = "legacy-initial:base_subscription:100:USD";
        let full_price_attempt_id = insert_v1_initial_attempt(
            &database.pool,
            gateway,
            Uuid::now_v7(),
            100,
            full_price_fingerprint,
            None,
        )
        .await?;
        let discounted_fingerprint =
            "legacy-initial:base_subscription:90:USD:discount:SAVE10";
        let discounted_attempt_id = insert_v1_discounted_initial_attempt(
            &database.pool,
            gateway,
            Uuid::now_v7(),
            discounted_fingerprint,
        )
        .await?;

        let mut transaction = database.pool.begin().await?;
        sqlx::raw_sql(V1_TO_V2_UPGRADE_SQL)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        assert_v2_conforms(&database.pool).await?;
        database.upgrade_v2_to_v3().await?;
        assert_v3_conforms(&database.pool).await?;
        database.upgrade_v3_to_v4().await?;
        assert_v4_conforms(&database.pool).await?;
        database.upgrade_v4_to_v5().await?;
        database.upgrade_v5_to_v6().await?;

        let active_is_legacy_recurring = sqlx::query_scalar::<_, bool>(
            r#"
            SELECT phase = 'recurring'
                AND recurring_period_kind = 'calendar_months'
                AND recurring_period_count = 1
                AND trial_amount_cents IS NULL
                AND dunning_retry_delays_seconds =
                    ARRAY[86400, 86400, 86400, 86400]::bigint[]
                AND dunning_exhaustion = 'remain_past_due'
                AND past_due_access = 'suspend_immediately'
                AND next_payment_attempt_at = next_renewal_at
                AND unpaid_at IS NULL
            FROM billing_subscriptions
            WHERE id = $1
            "#,
        )
        .bind(active_subscription_id)
        .fetch_one(&database.pool)
        .await?;
        if !active_is_legacy_recurring {
            return Err(io::Error::other("active legacy subscription backfill drifted").into());
        }

        let mixed_schedule_is_customer_derived = sqlx::query_scalar::<_, bool>(
            r#"
            SELECT status = 'past_due'
                AND next_payment_attempt_at =
                    '2026-02-03 00:31:00+00'::timestamptz
            FROM billing_subscriptions
            WHERE id = $1
            "#,
        )
        .bind(past_due_subscription_id)
        .fetch_one(&database.pool)
        .await?;
        if !mixed_schedule_is_customer_derived {
            return Err(io::Error::other(
                "mixed legacy history did not schedule from its sole customer failure",
            )
            .into());
        }

        let reclassified_schedule_resumes = sqlx::query_scalar::<_, bool>(
            r#"
            SELECT status = 'past_due'
                AND next_payment_attempt_at =
                    '2026-02-05 00:01:00+00'::timestamptz
            FROM billing_subscriptions
            WHERE id = $1
            "#,
        )
        .bind(reclassified_subscription_id)
        .fetch_one(&database.pool)
        .await?;
        if !reclassified_schedule_resumes {
            return Err(io::Error::other(
                "v2 did not intentionally resume renewal-only dunning after recovery failures were reclassified",
            )
            .into());
        }
        let recovery_only_schedule_resumes = sqlx::query_scalar::<_, bool>(
            r#"
            SELECT status = 'past_due'
                AND next_payment_attempt_at = next_renewal_at
            FROM billing_subscriptions
            WHERE id = $1
            "#,
        )
        .bind(recovery_subscription_id)
        .fetch_one(&database.pool)
        .await?;
        if !recovery_only_schedule_resumes {
            return Err(io::Error::other(
                "legacy recovery-only past-due state did not resume automatic dunning at its period anchor",
            )
            .into());
        }
        let due = crate::renewal::due_renewals(&database.pool).await?;
        if !due.iter().any(|dispatch| {
            dispatch.subscription_id().as_uuid() == &reclassified_subscription_id
        }) {
            return Err(io::Error::other(
                "reclassified legacy subscription did not become due under v2 policy",
            )
            .into());
        }
        if !due.iter().any(|dispatch| {
            dispatch.subscription_id().as_uuid() == &recovery_subscription_id
        }) {
            return Err(io::Error::other(
                "legacy recovery-only past-due subscription did not become due under v2 policy",
            )
            .into());
        }

        let first_v2_automatic_failure_id = insert_v1_terminal_subscription_attempt(
            &database.pool,
            gateway,
            recovery_subscriber_id,
            recovery_method_id,
            recovery_subscription_id,
            &recovery_initial_transaction_id,
            "subscription_renewal",
            "declined",
            "first-v2-failure-after-legacy-recovery",
            Some("2026-02-03 00:30:00+00"),
            "2026-02-03 00:31:00+00",
            None,
        )
        .await?;
        let mut transaction = database.pool.begin().await?;
        let first_v2_automatic_failure = crate::find_payment_attempt_by_id_in_transaction(
            &mut transaction,
            BillingScopeId::new(gateway.billing_scope_id),
            PaymentAttemptId::new(first_v2_automatic_failure_id),
        )
        .await?
        .ok_or_else(|| io::Error::other("first v2 automatic failure was not durable"))?;
        let failure_application =
            crate::renewal_failure::apply_resolved_automatic_renewal_failure(
                &mut transaction,
                &first_v2_automatic_failure,
            )
            .await?;
        transaction.commit().await?;
        if !matches!(
            failure_application,
            crate::renewal_failure::RenewalFailureApplication::Applied {
                disposition: RenewalFailureDisposition::RetryScheduled { .. },
                ..
            }
        ) {
            return Err(io::Error::other(
                "first v2 automatic failure did not advance legacy recovery-only dunning",
            )
            .into());
        }

        let cancel = CancelSubscription::new(
            BillingScopeId::new(gateway.billing_scope_id),
            SubscriberId::new(recovery_subscriber_id),
            PlanKey::new("base_subscription")?,
        );
        let mut transaction = database.pool.begin().await?;
        let cancellation =
            crate::cancel_subscription_in_transaction(&mut transaction, &cancel).await?;
        transaction.commit().await?;
        if !matches!(
            cancellation,
            CancelSubscriptionOutcome::Canceled {
                event: BillingEvent::SubscriptionCanceled { access_ends_at, .. },
                ..
            } if access_ends_at == recovery_failed_at
        ) {
            return Err(io::Error::other(
                "legacy recovery suspension timestamp was not preserved through cancellation",
            )
            .into());
        }

        for terminal_subscription_id in [exhausted_subscription_id, canceled_subscription_id] {
            let has_no_payment_time = sqlx::query_scalar::<_, bool>(
                "SELECT next_payment_attempt_at IS NULL FROM billing_subscriptions WHERE id = $1",
            )
            .bind(terminal_subscription_id)
            .fetch_one(&database.pool)
            .await?;
            if !has_no_payment_time {
                return Err(io::Error::other(format!(
                    "legacy subscription {terminal_subscription_id} retained a payment time"
                ))
                .into());
            }
        }
        let unpaid_count = sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM billing_subscriptions WHERE status = 'unpaid'",
        )
        .fetch_one(&database.pool)
        .await?;
        if unpaid_count != 0 {
            return Err(io::Error::other("upgrade terminalized a legacy row as unpaid").into());
        }

        for (attempt_id, expected_base, expected_fingerprint) in [
            (full_price_attempt_id, 100, full_price_fingerprint),
            (discounted_attempt_id, 100, discounted_fingerprint),
        ] {
            let terms_are_legacy = sqlx::query_scalar::<_, bool>(
                r#"
                SELECT subscription_initial_terms_version = 1
                    AND subscription_initial_start_kind = 'recurring_immediately'
                    AND subscription_initial_trial_amount_cents IS NULL
                    AND subscription_initial_recurring_base_amount_cents = $2
                    AND subscription_initial_recurring_period_kind = 'calendar_months'
                    AND subscription_initial_recurring_period_count = 1
                    AND subscription_initial_dunning_retry_delays_seconds =
                        ARRAY[86400, 86400, 86400, 86400]::bigint[]
                    AND subscription_initial_dunning_exhaustion = 'remain_past_due'
                    AND subscription_initial_past_due_access = 'suspend_immediately'
                    AND request_fingerprint = $3
                FROM billing_payment_attempts
                WHERE id = $1
                "#,
            )
            .bind(attempt_id)
            .bind(expected_base)
            .bind(expected_fingerprint)
            .fetch_one(&database.pool)
            .await?;
            if !terms_are_legacy {
                return Err(io::Error::other(format!(
                    "legacy initial attempt {attempt_id} backfill drifted"
                ))
                .into());
            }
        }

        let terminal_terms_are_null = sqlx::query_scalar::<_, bool>(
            r#"
            SELECT subscription_initial_terms_version IS NULL
                AND subscription_initial_start_kind IS NULL
                AND subscription_initial_dunning_retry_delays_seconds IS NULL
            FROM billing_payment_attempts
            WHERE id = $1
            "#,
        )
        .bind(customer_attempt_id)
        .fetch_one(&database.pool)
        .await?;
        if !terminal_terms_are_null {
            return Err(io::Error::other(
                "upgrade populated initial-only terms on a renewal attempt",
            )
            .into());
        }
        let recovery_terms_are_null = sqlx::query_scalar::<_, bool>(
            r#"
            SELECT subscription_initial_terms_version IS NULL
                AND subscription_initial_start_kind IS NULL
                AND subscription_initial_dunning_retry_delays_seconds IS NULL
            FROM billing_payment_attempts
            WHERE id = $1
            "#,
        )
        .bind(recovery_attempt_id)
        .fetch_one(&database.pool)
        .await?;
        if !recovery_terms_are_null {
            return Err(io::Error::other(
                "upgrade populated initial-only terms on a legacy recovery attempt",
            )
            .into());
        }

        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}
