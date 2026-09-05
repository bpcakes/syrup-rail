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
        approved_outcome_with_reference(Some("txn_method_parked"), "vault_method_parked")
            .with_diagnostics(vec![GatewayPaymentDiagnostic::ProcessorReportedDuplicate]);
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
    assert_eq!(parked.attempt().status(), PaymentAttemptStatus::Unknown);
    assert!(parked.subscription().is_none());
    assert_eq!(
        parked.observation_diagnostics(),
        &[GatewayPaymentDiagnostic::ProcessorReportedDuplicate]
    );
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

    let additional_transaction_id = "txn_method_unexpected_additional";
    let reconciled_outcome = approved_outcome_with_reference(
        Some(additional_transaction_id),
        "vault_method_unexpected_additional",
    )
    .with_diagnostics(vec![GatewayPaymentDiagnostic::ProcessorReportedDuplicate]);
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
            GatewayPaymentDiagnostic::ProcessorReportedDuplicate,
        ]
    );
    let additional_charge_count: i64 = sqlx::query_scalar(
        r#"
        SELECT count(*)
        FROM billing_processor_charges
        WHERE attempt_id = $1 AND gateway_transaction_id = $2
        "#,
    )
    .bind(result.attempt().identity().attempt_id().as_uuid())
    .bind(additional_transaction_id)
    .fetch_one(&fixture.database.pool)
    .await?;
    assert_eq!(additional_charge_count, 0);
    assert_eq!(gateway.store_calls.load(Ordering::SeqCst), 1);
    assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 0);
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
    assert_eq!(admission.calls.load(Ordering::SeqCst), 1);

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

#[tokio::test]
async fn foreground_readiness_throttle_resolves_attempt_and_provider_cooldown_atomically()
-> Result<(), Box<dyn Error>> {
    let fixture = enrollment_fixture("svc_ready_429", false, false, false).await?;
    let resolved = scripted_resolved_gateway(
        fixture.gateway_account,
        Arc::new(RateLimitedReadinessGateway),
    );
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

    let error = service
        .enroll(fixture.command.clone())
        .await
        .expect_err("provider readiness throttle must return a typed cooldown");
    assert!(matches!(
        error,
        SubscriptionBillingServiceError::GatewayMutationCooldown {
            scope: GatewayMutationCooldownScope::Provider
        }
    ));
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
    assert_eq!(admission.calls.load(Ordering::SeqCst), 1);
    let state: (String, Option<String>, Option<String>, bool, bool) = sqlx::query_as(
        r#"
        SELECT attempt.status, attempt.resolution_code, attempt.gateway_condition,
            COALESCE(account.mutation_rate_limited_until > clock_timestamp(), false),
            provider.rate_limited_until > clock_timestamp()
        FROM billing_payment_attempts AS attempt
        INNER JOIN billing_gateway_accounts AS account
            ON account.id = attempt.gateway_account_id
        INNER JOIN billing_gateway_provider_rate_limits AS provider
            ON provider.provider_key = account.provider_key
        WHERE attempt.id = $1
        "#,
    )
    .bind(fixture.command.attempt_id().as_uuid())
    .fetch_one(&fixture.database.pool)
    .await?;
    assert_eq!(state.0, "failed");
    assert_eq!(
        state.1.as_deref(),
        Some("gateway_provider_rate_limited_before_submission")
    );
    assert!(state.2.is_none());
    assert!(!state.3);
    assert!(state.4);
    fixture.cleanup().await
}

#[tokio::test]
async fn foreground_fresh_cooldown_after_readiness_prevents_the_admitted_sale()
-> Result<(), Box<dyn Error>> {
    let fixture = enrollment_fixture("svc_fresh_stop", false, false, false).await?;
    let gateway = Arc::new(CooldownDuringReadinessGateway {
        pool: fixture.database.pool.clone(),
        account_id: fixture.gateway_account.gateway_account_id,
    });
    let resolved = scripted_resolved_gateway(fixture.gateway_account, gateway);
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
        resolver,
        admission,
        Arc::new(fixture.coordinator.clone()),
    );

    let error = service
        .enroll(fixture.command.clone())
        .await
        .expect_err("fresh cooldown must close the one-shot sale boundary");
    assert!(matches!(
        error,
        SubscriptionBillingServiceError::GatewayMutationCooldown {
            scope: GatewayMutationCooldownScope::Account
        }
    ));
    let attempt: (String, Option<String>, bool) = sqlx::query_as(
        r#"
        SELECT status, resolution_code, submitted_at IS NOT NULL
        FROM billing_payment_attempts
        WHERE id = $1
        "#,
    )
    .bind(fixture.command.attempt_id().as_uuid())
    .fetch_one(&fixture.database.pool)
    .await?;
    assert_eq!(attempt.0, "failed");
    assert_eq!(
        attempt.1.as_deref(),
        Some("gateway_account_mutation_cooldown_before_submission")
    );
    assert!(!attempt.2);
    fixture.cleanup().await
}
