use super::*;

#[tokio::test]
async fn reconciled_unknown_payment_method_replacement_rejects_conflicting_identity_bundle()
-> Result<(), Box<dyn Error>> {
    let fixture = enrollment_fixture("repl_unk_conf", false, false, false).await?;
    let initial_gateway = Arc::new(ScriptedGateway::for_sale_with_readiness(
        [Ok(GatewayAccountMode::Test), Ok(GatewayAccountMode::Test)],
        Ok(approved_outcome_with_reference(
            Some("txn_method_unknown_initial"),
            "vault_method_unknown_initial",
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
    let subscription_id = initial
        .subscription()
        .expect("approved enrollment has subscription")
        .id();

    let replacement_gateway = Arc::new(ScriptedGateway::for_stored_method(Ok(
        approved_outcome_with_reference(Some("txn_unused_replacement"), "vault_unused_replacement"),
    )));
    let resolved_gateway =
        scripted_resolved_gateway(fixture.gateway_account, Arc::clone(&replacement_gateway));
    let service = SubscriptionBillingService::new(
        fixture.database.pool.clone(),
        Arc::new(TestOfferStore),
        Arc::new(StaticResolver {
            gateway: resolved_gateway.clone(),
            calls: AtomicUsize::new(0),
        }),
        Arc::new(PermitAdmission {
            calls: AtomicUsize::new(0),
        }),
        Arc::new(fixture.coordinator.clone()),
    )
    .with_required_gateway_account_mode(GatewayAccountMode::Test);
    let command = ReplaceSubscriptionPaymentMethod::new(
        syrup_rail::SubscriptionPaymentContext::new(
            PaymentAttemptId::new(Uuid::now_v7()),
            fixture.command.billing_scope_id(),
            fixture.command.subscriber_id(),
            fixture.command.gateway_configuration_id(),
            IdempotencyKey::new("unknown-replace-method-key")?,
            PaymentToken::new("opaque-unknown-replacement-token")?,
            fixture.command.billing_contact().clone(),
        ),
        fixture.command.plan_key().clone(),
    );
    let mut transaction = fixture.database.pool.begin().await?;
    let reservation = match reserve_subscription_payment_method_replacement_in_transaction(
        &mut transaction,
        &command,
        &resolved_gateway,
        GatewayAccountMode::Test,
    )
    .await?
    {
        SubscriptionPaymentMethodReplacementReservationOutcome::Reserved(reservation, _) => {
            *reservation
        }
        other => return Err(format!("unexpected replacement reservation: {other:?}").into()),
    };
    transaction.commit().await?;
    match admit_subscription_payment_method_replacement(&fixture.database.pool, &reservation)
        .await?
    {
        SubscriptionPaymentMethodReplacementAdmissionOutcome::Admitted(_) => {}
        other => return Err(format!("unexpected replacement admission: {other:?}").into()),
    }
    let unknown_outcome = GatewayPaymentOutcome::new(
        GatewayPaymentStatus::Unknown,
        ProcessorEvidence::new(
            syrup_rail::ProcessorApprovalEvidence::Unclassified,
            Some(GatewayTransactionId::new("txn_method_unknown_durable")?),
            Some(GatewayPaymentMethodReference::new(
                "vault_method_unknown_durable",
            )?),
            Some(GatewayDiagnostic::new("3")),
            Some(GatewayDiagnostic::new("400")),
            Some(GatewayDiagnostic::new("Processor outcome unknown")),
            Some(GatewayDiagnostic::new("unknown")),
            GatewayPaymentDescriptor::default(),
        ),
    );
    let unknown = apply_subscription_payment_method_replacement_gateway_outcome(
        &fixture.database.pool,
        &fixture.coordinator,
        &reservation,
        &unknown_outcome,
    )
    .await?;
    assert_eq!(unknown.attempt().status(), PaymentAttemptStatus::Unknown);

    let conflicting = service
        .apply_reconciled_outcome(
            unknown.attempt().identity().billing_scope_id(),
            unknown.attempt().identity().attempt_id(),
            &approved_outcome_with_reference(
                Some("txn_method_unknown_conflict"),
                "vault_method_unknown_conflict",
            ),
        )
        .await?;
    assert_eq!(
        conflicting.attempt().status(),
        PaymentAttemptStatus::Unknown
    );
    assert!(conflicting.subscription().is_none());
    assert_eq!(
        conflicting.observation_diagnostics(),
        &[
            GatewayPaymentDiagnostic::InvalidOrConflictingTransactionIdentifier,
            GatewayPaymentDiagnostic::InvalidOrConflictingPaymentMethodReference,
        ]
    );
    let preserved_identity: (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT gateway_transaction_id, gateway_payment_method_reference \
         FROM billing_payment_attempts WHERE id = $1",
    )
    .bind(unknown.attempt().identity().attempt_id().as_uuid())
    .fetch_one(&fixture.database.pool)
    .await?;
    assert_eq!(
        preserved_identity,
        (
            Some("txn_method_unknown_durable".to_owned()),
            Some("vault_method_unknown_durable".to_owned()),
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
    assert_eq!(current_method_reference, "vault_method_unknown_initial");
    assert_eq!(replacement_gateway.store_calls.load(Ordering::SeqCst), 0);
    let conflicting_charge: (String, Option<String>, String) = sqlx::query_as(
        "SELECT gateway_transaction_id, gateway_payment_method_reference, progression_state \
         FROM billing_processor_charges WHERE attempt_id = $1",
    )
    .bind(unknown.attempt().identity().attempt_id().as_uuid())
    .fetch_one(&fixture.database.pool)
    .await?;
    assert_eq!(
        conflicting_charge,
        (
            "txn_method_unknown_conflict".to_owned(),
            Some("vault_method_unknown_conflict".to_owned()),
            "reconciliation_required".to_owned(),
        ),
        "the conflicting approved observation must remain durable"
    );

    let matching = service
        .apply_reconciled_outcome(
            unknown.attempt().identity().billing_scope_id(),
            unknown.attempt().identity().attempt_id(),
            &approved_outcome_with_reference(
                Some("txn_method_unknown_durable"),
                "vault_method_unknown_durable",
            ),
        )
        .await?;
    assert_eq!(
        matching.attempt().status(),
        PaymentAttemptStatus::ReviewRequired
    );
    assert!(matching.subscription().is_none());
    assert!(matching.observation_diagnostics().is_empty());
    let charge_summary: (i64, i64) = sqlx::query_as(
        "SELECT count(*), count(*) FILTER (WHERE progression_state = 'reconciliation_required') \
         FROM billing_processor_charges WHERE attempt_id = $1",
    )
    .bind(unknown.attempt().identity().attempt_id().as_uuid())
    .fetch_one(&fixture.database.pool)
    .await?;
    assert_eq!(charge_summary, (2, 2));
    let unchanged_method_reference: String = sqlx::query_scalar(
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
    assert_eq!(unchanged_method_reference, "vault_method_unknown_initial");
    fixture.cleanup().await
}

#[tokio::test]
async fn reconciled_incomplete_replacement_approval_records_charge_before_parking()
-> Result<(), Box<dyn Error>> {
    let (fixture, reservation) =
        reconciled_payment_method_replacement_fixture("repl_incomplete").await?;
    let outcome =
        approved_outcome_with_optional_reference(Some("txn_repl_incomplete_reconciled"), None)
            .with_diagnostics(vec![
                GatewayPaymentDiagnostic::MissingPaymentMethodReference,
            ]);

    let result = apply_reconciled_subscription_payment_method_replacement_gateway_outcome(
        &fixture.database.pool,
        &fixture.coordinator,
        reservation.identity().billing_scope_id(),
        reservation.identity().attempt_id(),
        &outcome,
    )
    .await?;

    assert_eq!(
        result.attempt().status(),
        PaymentAttemptStatus::ReviewRequired
    );
    assert!(result.subscription().is_none());
    assert_eq!(
        result.observation_diagnostics(),
        &[GatewayPaymentDiagnostic::MissingPaymentMethodReference]
    );
    let charge: (i64, Option<String>, String) = sqlx::query_as(
        "SELECT count(*), min(gateway_payment_method_reference), min(progression_state) \
         FROM billing_processor_charges WHERE attempt_id = $1",
    )
    .bind(reservation.identity().attempt_id().as_uuid())
    .fetch_one(&fixture.database.pool)
    .await?;
    assert_eq!(
        charge,
        (1, None, "pending".to_owned()),
        "parking must not bypass the canonical processor-charge writer"
    );
    fixture.cleanup().await
}

#[tokio::test]
async fn reconciled_replacement_approval_survives_persistent_coordinator_failure()
-> Result<(), Box<dyn Error>> {
    let (fixture, reservation) =
        reconciled_payment_method_replacement_fixture("repl_coord_fail").await?;
    let failing_coordinator = TestCoordinator {
        fail_begin: true,
        ..fixture.coordinator.clone()
    };
    let outcome = approved_outcome_with_reference(
        Some("txn_repl_coord_fail_reconciled"),
        "vault_repl_coord_fail_reconciled",
    );

    let result = apply_reconciled_subscription_payment_method_replacement_gateway_outcome(
        &fixture.database.pool,
        &failing_coordinator,
        reservation.identity().billing_scope_id(),
        reservation.identity().attempt_id(),
        &outcome,
    )
    .await?;

    assert_eq!(
        result.attempt().status(),
        PaymentAttemptStatus::ReviewRequired
    );
    assert!(result.subscription().is_none());
    let charge: (i64, Option<String>, String) = sqlx::query_as(
        "SELECT count(*), min(gateway_payment_method_reference), min(progression_state) \
         FROM billing_processor_charges WHERE attempt_id = $1",
    )
    .bind(reservation.identity().attempt_id().as_uuid())
    .fetch_one(&fixture.database.pool)
    .await?;
    assert_eq!(
        charge,
        (
            1,
            Some("vault_repl_coord_fail_reconciled".to_owned()),
            "pending".to_owned(),
        ),
        "the pool fallback must retain approved evidence without the host coordinator"
    );
    fixture.cleanup().await
}

#[tokio::test]
async fn reconciled_replacement_never_restores_quarantined_durable_identity()
-> Result<(), Box<dyn Error>> {
    let (fixture, reservation) =
        reconciled_payment_method_replacement_fixture("repl_quarantine").await?;
    let durable_outcome = GatewayPaymentOutcome::new(
        GatewayPaymentStatus::Unknown,
        ProcessorEvidence::new(
            syrup_rail::ProcessorApprovalEvidence::Unclassified,
            Some(GatewayTransactionId::new("txn_repl_quarantine_durable")?),
            Some(GatewayPaymentMethodReference::new(
                "vault_repl_quarantine_durable",
            )?),
            Some(GatewayDiagnostic::new("3")),
            Some(GatewayDiagnostic::new("400")),
            Some(GatewayDiagnostic::new("Processor outcome unknown")),
            Some(GatewayDiagnostic::new("unknown")),
            GatewayPaymentDescriptor::default(),
        ),
    );
    let durable = apply_subscription_payment_method_replacement_gateway_outcome(
        &fixture.database.pool,
        &fixture.coordinator,
        &reservation,
        &durable_outcome,
    )
    .await?;
    assert_eq!(durable.attempt().status(), PaymentAttemptStatus::Unknown);
    let failing_coordinator = TestCoordinator {
        fail_begin: true,
        ..fixture.coordinator.clone()
    };

    for diagnostic in [
        GatewayPaymentDiagnostic::InvalidOrConflictingTransactionIdentifier,
        GatewayPaymentDiagnostic::InvalidOrConflictingPaymentMethodReference,
    ] {
        let observation = approved_outcome_with_reference(
            Some("txn_repl_quarantine_durable"),
            "vault_repl_quarantine_durable",
        )
        .with_diagnostics(vec![diagnostic]);
        let result = apply_reconciled_subscription_payment_method_replacement_gateway_outcome(
            &fixture.database.pool,
            &failing_coordinator,
            reservation.identity().billing_scope_id(),
            reservation.identity().attempt_id(),
            &observation,
        )
        .await?;

        assert_eq!(result.attempt().status(), PaymentAttemptStatus::Unknown);
        assert!(result.subscription().is_none());
        assert_eq!(result.observation_diagnostics(), &[diagnostic]);
    }

    let durable_identity: (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT gateway_transaction_id, gateway_payment_method_reference \
         FROM billing_payment_attempts WHERE id = $1",
    )
    .bind(reservation.identity().attempt_id().as_uuid())
    .fetch_one(&fixture.database.pool)
    .await?;
    assert_eq!(
        durable_identity,
        (
            Some("txn_repl_quarantine_durable".to_owned()),
            Some("vault_repl_quarantine_durable".to_owned()),
        )
    );
    let charge_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM billing_processor_charges WHERE attempt_id = $1")
            .bind(reservation.identity().attempt_id().as_uuid())
            .fetch_one(&fixture.database.pool)
            .await?;
    assert_eq!(
        charge_count, 0,
        "quarantined identity is not charge authority"
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
    .bind(reservation.subscription_id().as_uuid())
    .fetch_one(&fixture.database.pool)
    .await?;
    assert_eq!(current_method_reference, "vault_repl_quarantine_initial");
    fixture.cleanup().await
}

#[tokio::test]
async fn reconciled_approval_cannot_inherit_a_durable_payment_method_reference()
-> Result<(), Box<dyn Error>> {
    let (fixture, reservation) =
        reconciled_payment_method_replacement_fixture("repl_no_inherit").await?;
    let durable_outcome = GatewayPaymentOutcome::new(
        GatewayPaymentStatus::Unknown,
        ProcessorEvidence::new(
            syrup_rail::ProcessorApprovalEvidence::Unclassified,
            Some(GatewayTransactionId::new("txn_repl_no_inherit")?),
            Some(GatewayPaymentMethodReference::new(
                "vault_repl_no_inherit_durable",
            )?),
            Some(GatewayDiagnostic::new("3")),
            Some(GatewayDiagnostic::new("400")),
            Some(GatewayDiagnostic::new("Processor outcome unknown")),
            Some(GatewayDiagnostic::new("unknown")),
            GatewayPaymentDescriptor::default(),
        ),
    );
    apply_subscription_payment_method_replacement_gateway_outcome(
        &fixture.database.pool,
        &fixture.coordinator,
        &reservation,
        &durable_outcome,
    )
    .await?;
    let observation = approved_outcome_with_optional_reference(Some("txn_repl_no_inherit"), None)
        .with_diagnostics(vec![
            GatewayPaymentDiagnostic::MissingPaymentMethodReference,
        ]);

    let result = apply_reconciled_subscription_payment_method_replacement_gateway_outcome(
        &fixture.database.pool,
        &fixture.coordinator,
        reservation.identity().billing_scope_id(),
        reservation.identity().attempt_id(),
        &observation,
    )
    .await?;

    assert_eq!(
        result.attempt().status(),
        PaymentAttemptStatus::ReviewRequired
    );
    assert!(result.subscription().is_none());
    let current_method_reference: String = sqlx::query_scalar(
        r#"
        SELECT method.gateway_payment_method_reference
        FROM billing_subscriptions AS subscription
        INNER JOIN billing_payment_methods AS method
            ON method.id = subscription.payment_method_id
        WHERE subscription.id = $1
        "#,
    )
    .bind(reservation.subscription_id().as_uuid())
    .fetch_one(&fixture.database.pool)
    .await?;
    assert_eq!(current_method_reference, "vault_repl_no_inherit_initial");
    let durable_identity: (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT gateway_transaction_id, gateway_payment_method_reference \
         FROM billing_payment_attempts WHERE id = $1",
    )
    .bind(reservation.identity().attempt_id().as_uuid())
    .fetch_one(&fixture.database.pool)
    .await?;
    assert_eq!(
        durable_identity,
        (
            Some("txn_repl_no_inherit".to_owned()),
            Some("vault_repl_no_inherit_durable".to_owned()),
        ),
        "parking the sparse approval must preserve the prior attempt observation"
    );
    let charge: (Option<String>, Option<String>, String) = sqlx::query_as(
        "SELECT gateway_transaction_id, gateway_payment_method_reference, progression_state \
         FROM billing_processor_charges WHERE attempt_id = $1",
    )
    .bind(reservation.identity().attempt_id().as_uuid())
    .fetch_one(&fixture.database.pool)
    .await?;
    assert_eq!(
        charge,
        (
            Some("txn_repl_no_inherit".to_owned()),
            None,
            "pending".to_owned(),
        )
    );
    fixture.cleanup().await
}

#[tokio::test]
async fn unanchored_reconciliation_preserves_durable_identity_without_splicing_decisions()
-> Result<(), Box<dyn Error>> {
    let (fixture, reservation) =
        reconciled_payment_method_replacement_fixture("repl_no_splice").await?;
    let durable_outcome = GatewayPaymentOutcome::new(
        GatewayPaymentStatus::Unknown,
        ProcessorEvidence::new(
            syrup_rail::ProcessorApprovalEvidence::Unclassified,
            None,
            Some(GatewayPaymentMethodReference::new(
                "vault_repl_no_splice_durable",
            )?),
            None,
            None,
            None,
            Some(GatewayDiagnostic::new("pendingsettlement")),
            GatewayPaymentDescriptor::default(),
        ),
    );
    apply_subscription_payment_method_replacement_gateway_outcome(
        &fixture.database.pool,
        &fixture.coordinator,
        &reservation,
        &durable_outcome,
    )
    .await?;
    let observation = GatewayPaymentOutcome::new(
        GatewayPaymentStatus::Unknown,
        ProcessorEvidence::new(
            syrup_rail::ProcessorApprovalEvidence::Unclassified,
            Some(GatewayTransactionId::new("txn_repl_no_splice_observed")?),
            None,
            None,
            None,
            None,
            None,
            GatewayPaymentDescriptor::default(),
        ),
    );

    let result = apply_reconciled_subscription_payment_method_replacement_gateway_outcome(
        &fixture.database.pool,
        &fixture.coordinator,
        reservation.identity().billing_scope_id(),
        reservation.identity().attempt_id(),
        &observation,
    )
    .await?;

    assert_eq!(result.attempt().status(), PaymentAttemptStatus::Unknown);
    assert_eq!(
        result.observation_diagnostics(),
        &[GatewayPaymentDiagnostic::InvalidOrConflictingTransactionIdentifier]
    );
    let evidence = result.attempt().state().processor_evidence();
    assert!(evidence.transaction_id().is_none());
    assert_eq!(
        evidence
            .payment_method_reference()
            .map(GatewayPaymentMethodReference::expose),
        Some("vault_repl_no_splice_durable")
    );
    assert!(evidence.condition().is_none());
    assert!(!evidence.indicates_approved_payment());
    let charge_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM billing_processor_charges WHERE attempt_id = $1")
            .bind(reservation.identity().attempt_id().as_uuid())
            .fetch_one(&fixture.database.pool)
            .await?;
    assert_eq!(charge_count, 0);
    fixture.cleanup().await
}

#[tokio::test]
async fn sparse_terminal_reconciliation_is_not_promoted_to_an_exact_replay()
-> Result<(), Box<dyn Error>> {
    let (fixture, reservation) =
        reconciled_payment_method_replacement_fixture("repl_term_sparse").await?;
    let applied = apply_reconciled_subscription_payment_method_replacement_gateway_outcome(
        &fixture.database.pool,
        &fixture.coordinator,
        reservation.identity().billing_scope_id(),
        reservation.identity().attempt_id(),
        &approved_outcome_with_reference(
            Some("txn_repl_terminal_sparse"),
            "vault_repl_terminal_sparse",
        ),
    )
    .await?;
    assert_eq!(applied.attempt().status(), PaymentAttemptStatus::Approved);
    let sparse_observation =
        approved_outcome_with_optional_reference(None, Some("vault_repl_terminal_sparse"))
            .with_diagnostics(vec![GatewayPaymentDiagnostic::MissingTransactionIdentifier]);

    let replay = apply_reconciled_subscription_payment_method_replacement_gateway_outcome(
        &fixture.database.pool,
        &fixture.coordinator,
        reservation.identity().billing_scope_id(),
        reservation.identity().attempt_id(),
        &sparse_observation,
    )
    .await?;

    assert_eq!(replay.attempt().status(), PaymentAttemptStatus::Approved);
    assert!(replay.subscription().is_some());
    assert_eq!(
        replay.observation_diagnostics(),
        &[GatewayPaymentDiagnostic::MissingTransactionIdentifier]
    );
    let sparse_reconciliation_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM billing_processor_charges \
         WHERE attempt_id = $1 AND gateway_transaction_id IS NULL \
             AND progression_state = 'reconciliation_required'",
    )
    .bind(reservation.identity().attempt_id().as_uuid())
    .fetch_one(&fixture.database.pool)
    .await?;
    assert_eq!(sparse_reconciliation_count, 1);
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
