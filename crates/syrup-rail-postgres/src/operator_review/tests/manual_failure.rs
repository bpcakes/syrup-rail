use super::*;

#[tokio::test]
async fn manual_failure_uses_the_paid_trial_subscription_dunning_policy()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("rail_trial_rev").await?;
    let result = async {
        let account = create_gateway_account(&database.pool, "nmi").await?;
        let events = Arc::new(Mutex::new(Vec::new()));
        let coordinator = TestCoordinator {
            pool: database.pool.clone(),
            events: Arc::clone(&events),
            fail_event: false,
        };
        let subscriber_id = Uuid::now_v7();
        let (subscription_id, attempt_id) =
            insert_review_renewal(&database, &account, subscriber_id, "paid-trial").await?;
        sqlx::query(
            r#"
                UPDATE billing_subscriptions
                SET phase = 'paid_trial',
                    trial_amount_cents = 100,
                    trial_period_kind = 'fixed_days',
                    trial_period_count = 7,
                    dunning_retry_delays_seconds = ARRAY[60, 180]::bigint[],
                    dunning_exhaustion = 'mark_unpaid',
                    past_due_access = 'continue_until_dunning_exhausted'
                WHERE id = $1
                "#,
        )
        .bind(subscription_id)
        .execute(&database.pool)
        .await?;

        let outcome = fail_review_required_attempt(
            &database.pool,
            &coordinator,
            &ExactManualFailureHost,
            PaymentAttemptId::new(attempt_id),
        )
        .await?;
        assert!(matches!(outcome, ManualAttemptFailureOutcome::Failed(_)));

        let (status, phase, next_attempt_at, resolved_at): (
            String,
            String,
            DateTime<Utc>,
            DateTime<Utc>,
        ) = sqlx::query_as(
            r#"
                SELECT subscriptions.status, subscriptions.phase,
                    subscriptions.next_payment_attempt_at, attempts.resolved_at
                FROM billing_subscriptions AS subscriptions
                JOIN billing_payment_attempts AS attempts
                    ON attempts.subscription_id = subscriptions.id
                WHERE subscriptions.id = $1 AND attempts.id = $2
                "#,
        )
        .bind(subscription_id)
        .bind(attempt_id)
        .fetch_one(&database.pool)
        .await?;
        assert_eq!(status, "past_due");
        assert_eq!(phase, "paid_trial");
        assert_eq!(next_attempt_at, resolved_at + chrono::Duration::seconds(60));

        let recorded = events.lock().await;
        assert_eq!(recorded.len(), 1);
        assert!(matches!(
            &recorded[0],
            BillingEvent::SubscriptionPaymentFailed {
                attempt_id: event_attempt_id,
                disposition:
                    syrup_rail::SubscriptionPaymentFailureDisposition::RetryScheduled {
                        retry_at,
                    },
                ..
            } if *event_attempt_id == PaymentAttemptId::new(attempt_id)
                && *retry_at == next_attempt_at
        ));
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn manual_failure_of_preserved_active_recovery_does_not_consume_dunning()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("rail_rec_review").await?;
    let result = async {
            let account = create_gateway_account(&database.pool, "nmi").await?;
            let events = Arc::new(Mutex::new(Vec::new()));
            let coordinator = TestCoordinator {
                pool: database.pool.clone(),
                events: Arc::clone(&events),
                fail_event: false,
            };
            let subscriber_id = Uuid::now_v7();
            let (subscription_id, attempt_id) =
                insert_review_renewal(&database, &account, subscriber_id, "recovery").await?;

            // New recovery reservations require `past_due`, but v2 preserves an
            // already-durable v1 recovery whose expected subscription state was
            // `active`. Reclassify the fixture to exercise that cutover path.
            sqlx::query(
                "UPDATE billing_payment_attempts SET attempt_kind = 'subscription_recovery' WHERE id = $1",
            )
            .bind(attempt_id)
            .execute(&database.pool)
            .await?;

            let before: (String, DateTime<Utc>, Option<DateTime<Utc>>) = sqlx::query_as(
                "SELECT status, next_renewal_at, next_payment_attempt_at FROM billing_subscriptions WHERE id = $1",
            )
            .bind(subscription_id)
            .fetch_one(&database.pool)
            .await?;
            assert_eq!(before.0, "active");

            let outcome = fail_review_required_attempt(
                &database.pool,
                &coordinator,
                &ExactManualFailureHost,
                PaymentAttemptId::new(attempt_id),
            )
            .await?;
            let ManualAttemptFailureOutcome::Failed(attempt) = outcome else {
                panic!("preserved recovery should be manually failed");
            };
            assert_eq!(attempt.kind(), PaymentAttemptKind::SubscriptionRecovery);
            assert_eq!(attempt.status(), PaymentAttemptStatus::Failed);

            let after: (String, DateTime<Utc>, Option<DateTime<Utc>>) = sqlx::query_as(
                "SELECT status, next_renewal_at, next_payment_attempt_at FROM billing_subscriptions WHERE id = $1",
            )
            .bind(subscription_id)
            .fetch_one(&database.pool)
            .await?;
            assert_eq!(after, before);
            assert!(events.lock().await.is_empty());
            Ok::<_, Box<dyn Error>>(())
        }
        .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn manual_failure_is_policy_safe_atomic_eventful_and_host_exact() -> Result<(), Box<dyn Error>>
{
    let database = TestDatabase::start("rail_manual").await?;
    let account = create_gateway_account(&database.pool, "nmi").await?;
    sqlx::query(
        r#"
            CREATE TABLE manual_failure_host_targets (
                id uuid PRIMARY KEY,
                billing_scope_id uuid NOT NULL,
                subscriber_id uuid NOT NULL,
                status text NOT NULL
            )
            "#,
    )
    .execute(&database.pool)
    .await?;
    let events = Arc::new(Mutex::new(Vec::new()));
    let coordinator = TestCoordinator {
        pool: database.pool.clone(),
        events: Arc::clone(&events),
        fail_event: false,
    };
    let host = ExactManualFailureHost;

    let subscriber_id = Uuid::now_v7();
    let (subscription_id, renewal_attempt_id) =
        insert_review_renewal(&database, &account, subscriber_id, "success").await?;
    let outcome = fail_review_required_attempt(
        &database.pool,
        &coordinator,
        &host,
        PaymentAttemptId::new(renewal_attempt_id),
    )
    .await?;
    assert!(matches!(outcome, ManualAttemptFailureOutcome::Failed(_)));
    let (attempt_status, response_text, condition): (String, Option<String>, Option<String>) =
            sqlx::query_as(
                "SELECT status, gateway_response_text, gateway_condition FROM billing_payment_attempts WHERE id = $1",
            )
            .bind(renewal_attempt_id)
            .fetch_one(&database.pool)
            .await?;
    assert_eq!(attempt_status, "failed");
    assert_eq!(
        response_text.as_deref(),
        Some(syrup_rail::MANUAL_ATTEMPT_FAILURE_NOTE)
    );
    assert_eq!(condition.as_deref(), Some("failed"));
    let subscription_status: String =
        sqlx::query_scalar("SELECT status FROM billing_subscriptions WHERE id = $1")
            .bind(subscription_id)
            .fetch_one(&database.pool)
            .await?;
    assert_eq!(subscription_status, "past_due");
    let recorded_events = events.lock().await;
    assert_eq!(recorded_events.len(), 1);
    assert!(matches!(
        &recorded_events[0],
        BillingEvent::SubscriptionPaymentFailed { attempt_id, .. }
            if *attempt_id == PaymentAttemptId::new(renewal_attempt_id)
    ));
    drop(recorded_events);
    assert!(matches!(
        fail_review_required_attempt(
            &database.pool,
            &coordinator,
            &host,
            PaymentAttemptId::new(renewal_attempt_id),
        )
        .await?,
        ManualAttemptFailureOutcome::KeptOpen(_)
    ));
    assert_eq!(events.lock().await.len(), 1);

    let blocked_subscriber_id = Uuid::now_v7();
    let (blocked_subscription_id, blocked_attempt_id) =
        insert_review_renewal(&database, &account, blocked_subscriber_id, "blocked").await?;
    let failing_coordinator = TestCoordinator {
        pool: database.pool.clone(),
        events: Arc::new(Mutex::new(Vec::new())),
        fail_event: true,
    };
    assert!(
        fail_review_required_attempt(
            &database.pool,
            &failing_coordinator,
            &host,
            PaymentAttemptId::new(blocked_attempt_id),
        )
        .await
        .is_err()
    );
    let rolled_back: (String, String) = sqlx::query_as(
            "SELECT attempts.status, subscriptions.status FROM billing_payment_attempts attempts INNER JOIN billing_subscriptions subscriptions ON subscriptions.id = attempts.subscription_id WHERE attempts.id = $1",
        )
        .bind(blocked_attempt_id)
        .fetch_one(&database.pool)
        .await?;
    assert_eq!(
        rolled_back,
        ("review_required".to_owned(), "active".to_owned())
    );
    sqlx::query(
        r#"
            INSERT INTO billing_processor_charges (
                id, attempt_id, billing_scope_id, gateway_account_id,
                gateway_order_id, gateway_transaction_id, charge_role,
                progression_state, observed_at, attempt_kind, plan_key,
                amount_cents, currency
            ) SELECT
                $2, id, billing_scope_id, gateway_account_id, gateway_order_id,
                'txn-blocked', 'primary', 'pending', clock_timestamp(),
                attempt_kind, plan_key, amount_cents, currency
            FROM billing_payment_attempts WHERE id = $1
            "#,
    )
    .bind(blocked_attempt_id)
    .bind(Uuid::now_v7())
    .execute(&database.pool)
    .await?;
    assert!(matches!(
        fail_review_required_attempt(
            &database.pool,
            &coordinator,
            &host,
            PaymentAttemptId::new(blocked_attempt_id),
        )
        .await?,
        ManualAttemptFailureOutcome::KeptOpen(_)
    ));
    let blocked_subscription_status: String =
        sqlx::query_scalar("SELECT status FROM billing_subscriptions WHERE id = $1")
            .bind(blocked_subscription_id)
            .fetch_one(&database.pool)
            .await?;
    assert_eq!(blocked_subscription_status, "active");

    let host_subscriber_id = Uuid::now_v7();
    let host_target_id = Uuid::now_v7();
    let host_attempt_id = Uuid::now_v7();
    sqlx::query(
            "INSERT INTO manual_failure_host_targets (id, billing_scope_id, subscriber_id, status) VALUES ($1, $2, $3, 'pending')",
        )
        .bind(host_target_id)
        .bind(account.billing_scope_id)
        .bind(host_subscriber_id)
        .execute(&database.pool)
        .await?;
    sqlx::query(
        r#"
            INSERT INTO billing_payment_attempts (
                required_gateway_account_mode,
                id, billing_scope_id, subscriber_id, host_charge_target_id,
                attempt_kind, status, idempotency_key, request_fingerprint,
                amount_cents, currency, gateway_account_id,
                gateway_configuration_id, gateway_order_id, review_required_at
            ) VALUES (
                'live',
                $1, $2, $3, $4, 'host_charge', 'review_required', $5, $6,
                500, 'USD', $7, $8, $9, clock_timestamp()
            )
            "#,
    )
    .bind(host_attempt_id)
    .bind(account.billing_scope_id)
    .bind(host_subscriber_id)
    .bind(host_target_id)
    .bind(format!("idem-{host_attempt_id}"))
    .bind(format!("fingerprint-{host_attempt_id}"))
    .bind(account.gateway_account_id)
    .bind(account.gateway_configuration_id)
    .bind(format!("host-order-{host_attempt_id}"))
    .execute(&database.pool)
    .await?;
    assert!(matches!(
        fail_review_required_attempt(
            &database.pool,
            &coordinator,
            &host,
            PaymentAttemptId::new(host_attempt_id),
        )
        .await?,
        ManualAttemptFailureOutcome::Failed(_)
    ));
    let host_state: (String, String) = sqlx::query_as(
            "SELECT attempts.status, targets.status FROM billing_payment_attempts attempts INNER JOIN manual_failure_host_targets targets ON targets.id = attempts.host_charge_target_id WHERE attempts.id = $1",
        )
        .bind(host_attempt_id)
        .fetch_one(&database.pool)
        .await?;
    assert_eq!(
        host_state,
        ("failed".to_owned(), "payment_failed".to_owned())
    );
    Ok(())
}
