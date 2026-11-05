use super::*;

#[tokio::test]
async fn foreground_recovery_derives_locked_terms_applies_once_and_replays()
-> Result<(), Box<dyn Error>> {
    let fixture = enrollment_fixture("service_recovery")
        .discounted()
        .build()
        .await?;
    let initial_gateway = Arc::new(ScriptedGateway::for_sale_with_readiness(
        [Ok(GatewayAccountMode::Test), Ok(GatewayAccountMode::Test)],
        Ok(approved_outcome("txn_recovery_initial")),
    ));
    let initial_resolver = Arc::new(StaticResolver {
        gateway: scripted_resolved_gateway(fixture.gateway_account, Arc::clone(&initial_gateway)),
        calls: AtomicUsize::new(0),
    });
    let initial_service = SubscriptionBillingService::new(
        fixture.database.pool.clone(),
        Arc::new(TestOfferStore),
        initial_resolver,
        Arc::new(PermitAdmission {
            calls: AtomicUsize::new(0),
        }),
        Arc::new(fixture.coordinator.clone()),
    )
    .with_required_gateway_account_mode(GatewayAccountMode::Test);
    let initial = initial_service.enroll(fixture.command.clone()).await?;
    let subscription_id = initial
        .subscription()
        .expect("approved enrollment has a subscription")
        .id();
    let due_at = Utc::now() - ChronoDuration::days(1);
    let current_period_start_at = due_at - ChronoDuration::days(31);
    let persisted_due_at: DateTime<Utc> = sqlx::query_scalar(
        r#"
        UPDATE billing_subscriptions
        SET status = 'past_due',
            current_period_start_at = $2,
            current_period_end_at = $3,
            next_renewal_at = $3,
            next_payment_attempt_at = $3,
            updated_at = clock_timestamp()
        WHERE id = $1
        RETURNING next_renewal_at
        "#,
    )
    .bind(subscription_id.as_uuid())
    .bind(current_period_start_at)
    .bind(due_at)
    .fetch_one(&fixture.database.pool)
    .await?;

    let recovery_gateway = Arc::new(ScriptedGateway::for_sale_with_readiness(
        [Ok(GatewayAccountMode::Test), Ok(GatewayAccountMode::Test)],
        Ok(approved_outcome_with_reference(
            Some("txn_recovery_approved"),
            "vault_recovery",
        )),
    ));
    let resolved_gateway =
        scripted_resolved_gateway(fixture.gateway_account, Arc::clone(&recovery_gateway));
    let resolver = Arc::new(StaticResolver {
        gateway: resolved_gateway.clone(),
        calls: AtomicUsize::new(0),
    });
    let admission = Arc::new(PermitAdmission {
        calls: AtomicUsize::new(0),
    });
    let service = SubscriptionBillingService::new(
        fixture.database.pool.clone(),
        Arc::new(TestOfferStore),
        resolver.clone(),
        admission.clone(),
        Arc::new(fixture.coordinator.clone()),
    )
    .with_required_gateway_account_mode(GatewayAccountMode::Test);
    let prepared_command = RecoverSubscriptionPayment::new(
        syrup_rail::SubscriptionPaymentContext::new(
            PaymentAttemptId::new(Uuid::now_v7()),
            fixture.command.billing_scope_id(),
            fixture.command.subscriber_id(),
            fixture.command.gateway_configuration_id(),
            IdempotencyKey::new("recovery-key")?,
            PaymentToken::new("opaque-recovery-token")?,
            fixture.command.billing_contact().clone(),
        ),
        fixture.command.plan_key().clone(),
    );

    let mut transaction = fixture.database.pool.begin().await?;
    let fresh_cross_mode = reserve_subscription_recovery_in_transaction(
        &mut transaction,
        &prepared_command,
        &resolved_gateway,
        GatewayAccountMode::Live,
    )
    .await?;
    transaction.rollback().await?;
    assert!(matches!(
        fresh_cross_mode,
        SubscriptionRecoveryReservationOutcome::Rejected(
            SubscriptionRecoveryReservationRejection::GatewayAccountModeChanged
        )
    ));

    let mut transaction = fixture.database.pool.begin().await?;
    let prepared_attempt = match reserve_subscription_recovery_in_transaction(
        &mut transaction,
        &prepared_command,
        &resolved_gateway,
        GatewayAccountMode::Test,
    )
    .await?
    {
        SubscriptionRecoveryReservationOutcome::Reserved(_, attempt) => *attempt,
        other => return Err(format!("unexpected recovery reservation: {other:?}").into()),
    };
    transaction.commit().await?;
    let original_attempt_id = prepared_attempt.identity().attempt_id();
    let original_order_id = prepared_attempt.request().gateway_order_id().clone();
    let mut transaction = fixture.database.pool.begin().await?;
    let cross_mode = reserve_subscription_recovery_in_transaction(
        &mut transaction,
        &prepared_command,
        &resolved_gateway,
        GatewayAccountMode::Live,
    )
    .await?;
    transaction.rollback().await?;
    assert!(matches!(
        cross_mode,
        SubscriptionRecoveryReservationOutcome::Rejected(
            SubscriptionRecoveryReservationRejection::GatewayAccountModeChanged
        )
    ));
    let live_admission = Arc::new(PermitAdmission {
        calls: AtomicUsize::new(0),
    });
    let live_service = SubscriptionBillingService::new(
        fixture.database.pool.clone(),
        Arc::new(TestOfferStore),
        Arc::new(StaticResolver {
            gateway: resolved_gateway.clone(),
            calls: AtomicUsize::new(0),
        }),
        live_admission.clone(),
        Arc::new(fixture.coordinator.clone()),
    );
    assert!(matches!(
        live_service.recover(prepared_command.clone()).await,
        Err(SubscriptionBillingServiceError::GatewayConfigurationChanged)
    ));
    assert_eq!(live_admission.calls.load(Ordering::SeqCst), 0);
    let mut connection = fixture.database.pool.acquire().await?;
    let prepared_result =
        payment_result_for_attempt(&mut connection, prepared_attempt.clone()).await?;
    assert!(prepared_result.subscription().is_none());
    drop(connection);

    let command = RecoverSubscriptionPayment::new(
        syrup_rail::SubscriptionPaymentContext::new(
            PaymentAttemptId::new(Uuid::now_v7()),
            fixture.command.billing_scope_id(),
            fixture.command.subscriber_id(),
            fixture.command.gateway_configuration_id(),
            prepared_command.idempotency_key().clone(),
            PaymentToken::new("refreshed-recovery-success-token")?,
            prepared_command.billing_contact().clone(),
        ),
        fixture.command.plan_key().clone(),
    );
    assert_ne!(command.attempt_id(), original_attempt_id);

    let changed_contact_command = RecoverSubscriptionPayment::new(
        syrup_rail::SubscriptionPaymentContext::new(
            PaymentAttemptId::new(Uuid::now_v7()),
            fixture.command.billing_scope_id(),
            fixture.command.subscriber_id(),
            fixture.command.gateway_configuration_id(),
            prepared_command.idempotency_key().clone(),
            PaymentToken::new("refreshed-recovery-token")?,
            BillingContact::new(
                Some("Ada Lovelace".to_owned()),
                None,
                Some("ada@example.test".to_owned()),
            )?,
        ),
        fixture.command.plan_key().clone(),
    );
    assert!(matches!(
        service
            .recover(changed_contact_command)
            .await
            .expect_err("repartitioned durable contact must conflict"),
        SubscriptionBillingServiceError::IdempotencyConflict,
    ));
    assert_eq!(recovery_gateway.sale_calls.load(Ordering::SeqCst), 0);
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 0);
    assert_eq!(admission.calls.load(Ordering::SeqCst), 0);

    let result = service.recover(command.clone()).await?;
    assert_eq!(
        result.attempt().identity().attempt_id(),
        original_attempt_id
    );
    assert_eq!(result.attempt().status(), PaymentAttemptStatus::Approved);
    assert_eq!(
        result.attempt().kind(),
        PaymentAttemptKind::SubscriptionRecovery
    );
    let recovered = result
        .subscription()
        .expect("recovery applies subscription");
    assert_eq!(recovered.id(), subscription_id);
    assert_eq!(recovered.status(), SubscriptionStatus::Active);
    assert_eq!(*recovered.current_period().start_at(), persisted_due_at);
    assert_eq!(
        recovered.payment_method_id(),
        result
            .attempt()
            .request()
            .target()
            .payment_method_id()
            .unwrap()
    );
    assert_eq!(recovery_gateway.sale_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        recovery_gateway.account_mode_calls.load(Ordering::SeqCst),
        2
    );
    assert_eq!(
        recovery_gateway.sale_order_ids.lock().await.as_slice(),
        &[original_order_id]
    );
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
    assert_eq!(admission.calls.load(Ordering::SeqCst), 1);

    let discount: (i32, String, i32) = sqlx::query_as(
        r#"
        SELECT discounts.periods_applied, discounts.status, subscriptions.amount_cents
        FROM billing_subscription_discounts AS discounts
        INNER JOIN billing_subscriptions AS subscriptions
            ON subscriptions.id = discounts.subscription_id
        WHERE discounts.subscription_id = $1
        "#,
    )
    .bind(subscription_id.as_uuid())
    .fetch_one(&fixture.database.pool)
    .await?;
    assert_eq!(discount, (2, "active".to_owned(), 800));

    let aggregate_lock = hold_subscription_aggregate_lock(
        &fixture.database.pool,
        command.subscriber_id().into_uuid(),
        command.plan_key().as_str(),
    )
    .await?;
    let replay = service.recover(command.clone()).await?;
    aggregate_lock.rollback().await?;
    assert_eq!(replay, result);
    assert_eq!(recovery_gateway.sale_calls.load(Ordering::SeqCst), 1);
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
    assert_eq!(admission.calls.load(Ordering::SeqCst), 1);
    let next_due_at = Utc::now() - ChronoDuration::hours(12);
    sqlx::query(
        r#"
        UPDATE billing_subscriptions
        SET status = 'past_due',
            current_period_end_at = $2,
            next_renewal_at = $2,
            next_payment_attempt_at = $2,
            updated_at = clock_timestamp()
        WHERE id = $1
        "#,
    )
    .bind(subscription_id.as_uuid())
    .bind(next_due_at)
    .execute(&fixture.database.pool)
    .await?;
    let replay_after_period_advance = service.recover(command).await?;
    assert_eq!(replay_after_period_advance.attempt(), result.attempt());
    assert_eq!(
        replay_after_period_advance
            .subscription()
            .expect("replay loads the current subscription")
            .status(),
        SubscriptionStatus::PastDue
    );
    assert_eq!(recovery_gateway.sale_calls.load(Ordering::SeqCst), 1);

    let stale_gateway = Arc::new(ScriptedGateway::new(Ok(approved_outcome(
        "txn_stale_recovery_must_not_submit",
    ))));
    let stale_resolved_gateway =
        scripted_resolved_gateway(fixture.gateway_account, Arc::clone(&stale_gateway));
    let stale_command = RecoverSubscriptionPayment::new(
        syrup_rail::SubscriptionPaymentContext::new(
            PaymentAttemptId::new(Uuid::now_v7()),
            fixture.command.billing_scope_id(),
            fixture.command.subscriber_id(),
            fixture.command.gateway_configuration_id(),
            IdempotencyKey::new("stale-recovery-key")?,
            PaymentToken::new("opaque-stale-recovery-token")?,
            fixture.command.billing_contact().clone(),
        ),
        fixture.command.plan_key().clone(),
    );
    let mut transaction = fixture.database.pool.begin().await?;
    let stale_attempt_id = match reserve_subscription_recovery_in_transaction(
        &mut transaction,
        &stale_command,
        &stale_resolved_gateway,
        GatewayAccountMode::Test,
    )
    .await?
    {
        SubscriptionRecoveryReservationOutcome::Reserved(_, attempt) => {
            attempt.identity().attempt_id()
        }
        other => return Err(format!("unexpected stale recovery reservation: {other:?}").into()),
    };
    transaction.commit().await?;
    sqlx::query(
        r#"
        UPDATE billing_payment_attempts
        SET status = 'review_required',
            gateway_response_text = 'Legacy exact query found no transaction.',
            created_at = clock_timestamp() - interval '31 minutes',
            updated_at = clock_timestamp()
        WHERE id = $1
        "#,
    )
    .bind(stale_attempt_id.as_uuid())
    .execute(&fixture.database.pool)
    .await?;
    let stale_resolver = Arc::new(StaticResolver {
        gateway: stale_resolved_gateway,
        calls: AtomicUsize::new(0),
    });
    let stale_admission = Arc::new(PermitAdmission {
        calls: AtomicUsize::new(0),
    });
    let stale_service = SubscriptionBillingService::new(
        fixture.database.pool.clone(),
        Arc::new(TestOfferStore),
        stale_resolver.clone(),
        stale_admission.clone(),
        Arc::new(fixture.coordinator.clone()),
    )
    .with_required_gateway_account_mode(GatewayAccountMode::Test);
    let stale_result = stale_service.recover(stale_command).await?;
    assert_eq!(
        stale_result.attempt().status(),
        PaymentAttemptStatus::Failed
    );
    assert!(stale_result.subscription().is_none());
    assert_eq!(stale_gateway.sale_calls.load(Ordering::SeqCst), 0);
    assert_eq!(stale_resolver.calls.load(Ordering::SeqCst), 0);
    assert_eq!(stale_admission.calls.load(Ordering::SeqCst), 0);

    let events = fixture.coordinator.events.lock().await;
    assert_eq!(events.len(), 2);
    assert!(matches!(
        events[1],
        BillingEvent::SubscriptionRenewed { .. }
    ));
    drop(events);
    fixture.cleanup().await
}
