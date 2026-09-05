use super::*;

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
            gateway_transaction_id = 'txn_terminal_winner',
            gateway_payment_method_reference = 'vault_terminal_winner',
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
    assert_eq!(
        result.observation_diagnostics(),
        &[
            GatewayPaymentDiagnostic::InvalidOrConflictingTransactionIdentifier,
            GatewayPaymentDiagnostic::InvalidOrConflictingPaymentMethodReference,
        ]
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
