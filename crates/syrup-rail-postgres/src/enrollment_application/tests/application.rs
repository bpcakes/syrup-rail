use super::*;

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
        result.gateway_diagnostics(),
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
        diagnostic_replay.gateway_diagnostics(),
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

#[tokio::test]
async fn payment_method_admission_rejects_changed_idempotency_key_before_provider_io()
-> Result<(), Box<dyn Error>> {
    let fixture = application_fixture("replace_submit", false, false).await?;
    apply_subscription_enrollment_gateway_outcome(
        &fixture.database.pool,
        &fixture.coordinator,
        &fixture.reservation,
        &approved_outcome_with_reference(
            Some("txn_replacement_identity_initial"),
            "vault_replacement_identity_initial",
        ),
    )
    .await?;

    let gateway = Arc::new(ScriptedGateway::for_stored_method(Ok(
        approved_outcome_with_reference(
            Some("txn_replacement_identity_must_not_submit"),
            "vault_replacement_identity_must_not_submit",
        ),
    )));
    let resolved = scripted_resolved_gateway(fixture.gateway_account, Arc::clone(&gateway));
    let command = ReplaceSubscriptionPaymentMethod::new(
        syrup_rail::SubscriptionPaymentContext::new(
            PaymentAttemptId::new(Uuid::now_v7()),
            fixture.command.billing_scope_id(),
            fixture.command.subscriber_id(),
            fixture.command.gateway_configuration_id(),
            IdempotencyKey::new("replacement-submission-identity")?,
            PaymentToken::new("replacement-submission-token")?,
            fixture.command.billing_contact().clone(),
        ),
        fixture.command.plan_key().clone(),
    );
    let mut transaction = fixture.database.pool.begin().await?;
    let reservation = match reserve_subscription_payment_method_replacement_in_transaction(
        &mut transaction,
        &command,
        &resolved,
        GatewayAccountMode::Live,
    )
    .await?
    {
        SubscriptionPaymentMethodReplacementReservationOutcome::Reserved(reservation, _) => {
            *reservation
        }
        other => {
            return Err(format!("unexpected payment-method reservation: {other:?}").into());
        }
    };
    transaction.commit().await?;
    let admission =
        match admit_subscription_payment_method_replacement(&fixture.database.pool, &reservation)
            .await?
        {
            SubscriptionPaymentMethodReplacementAdmissionOutcome::Admitted(admission) => *admission,
            other => {
                return Err(format!("unexpected payment-method admission: {other:?}").into());
            }
        };
    let changed_idempotency = ReplaceSubscriptionPaymentMethod::new(
        syrup_rail::SubscriptionPaymentContext::new(
            PaymentAttemptId::new(Uuid::now_v7()),
            command.billing_scope_id(),
            command.subscriber_id(),
            command.gateway_configuration_id(),
            IdempotencyKey::new("replacement-submission-identity-changed")?,
            PaymentToken::new("refreshed-replacement-submission-token")?,
            command.billing_contact().clone(),
        ),
        command.plan_key().clone(),
    );

    let verified = crate::verify_gateway_account_mode(&resolved, GatewayAccountMode::Live).await?;
    let error = submit_admitted_subscription_payment_method_replacement(
        &fixture.database.pool,
        &fixture.coordinator,
        admission,
        &changed_idempotency,
        verified,
    )
    .await
    .expect_err("changed durable idempotency key must invalidate admission");
    assert!(matches!(
        error,
        SubscriptionEnrollmentApplicationError::SubmissionIdentityMismatch
    ));
    assert_eq!(gateway.store_calls.load(Ordering::SeqCst), 0);
    let not_transmitted = GatewayNotSubmittedError::NotTransmitted(GatewayDiagnostic::new(
        "payment-method transport was not transmitted",
    ));
    let policy = GatewayNotSubmittedPolicy::for_error(&not_transmitted);
    let restored = apply_resumable_not_submitted_policy(
        &fixture.database.pool,
        OutcomeReservation::PaymentMethodReplacement(&reservation),
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

#[tokio::test]
async fn provider_not_submitted_unavailable_restores_the_admitted_attempt_for_retry()
-> Result<(), Box<dyn Error>> {
    let mut fixture = application_fixture("not_submitted", false, false).await?;
    let gateway = Arc::new(ScriptedGateway::new(Err(
        GatewayMutationError::NotSubmitted(GatewayNotSubmittedError::NotTransmitted(
            GatewayDiagnostic::new("temporary provider outage"),
        )),
    )));
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
        PaymentAttemptStatus::Pending
    );
    assert_eq!(result.payment().attempt().state().resolution_code(), None);
    assert!(
        result
            .payment()
            .attempt()
            .state()
            .timestamps()
            .submitted_at()
            .is_none()
    );
    assert!(result.payment().subscription().is_none());
    fixture.cleanup().await
}

#[tokio::test]
async fn adapter_cannot_mint_retry_safe_account_mode_verification_errors()
-> Result<(), Box<dyn Error>> {
    let mut fixture = application_fixture("adapter_mode", false, false).await?;
    let gateway = Arc::new(ScriptedGateway::new(Err(
        GatewayMutationError::NotSubmitted(GatewayNotSubmittedError::AccountModeVerification(
            GatewayError::Unavailable(GatewayDiagnostic::new(
                "adapter-originated verification claim",
            )),
        )),
    )));
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
    assert!(matches!(
        result,
        SubscriptionEnrollmentProviderResult::NotSubmitted {
            error: GatewayNotSubmittedError::Malformed(_),
            ..
        }
    ));
    assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 1);
    fixture.cleanup().await
}

#[tokio::test]
async fn adapter_account_mode_mismatch_is_also_a_malformed_contract_error()
-> Result<(), Box<dyn Error>> {
    let mut fixture = application_fixture("adapter_mismatch", false, false).await?;
    let gateway = Arc::new(ScriptedGateway::new(Err(
        GatewayMutationError::NotSubmitted(GatewayNotSubmittedError::AccountModeMismatch {
            required: GatewayAccountMode::Live,
            observed: GatewayAccountMode::Test,
            detail: GatewayDiagnostic::new("adapter-originated mode mismatch claim"),
        }),
    )));
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
    assert!(matches!(
        result,
        SubscriptionEnrollmentProviderResult::NotSubmitted {
            error: GatewayNotSubmittedError::Malformed(_),
            ..
        }
    ));
    assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 1);
    fixture.cleanup().await
}

#[tokio::test]
async fn provider_not_submitted_throttle_atomically_extends_the_account_cooldown()
-> Result<(), Box<dyn Error>> {
    let mut fixture = application_fixture("account_throttle", false, false).await?;
    let gateway = Arc::new(ScriptedGateway::new(Err(
        GatewayMutationError::NotSubmitted(GatewayNotSubmittedError::RateLimited(
            GatewayDiagnostic::new("merchant throttle"),
        )),
    )));
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
    assert!(matches!(
        result,
        SubscriptionEnrollmentProviderResult::NotSubmitted {
            error: GatewayNotSubmittedError::RateLimited(_),
            ..
        }
    ));
    assert_eq!(
        result.payment().attempt().state().resolution_code(),
        Some(PaymentResolutionCode::GatewayAccountRateLimitedBeforeSubmission)
    );
    let deadlines: (bool, bool) = sqlx::query_as(
        r#"
        SELECT
            mutation_rate_limited_until > clock_timestamp(),
            provider.rate_limited_until > clock_timestamp()
        FROM billing_gateway_accounts AS account
        INNER JOIN billing_gateway_provider_rate_limits AS provider
            ON provider.provider_key = account.provider_key
        WHERE account.id = $1
        "#,
    )
    .bind(fixture.gateway_account.gateway_account_id)
    .fetch_one(&fixture.database.pool)
    .await?;
    assert_eq!(deadlines, (true, false));
    fixture.cleanup().await
}

#[tokio::test]
async fn event_failure_rolls_back_application_and_durably_parks_approval()
-> Result<(), Box<dyn Error>> {
    let fixture = application_fixture("event_fail", false, true).await?;
    let result = apply_subscription_enrollment_gateway_outcome(
        &fixture.database.pool,
        &fixture.coordinator,
        &fixture.reservation,
        &approved_outcome("txn_event_failure"),
    )
    .await?;
    assert_eq!(
        result.attempt().status(),
        PaymentAttemptStatus::ReviewRequired
    );
    assert!(result.subscription().is_none());
    let counts: (i64, i64, i64) = sqlx::query_as(
        r#"
        SELECT
            (SELECT count(*) FROM billing_payment_methods),
            (SELECT count(*) FROM billing_subscriptions),
            (SELECT count(*) FROM billing_processor_charges)
        "#,
    )
    .fetch_one(&fixture.database.pool)
    .await?;
    assert_eq!(counts, (0, 0, 1));
    assert!(fixture.coordinator.events.lock().await.is_empty());
    fixture.cleanup().await
}

#[tokio::test]
async fn failed_attempt_parking_falls_back_to_permanent_charge_observation()
-> Result<(), Box<dyn Error>> {
    let fixture = application_fixture("charge_fallback", false, false).await?;
    sqlx::raw_sql(
        r#"
        CREATE FUNCTION host_reject_attempt_status_update()
        RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN
            IF NEW.status IS DISTINCT FROM OLD.status THEN
                RAISE EXCEPTION 'injected attempt status write failure';
            END IF;
            RETURN NEW;
        END
        $$;
        CREATE TRIGGER host_reject_attempt_status_update
        BEFORE UPDATE OF status ON billing_payment_attempts
        FOR EACH ROW EXECUTE FUNCTION host_reject_attempt_status_update();
        "#,
    )
    .execute(&fixture.database.pool)
    .await?;
    let result = apply_subscription_enrollment_gateway_outcome(
        &fixture.database.pool,
        &fixture.coordinator,
        &fixture.reservation,
        &approved_outcome("txn_charge_fallback"),
    )
    .await?;
    assert_eq!(result.attempt().status(), PaymentAttemptStatus::Pending);
    assert_eq!(result.status(), PaymentAttemptStatus::Unknown);
    assert!(result.is_confirmation_pending());
    assert_eq!(
        result
            .processor_evidence()
            .transaction_id()
            .map(GatewayTransactionId::expose),
        Some("txn_charge_fallback")
    );
    assert!(result.subscription().is_none());
    let durable: (i64, String, Option<String>) = sqlx::query_as(
        r#"
        SELECT count(*), min(progression_state), min(gateway_transaction_id)
        FROM billing_processor_charges
        "#,
    )
    .fetch_one(&fixture.database.pool)
    .await?;
    assert_eq!(
        durable,
        (
            1,
            "pending".to_owned(),
            Some("txn_charge_fallback".to_owned())
        )
    );
    let subscription_count: i64 = sqlx::query_scalar("SELECT count(*) FROM billing_subscriptions")
        .fetch_one(&fixture.database.pool)
        .await?;
    assert_eq!(subscription_count, 0);
    fixture.cleanup().await
}

#[tokio::test]
async fn later_exact_approval_identifies_a_transactionless_fallback_charge()
-> Result<(), Box<dyn Error>> {
    let fixture = application_fixture("identify_charge", false, false).await?;
    sqlx::raw_sql(
        r#"
        CREATE FUNCTION host_reject_attempt_status_update()
        RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN
            IF NEW.status IS DISTINCT FROM OLD.status THEN
                RAISE EXCEPTION 'injected attempt status write failure';
            END IF;
            RETURN NEW;
        END
        $$;
        CREATE TRIGGER host_reject_attempt_status_update
        BEFORE UPDATE OF status ON billing_payment_attempts
        FOR EACH ROW EXECUTE FUNCTION host_reject_attempt_status_update();
        "#,
    )
    .execute(&fixture.database.pool)
    .await?;
    let parked = apply_subscription_enrollment_gateway_outcome(
        &fixture.database.pool,
        &fixture.coordinator,
        &fixture.reservation,
        &approved_outcome_with_transaction(None),
    )
    .await?;
    assert_eq!(parked.attempt().status(), PaymentAttemptStatus::Pending);
    let before: Option<String> =
        sqlx::query_scalar("SELECT gateway_transaction_id FROM billing_processor_charges")
            .fetch_one(&fixture.database.pool)
            .await?;
    assert!(before.is_none());

    sqlx::query("DROP TRIGGER host_reject_attempt_status_update ON billing_payment_attempts")
        .execute(&fixture.database.pool)
        .await?;
    let exact = approved_outcome("txn_identified_later");
    let applied = apply_subscription_enrollment_gateway_outcome(
        &fixture.database.pool,
        &fixture.coordinator,
        &fixture.reservation,
        &exact,
    )
    .await?;
    assert_eq!(applied.attempt().status(), PaymentAttemptStatus::Approved);
    let identified: (String, String) = sqlx::query_as(
        "SELECT gateway_transaction_id, progression_state FROM billing_processor_charges",
    )
    .fetch_one(&fixture.database.pool)
    .await?;
    assert_eq!(
        identified,
        ("txn_identified_later".to_owned(), "applied".to_owned())
    );
    fixture.cleanup().await
}

#[tokio::test]
async fn approval_after_terminal_failure_is_parked_with_reversal_required_charge()
-> Result<(), Box<dyn Error>> {
    let fixture = application_fixture("terminal_race", false, false).await?;
    sqlx::query(
        r#"
        UPDATE billing_payment_attempts
        SET status = 'failed', resolved_at = clock_timestamp(),
            gateway_response_text = 'local failure', updated_at = clock_timestamp()
        WHERE id = $1
        "#,
    )
    .bind(fixture.reservation.identity().attempt_id().as_uuid())
    .execute(&fixture.database.pool)
    .await?;
    let result = apply_subscription_enrollment_gateway_outcome(
        &fixture.database.pool,
        &fixture.coordinator,
        &fixture.reservation,
        &approved_outcome("txn_terminal_race"),
    )
    .await?;
    assert_eq!(
        result.attempt().status(),
        PaymentAttemptStatus::ReviewRequired
    );
    assert!(result.subscription().is_none());
    let progression: String =
        sqlx::query_scalar("SELECT progression_state FROM billing_processor_charges")
            .fetch_one(&fixture.database.pool)
            .await?;
    assert_eq!(progression, "external_reversal_required");
    assert!(fixture.coordinator.events.lock().await.is_empty());
    fixture.cleanup().await
}
