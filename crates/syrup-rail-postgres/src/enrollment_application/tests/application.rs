use super::*;

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

    let replay = apply_subscription_enrollment_gateway_outcome(
        &fixture.database.pool,
        &fixture.coordinator,
        &fixture.reservation,
        &outcome,
    )
    .await?;
    assert_eq!(replay.attempt().status(), PaymentAttemptStatus::Approved);
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
    let result = submit_admitted_subscription_enrollment(
        &fixture.database.pool,
        &fixture.coordinator,
        *fixture.admission.take().expect("committed admission"),
        &fixture.command,
        &resolved,
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
async fn provider_not_submitted_error_resolves_the_admitted_attempt_without_resubmission()
-> Result<(), Box<dyn Error>> {
    let mut fixture = application_fixture("not_submitted", false, false).await?;
    let gateway = Arc::new(ScriptedGateway::new(Err(
        GatewayMutationError::NotSubmitted(GatewayNotSubmittedError::Unavailable(
            GatewayDiagnostic::new("temporary provider outage"),
        )),
    )));
    let resolved = scripted_resolved_gateway(fixture.gateway_account, Arc::clone(&gateway));
    let result = submit_admitted_subscription_enrollment(
        &fixture.database.pool,
        &fixture.coordinator,
        *fixture.admission.take().expect("committed admission"),
        &fixture.command,
        &resolved,
    )
    .await?;
    assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        result.payment().attempt().status(),
        PaymentAttemptStatus::Failed
    );
    assert_eq!(
        result.payment().attempt().state().resolution_code(),
        Some(PaymentResolutionCode::GatewayUnavailableBeforeSubmission)
    );
    assert!(result.payment().subscription().is_none());
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
    let result = submit_admitted_subscription_enrollment(
        &fixture.database.pool,
        &fixture.coordinator,
        *fixture.admission.take().expect("committed admission"),
        &fixture.command,
        &resolved,
    )
    .await?;
    assert!(matches!(
        result,
        SubscriptionEnrollmentProviderResult::NotSubmitted {
            error: GatewayNotSubmittedError::RateLimited(_),
            ..
        }
    ));
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
