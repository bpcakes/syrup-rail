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
        &GatewayMutationError::NotSubmitted(not_transmitted).processor_evidence(),
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
async fn typed_approval_signals_survive_application_reload_and_replay() -> Result<(), Box<dyn Error>>
{
    use syrup_rail::ProcessorApprovalEvidence as Signal;
    for code in ["100", "0100", "+0100", "provider-specific-success"] {
        let fixture = application_fixture("approval_signal", false, false).await?;
        let evidence = ProcessorEvidence::new(
            Some(GatewayTransactionId::new("txn_approval_signal")?),
            None,
            None,
            Some(GatewayDiagnostic::new(code)),
            None,
            None,
            GatewayPaymentDescriptor::default(),
        )
        .with_approval_evidence(Signal::Structured);
        let outcome = GatewayPaymentOutcome::new(GatewayPaymentStatus::Unknown, evidence);
        for _ in 0..2 {
            let result = apply_subscription_enrollment_gateway_outcome(
                &fixture.database.pool,
                &fixture.coordinator,
                &fixture.reservation,
                &outcome,
            )
            .await?;
            assert_eq!(result.status(), PaymentAttemptStatus::Unknown);
            assert_eq!(
                result.processor_evidence().approval_evidence(),
                Signal::Structured
            );
            assert_eq!(
                result
                    .processor_evidence()
                    .response_code()
                    .unwrap()
                    .expose(),
                code
            );
        }
        let reconciled = apply_reconciled_subscription_enrollment_gateway_outcome(
            &fixture.database.pool,
            &fixture.coordinator,
            fixture.reservation.identity().billing_scope_id(),
            fixture.reservation.identity().attempt_id(),
            &outcome,
        )
        .await?;
        assert_eq!(
            reconciled.processor_evidence().approval_evidence(),
            Signal::Structured
        );
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT gateway_response_code, gateway_approval_evidence FROM billing_processor_charges WHERE attempt_id = $1",
        ).bind(fixture.reservation.identity().attempt_id().as_uuid()).fetch_all(&fixture.database.pool).await?;
        assert_eq!(rows, vec![(code.to_owned(), "structured".to_owned())]);
        // Immutable charge facts cannot be weakened by a later writer.
        assert!(
            sqlx::query(
                "UPDATE billing_processor_charges SET gateway_approval_evidence = 'absent'"
            )
            .execute(&fixture.database.pool)
            .await
            .is_err()
        );
        fixture.cleanup().await?;
    }
    Ok(())
}

#[tokio::test]
async fn manual_failure_uses_persisted_classification_and_protects_legacy_rows()
-> Result<(), Box<dyn Error>> {
    use syrup_rail::ProcessorApprovalEvidence as Signal;
    for signal in [
        Signal::Structured,
        Signal::TextOnly,
        Signal::Unclassified,
        Signal::Absent,
    ] {
        let fixture = application_fixture("manual_signal", false, false).await?;
        let evidence = ProcessorEvidence::new(
            None,
            None,
            None,
            Some(GatewayDiagnostic::new("0100")),
            None,
            None,
            GatewayPaymentDescriptor::default(),
        )
        .with_approval_evidence(signal);
        apply_subscription_enrollment_gateway_outcome(
            &fixture.database.pool,
            &fixture.coordinator,
            &fixture.reservation,
            &GatewayPaymentOutcome::new(GatewayPaymentStatus::Unknown, evidence),
        )
        .await?;
        let attempt_id = fixture.reservation.identity().attempt_id();
        sqlx::query("UPDATE billing_payment_attempts SET status = 'review_required', review_required_at = clock_timestamp() WHERE id = $1")
            .bind(attempt_id.as_uuid()).execute(&fixture.database.pool).await?;
        let result = fail_review_required_attempt(
            &fixture.database.pool,
            &fixture.coordinator,
            &NeverManualFailureHost,
            attempt_id,
        )
        .await?;
        if signal == Signal::Absent {
            assert!(matches!(result, ManualAttemptFailureOutcome::Failed(_)));
        } else {
            assert!(matches!(result, ManualAttemptFailureOutcome::KeptOpen(_)));
        }
        fixture.cleanup().await?;
    }
    Ok(())
}

#[tokio::test]
async fn local_exact_query_notes_do_not_create_approval_evidence() -> Result<(), Box<dyn Error>> {
    let fixture = application_fixture("empty_review", false, false).await?;
    let attempt_id = fixture.reservation.identity().attempt_id();
    // Submission committed, then the process died before saving any response.
    sqlx::query("UPDATE billing_payment_attempts SET created_at = clock_timestamp() - interval '31 minutes', submitted_at = clock_timestamp() - interval '31 minutes', updated_at = clock_timestamp() - interval '31 minutes' WHERE id = $1")
        .bind(attempt_id.as_uuid()).execute(&fixture.database.pool).await?;
    let claimed = crate::claim_exact_reconciliation_attempts(
        &fixture.database.pool,
        fixture.reservation.identity().gateway_account_id(),
    )
    .await?;
    let attempt = claimed
        .iter()
        .find(|attempt| attempt.identity().attempt_id() == attempt_id)
        .unwrap();
    assert_eq!(
        attempt.state().processor_evidence().approval_evidence(),
        syrup_rail::ProcessorApprovalEvidence::Absent
    );
    for _ in 0..2 {
        crate::apply_exact_query_observation(
            &fixture.database.pool,
            attempt,
            crate::ExactQueryObservation::NoTransaction,
        )
        .await?;
    }
    let result = fail_review_required_attempt(
        &fixture.database.pool,
        &fixture.coordinator,
        &NeverManualFailureHost,
        attempt_id,
    )
    .await?;
    assert!(matches!(result, ManualAttemptFailureOutcome::Failed(_)));
    fixture.cleanup().await
}

#[tokio::test]
async fn indeterminate_error_evidence_survives_negative_queries_and_blocks_manual_failure()
-> Result<(), Box<dyn Error>> {
    for error in [
        GatewayMutationError::Indeterminate(GatewayDiagnostic::new(
            "Approved but confirmation failed",
        )),
        GatewayMutationError::RateLimitedIndeterminate(GatewayDiagnostic::new("Approved")),
        GatewayMutationError::Indeterminate(GatewayDiagnostic::new("")),
    ] {
        let fixture = application_fixture("error_review", false, false).await?;
        let detail = error.detail().expose().to_owned();
        let result = apply_subscription_enrollment_gateway_outcome(
            &fixture.database.pool,
            &fixture.coordinator,
            &fixture.reservation,
            &GatewayPaymentOutcome::new(GatewayPaymentStatus::Unknown, error.processor_evidence()),
        )
        .await?;
        let attempt_id = result.attempt().identity().attempt_id();
        sqlx::query("UPDATE billing_payment_attempts SET created_at = clock_timestamp() - interval '31 minutes', submitted_at = clock_timestamp() - interval '31 minutes', updated_at = clock_timestamp() - interval '31 minutes' WHERE id = $1")
            .bind(attempt_id.as_uuid()).execute(&fixture.database.pool).await?;
        for _ in 0..2 {
            crate::apply_exact_query_observation(
                &fixture.database.pool,
                result.attempt(),
                crate::ExactQueryObservation::NoTransaction,
            )
            .await?;
        }
        let manual = fail_review_required_attempt(
            &fixture.database.pool,
            &fixture.coordinator,
            &NeverManualFailureHost,
            attempt_id,
        )
        .await?;
        let ManualAttemptFailureOutcome::KeptOpen(attempt) = manual else {
            panic!("indeterminate evidence must remain protected")
        };
        assert_eq!(
            attempt.state().processor_evidence().approval_evidence(),
            syrup_rail::ProcessorApprovalEvidence::Unclassified
        );
        assert_eq!(
            attempt
                .state()
                .processor_evidence()
                .response_text()
                .unwrap()
                .expose(),
            if detail.is_empty() {
                "Payment processor did not return a transaction before the reconciliation deadline."
            } else {
                &detail
            }
        );
        fixture.cleanup().await?;
    }
    Ok(())
}

#[tokio::test]
async fn unidentified_reconciliation_cannot_erase_approval_signals() -> Result<(), Box<dyn Error>> {
    use syrup_rail::ProcessorApprovalEvidence as Signal;
    let fixture = application_fixture("noid_signal", false, false).await?;
    let result = async {
        let prior = ProcessorEvidence::default().with_approval_evidence(Signal::Structured);
        let initial = apply_subscription_enrollment_gateway_outcome(&fixture.database.pool, &fixture.coordinator, &fixture.reservation, &GatewayPaymentOutcome::new(GatewayPaymentStatus::Unknown, prior)).await?;
        let id = initial.attempt().identity().attempt_id();
        let reconciled = apply_reconciled_subscription_enrollment_gateway_outcome(&fixture.database.pool, &fixture.coordinator, fixture.reservation.identity().billing_scope_id(), id, &GatewayPaymentOutcome::new(GatewayPaymentStatus::Unknown, ProcessorEvidence::default())).await?;
        assert_eq!(reconciled.processor_evidence().approval_evidence(), Signal::Structured);
        sqlx::query("UPDATE billing_payment_attempts SET status = 'review_required', review_required_at = clock_timestamp() WHERE id = $1").bind(id.as_uuid()).execute(&fixture.database.pool).await?;
        assert!(matches!(fail_review_required_attempt(&fixture.database.pool, &fixture.coordinator, &NeverManualFailureHost, id).await?, ManualAttemptFailureOutcome::KeptOpen(_)));
        let matched = apply_reconciled_subscription_enrollment_gateway_outcome(
            &fixture.database.pool, &fixture.coordinator,
            fixture.reservation.identity().billing_scope_id(), id,
            &GatewayPaymentOutcome::new(GatewayPaymentStatus::Declined,
                ProcessorEvidence::new(Some(GatewayTransactionId::new("txn_verified_decline")?), None, None, None, None, None, GatewayPaymentDescriptor::default()).with_approval_evidence(Signal::Absent)),
        ).await?;
        assert_eq!(matched.processor_evidence().approval_evidence(), Signal::Absent);
        assert_eq!(matched.status(), PaymentAttemptStatus::Declined);
        Ok::<(), Box<dyn Error>>(())
    }.await;
    let cleanup = fixture.cleanup().await;
    result?;
    cleanup
}
