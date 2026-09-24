use super::*;

async fn retry_policy(database: &TestDatabase, subscription_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE billing_subscriptions SET dunning_retry_delays_seconds = ARRAY[86400, 259200]::bigint[], \
         dunning_exhaustion = 'mark_unpaid', past_due_access = 'continue_until_dunning_exhausted' \
         WHERE id = $1",
    )
    .bind(subscription_id)
    .execute(&database.pool)
    .await?;
    Ok(())
}

#[tokio::test]
async fn immediate_renewal_retry_is_due_atomic_and_concurrent_replay_safe()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("rail_retry_now").await?;
    let result = async {
        let account = create_gateway_account(&database.pool, "nmi").await?;
        let (subscription_id, attempt_id) =
            insert_review_renewal(&database, &account, Uuid::now_v7(), "retry-now").await?;
        retry_policy(&database, subscription_id).await?;
        let events = Arc::new(Mutex::new(Vec::new()));
        let coordinator = TestCoordinator {
            pool: database.pool.clone(),
            events: Arc::clone(&events),
            fail_event: false,
        };
        let before = payment_attempt_by_id_for_test(&database, account.billing_scope_id, attempt_id).await?;
        let before_clock: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
            .fetch_one(&database.pool).await?;
        let (first, concurrent) = tokio::join!(
            fail_review_required_renewal_for_retry(&database.pool, &coordinator, PaymentAttemptId::new(attempt_id)),
            fail_review_required_renewal_for_retry(&database.pool, &coordinator, PaymentAttemptId::new(attempt_id)),
        );
        let outcomes = [first?, concurrent?];
        assert_eq!(outcomes.iter().filter(|outcome| matches!(outcome, ManualAttemptFailureOutcome::Failed(_))).count(), 1);
        assert_eq!(outcomes.iter().filter(|outcome| matches!(outcome, ManualAttemptFailureOutcome::KeptOpen(_))).count(), 1);
        let after_clock: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
            .fetch_one(&database.pool).await?;
        let (status, anchor, retry_at): (String, DateTime<Utc>, DateTime<Utc>) = sqlx::query_as(
            "SELECT status, next_renewal_at, next_payment_attempt_at FROM billing_subscriptions WHERE id = $1",
        ).bind(subscription_id).fetch_one(&database.pool).await?;
        assert_eq!(status, "past_due");
        assert_eq!(anchor, *before.request().target().period().unwrap().start_at());
        assert!(retry_at >= before_clock && retry_at <= after_clock);
        let after = payment_attempt_by_id_for_test(&database, account.billing_scope_id, attempt_id).await?;
        assert_eq!(after.identity(), before.identity());
        assert_eq!(after.request(), before.request());
        assert_eq!(after.state().timestamps().submitted_at(), before.state().timestamps().submitted_at());
        assert_eq!(after.state().timestamps().review_required_at(), before.state().timestamps().review_required_at());
        assert_eq!(after.status(), PaymentAttemptStatus::Failed);
        assert_eq!(after.state().resolution_code(), None);
        let dispatches = crate::due_renewals(&database.pool).await?;
        assert_eq!(dispatches.len(), 1);
        assert_eq!(dispatches[0].subscription_id().into_uuid(), subscription_id);
        assert_eq!(dispatches[0].attempt_sequence_count(), 1);
        assert!(matches!(events.lock().await.as_slice(), [BillingEvent::SubscriptionPaymentFailed {
            attempt_id: event_attempt, disposition: syrup_rail::SubscriptionPaymentFailureDisposition::RetryScheduled { retry_at: event_retry },
            access: syrup_rail::SubscriptionPaymentFailureAccess::ContinuesDuringDunning, ..
        }] if event_attempt.into_uuid() == attempt_id && *event_retry == retry_at));
        assert!(matches!(fail_review_required_renewal_for_retry(&database.pool, &coordinator, PaymentAttemptId::new(attempt_id)).await?, ManualAttemptFailureOutcome::KeptOpen(_)));
        let unchanged: DateTime<Utc> = sqlx::query_scalar("SELECT next_payment_attempt_at FROM billing_subscriptions WHERE id = $1")
            .bind(subscription_id).fetch_one(&database.pool).await?;
        assert_eq!(unchanged, retry_at);
        assert_eq!(events.lock().await.len(), 1);

        // Model a submitted renewal and apply a reconciled processor decline.
        // It must consume the second, unmodified dunning delay.
        let next_id = Uuid::now_v7();
        let target = before.request().target();
        let fingerprint = syrup_rail::PaymentAttemptFingerprint::for_subscription_renewal(
            target.plan_key().unwrap(), target.subscription_id().unwrap(),
            target.payment_method_id().unwrap(), anchor, before.request().amount(),
        );
        sqlx::query(r#"
            INSERT INTO billing_payment_attempts (
                id, billing_scope_id, subscriber_id, plan_key, subscription_id,
                payment_method_id, attempt_kind, status, idempotency_key,
                request_fingerprint, amount_cents, currency, billing_period_start_at,
                billing_period_end_at, gateway_account_id, gateway_configuration_id,
                gateway_order_id, submitted_at, review_required_at,
                subscription_expected_payment_method_id,
                subscription_expected_initial_transaction_id, subscription_expected_status,
                required_gateway_account_mode
            ) SELECT $2, billing_scope_id, subscriber_id, plan_key, subscription_id,
                payment_method_id, attempt_kind, 'review_required', 'next-retry',
                $3, amount_cents, currency, billing_period_start_at,
                billing_period_end_at, gateway_account_id, gateway_configuration_id,
                'next-retry-order', clock_timestamp(), clock_timestamp(),
                subscription_expected_payment_method_id,
                subscription_expected_initial_transaction_id, 'past_due', required_gateway_account_mode
            FROM billing_payment_attempts WHERE id = $1
        "#).bind(attempt_id).bind(next_id).bind(fingerprint.expose()).execute(&database.pool).await?;
        let decline = syrup_rail::GatewayPaymentOutcome::new(
            syrup_rail::GatewayPaymentStatus::Declined,
            ProcessorEvidence::new(
                None, None, Some(GatewayDiagnostic::new("2")),
                Some(GatewayDiagnostic::new("200")),
                Some(GatewayDiagnostic::new("Declined")), None,
                GatewayPaymentDescriptor::default(),
            ),
        );
        let declined = crate::apply_reconciled_subscription_renewal_gateway_outcome(
            &database.pool, &coordinator, BillingScopeId::new(account.billing_scope_id),
            PaymentAttemptId::new(next_id), &decline,
        ).await?;
        assert_eq!(declined.attempt().status(), PaymentAttemptStatus::Declined);
        let (next_retry, next_resolved): (DateTime<Utc>, DateTime<Utc>) = sqlx::query_as(
            "SELECT subscriptions.next_payment_attempt_at, attempts.resolved_at FROM billing_subscriptions subscriptions \
             JOIN billing_payment_attempts attempts ON attempts.subscription_id = subscriptions.id WHERE attempts.id = $1",
        ).bind(next_id).fetch_one(&database.pool).await?;
        assert_eq!(next_retry, next_resolved + chrono::Duration::days(3));
        assert!(crate::due_renewals(&database.pool).await?.is_empty());
        Ok::<_, Box<dyn Error>>(())
    }.await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

async fn payment_attempt_by_id_for_test(
    database: &TestDatabase,
    scope_id: Uuid,
    attempt_id: Uuid,
) -> Result<PaymentAttempt, Box<dyn Error>> {
    let mut transaction = database.pool.begin().await?;
    let attempt = crate::find_payment_attempt_by_id_in_transaction(
        &mut transaction,
        BillingScopeId::new(scope_id),
        PaymentAttemptId::new(attempt_id),
    )
    .await?
    .expect("fixture attempt exists");
    transaction.commit().await?;
    Ok(attempt)
}

#[tokio::test]
async fn immediate_renewal_retry_rolls_back_when_host_event_fails() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("rail_retry_rb").await?;
    let result = async {
        let account = create_gateway_account(&database.pool, "nmi").await?;
        let (subscription_id, attempt_id) = insert_review_renewal(&database, &account, Uuid::now_v7(), "rollback").await?;
        retry_policy(&database, subscription_id).await?;
        let coordinator = TestCoordinator { pool: database.pool.clone(), events: Arc::new(Mutex::new(Vec::new())), fail_event: true };
        let before = payment_attempt_by_id_for_test(&database, account.billing_scope_id, attempt_id).await?;
        assert!(fail_review_required_renewal_for_retry(&database.pool, &coordinator, PaymentAttemptId::new(attempt_id)).await.is_err());
        let after = payment_attempt_by_id_for_test(&database, account.billing_scope_id, attempt_id).await?;
        assert_eq!(before, after);
        let unchanged: (String, bool) = sqlx::query_as("SELECT status, next_payment_attempt_at = next_renewal_at FROM billing_subscriptions WHERE id = $1")
            .bind(subscription_id).fetch_one(&database.pool).await?;
        assert_eq!(unchanged, ("active".to_owned(), true));
        assert!(crate::due_renewals(&database.pool).await?.is_empty());
        Ok::<_, Box<dyn Error>>(())
    }.await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn immediate_renewal_retry_keeps_unsafe_or_ineligible_attempts_open()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("rail_retry_no").await?;
    let result = async {
        let account = create_gateway_account(&database.pool, "nmi").await?;
        let events = Arc::new(Mutex::new(Vec::new()));
        let coordinator = TestCoordinator { pool: database.pool.clone(), events: Arc::clone(&events), fail_event: false };
        for (suffix, update) in [
            ("charge-only", "INSERT INTO billing_processor_charges (id, attempt_id, billing_scope_id, gateway_account_id, gateway_order_id, gateway_transaction_id, charge_role, progression_state, observed_at, attempt_kind, plan_key, amount_cents, currency) SELECT gen_random_uuid(), id, billing_scope_id, gateway_account_id, gateway_order_id, 'existing-charge', 'primary', 'pending', clock_timestamp(), attempt_kind, plan_key, amount_cents, currency FROM billing_payment_attempts WHERE id = $1"),
            ("processor-id", "UPDATE billing_payment_attempts SET gateway_transaction_id = 'existing-transaction' WHERE id = $1"),
            ("approval", "UPDATE billing_payment_attempts SET gateway_response = '1' WHERE id = $1"),
            ("unsubmitted", "UPDATE billing_payment_attempts SET submitted_at = NULL WHERE id = $1"),
            ("recovery", "UPDATE billing_payment_attempts SET attempt_kind = 'subscription_recovery' WHERE id = $1"),
            ("canceled", "UPDATE billing_subscriptions SET status = 'canceled', canceled_at = clock_timestamp(), next_payment_attempt_at = NULL WHERE id = (SELECT subscription_id FROM billing_payment_attempts WHERE id = $1)"),
            ("unpaid", "UPDATE billing_subscriptions SET status = 'unpaid', unpaid_at = clock_timestamp(), next_payment_attempt_at = NULL WHERE id = (SELECT subscription_id FROM billing_payment_attempts WHERE id = $1)"),
            ("exhausted", "UPDATE billing_subscriptions SET status = 'past_due', next_payment_attempt_at = NULL WHERE id = (SELECT subscription_id FROM billing_payment_attempts WHERE id = $1)"),
        ] {
            let (_, attempt_id) = insert_review_renewal(&database, &account, Uuid::now_v7(), suffix).await?;
            sqlx::query(update).bind(attempt_id).execute(&database.pool).await?;
            let before = payment_attempt_by_id_for_test(&database, account.billing_scope_id, attempt_id).await?;
            assert!(matches!(fail_review_required_renewal_for_retry(&database.pool, &coordinator, PaymentAttemptId::new(attempt_id)).await?, ManualAttemptFailureOutcome::KeptOpen(_)), "{suffix}");
            assert_eq!(payment_attempt_by_id_for_test(&database, account.billing_scope_id, attempt_id).await?, before, "{suffix}");
        }
        assert!(events.lock().await.is_empty());
        Ok::<_, Box<dyn Error>>(())
    }.await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn immediate_renewal_retry_does_not_override_policy_exhaustion() -> Result<(), Box<dyn Error>>
{
    let database = TestDatabase::start("rail_retry_end").await?;
    let result = async {
        let account = create_gateway_account(&database.pool, "nmi").await?;
        let events = Arc::new(Mutex::new(Vec::new()));
        let coordinator = TestCoordinator {
            pool: database.pool.clone(),
            events: Arc::clone(&events),
            fail_event: false,
        };
        for (policy, expected_status) in
            [("remain_past_due", "past_due"), ("mark_unpaid", "unpaid")]
        {
            let (subscription_id, attempt_id) =
                insert_review_renewal(&database, &account, Uuid::now_v7(), policy).await?;
            sqlx::query("UPDATE billing_subscriptions SET dunning_exhaustion = $2 WHERE id = $1")
                .bind(subscription_id)
                .bind(policy)
                .execute(&database.pool)
                .await?;
            assert!(matches!(
                fail_review_required_renewal_for_retry(
                    &database.pool,
                    &coordinator,
                    PaymentAttemptId::new(attempt_id)
                )
                .await?,
                ManualAttemptFailureOutcome::Failed(_)
            ));
            let (status, retry_at): (String, Option<DateTime<Utc>>) = sqlx::query_as(
                "SELECT status, next_payment_attempt_at FROM billing_subscriptions WHERE id = $1",
            )
            .bind(subscription_id)
            .fetch_one(&database.pool)
            .await?;
            assert_eq!(status, expected_status);
            assert_eq!(retry_at, None);
        }
        assert!(crate::due_renewals(&database.pool).await?.is_empty());
        assert_eq!(events.lock().await.len(), 3);
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}
