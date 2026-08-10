use super::*;

#[derive(Default)]
struct ExactHostRelease {
    calls: AtomicU64,
}

#[async_trait]
impl ExternalReversalHostStore for ExactHostRelease {
    async fn release(
        &self,
        connection: &mut PgConnection,
        release: ExternalReversalHostChargeRelease,
    ) -> Result<ExternalReversalHostTransitionOutcome, ExternalReversalHostStoreError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let changed = sqlx::query(
            r#"
                UPDATE host_targets SET released = true
                WHERE id = $1 AND billing_scope_id = $2 AND subscriber_id = $3
                    AND released = false
                "#,
        )
        .bind(release.target_id().as_uuid())
        .bind(release.billing_scope_id().as_uuid())
        .bind(release.subscriber_id().as_uuid())
        .execute(connection)
        .await
        .map_err(ExternalReversalHostStoreError::new)?
        .rows_affected()
            == 1;
        Ok(if changed {
            ExternalReversalHostTransitionOutcome::Changed
        } else {
            ExternalReversalHostTransitionOutcome::Unchanged
        })
    }
}

#[tokio::test]
async fn external_reversal_is_exact_atomic_replayable_and_conflict_safe()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("rail_operator").await?;
    let account = create_gateway_account(&database.pool, "nmi").await?;
    let attempt_id = Uuid::now_v7();
    let charge_id = Uuid::now_v7();
    let additional_charge_id = Uuid::now_v7();
    let subscriber_id = Uuid::now_v7();
    let target_id = Uuid::now_v7();
    let order_id = format!("ck_{}", attempt_id.simple());
    sqlx::query(
        r#"
            CREATE TABLE host_targets (
                id uuid PRIMARY KEY, billing_scope_id uuid NOT NULL,
                subscriber_id uuid NOT NULL, released boolean NOT NULL DEFAULT false
            )
            "#,
    )
    .execute(&database.pool)
    .await?;
    sqlx::query(
        "INSERT INTO host_targets (id, billing_scope_id, subscriber_id) VALUES ($1, $2, $3)",
    )
    .bind(target_id)
    .bind(account.billing_scope_id)
    .bind(subscriber_id)
    .execute(&database.pool)
    .await?;
    sqlx::query(
        r#"
            INSERT INTO billing_payment_attempts (
                id, billing_scope_id, subscriber_id, host_charge_target_id,
                attempt_kind, status, idempotency_key, request_fingerprint,
                amount_cents, currency, gateway_account_id,
                gateway_configuration_id, gateway_order_id, review_required_at
            ) VALUES (
                $1, $2, $3, $4, 'host_charge', 'review_required', $5, $6,
                500, 'USD', $7, $8, $9, clock_timestamp()
            )
            "#,
    )
    .bind(attempt_id)
    .bind(account.billing_scope_id)
    .bind(subscriber_id)
    .bind(target_id)
    .bind(format!("idem-{attempt_id}"))
    .bind(format!("fingerprint-{attempt_id}"))
    .bind(account.gateway_account_id)
    .bind(account.gateway_configuration_id)
    .bind(&order_id)
    .execute(&database.pool)
    .await?;
    sqlx::query(
        r#"
            INSERT INTO billing_processor_charges (
                id, attempt_id, billing_scope_id, gateway_account_id,
                gateway_order_id, gateway_transaction_id, gateway_response,
                gateway_response_code, gateway_response_text, gateway_condition,
                charge_role, progression_state, observed_at, attempt_kind,
                host_charge_target_id, amount_cents, currency,
                external_reversal_required_at
            ) VALUES (
                $1, $2, $3, $4, $5, 'txn-operator', '1', '100', 'Approved',
                'complete', 'primary', 'external_reversal_required',
                clock_timestamp(), 'host_charge', $6, 500, 'USD', clock_timestamp()
            )
            "#,
    )
    .bind(charge_id)
    .bind(attempt_id)
    .bind(account.billing_scope_id)
    .bind(account.gateway_account_id)
    .bind(&order_id)
    .bind(target_id)
    .execute(&database.pool)
    .await?;
    sqlx::query(
        r#"
            INSERT INTO billing_processor_charges (
                id, attempt_id, billing_scope_id, gateway_account_id,
                gateway_order_id, gateway_transaction_id, gateway_response,
                gateway_response_code, gateway_response_text, gateway_condition,
                charge_role, progression_state, observed_at, attempt_kind,
                host_charge_target_id, amount_cents, currency,
                external_reversal_required_at
            ) VALUES (
                $1, $2, $3, $4, $5, 'txn-operator-additional', '1', '100',
                'Approved additional charge', 'complete', 'additional',
                'external_reversal_required', clock_timestamp(), 'host_charge',
                $6, 500, 'USD', clock_timestamp()
            )
            "#,
    )
    .bind(additional_charge_id)
    .bind(attempt_id)
    .bind(account.billing_scope_id)
    .bind(account.gateway_account_id)
    .bind(&order_id)
    .bind(target_id)
    .execute(&database.pool)
    .await?;

    let page_limit = OperatorReviewPageLimit::new(1)?;
    let attempt_page = attempt_review_page(&database.pool, page_limit, None).await?;
    assert!(attempt_page.into_items().is_empty());
    let first_charge_page = processor_charge_review_page(&database.pool, page_limit, None).await?;
    let next_cursor = first_charge_page.next_cursor().expect("second charge page");
    let first_charge_items = first_charge_page.into_items();
    assert_eq!(first_charge_items.len(), 1);
    let second_charge_page =
        processor_charge_review_page(&database.pool, page_limit, Some(next_cursor)).await?;
    assert!(second_charge_page.next_cursor().is_none());
    let second_charge_items = second_charge_page.into_items();
    assert_eq!(second_charge_items.len(), 1);
    let returned_charge_ids = [
        first_charge_items[0].charge().id(),
        second_charge_items[0].charge().id(),
    ];
    assert!(returned_charge_ids.contains(&ProcessorChargeId::new(charge_id)));
    assert!(returned_charge_ids.contains(&ProcessorChargeId::new(additional_charge_id)));
    assert!(
            first_charge_items
                .iter()
                .chain(&second_charge_items)
                .all(|item| item.attempt().identity().attempt_id()
                    == PaymentAttemptId::new(attempt_id))
        );

    let mut preflight = database.pool.begin().await?;
    let locator = charge_locator(&mut preflight, ProcessorChargeId::new(charge_id))
        .await?
        .expect("charge locator");
    lock_payment_attempt_by_id_on_connection(
        &mut preflight,
        locator.billing_scope_id,
        locator.attempt_id,
    )
    .await
    .expect("attempt parser")
    .expect("attempt exists");
    lock_processor_charge(&mut preflight, ProcessorChargeId::new(charge_id))
        .await
        .expect("charge parser")
        .expect("charge parser");
    preflight.rollback().await?;

    let host = ExactHostRelease::default();
    let actor = ActorId::new(Uuid::now_v7());
    let reason = ExternalReversalReason::new("processor refund verified")?;
    let mismatch = GatewayTransactionId::new("txn-other")?;
    assert_eq!(
        attest_external_reversal(
            &database.pool,
            &host,
            ProcessorChargeId::new(charge_id),
            actor,
            ExternalReversalKind::Refund,
            &mismatch,
            &reason,
        )
        .await?,
        ExternalReversalAttestationOutcome::Ineligible
    );
    let transaction_id = GatewayTransactionId::new("txn-operator")?;
    let attested = attest_external_reversal(
        &database.pool,
        &host,
        ProcessorChargeId::new(charge_id),
        actor,
        ExternalReversalKind::Refund,
        &transaction_id,
        &reason,
    )
    .await?;
    let ExternalReversalAttestationOutcome::Attested {
        attempt,
        attestation,
    } = attested
    else {
        panic!("expected attestation");
    };
    assert_eq!(attempt.status(), PaymentAttemptStatus::Failed);
    assert_eq!(attestation.actor_id(), actor);
    assert_eq!(host.calls.load(Ordering::SeqCst), 1);
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT released FROM host_targets WHERE id = $1")
            .bind(target_id)
            .fetch_one(&database.pool)
            .await?
    );

    assert!(matches!(
        attest_external_reversal(
            &database.pool,
            &host,
            ProcessorChargeId::new(charge_id),
            actor,
            ExternalReversalKind::Refund,
            &transaction_id,
            &reason,
        )
        .await?,
        ExternalReversalAttestationOutcome::Replayed { .. }
    ));
    assert_eq!(host.calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        attest_external_reversal(
            &database.pool,
            &host,
            ProcessorChargeId::new(charge_id),
            ActorId::new(Uuid::now_v7()),
            ExternalReversalKind::Refund,
            &transaction_id,
            &reason,
        )
        .await?,
        ExternalReversalAttestationOutcome::ReplayConflict
    );
    let counts: (i64, String) = sqlx::query_as(
            "SELECT COUNT(*)::bigint, MIN(progression_state) FROM billing_processor_charges WHERE id = $1",
        ).bind(charge_id).fetch_one(&database.pool).await?;
    assert_eq!(counts, (1, "externally_reversed".to_owned()));

    database.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn grant_conflict_replay_uses_the_persisted_prior_charge_classification()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("rail_op_grant").await?;
    let account = create_gateway_account(&database.pool, "nmi").await?;
    let attempt_id = Uuid::now_v7();
    let charge_id = Uuid::now_v7();
    let subscriber_id = Uuid::now_v7();
    let order_id = format!("subscription_{}", attempt_id.simple());
    sqlx::query(
        r#"
            INSERT INTO billing_payment_attempts (
                id, billing_scope_id, subscriber_id, plan_key,
                attempt_kind, status, idempotency_key, request_fingerprint,
                amount_cents, currency, gateway_account_id,
                gateway_configuration_id, gateway_order_id,
                gateway_transaction_id, gateway_response, gateway_response_code,
                gateway_response_text, gateway_condition, resolution_code,
                submitted_at, review_required_at,
                subscription_initial_terms_version,
                subscription_initial_start_kind,
                subscription_initial_recurring_base_amount_cents,
                subscription_initial_recurring_period_kind,
                subscription_initial_recurring_period_count,
                subscription_initial_dunning_retry_delays_seconds,
                subscription_initial_dunning_exhaustion,
                subscription_initial_past_due_access
            ) VALUES (
                $1, $2, $3, 'base', 'subscription_initial', 'review_required',
                $4, $5, 500, 'USD', $6, $7, $8, 'txn-grant-conflict',
                '1', '100', 'Approved', 'complete',
                'subscription_initial_current_grant_conflict',
                clock_timestamp(), clock_timestamp(), 2, 'recurring_immediately',
                500, 'calendar_months', 1, ARRAY[]::bigint[],
                'remain_past_due', 'suspend_immediately'
            )
            "#,
    )
    .bind(attempt_id)
    .bind(account.billing_scope_id)
    .bind(subscriber_id)
    .bind(format!("idem-{attempt_id}"))
    .bind(format!("fingerprint-{attempt_id}"))
    .bind(account.gateway_account_id)
    .bind(account.gateway_configuration_id)
    .bind(&order_id)
    .execute(&database.pool)
    .await?;
    sqlx::query(
        r#"
            INSERT INTO billing_processor_charges (
                id, attempt_id, billing_scope_id, gateway_account_id,
                gateway_order_id, gateway_transaction_id, gateway_response,
                gateway_response_code, gateway_response_text, gateway_condition,
                charge_role, progression_state, state_code, observed_at,
                attempt_kind, plan_key, amount_cents, currency,
                external_reversal_required_at
            ) VALUES (
                $1, $2, $3, $4, $5, 'txn-grant-conflict', '1', '100',
                'Approved', 'complete', 'primary', 'external_reversal_required',
                'processor_charge_external_reversal_required', clock_timestamp(),
                'subscription_initial', 'base', 500, 'USD', clock_timestamp()
            )
            "#,
    )
    .bind(charge_id)
    .bind(attempt_id)
    .bind(account.billing_scope_id)
    .bind(account.gateway_account_id)
    .bind(&order_id)
    .execute(&database.pool)
    .await?;

    let host = ExactHostRelease::default();
    let actor = ActorId::new(Uuid::now_v7());
    let reason = ExternalReversalReason::new("processor refund verified")?;
    let transaction_id = GatewayTransactionId::new("txn-grant-conflict")?;
    let attested = attest_external_reversal(
        &database.pool,
        &host,
        ProcessorChargeId::new(charge_id),
        actor,
        ExternalReversalKind::Refund,
        &transaction_id,
        &reason,
    )
    .await?;
    assert!(matches!(
        attested,
        ExternalReversalAttestationOutcome::Attested { .. }
    ));
    let state_code: String =
        sqlx::query_scalar("SELECT state_code FROM billing_processor_charges WHERE id = $1")
            .bind(charge_id)
            .fetch_one(&database.pool)
            .await?;
    assert_eq!(
        state_code,
        PaymentResolutionCode::SubscriptionInitialCurrentGrantConflict.as_str()
    );
    assert!(matches!(
        attest_external_reversal(
            &database.pool,
            &host,
            ProcessorChargeId::new(charge_id),
            actor,
            ExternalReversalKind::Refund,
            &transaction_id,
            &reason,
        )
        .await?,
        ExternalReversalAttestationOutcome::Replayed { .. }
    ));

    database.cleanup().await?;
    Ok(())
}
