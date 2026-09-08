// agentic-loc-exception: Preserve the 0.5.2 test layout for this metadata patch; master already splits application/mode_and_evidence.rs for 0.6.0.

use super::*;

mod metadata_upgrade;
mod retry_and_parking;

#[tokio::test]
async fn foreground_duplicate_diagnostic_crosses_the_application_boundary()
-> Result<(), Box<dyn Error>> {
    let fixture = application_fixture("duplicate_diag", false, false).await?;
    let result = apply_subscription_enrollment_gateway_outcome(
        &fixture.database.pool,
        &fixture.coordinator,
        &fixture.reservation,
        &processor_duplicate_outcome(),
    )
    .await?;

    assert_eq!(result.status(), PaymentAttemptStatus::Unknown);
    assert_eq!(
        result.observation_diagnostics(),
        &[GatewayPaymentDiagnostic::ProcessorReportedDuplicate]
    );
    assert_eq!(
        result
            .processor_evidence()
            .response_code()
            .map(GatewayDiagnostic::expose),
        Some("430")
    );
    fixture.cleanup().await
}

#[tokio::test]
async fn indeterminate_processor_error_exits_empty_reconciliation_through_manual_review()
-> Result<(), Box<dyn Error>> {
    let fixture = application_fixture("indeterminate", false, false).await?;
    let outcome = indeterminate_processor_error_outcome();
    assert_eq!(outcome.status(), GatewayPaymentStatus::Unknown);
    assert!(outcome.approved_evidence().is_none());
    let result = apply_subscription_enrollment_gateway_outcome(
        &fixture.database.pool,
        &fixture.coordinator,
        &fixture.reservation,
        &outcome,
    )
    .await?;

    assert_eq!(result.status(), PaymentAttemptStatus::Unknown);
    assert_eq!(
        result.observation_diagnostics(),
        &[GatewayPaymentDiagnostic::IndeterminatePaymentOutcome]
    );
    sqlx::query(
        "UPDATE billing_payment_attempts \
         SET created_at = clock_timestamp() - interval '31 minutes', \
             submitted_at = clock_timestamp() - interval '31 minutes' \
         WHERE id = $1",
    )
    .bind(result.attempt().identity().attempt_id().as_uuid())
    .execute(&fixture.database.pool)
    .await?;

    assert!(
        crate::apply_exact_query_observation(
            &fixture.database.pool,
            result.attempt(),
            crate::ExactQueryObservation::NoTransaction,
        )
        .await?
    );
    let status: String =
        sqlx::query_scalar("SELECT status FROM billing_payment_attempts WHERE id = $1")
            .bind(result.attempt().identity().attempt_id().as_uuid())
            .fetch_one(&fixture.database.pool)
            .await?;
    assert_eq!(status, "review_required");

    let manual = fail_review_required_attempt(
        &fixture.database.pool,
        &fixture.coordinator,
        &NeverManualFailureHost,
        result.attempt().identity().attempt_id(),
    )
    .await?;
    let ManualAttemptFailureOutcome::Failed(failed) = manual else {
        return Err(format!("expected manual failure, got {manual:?}").into());
    };
    assert_eq!(failed.status(), PaymentAttemptStatus::Failed);
    let status: String =
        sqlx::query_scalar("SELECT status FROM billing_payment_attempts WHERE id = $1")
            .bind(result.attempt().identity().attempt_id().as_uuid())
            .fetch_one(&fixture.database.pool)
            .await?;
    assert_eq!(status, "failed");
    fixture.cleanup().await
}

#[tokio::test]
async fn discounted_approval_applies_one_atomic_subscription_event_and_replays()
-> Result<(), Box<dyn Error>> {
    let fixture = application_fixture("enroll_apply", true, false).await?;
    let outcome = approved_outcome("txn_application_approved");
    let result = apply_subscription_enrollment_gateway_outcome(
        &fixture.database.pool,
        &fixture.coordinator,
        &fixture.reservation,
        &outcome,
    )
    .await?;
    assert_eq!(result.attempt().status(), PaymentAttemptStatus::Approved);
    assert_eq!(
        result
            .subscription()
            .expect("applied subscription")
            .recurring_charge()
            .cents(),
        800
    );
    let events = fixture.coordinator.events.lock().await.clone();
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].semantic_key(),
        BillingEventKey::SubscriptionStarted(
            result.subscription().expect("applied subscription").id()
        )
    );
    assert!(matches!(
        &events[0],
        BillingEvent::SubscriptionStarted { charge, .. } if charge.cents() == 800
    ));
    let rows: (i64, i64, i64, String, i32) = sqlx::query_as(
        r#"
        SELECT
            (SELECT count(*) FROM billing_payment_methods),
            (SELECT count(*) FROM billing_subscriptions),
            (SELECT count(*) FROM billing_processor_charges),
            (SELECT status FROM billing_subscription_discount_claims LIMIT 1),
            (SELECT periods_applied FROM billing_subscription_discounts LIMIT 1)
        "#,
    )
    .fetch_one(&fixture.database.pool)
    .await?;
    assert_eq!(rows, (1, 1, 1, "applied".to_owned(), 1));
    let progression: String =
        sqlx::query_scalar("SELECT progression_state FROM billing_processor_charges")
            .fetch_one(&fixture.database.pool)
            .await?;
    assert_eq!(progression, "applied");

    let identical_replay = apply_subscription_enrollment_gateway_outcome(
        &fixture.database.pool,
        &fixture.coordinator,
        &fixture.reservation,
        &outcome,
    )
    .await?;
    assert_eq!(identical_replay, result);
    assert_eq!(fixture.coordinator.events.lock().await.len(), 1);

    let diagnostic_replay = apply_subscription_enrollment_gateway_outcome(
        &fixture.database.pool,
        &fixture.coordinator,
        &fixture.reservation,
        &processor_duplicate_outcome(),
    )
    .await?;
    assert_eq!(
        diagnostic_replay.attempt().status(),
        PaymentAttemptStatus::Approved
    );
    assert_eq!(
        diagnostic_replay.observation_diagnostics(),
        &[GatewayPaymentDiagnostic::ProcessorReportedDuplicate],
        "a current observation annotates but never overrides the durable result"
    );
    assert_eq!(fixture.coordinator.events.lock().await.len(), 1);
    fixture.cleanup().await
}

#[tokio::test]
async fn approved_application_projects_the_locked_attempt_not_mismatched_reservation_terms()
-> Result<(), Box<dyn Error>> {
    let fixture = application_fixture("durable_terms", false, false).await?;
    let mismatched_command = EnrollSubscription::new(
        syrup_rail::SubscriptionPaymentContext::new(
            fixture.command.attempt_id(),
            fixture.command.billing_scope_id(),
            fixture.command.subscriber_id(),
            fixture.command.gateway_configuration_id(),
            fixture.command.idempotency_key().clone(),
            fixture.command.payment_token().clone(),
            fixture.command.billing_contact().clone(),
        ),
        SubscriptionEnrollmentExpectedTerms::full_price(immediate_offer(
            PlanKey::new("base_subscription")?,
            ChargeAmount::new(700, CurrencyCode::new("USD")?)?,
        )),
    );
    let gateway = scripted_resolved_gateway(fixture.gateway_account, Arc::new(NeverCalledGateway));
    let mismatched_reservation = SubscriptionEnrollmentReservation::from_command_for_attempt(
        &mismatched_command,
        &gateway,
        fixture.command.attempt_id(),
        GatewayAccountMode::Live,
    )?;
    assert_eq!(
        mismatched_reservation
            .expected_terms()
            .initial_charge()
            .cents(),
        700
    );

    let result = apply_subscription_enrollment_gateway_outcome(
        &fixture.database.pool,
        &fixture.coordinator,
        &mismatched_reservation,
        &approved_outcome("txn_durable_terms"),
    )
    .await?;

    let subscription = result.subscription().expect("applied subscription");
    assert_eq!(result.attempt().request().amount().cents(), 1_000);
    assert_eq!(subscription.recurring_charge().cents(), 1_000);
    assert_eq!(fixture.coordinator.events.lock().await.len(), 1);
    fixture.cleanup().await
}

#[tokio::test]
async fn reconciled_initial_approval_uses_durable_attempt_after_configuration_rotation()
-> Result<(), Box<dyn Error>> {
    let fixture = application_fixture("rec_initial", false, false).await?;
    sqlx::query(
        r#"
        UPDATE billing_gateway_accounts
        SET gateway_configuration_id = $2, updated_at = clock_timestamp()
        WHERE id = $1
        "#,
    )
    .bind(fixture.gateway_account.gateway_account_id)
    .bind(Uuid::now_v7())
    .execute(&fixture.database.pool)
    .await?;
    let outcome = approved_outcome("txn_reconciled_approved");

    let result = apply_reconciled_subscription_enrollment_gateway_outcome(
        &fixture.database.pool,
        &fixture.coordinator,
        fixture.command.billing_scope_id(),
        fixture.command.attempt_id(),
        &outcome,
    )
    .await?;
    assert_eq!(result.attempt().status(), PaymentAttemptStatus::Approved);
    assert!(result.subscription().is_some());
    assert_eq!(fixture.coordinator.events.lock().await.len(), 1);

    let replay = apply_reconciled_subscription_enrollment_gateway_outcome(
        &fixture.database.pool,
        &fixture.coordinator,
        fixture.command.billing_scope_id(),
        fixture.command.attempt_id(),
        &outcome,
    )
    .await?;
    assert_eq!(replay, result);
    assert_eq!(fixture.coordinator.events.lock().await.len(), 1);
    fixture.cleanup().await
}

#[tokio::test]
async fn reconciled_initial_outcomes_cannot_replace_an_established_processor_identity()
-> Result<(), Box<dyn Error>> {
    let fixture = application_fixture("rec_init_id", false, false).await?;
    let unknown = GatewayPaymentOutcome::new(
        GatewayPaymentStatus::Unknown,
        ProcessorEvidence::new(
            Some(GatewayTransactionId::new("txn_initial_durable")?),
            Some(GatewayPaymentMethodReference::new("vault_initial_durable")?),
            Some(GatewayDiagnostic::new("3")),
            Some(GatewayDiagnostic::new("400")),
            Some(GatewayDiagnostic::new("Processor outcome unknown")),
            Some(GatewayDiagnostic::new("unknown")),
            GatewayPaymentDescriptor::default(),
        ),
    );
    let durable = apply_subscription_enrollment_gateway_outcome(
        &fixture.database.pool,
        &fixture.coordinator,
        &fixture.reservation,
        &unknown,
    )
    .await?;
    assert_eq!(durable.attempt().status(), PaymentAttemptStatus::Unknown);

    let conflicting_decline = GatewayPaymentOutcome::new(
        GatewayPaymentStatus::Declined,
        ProcessorEvidence::new(
            Some(GatewayTransactionId::new("txn_initial_conflict")?),
            None,
            Some(GatewayDiagnostic::new("2")),
            Some(GatewayDiagnostic::new("200")),
            Some(GatewayDiagnostic::new("Declined")),
            Some(GatewayDiagnostic::new("declined")),
            GatewayPaymentDescriptor::default(),
        ),
    );
    let declined = apply_reconciled_subscription_enrollment_gateway_outcome(
        &fixture.database.pool,
        &fixture.coordinator,
        fixture.command.billing_scope_id(),
        fixture.command.attempt_id(),
        &conflicting_decline,
    )
    .await?;
    assert_eq!(declined.attempt().status(), PaymentAttemptStatus::Unknown);
    assert_eq!(
        declined.observation_diagnostics(),
        &[GatewayPaymentDiagnostic::InvalidOrConflictingTransactionIdentifier]
    );

    let conflicting_approval =
        approved_outcome_with_reference(Some("txn_initial_durable"), "vault_initial_conflict");
    let approved = apply_reconciled_subscription_enrollment_gateway_outcome(
        &fixture.database.pool,
        &fixture.coordinator,
        fixture.command.billing_scope_id(),
        fixture.command.attempt_id(),
        &conflicting_approval,
    )
    .await?;
    assert_eq!(approved.attempt().status(), PaymentAttemptStatus::Unknown);
    assert!(approved.subscription().is_none());
    assert_eq!(
        approved.observation_diagnostics(),
        &[GatewayPaymentDiagnostic::InvalidOrConflictingPaymentMethodReference]
    );
    let identity: (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT gateway_transaction_id, gateway_payment_method_reference \
         FROM billing_payment_attempts WHERE id = $1",
    )
    .bind(fixture.command.attempt_id().as_uuid())
    .fetch_one(&fixture.database.pool)
    .await?;
    assert_eq!(
        identity,
        (
            Some("txn_initial_durable".to_owned()),
            Some("vault_initial_durable".to_owned()),
        )
    );
    let conflicting_charge: (String, Option<String>, String) = sqlx::query_as(
        "SELECT gateway_transaction_id, gateway_payment_method_reference, progression_state \
         FROM billing_processor_charges WHERE attempt_id = $1",
    )
    .bind(fixture.command.attempt_id().as_uuid())
    .fetch_one(&fixture.database.pool)
    .await?;
    assert_eq!(
        conflicting_charge,
        (
            "txn_initial_durable".to_owned(),
            Some("vault_initial_conflict".to_owned()),
            "reconciliation_required".to_owned(),
        )
    );
    assert!(fixture.coordinator.events.lock().await.is_empty());
    fixture.cleanup().await
}

#[tokio::test]
async fn unresolved_same_transaction_approval_with_immutable_conflicting_metadata_fails_visibly()
-> Result<(), Box<dyn Error>> {
    let fixture = application_fixture("rec_immut_id", false, false).await?;
    let transaction_id = "txn_initial_immutable";
    let durable_observation = GatewayPaymentOutcome::new(
        GatewayPaymentStatus::Unknown,
        approved_outcome_with_reference(Some(transaction_id), "vault_initial_immutable_durable")
            .evidence()
            .clone(),
    );
    let durable = apply_subscription_enrollment_gateway_outcome(
        &fixture.database.pool,
        &fixture.coordinator,
        &fixture.reservation,
        &durable_observation,
    )
    .await?;
    assert_eq!(durable.attempt().status(), PaymentAttemptStatus::Unknown);

    let error = apply_reconciled_subscription_enrollment_gateway_outcome(
        &fixture.database.pool,
        &fixture.coordinator,
        fixture.command.billing_scope_id(),
        fixture.command.attempt_id(),
        &approved_outcome_with_reference(Some(transaction_id), "vault_initial_immutable_conflict"),
    )
    .await
    .expect_err("an immutable contradictory observation must not be silently discarded");
    assert!(matches!(
        error,
        SubscriptionEnrollmentApplicationError::InvalidState(
            "processor charge replay evidence changed"
        )
    ));
    let charge: (Option<String>, Option<String>, String) = sqlx::query_as(
        "SELECT gateway_transaction_id, gateway_payment_method_reference, progression_state \
         FROM billing_processor_charges WHERE attempt_id = $1",
    )
    .bind(fixture.command.attempt_id().as_uuid())
    .fetch_one(&fixture.database.pool)
    .await?;
    assert_eq!(
        charge,
        (
            Some(transaction_id.to_owned()),
            Some("vault_initial_immutable_durable".to_owned()),
            "pending".to_owned(),
        )
    );
    fixture.cleanup().await
}

#[tokio::test]
async fn terminal_initial_reconciliation_reports_conflicts_without_rewriting_the_winner()
-> Result<(), Box<dyn Error>> {
    let fixture = application_fixture("term_init_id", false, false).await?;
    let winner = apply_subscription_enrollment_gateway_outcome(
        &fixture.database.pool,
        &fixture.coordinator,
        &fixture.reservation,
        &approved_outcome("txn_initial_winner"),
    )
    .await?;
    assert_eq!(winner.attempt().status(), PaymentAttemptStatus::Approved);

    let same_transaction_conflict = apply_reconciled_subscription_enrollment_gateway_outcome(
        &fixture.database.pool,
        &fixture.coordinator,
        fixture.command.billing_scope_id(),
        fixture.command.attempt_id(),
        &approved_outcome_with_reference(
            Some("txn_initial_winner"),
            "vault_initial_conflicting_metadata",
        ),
    )
    .await?;
    assert_eq!(same_transaction_conflict.attempt(), winner.attempt());
    assert_eq!(
        same_transaction_conflict.observation_diagnostics(),
        &[GatewayPaymentDiagnostic::InvalidOrConflictingPaymentMethodReference]
    );
    let applied_charge_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM billing_processor_charges \
         WHERE attempt_id = $1 AND progression_state = 'applied'",
    )
    .bind(fixture.command.attempt_id().as_uuid())
    .fetch_one(&fixture.database.pool)
    .await?;
    assert_eq!(applied_charge_count, 1);

    let conflicting_decline = GatewayPaymentOutcome::new(
        GatewayPaymentStatus::Declined,
        ProcessorEvidence::new(
            Some(GatewayTransactionId::new("txn_initial_late_decline")?),
            None,
            Some(GatewayDiagnostic::new("2")),
            Some(GatewayDiagnostic::new("200")),
            Some(GatewayDiagnostic::new("Declined")),
            Some(GatewayDiagnostic::new("declined")),
            GatewayPaymentDescriptor::default(),
        ),
    );
    let decline_replay = apply_reconciled_subscription_enrollment_gateway_outcome(
        &fixture.database.pool,
        &fixture.coordinator,
        fixture.command.billing_scope_id(),
        fixture.command.attempt_id(),
        &conflicting_decline,
    )
    .await?;
    assert_eq!(decline_replay.attempt(), winner.attempt());
    assert_eq!(
        decline_replay.observation_diagnostics(),
        &[GatewayPaymentDiagnostic::InvalidOrConflictingTransactionIdentifier]
    );

    let approval_replay = apply_reconciled_subscription_enrollment_gateway_outcome(
        &fixture.database.pool,
        &fixture.coordinator,
        fixture.command.billing_scope_id(),
        fixture.command.attempt_id(),
        &approved_outcome("txn_initial_late_approval"),
    )
    .await?;
    assert_eq!(approval_replay.attempt(), winner.attempt());
    assert_eq!(
        approval_replay.observation_diagnostics(),
        &[GatewayPaymentDiagnostic::InvalidOrConflictingTransactionIdentifier]
    );
    let charges: Vec<(String, String)> = sqlx::query_as(
        "SELECT gateway_transaction_id, progression_state \
         FROM billing_processor_charges WHERE attempt_id = $1 \
         ORDER BY gateway_transaction_id",
    )
    .bind(fixture.command.attempt_id().as_uuid())
    .fetch_all(&fixture.database.pool)
    .await?;
    assert_eq!(
        charges,
        vec![
            (
                "txn_initial_late_approval".to_owned(),
                "external_reversal_required".to_owned(),
            ),
            ("txn_initial_winner".to_owned(), "applied".to_owned()),
        ]
    );
    sqlx::query(
        "UPDATE billing_processor_charges \
         SET progression_state = 'reconciliation_required', \
             reconciliation_required_at = clock_timestamp(), \
             external_reversal_required_at = NULL \
         WHERE attempt_id = $1 AND gateway_transaction_id = $2",
    )
    .bind(fixture.command.attempt_id().as_uuid())
    .bind("txn_initial_late_approval")
    .execute(&fixture.database.pool)
    .await?;
    let promoted_replay = apply_reconciled_subscription_enrollment_gateway_outcome(
        &fixture.database.pool,
        &fixture.coordinator,
        fixture.command.billing_scope_id(),
        fixture.command.attempt_id(),
        &approved_outcome("txn_initial_late_approval"),
    )
    .await?;
    assert_eq!(promoted_replay.attempt(), winner.attempt());
    assert_eq!(
        promoted_replay.observation_diagnostics(),
        &[GatewayPaymentDiagnostic::InvalidOrConflictingTransactionIdentifier]
    );
    let promoted_progression: String = sqlx::query_scalar(
        "SELECT progression_state FROM billing_processor_charges \
         WHERE attempt_id = $1 AND gateway_transaction_id = $2",
    )
    .bind(fixture.command.attempt_id().as_uuid())
    .bind("txn_initial_late_approval")
    .fetch_one(&fixture.database.pool)
    .await?;
    assert_eq!(promoted_progression, "external_reversal_required");
    assert_eq!(fixture.coordinator.events.lock().await.len(), 1);
    fixture.cleanup().await
}

#[tokio::test]
async fn committed_admission_capability_submits_and_applies_exactly_one_sale()
-> Result<(), Box<dyn Error>> {
    let mut fixture = application_fixture("submit_once", false, false).await?;
    let gateway = Arc::new(ScriptedGateway::new(Ok(approved_outcome(
        "txn_submitted_once",
    ))));
    let resolved = scripted_resolved_gateway(fixture.gateway_account, Arc::clone(&gateway));
    let verified = crate::verify_gateway_account_mode(&resolved, GatewayAccountMode::Live).await?;
    let result = submit_admitted_subscription_enrollment(
        &fixture.database.pool,
        &fixture.coordinator,
        *fixture.admission.take().expect("committed admission"),
        &fixture.command,
        verified,
    )
    .await?;
    assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        result.payment().attempt().status(),
        PaymentAttemptStatus::Approved
    );
    assert!(result.payment().subscription().is_some());
    fixture.cleanup().await
}

#[tokio::test]
async fn mode_capability_mismatch_cannot_reach_the_low_level_submission()
-> Result<(), Box<dyn Error>> {
    let mut fixture = application_fixture("mode_guard", false, false).await?;
    let gateway = Arc::new(ScriptedGateway::for_sale_with_readiness(
        [Ok(GatewayAccountMode::Test), Ok(GatewayAccountMode::Test)],
        Ok(approved_outcome("txn_mode_guard_must_not_submit")),
    ));
    let resolved = scripted_resolved_gateway(fixture.gateway_account, Arc::clone(&gateway));

    let mismatch =
        match crate::verify_gateway_account_mode(&resolved, GatewayAccountMode::Live).await {
            Err(mismatch) => mismatch,
            Ok(_) => panic!("a test account cannot mint live submission authority"),
        };
    assert!(matches!(
        mismatch,
        crate::GatewayAccountModeVerificationError::AccountModeMismatch {
            required: GatewayAccountMode::Live,
            observed: GatewayAccountMode::Test,
        }
    ));

    let test_capability =
        crate::verify_gateway_account_mode(&resolved, GatewayAccountMode::Test).await?;
    let error = submit_admitted_subscription_enrollment(
        &fixture.database.pool,
        &fixture.coordinator,
        *fixture.admission.take().expect("committed admission"),
        &fixture.command,
        test_capability,
    )
    .await
    .expect_err("durable live admission must reject test authority");
    assert!(matches!(
        error,
        SubscriptionEnrollmentApplicationError::SubmissionIdentityMismatch
    ));
    assert_eq!(gateway.account_mode_calls.load(Ordering::SeqCst), 2);
    assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 0);
    fixture.cleanup().await
}

#[tokio::test]
async fn unsubmitted_review_attempt_replays_across_gateway_mode_change()
-> Result<(), Box<dyn Error>> {
    let fixture = enrollment_fixture("review_mode", false, false, false).await?;
    let mut transaction = fixture.database.pool.begin().await?;
    let attempt_id = match reserve_subscription_enrollment_in_transaction(
        &mut transaction,
        &TestOfferStore,
        &fixture.reservation,
    )
    .await?
    {
        SubscriptionEnrollmentReservationOutcome::Reserved(attempt) => {
            attempt.identity().attempt_id()
        }
        other => return Err(format!("unexpected initial reservation: {other:?}").into()),
    };
    transaction.commit().await?;
    sqlx::query("UPDATE billing_payment_attempts SET status = 'review_required' WHERE id = $1")
        .bind(attempt_id.as_uuid())
        .execute(&fixture.database.pool)
        .await?;

    let resolved = scripted_resolved_gateway(fixture.gateway_account, Arc::new(NeverCalledGateway));
    let test_reservation = SubscriptionEnrollmentReservation::from_command(
        &fixture.command,
        &resolved,
        GatewayAccountMode::Test,
    )?;
    let mut transaction = fixture.database.pool.begin().await?;
    let replay = match reserve_subscription_enrollment_in_transaction(
        &mut transaction,
        &TestOfferStore,
        &test_reservation,
    )
    .await?
    {
        SubscriptionEnrollmentReservationOutcome::Replay(attempt) => attempt,
        other => return Err(format!("unexpected cross-mode reservation: {other:?}").into()),
    };
    transaction.commit().await?;

    assert_eq!(replay.identity().attempt_id(), attempt_id);
    assert_eq!(
        replay.identity().required_gateway_account_mode(),
        GatewayAccountMode::Live
    );
    assert_eq!(replay.status(), PaymentAttemptStatus::ReviewRequired);
    assert!(replay.state().timestamps().submitted_at().is_none());
    fixture.cleanup().await
}

#[tokio::test]
async fn stale_initial_attempt_expires_before_cross_mode_replay_policy()
-> Result<(), Box<dyn Error>> {
    let fixture = enrollment_fixture("stale_mode", false, false, false).await?;
    let mut transaction = fixture.database.pool.begin().await?;
    let attempt_id = match reserve_subscription_enrollment_in_transaction(
        &mut transaction,
        &TestOfferStore,
        &fixture.reservation,
    )
    .await?
    {
        SubscriptionEnrollmentReservationOutcome::Reserved(attempt) => {
            attempt.identity().attempt_id()
        }
        other => return Err(format!("unexpected initial reservation: {other:?}").into()),
    };
    transaction.commit().await?;
    sqlx::query(
        "UPDATE billing_payment_attempts \
         SET created_at = clock_timestamp() - interval '31 minutes' WHERE id = $1",
    )
    .bind(attempt_id.as_uuid())
    .execute(&fixture.database.pool)
    .await?;

    let resolved = scripted_resolved_gateway(fixture.gateway_account, Arc::new(NeverCalledGateway));
    let test_reservation = SubscriptionEnrollmentReservation::from_command(
        &fixture.command,
        &resolved,
        GatewayAccountMode::Test,
    )?;
    let mut transaction = fixture.database.pool.begin().await?;
    let replay = match reserve_subscription_enrollment_in_transaction(
        &mut transaction,
        &TestOfferStore,
        &test_reservation,
    )
    .await?
    {
        SubscriptionEnrollmentReservationOutcome::Replay(attempt) => attempt,
        other => return Err(format!("unexpected stale cross-mode reservation: {other:?}").into()),
    };
    transaction.commit().await?;

    assert_eq!(replay.status(), PaymentAttemptStatus::Failed);
    assert_eq!(
        replay.state().resolution_code(),
        Some(PaymentResolutionCode::SubscriptionInitialPreparedAttemptExpired)
    );
    assert_eq!(
        replay.identity().required_gateway_account_mode(),
        GatewayAccountMode::Live
    );
    fixture.cleanup().await
}

#[tokio::test]
async fn renewal_mode_capability_mismatch_cannot_reach_the_low_level_submission()
-> Result<(), Box<dyn Error>> {
    let fixture = application_fixture("renew_mode_guard", false, false).await?;
    let initial = apply_subscription_enrollment_gateway_outcome(
        &fixture.database.pool,
        &fixture.coordinator,
        &fixture.reservation,
        &approved_outcome("txn_renewal_mode_guard_initial"),
    )
    .await?;
    let subscription_id = initial
        .subscription()
        .expect("approved enrollment creates a subscription")
        .id();
    let requested_due_at = chrono::Utc::now() - ChronoDuration::days(1);
    let due_at: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(
        r#"
        UPDATE billing_subscriptions
        SET current_period_start_at = $2,
            current_period_end_at = $3,
            next_renewal_at = $3,
            next_payment_attempt_at = $3,
            updated_at = clock_timestamp()
        WHERE id = $1
        RETURNING next_renewal_at
        "#,
    )
    .bind(subscription_id.as_uuid())
    .bind(requested_due_at - ChronoDuration::days(30))
    .bind(requested_due_at)
    .fetch_one(&fixture.database.pool)
    .await?;

    let gateway = Arc::new(ScriptedGateway::for_sale_with_readiness(
        [Ok(GatewayAccountMode::Test)],
        Ok(approved_outcome("txn_renewal_mode_guard_must_not_submit")),
    ));
    let resolved = scripted_resolved_gateway(fixture.gateway_account, Arc::clone(&gateway));
    let command = ChargeRenewal::new(fixture.command.billing_scope_id(), subscription_id, due_at);
    let mut transaction = fixture.database.pool.begin().await?;
    let reservation = match crate::reserve_subscription_renewal_in_transaction(
        &mut transaction,
        command,
        &resolved,
        GatewayAccountMode::Live,
    )
    .await?
    {
        syrup_rail::SubscriptionRenewalReservationOutcome::Reserved(reservation, _) => *reservation,
        other => return Err(format!("unexpected renewal reservation: {other:?}").into()),
    };
    transaction.commit().await?;
    let admission =
        match crate::admit_subscription_renewal_submission(&fixture.database.pool, &reservation)
            .await?
        {
            crate::SubscriptionRenewalAdmissionOutcome::Admitted(admission) => *admission,
            other => return Err(format!("unexpected renewal admission: {other:?}").into()),
        };

    let test_capability =
        crate::verify_gateway_account_mode(&resolved, GatewayAccountMode::Test).await?;
    let error = submit_admitted_subscription_renewal(
        &fixture.database.pool,
        &fixture.coordinator,
        admission,
        test_capability,
    )
    .await
    .expect_err("durable live renewal admission must reject test authority");
    assert!(matches!(
        error,
        SubscriptionEnrollmentApplicationError::SubmissionIdentityMismatch
    ));
    assert_eq!(gateway.account_mode_calls.load(Ordering::SeqCst), 1);
    assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 0);
    fixture.cleanup().await
}

#[tokio::test]
async fn recovery_admission_rejects_changed_contact_before_provider_io()
-> Result<(), Box<dyn Error>> {
    let fixture = application_fixture("rec_submit_id", false, false).await?;
    let initial = apply_subscription_enrollment_gateway_outcome(
        &fixture.database.pool,
        &fixture.coordinator,
        &fixture.reservation,
        &approved_outcome("txn_recovery_identity_initial"),
    )
    .await?;
    let subscription_id = initial
        .subscription()
        .expect("approved enrollment creates a subscription")
        .id();
    let due_at = chrono::Utc::now() - ChronoDuration::days(1);
    sqlx::query(
        r#"
        UPDATE billing_subscriptions
        SET status = 'past_due',
            current_period_start_at = $2,
            current_period_end_at = $3,
            next_renewal_at = $3,
            next_payment_attempt_at = $3,
            updated_at = clock_timestamp()
        WHERE id = $1
        "#,
    )
    .bind(subscription_id.as_uuid())
    .bind(due_at - ChronoDuration::days(30))
    .bind(due_at)
    .execute(&fixture.database.pool)
    .await?;

    let gateway = Arc::new(ScriptedGateway::new(Ok(approved_outcome(
        "txn_recovery_identity_must_not_submit",
    ))));
    let resolved = scripted_resolved_gateway(fixture.gateway_account, Arc::clone(&gateway));
    let command = RecoverSubscriptionPayment::new(
        syrup_rail::SubscriptionPaymentContext::new(
            PaymentAttemptId::new(Uuid::now_v7()),
            fixture.command.billing_scope_id(),
            fixture.command.subscriber_id(),
            fixture.command.gateway_configuration_id(),
            IdempotencyKey::new("recovery-submission-identity")?,
            PaymentToken::new("recovery-submission-token")?,
            fixture.command.billing_contact().clone(),
        ),
        fixture.command.plan_key().clone(),
    );
    let mut transaction = fixture.database.pool.begin().await?;
    let reservation = match reserve_subscription_recovery_in_transaction(
        &mut transaction,
        &command,
        &resolved,
        GatewayAccountMode::Live,
    )
    .await?
    {
        SubscriptionRecoveryReservationOutcome::Reserved(reservation, _) => *reservation,
        other => return Err(format!("unexpected recovery reservation: {other:?}").into()),
    };
    transaction.commit().await?;
    let admission =
        match admit_subscription_recovery_submission(&fixture.database.pool, &reservation).await? {
            SubscriptionRecoveryAdmissionOutcome::Admitted(admission) => *admission,
            other => return Err(format!("unexpected recovery admission: {other:?}").into()),
        };
    let changed_contact = RecoverSubscriptionPayment::new(
        syrup_rail::SubscriptionPaymentContext::new(
            PaymentAttemptId::new(Uuid::now_v7()),
            command.billing_scope_id(),
            command.subscriber_id(),
            command.gateway_configuration_id(),
            command.idempotency_key().clone(),
            PaymentToken::new("refreshed-recovery-submission-token")?,
            BillingContact::new(
                Some("Grace".to_owned()),
                Some("Hopper".to_owned()),
                Some("grace@example.test".to_owned()),
            )?,
        ),
        command.plan_key().clone(),
    );

    let verified = crate::verify_gateway_account_mode(&resolved, GatewayAccountMode::Live).await?;
    let error = submit_admitted_subscription_recovery(
        &fixture.database.pool,
        &fixture.coordinator,
        admission,
        &changed_contact,
        verified,
    )
    .await
    .expect_err("changed durable contact must invalidate admission");
    assert!(matches!(
        error,
        SubscriptionEnrollmentApplicationError::SubmissionIdentityMismatch
    ));
    assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 0);
    let not_transmitted = GatewayNotSubmittedError::NotTransmitted(GatewayDiagnostic::new(
        "recovery transport was not transmitted",
    ));
    let policy = GatewayNotSubmittedPolicy::for_error(&not_transmitted);
    let restored = apply_resumable_not_submitted_policy(
        &fixture.database.pool,
        OutcomeReservation::Recovery(&reservation),
        &mutation_error_evidence(not_transmitted.detail()),
        policy,
    )
    .await?;
    assert!(restored.should_surface_not_submitted(policy));
    assert_eq!(
        restored.payment.attempt().status(),
        PaymentAttemptStatus::Pending
    );
    assert!(
        restored
            .payment
            .attempt()
            .state()
            .timestamps()
            .submitted_at()
            .is_none()
    );
    fixture.cleanup().await
}
