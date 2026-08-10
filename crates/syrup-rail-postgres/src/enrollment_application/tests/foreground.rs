use super::*;

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
    let initial_gateway = Arc::new(ScriptedGateway::new(Ok(approved_outcome(
        "txn_renewal_initial",
    ))));
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
    );
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

    let gateway = Arc::new(ScriptedGateway::new(Ok(approved_outcome(
        "txn_renewal_recurring",
    ))));
    let resolver = Arc::new(StaticResolver {
        gateway: scripted_resolved_gateway(fixture.gateway_account, Arc::clone(&gateway)),
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
    let command = ChargeRenewal::new(
        fixture.command.billing_scope_id(),
        subscription_id,
        period_start_at,
    );
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
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
    assert_eq!(admission.calls.load(Ordering::SeqCst), 0);

    assert!(matches!(
        service.renew(command).await?,
        SubscriptionRenewalOutcome::Noop
    ));
    assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 1);
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
    let initial_gateway = Arc::new(ScriptedGateway::new(Ok(approved_outcome(
        "txn_recovery_initial",
    ))));
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
    );
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

    let recovery_gateway = Arc::new(ScriptedGateway::new(Ok(approved_outcome_with_reference(
        Some("txn_recovery_approved"),
        "vault_recovery",
    ))));
    let resolver = Arc::new(StaticResolver {
        gateway: scripted_resolved_gateway(fixture.gateway_account, Arc::clone(&recovery_gateway)),
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
    let command = RecoverSubscriptionPayment::new(
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

    let result = service.recover(command.clone()).await?;
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

    let replay = service.recover(command.clone()).await?;
    assert_eq!(replay, result);
    assert_eq!(recovery_gateway.sale_calls.load(Ordering::SeqCst), 1);
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
    assert_eq!(admission.calls.load(Ordering::SeqCst), 1);
    let next_due_at = Utc::now() - ChronoDuration::hours(12);
    sqlx::query(
        r#"
        UPDATE billing_subscriptions
        SET current_period_end_at = $2,
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
    assert!(matches!(
        service.recover(command).await,
        Err(SubscriptionEnrollmentServiceError::IdempotencyConflict)
    ));
    assert_eq!(recovery_gateway.sale_calls.load(Ordering::SeqCst), 1);
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
    let initial_gateway = Arc::new(ScriptedGateway::new(Ok(approved_outcome_with_reference(
        Some("txn_method_initial"),
        "vault_method_old",
    ))));
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
    );
    let initial = initial_service.enroll(fixture.command.clone()).await?;
    let subscription = initial
        .subscription()
        .expect("approved enrollment has subscription");
    let subscription_id = subscription.id();
    let old_method_id = subscription.payment_method_id();

    let gateway = Arc::new(ScriptedGateway::for_stored_method(Ok(
        approved_outcome_with_reference(Some("txn_method_new"), "vault_method_new"),
    )));
    let resolver = Arc::new(StaticResolver {
        gateway: scripted_resolved_gateway(fixture.gateway_account, Arc::clone(&gateway)),
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
    let command = ReplaceSubscriptionPaymentMethod::new(
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

    let result = service.replace_payment_method(command.clone()).await?;
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
    assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 0);
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
    assert_eq!(admission.calls.load(Ordering::SeqCst), 1);

    let replay = service.replace_payment_method(command).await?;
    assert_eq!(replay, result);
    assert_eq!(gateway.store_calls.load(Ordering::SeqCst), 1);
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
    assert_eq!(admission.calls.load(Ordering::SeqCst), 1);

    let additional_transaction_id = "txn_method_unexpected_additional";
    let reconciled = service
        .apply_reconciled_outcome(
            result.attempt().identity().billing_scope_id(),
            result.attempt().identity().attempt_id(),
            &approved_outcome_with_reference(
                Some(additional_transaction_id),
                "vault_method_unexpected_additional",
            ),
        )
        .await?;
    assert_eq!(
        reconciled.attempt().status(),
        PaymentAttemptStatus::Approved
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
    assert!(matches!(
        events[1],
        BillingEvent::PaymentMethodChanged { .. }
    ));
    drop(events);
    fixture.cleanup().await
}

#[tokio::test]
async fn foreground_service_pre_reservation_cooldown_creates_no_attempt_or_provider_io()
-> Result<(), Box<dyn Error>> {
    let fixture = enrollment_fixture("service_cooldown", false, false, false).await?;
    sqlx::query(
        r#"
        UPDATE billing_gateway_accounts
        SET mutation_rate_limited_until = clock_timestamp() + interval '1 minute'
        WHERE id = $1
        "#,
    )
    .bind(fixture.gateway_account.gateway_account_id)
    .execute(&fixture.database.pool)
    .await?;
    let gateway = Arc::new(ScriptedGateway::new(Ok(approved_outcome(
        "txn_must_not_submit",
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

    let error = service
        .enroll(fixture.command.clone())
        .await
        .expect_err("active local cooldown must reject before reservation");
    assert!(matches!(
        error,
        SubscriptionEnrollmentServiceError::GatewayMutationCooldown {
            scope: GatewayMutationCooldownScope::Account
        }
    ));
    assert_eq!(admission.calls.load(Ordering::SeqCst), 1);
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 0);
    assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 0);
    let attempts: i64 = sqlx::query_scalar("SELECT count(*) FROM billing_payment_attempts")
        .fetch_one(&fixture.database.pool)
        .await?;
    assert_eq!(attempts, 0);
    fixture.cleanup().await
}

#[tokio::test]
async fn foreground_service_resumes_the_durable_attempt_not_the_retry_candidate_id()
-> Result<(), Box<dyn Error>> {
    let fixture = enrollment_fixture("service_resume", false, false, false).await?;
    let original_attempt_id = fixture.command.attempt_id();
    let mut transaction = fixture.database.pool.begin().await?;
    assert!(matches!(
        reserve_subscription_enrollment_in_transaction(
            &mut transaction,
            &TestOfferStore,
            &fixture.reservation,
        )
        .await?,
        SubscriptionEnrollmentReservationOutcome::Reserved(_)
    ));
    transaction.commit().await?;

    let retry = EnrollSubscription::new(
        syrup_rail::SubscriptionPaymentContext::new(
            PaymentAttemptId::new(Uuid::now_v7()),
            fixture.command.billing_scope_id(),
            fixture.command.subscriber_id(),
            fixture.command.gateway_configuration_id(),
            fixture.command.idempotency_key().clone(),
            fixture.command.payment_token().clone(),
            fixture.command.billing_contact().clone(),
        ),
        fixture.command.expected_terms().clone(),
    );
    let gateway = Arc::new(ScriptedGateway::new(Ok(approved_outcome(
        "txn_service_resume",
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
        resolver,
        admission.clone(),
        Arc::new(fixture.coordinator.clone()),
    );

    let result = service.enroll(retry).await?;
    assert_eq!(
        result.attempt().identity().attempt_id(),
        original_attempt_id
    );
    assert_eq!(result.attempt().status(), PaymentAttemptStatus::Approved);
    assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 1);
    assert_eq!(admission.calls.load(Ordering::SeqCst), 1);
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
        SubscriptionEnrollmentServiceError::GatewayMutationCooldown {
            scope: GatewayMutationCooldownScope::Provider
        }
    ));
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
    assert_eq!(admission.calls.load(Ordering::SeqCst), 1);
    let state: (String, Option<String>, bool, bool) = sqlx::query_as(
        r#"
        SELECT attempt.status, attempt.resolution_code,
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
    assert!(!state.2);
    assert!(state.3);
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
        SubscriptionEnrollmentServiceError::GatewayMutationCooldown {
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

#[tokio::test]
async fn foreground_stale_prepared_replay_expires_before_admission_or_live_terms()
-> Result<(), Box<dyn Error>> {
    let fixture = enrollment_fixture("svc_stale_replay", false, false, false).await?;
    let mut transaction = fixture.database.pool.begin().await?;
    assert!(matches!(
        reserve_subscription_enrollment_in_transaction(
            &mut transaction,
            &TestOfferStore,
            &fixture.reservation,
        )
        .await?,
        SubscriptionEnrollmentReservationOutcome::Reserved(_)
    ));
    transaction.commit().await?;
    sqlx::query(
        r#"
        UPDATE billing_payment_attempts
        SET created_at = clock_timestamp() - interval '30 minutes'
        WHERE id = $1
        "#,
    )
    .bind(fixture.command.attempt_id().as_uuid())
    .execute(&fixture.database.pool)
    .await?;
    sqlx::query("DELETE FROM host_subscription_offers")
        .execute(&fixture.database.pool)
        .await?;

    let gateway = Arc::new(ScriptedGateway::new(Ok(approved_outcome(
        "txn_stale_must_not_submit",
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
    assert_eq!(result.attempt().status(), PaymentAttemptStatus::Failed);
    assert_eq!(
        result.attempt().state().resolution_code(),
        Some(PaymentResolutionCode::SubscriptionInitialPreparedAttemptExpired)
    );
    assert_eq!(admission.calls.load(Ordering::SeqCst), 0);
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 0);
    assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 0);
    fixture.cleanup().await
}
