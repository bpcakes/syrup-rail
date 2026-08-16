use super::*;

#[tokio::test]
async fn active_grant_blocks_only_its_exact_plan() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("enroll_grant").await?;
    install_host_offers(&database).await?;
    let account = create_gateway_account(&database.pool, "nmi").await?;
    set_offer(&database, account.billing_scope_id, "basic", 1_000).await?;
    set_offer(&database, account.billing_scope_id, "premium", 2_000).await?;
    let subscriber_id = Uuid::now_v7();
    sqlx::query(
        r#"
            INSERT INTO billing_subscription_grants (
                id, billing_scope_id, subscriber_id, plan_key, grant_kind,
                reason, starts_at, ends_at, granted_by_actor_id
            ) VALUES (
                $1, $2, $3, 'basic', 'promotion', 'launch',
                clock_timestamp() - interval '1 minute',
                clock_timestamp() + interval '1 day', $4
            )
            "#,
    )
    .bind(Uuid::now_v7())
    .bind(account.billing_scope_id)
    .bind(subscriber_id)
    .bind(Uuid::now_v7())
    .execute(&database.pool)
    .await?;
    let gateway = resolved_gateway(account);

    let basic = SubscriptionEnrollmentReservation::from_command(
        &enrollment_command(
            account,
            subscriber_id,
            Uuid::now_v7(),
            "basic-grant",
            full_price("basic", 1_000),
        ),
        &gateway,
    )?;
    let mut transaction = database.pool.begin().await?;
    assert_eq!(
        reserve_subscription_enrollment_in_transaction(&mut transaction, &TestOfferStore, &basic,)
            .await?,
        SubscriptionEnrollmentReservationOutcome::Rejected(
            SubscriptionEnrollmentReservationRejection::ActiveGrant,
        )
    );
    transaction.rollback().await?;

    let premium = SubscriptionEnrollmentReservation::from_command(
        &enrollment_command(
            account,
            subscriber_id,
            Uuid::now_v7(),
            "premium-with-basic-grant",
            full_price("premium", 2_000),
        ),
        &gateway,
    )?;
    let mut transaction = database.pool.begin().await?;
    assert!(matches!(
            reserve_subscription_enrollment_in_transaction(
                &mut transaction,
                &TestOfferStore,
                &premium,
            )
            .await?,
            SubscriptionEnrollmentReservationOutcome::Reserved(_)
        ));
    transaction.commit().await?;
    database.cleanup().await
}

#[tokio::test]
async fn gateway_configuration_rotation_rejects_prepared_submission() -> Result<(), Box<dyn Error>>
{
    let database = TestDatabase::start("enroll_rotate").await?;
    install_host_offers(&database).await?;
    let account = create_gateway_account(&database.pool, "nmi").await?;
    let plan_key = "base_subscription";
    set_offer(&database, account.billing_scope_id, plan_key, 1_000).await?;
    let gateway = resolved_gateway(account);
    let reservation = SubscriptionEnrollmentReservation::from_command(
        &enrollment_command(
            account,
            Uuid::now_v7(),
            Uuid::now_v7(),
            "rotated-configuration",
            full_price(plan_key, 1_000),
        ),
        &gateway,
    )?;
    let mut transaction = database.pool.begin().await?;
    assert!(matches!(
        reserve_subscription_enrollment_in_transaction(
            &mut transaction,
            &TestOfferStore,
            &reservation,
        )
        .await?,
        SubscriptionEnrollmentReservationOutcome::Reserved(_)
    ));
    transaction.commit().await?;

    sqlx::query(
            "UPDATE billing_gateway_accounts SET gateway_configuration_id = $2, updated_at = clock_timestamp() WHERE id = $1",
        )
        .bind(account.gateway_account_id)
        .bind(Uuid::now_v7())
        .execute(&database.pool)
        .await?;
    let mut transaction = database.pool.begin().await?;
    let rejected = admit_subscription_enrollment_submission_in_transaction(
        &mut transaction,
        &TestOfferStore,
        &reservation,
    )
    .await?;
    assert!(matches!(
        rejected,
        SubscriptionEnrollmentSubmissionOutcome::Rejected {
            ref attempt,
            reason: SubscriptionEnrollmentSubmissionRejection::GatewayConfigurationChanged,
        } if attempt.status() == PaymentAttemptStatus::Failed
            && attempt.state().timestamps().submitted_at().is_none()
    ));
    transaction.commit().await?;
    database.cleanup().await
}

#[tokio::test]
async fn loaders_preserve_exact_scope_and_redact_durable_values() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("attempt_owner").await?;
    let account = create_gateway_account(&database.pool, "nmi").await?;
    let attempt_id = Uuid::now_v7();
    let subscriber_id = Uuid::now_v7();
    sqlx::query(
        r#"
            INSERT INTO billing_payment_attempts (
                id, billing_scope_id, subscriber_id, host_charge_target_id,
                attempt_kind, status, idempotency_key, request_fingerprint,
                amount_cents, currency, gateway_account_id,
                gateway_configuration_id, gateway_order_id,
                gateway_transaction_id, gateway_payment_method_reference,
                gateway_response, gateway_response_code, gateway_response_text,
                gateway_condition, billing_first_name, billing_last_name,
                billing_email
            ) VALUES (
                $1, $2, $3, $4, 'host_charge', 'pending', $5, $6,
                1000, 'USD', $7, $8, $9, $10, $11, $12, $13, $14,
                $15, $16, $17, $18
            )
            "#,
    )
    .bind(attempt_id)
    .bind(account.billing_scope_id)
    .bind(subscriber_id)
    .bind(Uuid::now_v7())
    .bind("idempotency-secret")
    .bind("fingerprint-secret")
    .bind(account.gateway_account_id)
    .bind(account.gateway_configuration_id)
    .bind("order-secret")
    .bind("transaction-secret")
    .bind("method-secret")
    .bind("response-secret")
    .bind("code-secret")
    .bind("text-secret")
    .bind("condition-secret")
    .bind("Sensitive")
    .bind("Name")
    .bind("secret@example.test")
    .execute(&database.pool)
    .await?;

    let mut transaction = database.pool.begin().await?;
    assert!(
        find_payment_attempt_by_id_in_transaction(
            &mut transaction,
            BillingScopeId::new(Uuid::now_v7()),
            PaymentAttemptId::new(attempt_id),
        )
        .await?
        .is_none()
    );
    let attempt = lock_payment_attempt_by_idempotency_in_transaction(
        &mut transaction,
        BillingScopeId::new(account.billing_scope_id),
        SubscriberId::new(subscriber_id),
        &IdempotencyKey::new("idempotency-secret")?,
    )
    .await?
    .expect("exact owner row should load");
    assert_eq!(attempt.identity().attempt_id().as_uuid(), &attempt_id);
    assert_eq!(attempt.kind(), PaymentAttemptKind::HostCharge);
    assert_eq!(attempt.request().amount().cents(), 1_000);
    assert_eq!(
        attempt
            .state()
            .processor_evidence()
            .transaction_id()
            .expect("transaction ID")
            .expose(),
        "transaction-secret"
    );
    let debug = format!("{attempt:?}");
    for secret in [
        "idempotency-secret",
        "fingerprint-secret",
        "order-secret",
        "transaction-secret",
        "method-secret",
        "response-secret",
        "code-secret",
        "text-secret",
        "condition-secret",
        "Sensitive Name",
        "secret@example.test",
    ] {
        assert!(!debug.contains(secret), "debug leaked {secret}");
    }
    transaction.rollback().await?;
    database.cleanup().await
}

#[tokio::test]
async fn recovery_keeps_related_and_expected_payment_methods_distinct() -> Result<(), Box<dyn Error>>
{
    let database = TestDatabase::start("attempt_recovery").await?;
    let account = create_gateway_account(&database.pool, "nmi").await?;
    let subscriber_id = Uuid::now_v7();
    let expected_method_id = Uuid::now_v7();
    let related_method_id = Uuid::now_v7();
    for (method_id, reference) in [
        (expected_method_id, "vault-expected"),
        (related_method_id, "vault-related"),
    ] {
        sqlx::query(
            r#"
                INSERT INTO billing_payment_methods (
                    id, billing_scope_id, subscriber_id, gateway_account_id,
                    gateway_payment_method_reference, status
                ) VALUES ($1, $2, $3, $4, $5, 'active')
                "#,
        )
        .bind(method_id)
        .bind(account.billing_scope_id)
        .bind(subscriber_id)
        .bind(account.gateway_account_id)
        .bind(reference)
        .execute(&database.pool)
        .await?;
    }
    let subscription_id = Uuid::now_v7();
    let period_start = Utc::now();
    let period_end = period_start + Duration::days(30);
    sqlx::query(
        r#"
            INSERT INTO billing_subscriptions (
                id, billing_scope_id, subscriber_id, plan_key, status,
                gateway_account_id, payment_method_id, amount_cents, currency,
                current_period_start_at, current_period_end_at, next_renewal_at,
                initial_transaction_id, phase, recurring_period_kind,
                recurring_period_count, dunning_retry_delays_seconds,
                dunning_exhaustion, past_due_access, next_payment_attempt_at
            ) VALUES (
                $1, $2, $3, 'premium', 'active', $4, $5, 1000, 'USD',
                $6, $7, $7, 'txn-initial', 'recurring', 'calendar_months', 1,
                ARRAY[]::bigint[], 'remain_past_due', 'suspend_immediately', $7
            )
            "#,
    )
    .bind(subscription_id)
    .bind(account.billing_scope_id)
    .bind(subscriber_id)
    .bind(account.gateway_account_id)
    .bind(related_method_id)
    .bind(period_start)
    .bind(period_end)
    .execute(&database.pool)
    .await?;

    let attempt_id = Uuid::now_v7();
    let charge_start = period_end;
    let charge_end = charge_start + Duration::days(30);
    let order_id = format!("sr_recovery_{}", attempt_id.simple());
    sqlx::query(
        r#"
            INSERT INTO billing_payment_attempts (
                id, billing_scope_id, subscriber_id, plan_key,
                subscription_id, payment_method_id, attempt_kind, status,
                idempotency_key, request_fingerprint, amount_cents, currency,
                billing_period_start_at, billing_period_end_at,
                gateway_account_id, gateway_configuration_id, gateway_order_id,
                gateway_transaction_id, submitted_at, resolved_at,
                subscription_expected_payment_method_id,
                subscription_expected_initial_transaction_id,
                subscription_expected_status
            ) VALUES (
                $1, $2, $3, 'premium', $4, $5,
                'subscription_recovery', 'approved', $6, $7, 1000, 'USD',
                $8, $9, $10, $11, $12, 'txn-recovery', now(), now(),
                $13, 'txn-initial', 'past_due'
            )
            "#,
    )
    .bind(attempt_id)
    .bind(account.billing_scope_id)
    .bind(subscriber_id)
    .bind(subscription_id)
    .bind(related_method_id)
    .bind("recovery-key")
    .bind("recovery-fingerprint")
    .bind(charge_start)
    .bind(charge_end)
    .bind(account.gateway_account_id)
    .bind(account.gateway_configuration_id)
    .bind(order_id)
    .bind(expected_method_id)
    .execute(&database.pool)
    .await?;

    let mut transaction = database.pool.begin().await?;
    let attempt = find_payment_attempt_by_id_in_transaction(
        &mut transaction,
        BillingScopeId::new(account.billing_scope_id),
        PaymentAttemptId::new(attempt_id),
    )
    .await?
    .expect("recovery row should load");
    let target = attempt.request().target();
    assert_eq!(
        target.payment_method_id().unwrap().as_uuid(),
        &related_method_id
    );
    assert_eq!(
        target.subscription_id().unwrap().as_uuid(),
        &subscription_id
    );
    assert_eq!(
        target
            .subscription_payment_state_snapshot()
            .expect("expected state")
            .payment_method_id()
            .as_uuid(),
        &expected_method_id
    );
    assert_eq!(
        target
            .subscription_payment_state_snapshot()
            .expect("expected state")
            .status(),
        SubscriptionStatus::PastDue
    );
    transaction.rollback().await?;
    database.cleanup().await
}
