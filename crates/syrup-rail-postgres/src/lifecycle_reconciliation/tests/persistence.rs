#[tokio::test]
async fn staged_candidate_changed_after_selection_remains_staged_and_is_counted()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("life_staged_race").await?;
    let fixture = create_gateway_account(&database.pool, "nmi").await?;
    let account = lifecycle_account(fixture, "nmi");
    let host_targets = ExactHostTargets::default();
    let observed_at = Utc::now() - chrono::Duration::minutes(1);
    let staged = reconcile_gateway_transaction_reports(
        &database.pool,
        &host_targets,
        &account,
        vec![evidence(
            "txn-candidate-changed",
            GatewayLifecycleState::PendingSettlement,
            observed_at,
        )?],
    )
    .await?;
    assert_eq!(staged.staged(), 1);
    let attempt_id = insert_host_attempt(
        &database.pool,
        fixture,
        Uuid::now_v7(),
        Uuid::now_v7(),
        "txn-candidate-changed",
        1_000,
        observed_at,
    )
    .await?;

    let selected = actionable_pending(&database.pool, &account).await?;
    assert_eq!(selected.len(), 1);
    sqlx::query("UPDATE billing_payment_attempts SET status = 'failed' WHERE id = $1")
        .bind(attempt_id)
        .execute(&database.pool)
        .await?;
    let mut summary = GatewayLifecycleReconciliationSummary::default();
    for (pending_id, evidence) in selected {
        let application = apply_or_stage_evidence(
            &database.pool,
            &host_targets,
            &account,
            evidence,
            Some(pending_id),
        )
        .await?;
        summary.record_application(application);
    }
    assert_eq!(summary.applied(), 0);
    assert_eq!(summary.staged(), 0);
    assert_eq!(summary.quarantined(), 0);
    let pending_count: i64 = sqlx::query_scalar(
        r#"
            SELECT COUNT(*)
            FROM billing_gateway_lifecycle_pending_updates
            WHERE gateway_account_id = $1
                AND gateway_transaction_id = 'txn-candidate-changed'
            "#,
    )
    .bind(fixture.gateway_account_id)
    .fetch_one(&database.pool)
    .await?;
    assert_eq!(pending_count, 1);

    database.cleanup().await
}

#[tokio::test]
async fn expiry_cleanup_preserves_bounded_pass_counts_and_backlog_query_limits()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("life_pending").await?;
    let fixture = create_gateway_account(&database.pool, "nmi").await?;
    let account = lifecycle_account(fixture, "nmi");
    let host_targets = ExactHostTargets::default();
    sqlx::query(
        r#"
            INSERT INTO billing_gateway_lifecycle_pending_updates (
                billing_scope_id,
                gateway_account_id,
                gateway_transaction_id,
                gateway_lifecycle_status,
                first_seen_at,
                updated_at,
                expires_at
            )
            SELECT $1,
                $2,
                'expired-' || value::text,
                'unknown',
                now() - interval '8 days',
                now() - interval '8 days',
                now() - interval '1 day'
            FROM generate_series(1, 1001) value
            "#,
    )
    .bind(fixture.billing_scope_id)
    .bind(fixture.gateway_account_id)
    .execute(&database.pool)
    .await?;
    sqlx::query(
        r#"
            INSERT INTO billing_gateway_lifecycle_pending_updates (
                billing_scope_id,
                gateway_account_id,
                gateway_transaction_id,
                gateway_lifecycle_status
            )
            SELECT $1, $2, 'live-' || value::text, 'unknown'
            FROM generate_series(1, 4096) value
            "#,
    )
    .bind(fixture.billing_scope_id)
    .bind(fixture.gateway_account_id)
    .execute(&database.pool)
    .await?;
    sqlx::raw_sql(
        "ANALYZE billing_gateway_lifecycle_pending_updates; ANALYZE billing_payment_attempts;",
    )
    .execute(&database.pool)
    .await?;

    for (name, sql, limit) in [
        ("cleanup", cleanup_pending_sql(), PENDING_CLEANUP_BATCH_SIZE),
        (
            "actionable",
            actionable_pending_sql(),
            STAGED_APPLICATION_BATCH_SIZE,
        ),
    ] {
        let explain_sql = format!("EXPLAIN (FORMAT JSON, COSTS OFF) {sql}");
        let plan: serde_json::Value = sqlx::query_scalar(&explain_sql)
            .bind(fixture.billing_scope_id)
            .bind(fixture.gateway_account_id)
            .bind(limit)
            .fetch_one(&database.pool)
            .await?;
        let root = explain_plan_root(&plan)?;
        if !plan_has_node_type(root, "Limit") {
            return Err(io::Error::other(format!(
                "representative lifecycle {name} plan lost its bounded candidate limit:\n{}",
                serde_json::to_string_pretty(root)?
            ))
            .into());
        }
    }

    let first =
        apply_staged_gateway_lifecycle_evidence(&database.pool, &host_targets, &account).await?;
    assert_eq!(first.cleaned(), 1_000);
    assert_eq!(first.applied(), 0);
    let second =
        apply_staged_gateway_lifecycle_evidence(&database.pool, &host_targets, &account).await?;
    assert_eq!(second.cleaned(), 1);
    let third =
        apply_staged_gateway_lifecycle_evidence(&database.pool, &host_targets, &account).await?;
    assert_eq!(third.cleaned(), 0);
    let remaining: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM billing_gateway_lifecycle_pending_updates WHERE gateway_account_id = $1",
        )
        .bind(fixture.gateway_account_id)
        .fetch_one(&database.pool)
        .await?;
    assert_eq!(remaining, 4_096);

    database.cleanup().await
}

#[tokio::test]
async fn report_lifecycle_is_crash_safe_monotonic_and_exact_targeted() -> Result<(), Box<dyn Error>>
{
    let database = TestDatabase::start("rail_lifecycle").await?;
    let fixture = create_gateway_account(&database.pool, "nmi").await?;
    let account = lifecycle_account(fixture, "nmi");
    let host_targets = ExactHostTargets::default();
    sqlx::query(
        r#"
            CREATE TABLE host_charge_targets (
                id uuid PRIMARY KEY,
                billing_scope_id uuid NOT NULL,
                subscriber_id uuid NOT NULL,
                status text NOT NULL,
                reversal_kind text,
                paid_at timestamptz,
                reversed_at timestamptz
            )
            "#,
    )
    .execute(&database.pool)
    .await?;

    let staged_at = Utc::now() - chrono::Duration::minutes(3);
    let staged = reconcile_gateway_transaction_reports(
        &database.pool,
        &host_targets,
        &account,
        vec![evidence(
            "txn-late",
            GatewayLifecycleState::Settled {
                cumulative_refunded_cents: Some(CumulativeRefundCents::new(125)?),
            },
            staged_at,
        )?],
    )
    .await?;
    assert_eq!(staged.staged(), 1);
    let pending_state: (String, Option<i32>, String) = sqlx::query_as(
        r#"
            SELECT gateway_lifecycle_status, refunded_amount_cents,
                gateway_lifecycle_action
            FROM billing_gateway_lifecycle_pending_updates
            WHERE gateway_account_id = $1
            "#,
    )
    .bind(fixture.gateway_account_id)
    .fetch_one(&database.pool)
    .await?;
    assert_eq!(
        pending_state,
        (
            "settled".to_owned(),
            Some(125),
            "diagnostic action".to_owned()
        )
    );

    for _ in 0..25 {
        let no_match =
            apply_staged_gateway_lifecycle_evidence(&database.pool, &host_targets, &account)
                .await?;
        assert_eq!(no_match, GatewayLifecycleReconciliationSummary::default());
    }
    let inert_check_state: (i32, Option<DateTime<Utc>>) = sqlx::query_as(
        r#"
            SELECT check_count, last_checked_at
            FROM billing_gateway_lifecycle_pending_updates
            WHERE gateway_account_id = $1
            "#,
    )
    .bind(fixture.gateway_account_id)
    .fetch_one(&database.pool)
    .await?;
    assert_eq!(inert_check_state, (0, None));

    let late_subscriber = Uuid::now_v7();
    let late_target = Uuid::now_v7();
    insert_host_attempt(
        &database.pool,
        fixture,
        late_subscriber,
        late_target,
        "txn-late",
        1_000,
        staged_at - chrono::Duration::minutes(1),
    )
    .await?;
    let applied =
        apply_staged_gateway_lifecycle_evidence(&database.pool, &host_targets, &account).await?;
    assert_eq!(applied.applied(), 1);
    let stored: (String, i32, String) = sqlx::query_as(
        r#"
            SELECT gateway_lifecycle_status, refunded_amount_cents,
                gateway_lifecycle_action
            FROM billing_payment_attempts
            WHERE gateway_transaction_id = 'txn-late'
            "#,
    )
    .fetch_one(&database.pool)
    .await?;
    assert_eq!(
        stored,
        ("settled".to_owned(), 125, "diagnostic action".to_owned())
    );
    assert_eq!(host_targets.calls.load(Ordering::SeqCst), 0);

    let subscriber_id = Uuid::now_v7();
    let target_id = Uuid::now_v7();
    sqlx::query(
            "INSERT INTO host_charge_targets (id, billing_scope_id, subscriber_id, status) VALUES ($1, $2, $3, 'paid')",
        )
        .bind(target_id)
        .bind(fixture.billing_scope_id)
        .bind(subscriber_id)
        .execute(&database.pool)
        .await?;
    insert_host_attempt(
        &database.pool,
        fixture,
        subscriber_id,
        target_id,
        "txn-refund",
        2_500,
        staged_at,
    )
    .await?;
    let refunded_at = Utc::now();
    let report = evidence(
        "txn-refund",
        GatewayLifecycleState::Refunded {
            cumulative_refunded_cents: CumulativeRefundCents::new(2_500)?,
        },
        refunded_at,
    )?;
    let first = reconcile_gateway_transaction_reports(
        &database.pool,
        &host_targets,
        &account,
        vec![report.clone()],
    )
    .await?;
    let replay = reconcile_gateway_transaction_reports(
        &database.pool,
        &host_targets,
        &account,
        vec![report],
    )
    .await?;
    assert_eq!(first.applied(), 1);
    assert_eq!(replay.applied(), 0);
    assert_eq!(host_targets.calls.load(Ordering::SeqCst), 1);
    let target: (String, String, DateTime<Utc>) = sqlx::query_as(
        "SELECT status, reversal_kind, reversed_at FROM host_charge_targets WHERE id = $1",
    )
    .bind(target_id)
    .fetch_one(&database.pool)
    .await?;
    assert_eq!(target.0, "reversed");
    assert_eq!(target.1, "refunded");
    assert_eq!(target.2, postgres_timestamp_precision(refunded_at));

    let refused_subscriber_id = Uuid::now_v7();
    let refused_target_id = Uuid::now_v7();
    sqlx::query(
            "INSERT INTO host_charge_targets (id, billing_scope_id, subscriber_id, status) VALUES ($1, $2, $3, 'pending')",
        )
        .bind(refused_target_id)
        .bind(fixture.billing_scope_id)
        .bind(refused_subscriber_id)
        .execute(&database.pool)
        .await?;
    insert_host_attempt(
        &database.pool,
        fixture,
        refused_subscriber_id,
        refused_target_id,
        "txn-refund-refused",
        800,
        staged_at,
    )
    .await?;
    let refused_attempt_before: (Option<String>, Option<i32>) = sqlx::query_as(
            "SELECT gateway_lifecycle_status, refunded_amount_cents FROM billing_payment_attempts WHERE gateway_transaction_id = 'txn-refund-refused'",
        )
        .fetch_one(&database.pool)
        .await?;
    let following_subscriber_id = Uuid::now_v7();
    let following_target_id = Uuid::now_v7();
    sqlx::query(
            "INSERT INTO host_charge_targets (id, billing_scope_id, subscriber_id, status) VALUES ($1, $2, $3, 'paid')",
        )
        .bind(following_target_id)
        .bind(fixture.billing_scope_id)
        .bind(following_subscriber_id)
        .execute(&database.pool)
        .await?;
    insert_host_attempt(
        &database.pool,
        fixture,
        following_subscriber_id,
        following_target_id,
        "txn-refund-following",
        900,
        staged_at,
    )
    .await?;
    let refused_evidence = evidence(
        "txn-refund-refused",
        GatewayLifecycleState::Refunded {
            cumulative_refunded_cents: CumulativeRefundCents::new(800)?,
        },
        Utc::now(),
    )?;
    let summary = reconcile_gateway_transaction_reports(
        &database.pool,
        &host_targets,
        &account,
        vec![
            refused_evidence.clone(),
            evidence(
                "txn-refund-following",
                GatewayLifecycleState::Refunded {
                    cumulative_refunded_cents: CumulativeRefundCents::new(900)?,
                },
                Utc::now(),
            )?,
        ],
    )
    .await?;
    assert_eq!(summary.skipped(), 1);
    assert_eq!(summary.staged(), 1);
    assert_eq!(summary.applied(), 1);
    let redelivery = reconcile_gateway_transaction_reports(
        &database.pool,
        &host_targets,
        &account,
        vec![refused_evidence],
    )
    .await?;
    assert_eq!(redelivery.skipped(), 1);
    assert_eq!(redelivery.staged(), 0);
    assert_eq!(redelivery.applied(), 0);
    let refused_attempt_state: (Option<String>, Option<i32>) = sqlx::query_as(
            "SELECT gateway_lifecycle_status, refunded_amount_cents FROM billing_payment_attempts WHERE gateway_transaction_id = 'txn-refund-refused'",
        )
        .fetch_one(&database.pool)
        .await?;
    assert_eq!(refused_attempt_state, refused_attempt_before);
    let refused_pending_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM billing_gateway_lifecycle_pending_updates WHERE gateway_account_id = $1 AND gateway_transaction_id = 'txn-refund-refused'",
        )
        .bind(fixture.gateway_account_id)
        .fetch_one(&database.pool)
        .await?;
    assert_eq!(refused_pending_count, 1);
    let staged_retry =
        apply_staged_gateway_lifecycle_evidence(&database.pool, &host_targets, &account).await?;
    assert_eq!(staged_retry.skipped(), 1);
    assert_eq!(staged_retry.staged(), 0);
    let following_target_state: String =
        sqlx::query_scalar("SELECT status FROM host_charge_targets WHERE id = $1")
            .bind(following_target_id)
            .fetch_one(&database.pool)
            .await?;
    assert_eq!(following_target_state, "reversed");

    let cursor_key = GatewayLifecycleCursorKey::new("approved_lifecycle")?;
    let initial =
        gateway_lifecycle_reconciliation_start(&database.pool, &account, &cursor_key).await?;
    assert!(initial.is_some());
    let later = Utc::now() + chrono::Duration::minutes(5);
    save_gateway_lifecycle_reconciliation_cursor(&database.pool, &account, &cursor_key, later)
        .await?;
    save_gateway_lifecycle_reconciliation_cursor(&database.pool, &account, &cursor_key, staged_at)
        .await?;
    assert_eq!(
        gateway_lifecycle_reconciliation_start(&database.pool, &account, &cursor_key).await?,
        Some(postgres_timestamp_precision(later))
    );

    let wrong_provider = lifecycle_account(fixture, "other");
    assert!(matches!(
        gateway_lifecycle_reconciliation_start(&database.pool, &wrong_provider, &cursor_key).await,
        Err(GatewayLifecycleReconciliationError::AccountNotFound)
    ));

    database.cleanup().await?;
    Ok(())
}
