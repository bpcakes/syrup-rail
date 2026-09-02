use super::*;
use crate::{
    reserve_subscription_payment_method_replacement_in_transaction,
    reserve_subscription_recovery_in_transaction,
};

mod readiness;
mod reconciliation;
mod reservation;
mod stale_replay;

#[tokio::test]
async fn foreground_service_applies_once_and_replays_before_host_admission()
-> Result<(), Box<dyn Error>> {
    let fixture = enrollment_fixture("service_enroll", false, false, false).await?;
    let gateway = Arc::new(ScriptedGateway::new(Ok(approved_outcome(
        "txn_service_enroll",
    ))));
    let resolved = scripted_resolved_gateway(fixture.gateway_account, Arc::clone(&gateway));
    let resolver = Arc::new(StaticResolver {
        gateway: resolved,
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
    );

    let result = service.enroll(fixture.command.clone()).await?;
    assert_eq!(result.attempt().status(), PaymentAttemptStatus::Approved);
    assert!(result.subscription().is_some());
    assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 1);
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
    assert_eq!(admission.calls.load(Ordering::SeqCst), 1);

    let replay = service.enroll(fixture.command.clone()).await?;
    assert_eq!(replay, result);
    assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 1);
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
    assert_eq!(admission.calls.load(Ordering::SeqCst), 1);
    fixture.cleanup().await
}

#[tokio::test]
async fn foreground_automatic_renewal_uses_stored_credential_once_and_skips_stale_job()
-> Result<(), Box<dyn Error>> {
    let fixture = enrollment_fixture("service_renewal", false, false, false).await?;
    let initial_gateway = Arc::new(ScriptedGateway::for_sale_with_readiness(
        [Ok(GatewayAccountMode::Test), Ok(GatewayAccountMode::Test)],
        Ok(approved_outcome("txn_renewal_initial")),
    ));
    let initial_service = SubscriptionBillingService::new(
        fixture.database.pool.clone(),
        Arc::new(TestOfferStore),
        Arc::new(StaticResolver {
            gateway: scripted_resolved_gateway(
                fixture.gateway_account,
                Arc::clone(&initial_gateway),
            ),
            calls: AtomicUsize::new(0),
        }),
        Arc::new(PermitAdmission {
            calls: AtomicUsize::new(0),
        }),
        Arc::new(fixture.coordinator.clone()),
    )
    .with_required_gateway_account_mode(GatewayAccountMode::Test);
    let initial = initial_service.enroll(fixture.command.clone()).await?;
    let subscription_id = initial
        .subscription()
        .expect("approved enrollment creates subscription")
        .id();
    let requested_period_start_at = Utc::now() - ChronoDuration::hours(1);
    sqlx::query(
        r#"
        UPDATE billing_subscriptions
        SET current_period_start_at = $2 - interval '1 month',
            current_period_end_at = $2, next_renewal_at = $2,
            next_payment_attempt_at = $2,
            updated_at = clock_timestamp()
        WHERE id = $1
        "#,
    )
    .bind(subscription_id.as_uuid())
    .bind(requested_period_start_at)
    .execute(&fixture.database.pool)
    .await?;
    let period_start_at: DateTime<Utc> =
        sqlx::query_scalar("SELECT next_renewal_at FROM billing_subscriptions WHERE id = $1")
            .bind(subscription_id.as_uuid())
            .fetch_one(&fixture.database.pool)
            .await?;

    let gateway = Arc::new(ScriptedGateway::for_sale_with_readiness(
        [Ok(GatewayAccountMode::Test), Ok(GatewayAccountMode::Test)],
        Ok(approved_outcome("txn_renewal_recurring")),
    ));
    let resolved_gateway = scripted_resolved_gateway(fixture.gateway_account, Arc::clone(&gateway));
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
    let command = ChargeRenewal::new(
        fixture.command.billing_scope_id(),
        subscription_id,
        period_start_at,
    );
    let stale_update_command = ReplaceSubscriptionPaymentMethod::new(
        syrup_rail::SubscriptionPaymentContext::new(
            PaymentAttemptId::new(Uuid::now_v7()),
            fixture.command.billing_scope_id(),
            fixture.command.subscriber_id(),
            fixture.command.gateway_configuration_id(),
            IdempotencyKey::new("renewal-stale-review-update")?,
            PaymentToken::new("opaque-renewal-stale-review-token")?,
            fixture.command.billing_contact().clone(),
        ),
        fixture.command.plan_key().clone(),
    );
    let mut transaction = fixture.database.pool.begin().await?;
    let stale_update_id = match reserve_subscription_payment_method_replacement_in_transaction(
        &mut transaction,
        &stale_update_command,
        &resolved_gateway,
        GatewayAccountMode::Test,
    )
    .await?
    {
        SubscriptionPaymentMethodReplacementReservationOutcome::Reserved(_, attempt) => {
            attempt.identity().attempt_id()
        }
        other => return Err(format!("unexpected stale update reservation: {other:?}").into()),
    };
    transaction.commit().await?;
    sqlx::query(
        r#"
        UPDATE billing_payment_attempts
        SET status = 'review_required',
            created_at = clock_timestamp() - interval '4 minutes',
            updated_at = clock_timestamp()
        WHERE id = $1
        "#,
    )
    .bind(stale_update_id.as_uuid())
    .execute(&fixture.database.pool)
    .await?;

    let result = service.renew(command).await?;
    let SubscriptionRenewalOutcome::Payment(payment) = result else {
        panic!("due renewal must return its applied payment");
    };
    assert_eq!(
        payment.attempt().kind(),
        PaymentAttemptKind::SubscriptionRenewal
    );
    assert_eq!(payment.attempt().status(), PaymentAttemptStatus::Approved);
    assert_eq!(
        payment
            .subscription()
            .expect("approved renewal returns subscription")
            .current_period()
            .start_at(),
        &period_start_at
    );
    assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 1);
    assert_eq!(gateway.account_mode_calls.load(Ordering::SeqCst), 2);
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
    assert_eq!(admission.calls.load(Ordering::SeqCst), 0);
    let stale_update_status: String =
        sqlx::query_scalar("SELECT status FROM billing_payment_attempts WHERE id = $1")
            .bind(stale_update_id.as_uuid())
            .fetch_one(&fixture.database.pool)
            .await?;
    assert_eq!(stale_update_status, "failed");

    assert!(matches!(
        service.renew(command).await?,
        SubscriptionRenewalOutcome::Noop
    ));
    assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 1);
    assert_eq!(gateway.account_mode_calls.load(Ordering::SeqCst), 2);
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
    let events = fixture.coordinator.events.lock().await;
    assert_eq!(events.len(), 2);
    assert!(matches!(
        events[1],
        BillingEvent::SubscriptionRenewed { .. }
    ));
    drop(events);
    fixture.cleanup().await
}

#[tokio::test]
async fn foreground_recovery_derives_locked_terms_applies_once_and_replays()
-> Result<(), Box<dyn Error>> {
    let fixture = enrollment_fixture("service_recovery", true, false, false).await?;
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

#[tokio::test]
async fn foreground_payment_method_replacement_applies_once_and_replays_before_admission()
-> Result<(), Box<dyn Error>> {
    let fixture = enrollment_fixture("replace_method", false, false, false).await?;
    let initial_gateway = Arc::new(ScriptedGateway::for_sale_with_readiness(
        [Ok(GatewayAccountMode::Test), Ok(GatewayAccountMode::Test)],
        Ok(approved_outcome_with_reference(
            Some("txn_method_initial"),
            "vault_method_old",
        )),
    ));
    let initial_service = SubscriptionBillingService::new(
        fixture.database.pool.clone(),
        Arc::new(TestOfferStore),
        Arc::new(StaticResolver {
            gateway: scripted_resolved_gateway(
                fixture.gateway_account,
                Arc::clone(&initial_gateway),
            ),
            calls: AtomicUsize::new(0),
        }),
        Arc::new(PermitAdmission {
            calls: AtomicUsize::new(0),
        }),
        Arc::new(fixture.coordinator.clone()),
    )
    .with_required_gateway_account_mode(GatewayAccountMode::Test);
    let initial = initial_service.enroll(fixture.command.clone()).await?;
    let subscription = initial
        .subscription()
        .expect("approved enrollment has subscription");
    let subscription_id = subscription.id();
    let old_method_id = subscription.payment_method_id();

    let gateway = Arc::new(ScriptedGateway::for_stored_method_with_readiness(
        [Ok(GatewayAccountMode::Test), Ok(GatewayAccountMode::Test)],
        Ok(approved_outcome_with_reference(
            Some("txn_method_new"),
            "vault_method_new",
        )),
    ));
    let resolved_gateway = scripted_resolved_gateway(fixture.gateway_account, Arc::clone(&gateway));
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
    let prepared_command = ReplaceSubscriptionPaymentMethod::new(
        syrup_rail::SubscriptionPaymentContext::new(
            PaymentAttemptId::new(Uuid::now_v7()),
            fixture.command.billing_scope_id(),
            fixture.command.subscriber_id(),
            fixture.command.gateway_configuration_id(),
            IdempotencyKey::new("replace-method-key")?,
            PaymentToken::new("opaque-replacement-token")?,
            fixture.command.billing_contact().clone(),
        ),
        fixture.command.plan_key().clone(),
    );

    let mut transaction = fixture.database.pool.begin().await?;
    let fresh_cross_mode = reserve_subscription_payment_method_replacement_in_transaction(
        &mut transaction,
        &prepared_command,
        &resolved_gateway,
        GatewayAccountMode::Live,
    )
    .await?;
    transaction.rollback().await?;
    assert!(matches!(
        fresh_cross_mode,
        SubscriptionPaymentMethodReplacementReservationOutcome::Rejected(
            SubscriptionPaymentMethodReplacementRejection::GatewayAccountModeChanged
        )
    ));

    let mut transaction = fixture.database.pool.begin().await?;
    let prepared_attempt = match reserve_subscription_payment_method_replacement_in_transaction(
        &mut transaction,
        &prepared_command,
        &resolved_gateway,
        GatewayAccountMode::Test,
    )
    .await?
    {
        SubscriptionPaymentMethodReplacementReservationOutcome::Reserved(_, attempt) => *attempt,
        other => {
            return Err(format!("unexpected payment-method reservation: {other:?}").into());
        }
    };
    transaction.commit().await?;
    let original_attempt_id = prepared_attempt.identity().attempt_id();
    let original_order_id = prepared_attempt.request().gateway_order_id().clone();
    let mut transaction = fixture.database.pool.begin().await?;
    let cross_mode = reserve_subscription_payment_method_replacement_in_transaction(
        &mut transaction,
        &prepared_command,
        &resolved_gateway,
        GatewayAccountMode::Live,
    )
    .await?;
    transaction.rollback().await?;
    assert!(matches!(
        cross_mode,
        SubscriptionPaymentMethodReplacementReservationOutcome::Rejected(
            SubscriptionPaymentMethodReplacementRejection::GatewayAccountModeChanged
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
        live_service
            .replace_payment_method(prepared_command.clone())
            .await,
        Err(SubscriptionBillingServiceError::GatewayConfigurationChanged)
    ));
    assert_eq!(live_admission.calls.load(Ordering::SeqCst), 0);
    let command = ReplaceSubscriptionPaymentMethod::new(
        syrup_rail::SubscriptionPaymentContext::new(
            PaymentAttemptId::new(Uuid::now_v7()),
            fixture.command.billing_scope_id(),
            fixture.command.subscriber_id(),
            fixture.command.gateway_configuration_id(),
            prepared_command.idempotency_key().clone(),
            PaymentToken::new("refreshed-replacement-success-token")?,
            prepared_command.billing_contact().clone(),
        ),
        fixture.command.plan_key().clone(),
    );
    assert_ne!(command.attempt_id(), original_attempt_id);

    let changed_contact_command = ReplaceSubscriptionPaymentMethod::new(
        syrup_rail::SubscriptionPaymentContext::new(
            PaymentAttemptId::new(Uuid::now_v7()),
            fixture.command.billing_scope_id(),
            fixture.command.subscriber_id(),
            fixture.command.gateway_configuration_id(),
            prepared_command.idempotency_key().clone(),
            PaymentToken::new("refreshed-replacement-token")?,
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
            .replace_payment_method(changed_contact_command)
            .await
            .expect_err("repartitioned durable contact must conflict"),
        SubscriptionBillingServiceError::IdempotencyConflict,
    ));
    assert_eq!(gateway.store_calls.load(Ordering::SeqCst), 0);
    assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 0);
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 0);
    assert_eq!(admission.calls.load(Ordering::SeqCst), 0);

    let result = service.replace_payment_method(command.clone()).await?;
    assert_eq!(
        result.attempt().identity().attempt_id(),
        original_attempt_id
    );
    assert_eq!(result.attempt().status(), PaymentAttemptStatus::Approved);
    assert_eq!(
        result.attempt().kind(),
        PaymentAttemptKind::SubscriptionPaymentMethodUpdate
    );
    let updated = result
        .subscription()
        .expect("approved replacement returns subscription");
    assert_eq!(updated.id(), subscription_id);
    assert_ne!(updated.payment_method_id(), old_method_id);
    assert_eq!(
        updated.payment_method_id(),
        result
            .attempt()
            .request()
            .target()
            .payment_method_id()
            .expect("applied attempt carries replacement method")
    );
    let old_status: String =
        sqlx::query_scalar("SELECT status FROM billing_payment_methods WHERE id = $1")
            .bind(old_method_id.as_uuid())
            .fetch_one(&fixture.database.pool)
            .await?;
    assert_eq!(old_status, "disabled");
    assert_eq!(gateway.store_calls.load(Ordering::SeqCst), 1);
    assert_eq!(gateway.account_mode_calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        gateway.store_order_ids.lock().await.as_slice(),
        &[original_order_id]
    );
    assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 0);
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
    assert_eq!(admission.calls.load(Ordering::SeqCst), 1);

    let aggregate_lock = hold_subscription_aggregate_lock(
        &fixture.database.pool,
        command.subscriber_id().into_uuid(),
        command.plan_key().as_str(),
    )
    .await?;
    let replay = service.replace_payment_method(command.clone()).await?;
    aggregate_lock.rollback().await?;
    assert_eq!(replay, result);
    assert_eq!(gateway.store_calls.load(Ordering::SeqCst), 1);
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
    assert_eq!(admission.calls.load(Ordering::SeqCst), 1);

    sqlx::query(
        "UPDATE billing_subscriptions \
         SET initial_transaction_id = 'txn_external_change', updated_at = clock_timestamp() \
         WHERE id = $1",
    )
    .bind(subscription_id.as_uuid())
    .execute(&fixture.database.pool)
    .await?;
    let replay_after_state_change = service.replace_payment_method(command).await?;
    assert_eq!(replay_after_state_change.attempt(), result.attempt());
    assert!(replay_after_state_change.subscription().is_some());
    assert_eq!(gateway.store_calls.load(Ordering::SeqCst), 1);
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
    assert_eq!(admission.calls.load(Ordering::SeqCst), 1);
    sqlx::query(
        "UPDATE billing_subscriptions \
         SET initial_transaction_id = 'txn_method_new', updated_at = clock_timestamp() \
         WHERE id = $1",
    )
    .bind(subscription_id.as_uuid())
    .execute(&fixture.database.pool)
    .await?;

    let stale_command = ReplaceSubscriptionPaymentMethod::new(
        syrup_rail::SubscriptionPaymentContext::new(
            PaymentAttemptId::new(Uuid::now_v7()),
            fixture.command.billing_scope_id(),
            fixture.command.subscriber_id(),
            fixture.command.gateway_configuration_id(),
            IdempotencyKey::new("stale-replace-method-key")?,
            PaymentToken::new("opaque-stale-replacement-token")?,
            fixture.command.billing_contact().clone(),
        ),
        fixture.command.plan_key().clone(),
    );
    let mut transaction = fixture.database.pool.begin().await?;
    let stale_attempt_id = match reserve_subscription_payment_method_replacement_in_transaction(
        &mut transaction,
        &stale_command,
        &resolved_gateway,
        GatewayAccountMode::Test,
    )
    .await?
    {
        SubscriptionPaymentMethodReplacementReservationOutcome::Reserved(_, attempt) => {
            attempt.identity().attempt_id()
        }
        other => {
            return Err(format!("unexpected stale payment-method reservation: {other:?}").into());
        }
    };
    transaction.commit().await?;
    sqlx::query(
        r#"
        UPDATE billing_payment_attempts
        SET status = 'review_required',
            gateway_response_text = 'Persisted local review before submission.',
            created_at = clock_timestamp() - interval '4 minutes',
            updated_at = clock_timestamp()
        WHERE id = $1
        "#,
    )
    .bind(stale_attempt_id.as_uuid())
    .execute(&fixture.database.pool)
    .await?;
    let mut transaction = fixture.database.pool.begin().await?;
    let stale_result = match reserve_subscription_payment_method_replacement_in_transaction(
        &mut transaction,
        &stale_command,
        &resolved_gateway,
        GatewayAccountMode::Test,
    )
    .await?
    {
        SubscriptionPaymentMethodReplacementReservationOutcome::Replay(attempt) => *attempt,
        other => return Err(format!("unexpected stale replay reservation: {other:?}").into()),
    };
    transaction.commit().await?;
    assert_eq!(stale_result.status(), PaymentAttemptStatus::Failed);
    assert_eq!(gateway.store_calls.load(Ordering::SeqCst), 1);

    let parked_outcome =
        approved_outcome_with_reference(Some("txn_method_parked"), "vault_method_parked");
    let parked_gateway = Arc::new(ScriptedGateway::for_stored_method(Ok(
        parked_outcome.clone()
    )));
    let parked_resolved_gateway =
        scripted_resolved_gateway(fixture.gateway_account, Arc::clone(&parked_gateway));
    let parked_resolver = Arc::new(StaticResolver {
        gateway: parked_resolved_gateway.clone(),
        calls: AtomicUsize::new(0),
    });
    let parked_admission = Arc::new(PermitAdmission {
        calls: AtomicUsize::new(0),
    });
    let parked_service = SubscriptionBillingService::new(
        fixture.database.pool.clone(),
        Arc::new(TestOfferStore),
        parked_resolver.clone(),
        parked_admission.clone(),
        Arc::new(fixture.coordinator.clone()),
    )
    .with_required_gateway_account_mode(GatewayAccountMode::Test);
    let parked_command = ReplaceSubscriptionPaymentMethod::new(
        syrup_rail::SubscriptionPaymentContext::new(
            PaymentAttemptId::new(Uuid::now_v7()),
            fixture.command.billing_scope_id(),
            fixture.command.subscriber_id(),
            fixture.command.gateway_configuration_id(),
            IdempotencyKey::new("parked-replace-method-key")?,
            PaymentToken::new("opaque-parked-replacement-token")?,
            fixture.command.billing_contact().clone(),
        ),
        fixture.command.plan_key().clone(),
    );
    let mut transaction = fixture.database.pool.begin().await?;
    let parked_reservation = match reserve_subscription_payment_method_replacement_in_transaction(
        &mut transaction,
        &parked_command,
        &parked_resolved_gateway,
        GatewayAccountMode::Test,
    )
    .await?
    {
        SubscriptionPaymentMethodReplacementReservationOutcome::Reserved(reservation, _) => {
            *reservation
        }
        other => {
            return Err(format!("unexpected parked replacement reservation: {other:?}").into());
        }
    };
    transaction.commit().await?;
    match admit_subscription_payment_method_replacement(&fixture.database.pool, &parked_reservation)
        .await?
    {
        SubscriptionPaymentMethodReplacementAdmissionOutcome::Admitted(_) => {}
        other => return Err(format!("unexpected parked replacement admission: {other:?}").into()),
    }
    sqlx::query(
        "UPDATE billing_subscriptions \
         SET initial_transaction_id = 'txn_external_before_park', updated_at = clock_timestamp() \
         WHERE id = $1",
    )
    .bind(subscription_id.as_uuid())
    .execute(&fixture.database.pool)
    .await?;

    let parked = apply_subscription_payment_method_replacement_gateway_outcome(
        &fixture.database.pool,
        &fixture.coordinator,
        &parked_reservation,
        &parked_outcome,
    )
    .await?;
    assert_eq!(
        parked.attempt().status(),
        PaymentAttemptStatus::ReviewRequired
    );
    assert!(parked.subscription().is_none());
    assert!(parked.observation_diagnostics().is_empty());
    let parked_replay = parked_service
        .replace_payment_method(parked_command)
        .await?;
    assert_eq!(parked_replay, parked);
    assert_eq!(parked_replay.attempt(), parked.attempt());
    assert!(parked_replay.observation_diagnostics().is_empty());
    assert!(parked_replay.subscription().is_none());
    assert_eq!(parked_gateway.store_calls.load(Ordering::SeqCst), 0);
    assert_eq!(parked_resolver.calls.load(Ordering::SeqCst), 0);
    assert_eq!(parked_admission.calls.load(Ordering::SeqCst), 0);

    sqlx::query(
        "UPDATE billing_subscriptions \
         SET initial_transaction_id = 'txn_method_new', updated_at = clock_timestamp() \
         WHERE id = $1",
    )
    .bind(subscription_id.as_uuid())
    .execute(&fixture.database.pool)
    .await?;
    let conflicting_reconciliation = parked_service
        .apply_reconciled_outcome(
            parked.attempt().identity().billing_scope_id(),
            parked.attempt().identity().attempt_id(),
            &approved_outcome_with_reference(
                Some("txn_method_conflicting_observation"),
                "vault_method_conflicting_observation",
            ),
        )
        .await?;
    assert_eq!(
        conflicting_reconciliation.attempt().status(),
        PaymentAttemptStatus::ReviewRequired
    );
    assert!(conflicting_reconciliation.subscription().is_none());
    assert_eq!(
        conflicting_reconciliation.observation_diagnostics(),
        &[
            GatewayPaymentDiagnostic::InvalidOrConflictingTransactionIdentifier,
            GatewayPaymentDiagnostic::InvalidOrConflictingPaymentMethodReference,
        ]
    );
    let preserved_review_identity: (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT gateway_transaction_id, gateway_payment_method_reference \
         FROM billing_payment_attempts WHERE id = $1",
    )
    .bind(parked.attempt().identity().attempt_id().as_uuid())
    .fetch_one(&fixture.database.pool)
    .await?;
    assert_eq!(
        preserved_review_identity,
        (
            Some("txn_method_parked".to_owned()),
            Some("vault_method_parked".to_owned()),
        )
    );
    let current_method_reference: String = sqlx::query_scalar(
        r#"
        SELECT method.gateway_payment_method_reference
        FROM billing_subscriptions AS subscription
        INNER JOIN billing_payment_methods AS method
            ON method.id = subscription.payment_method_id
        WHERE subscription.id = $1
        "#,
    )
    .bind(subscription_id.as_uuid())
    .fetch_one(&fixture.database.pool)
    .await?;
    assert_eq!(current_method_reference, "vault_method_new");

    let additional_transaction_id = "txn_method_unexpected_additional";
    let reconciled_outcome = approved_outcome_with_reference(
        Some(additional_transaction_id),
        "vault_method_unexpected_additional",
    );
    let reconciled = service
        .apply_reconciled_outcome(
            result.attempt().identity().billing_scope_id(),
            result.attempt().identity().attempt_id(),
            &reconciled_outcome,
        )
        .await?;
    assert_eq!(
        reconciled.attempt().status(),
        PaymentAttemptStatus::Approved
    );
    assert_eq!(
        reconciled.observation_diagnostics(),
        &[
            GatewayPaymentDiagnostic::InvalidOrConflictingTransactionIdentifier,
            GatewayPaymentDiagnostic::InvalidOrConflictingPaymentMethodReference,
        ]
    );
    let additional_progression: String = sqlx::query_scalar(
        r#"
        SELECT progression_state
        FROM billing_processor_charges
        WHERE attempt_id = $1 AND gateway_transaction_id = $2
        "#,
    )
    .bind(result.attempt().identity().attempt_id().as_uuid())
    .bind(additional_transaction_id)
    .fetch_one(&fixture.database.pool)
    .await?;
    assert_eq!(additional_progression, "reconciliation_required");

    let events = fixture.coordinator.events.lock().await;
    assert_eq!(events.len(), 2);
    let BillingEvent::PaymentMethodChanged {
        card: Some(card), ..
    } = &events[1]
    else {
        panic!("approved replacement must emit its canonical masked-card display")
    };
    assert_eq!(card.brand(), &PaymentCardBrand::Visa);
    assert_eq!(card.last_four().expose(), "4242");
    drop(events);
    fixture.cleanup().await
}
