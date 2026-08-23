use std::io;

use super::*;

fn assert_application_invalid_state(
    error: SubscriptionEnrollmentApplicationError,
    expected: &'static str,
) {
    match error {
        SubscriptionEnrollmentApplicationError::InvalidState(actual) => {
            assert_eq!(actual, expected);
        }
        other => panic!("expected application invalid state, got {other}"),
    }
}

fn assert_service_invalid_state(error: SubscriptionBillingServiceError, expected: &'static str) {
    match error {
        SubscriptionBillingServiceError::InvalidState(actual) => assert_eq!(actual, expected),
        other => panic!("expected service invalid state, got {other}"),
    }
}

#[tokio::test]
async fn cancellation_and_enrollment_workflows_contend_on_the_canonical_subscription_aggregate()
-> Result<(), Box<dyn Error>> {
    let fixture = application_fixture("enroll_agg_lock", false, false).await?;
    let result = async {
        let mut aggregate_lock = fixture.database.pool.begin().await?;
        let cancellation = syrup_rail::CancelSubscription::new(
            fixture.command.billing_scope_id(),
            fixture.command.subscriber_id(),
            fixture.command.plan_key().clone(),
        );
        let held =
            crate::cancel_subscription_in_transaction(&mut aggregate_lock, &cancellation).await?;
        if held != syrup_rail::CancelSubscriptionOutcome::NotFound {
            return Err(
                "cancellation workflow did not retain its empty aggregate transaction".into(),
            );
        }
        let approved = approved_outcome("txn_aggregate_contention");
        let declined =
            GatewayPaymentOutcome::new(GatewayPaymentStatus::Declined, approved.evidence().clone());
        let error = apply_subscription_enrollment_gateway_outcome(
            &fixture.database.pool,
            &fixture.coordinator,
            &fixture.reservation,
            &declined,
        )
        .await
        .expect_err("canonical aggregate holder must block enrollment application");
        aggregate_lock.rollback().await?;
        let SubscriptionEnrollmentApplicationError::Sql(sqlx::Error::Database(error)) = error
        else {
            return Err(format!("expected enrollment lock timeout, got {error:?}").into());
        };
        if error.code().as_deref() != Some("55P03") {
            return Err(format!(
                "expected enrollment lock timeout SQLSTATE 55P03, got {:?}",
                error.code()
            )
            .into());
        }
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = fixture.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn approved_payment_method_writer_blocks_scrub_before_any_row_change()
-> Result<(), Box<dyn Error>> {
    let mut fixture = application_fixture("wr_scrub_lock", false, false).await?;
    let pause = Arc::new(AppendPause::new());
    fixture.coordinator.append_pause = Some(Arc::clone(&pause));
    let result = async {
        let attempt_id = fixture.reservation.identity().attempt_id();
        let attempt_before: String = sqlx::query_scalar(
            "SELECT to_jsonb(attempts)::text FROM billing_payment_attempts AS attempts WHERE id = $1",
        )
        .bind(attempt_id.as_uuid())
        .fetch_one(&fixture.database.pool)
        .await?;
        let outcome = approved_outcome("txn_writer_scrub_contention");
        let mut application = Box::pin(apply_subscription_enrollment_gateway_outcome(
            &fixture.database.pool,
            &fixture.coordinator,
            &fixture.reservation,
            &outcome,
        ));

        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::select! {
                result = &mut application => Err(io::Error::other(format!(
                    "approved writer completed before contention probe: {result:?}"
                ))),
                permit = pause.reached.acquire() => {
                    permit
                        .map_err(|_| io::Error::other("append pause closed before writer arrived"))?
                        .forget();
                    Ok(())
                }
            }
        })
        .await
        .map_err(|_| io::Error::other("approved writer did not reach its held transaction"))??;

        let mut scrub = fixture.database.pool.begin().await?;
        sqlx::query("SET LOCAL lock_timeout = '100ms'")
            .execute(&mut *scrub)
            .await?;
        let error = crate::scrub_subscriber_billing_data(
            &mut scrub,
            syrup_rail::ScrubSubscriberBillingData::new(
                fixture.command.billing_scope_id(),
                fixture.command.subscriber_id(),
            ),
        )
        .await
        .expect_err("approved writer must block subscriber scrub on the shared method domain");
        let sqlstate = error
            .as_database_error()
            .and_then(|error| error.code())
            .map(|code| code.into_owned());
        scrub.rollback().await?;
        if sqlstate.as_deref() != Some("55P03") {
            pause.release.add_permits(1);
            return Err(io::Error::other(format!(
                "expected scrub lock timeout SQLSTATE 55P03, got {sqlstate:?}"
            ))
            .into());
        }
        let attempt_after: String = sqlx::query_scalar(
            "SELECT to_jsonb(attempts)::text FROM billing_payment_attempts AS attempts WHERE id = $1",
        )
        .bind(attempt_id.as_uuid())
        .fetch_one(&fixture.database.pool)
        .await?;
        if attempt_after != attempt_before {
            pause.release.add_permits(1);
            return Err(
                io::Error::other("scrub changed rows before acquiring the method domain").into(),
            );
        }

        pause.release.add_permits(1);
        let applied = application.await?;
        if applied.attempt().status() != PaymentAttemptStatus::Approved {
            return Err(
                io::Error::other("approved writer did not resume after scrub rollback").into(),
            );
        }
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = fixture.cleanup().await;
    result?;
    cleanup
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
    let applied_claim: (chrono::DateTime<chrono::Utc>, Uuid, Uuid) = sqlx::query_as(
        r#"
        SELECT applied_at, applied_subscription_id, applied_payment_attempt_id
        FROM billing_subscription_discount_claims
        LIMIT 1
        "#,
    )
    .fetch_one(&fixture.database.pool)
    .await?;
    let applied_state = syrup_rail::SubscriptionDiscountClaimState::from_legacy_parts(
        syrup_rail::SubscriptionDiscountClaimStatus::Applied,
        Some(applied_claim.0),
        Some(syrup_rail::SubscriptionId::new(applied_claim.1)),
        Some(PaymentAttemptId::new(applied_claim.2)),
        None,
    )?;
    assert!(matches!(
        applied_state,
        syrup_rail::SubscriptionDiscountClaimState::Applied {
            subscription_id,
            payment_attempt_id,
            ..
        } if subscription_id == result.subscription().expect("applied subscription").id()
            && payment_attempt_id == result.attempt().identity().attempt_id()
    ));
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
async fn reconciliation_dispatch_preserves_missing_kind_and_host_charge_errors_without_provider_io()
-> Result<(), Box<dyn Error>> {
    let fixture = application_fixture("rec_dispatch", false, false).await?;
    let gateway = Arc::new(ScriptedGateway::new(Ok(approved_outcome(
        "txn_dispatch_must_not_submit",
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
    let outcome = approved_outcome("txn_dispatch_observed");
    let missing_attempt_id = PaymentAttemptId::new(Uuid::now_v7());
    let scope = fixture.command.billing_scope_id();

    assert_application_invalid_state(
        apply_reconciled_subscription_enrollment_gateway_outcome(
            &fixture.database.pool,
            &fixture.coordinator,
            scope,
            missing_attempt_id,
            &outcome,
        )
        .await
        .expect_err("missing initial attempt must fail"),
        "subscription enrollment attempt was not found",
    );
    assert_application_invalid_state(
        apply_reconciled_subscription_recovery_gateway_outcome(
            &fixture.database.pool,
            &fixture.coordinator,
            scope,
            missing_attempt_id,
            &outcome,
        )
        .await
        .expect_err("missing recovery attempt must fail"),
        "subscription recovery attempt was not found",
    );
    assert_application_invalid_state(
        apply_reconciled_subscription_renewal_gateway_outcome(
            &fixture.database.pool,
            &fixture.coordinator,
            scope,
            missing_attempt_id,
            &outcome,
        )
        .await
        .expect_err("missing renewal attempt must fail"),
        "subscription renewal attempt was not found",
    );
    assert_application_invalid_state(
        apply_reconciled_subscription_payment_method_replacement_gateway_outcome(
            &fixture.database.pool,
            &fixture.coordinator,
            scope,
            missing_attempt_id,
            &outcome,
        )
        .await
        .expect_err("missing replacement attempt must fail"),
        "payment method replacement attempt was not found",
    );
    assert_service_invalid_state(
        service
            .apply_reconciled_outcome(scope, missing_attempt_id, &outcome)
            .await
            .expect_err("missing service attempt must fail"),
        "reconciled subscription payment attempt was not found",
    );
    assert_service_invalid_state(
        service
            .apply_reconciled_outcome(
                BillingScopeId::new(Uuid::now_v7()),
                fixture.command.attempt_id(),
                &outcome,
            )
            .await
            .expect_err("wrong-scope service attempt must look missing"),
        "reconciled subscription payment attempt was not found",
    );

    assert_application_invalid_state(
        apply_reconciled_subscription_recovery_gateway_outcome(
            &fixture.database.pool,
            &fixture.coordinator,
            scope,
            fixture.command.attempt_id(),
            &outcome,
        )
        .await
        .expect_err("initial attempt must not reconstruct as recovery"),
        "reconciled attempt is not a valid subscription recovery",
    );
    assert_application_invalid_state(
        apply_reconciled_subscription_renewal_gateway_outcome(
            &fixture.database.pool,
            &fixture.coordinator,
            scope,
            fixture.command.attempt_id(),
            &outcome,
        )
        .await
        .expect_err("initial attempt must not reconstruct as renewal"),
        "reconciled attempt is not a valid subscription renewal",
    );
    assert_application_invalid_state(
        apply_reconciled_subscription_payment_method_replacement_gateway_outcome(
            &fixture.database.pool,
            &fixture.coordinator,
            scope,
            fixture.command.attempt_id(),
            &outcome,
        )
        .await
        .expect_err("initial attempt must not reconstruct as replacement"),
        "reconciled attempt is not a valid payment method replacement",
    );

    let host_attempt_id = PaymentAttemptId::new(Uuid::now_v7());
    let host_target_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO billing_payment_attempts (
            id, billing_scope_id, subscriber_id, host_charge_target_id,
            attempt_kind, status, idempotency_key, request_fingerprint,
            amount_cents, currency, gateway_account_id,
            gateway_configuration_id, gateway_order_id
        ) VALUES (
            $1, $2, $3, $4, 'host_charge', 'pending', $5, $6,
            100, 'USD', $7, $8, $9
        )
        "#,
    )
    .bind(host_attempt_id.as_uuid())
    .bind(scope.as_uuid())
    .bind(Uuid::now_v7())
    .bind(host_target_id)
    .bind(format!("host-dispatch-{}", host_attempt_id.as_uuid()))
    .bind(format!("host_charge:{host_target_id}:100:USD"))
    .bind(fixture.gateway_account.gateway_account_id)
    .bind(fixture.gateway_account.gateway_configuration_id)
    .bind(format!("host_order_{}", host_attempt_id.as_uuid()))
    .execute(&fixture.database.pool)
    .await?;

    assert_application_invalid_state(
        apply_reconciled_subscription_enrollment_gateway_outcome(
            &fixture.database.pool,
            &fixture.coordinator,
            scope,
            host_attempt_id,
            &outcome,
        )
        .await
        .expect_err("host charge must not reconstruct as initial enrollment"),
        "reconciled attempt is not a valid subscription enrollment",
    );
    match service
        .apply_reconciled_outcome(scope, host_attempt_id, &outcome)
        .await
        .expect_err("subscription service must reject a host charge")
    {
        SubscriptionBillingServiceError::Application(
            SubscriptionEnrollmentApplicationError::InvalidState(actual),
        ) => assert_eq!(
            actual,
            "attempt kind is not owned by the subscription billing service"
        ),
        other => panic!("expected application host-charge rejection, got {other}"),
    }
    assert_eq!(gateway.account_mode_calls.load(Ordering::SeqCst), 0);
    assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 0);
    assert_eq!(gateway.store_calls.load(Ordering::SeqCst), 0);
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 0);
    assert_eq!(admission.calls.load(Ordering::SeqCst), 0);
    fixture.cleanup().await
}

#[tokio::test]
async fn reconciled_wrapper_reports_missing_gateway_account() -> Result<(), Box<dyn Error>> {
    let fixture = application_fixture("rec_miss_acct", false, false).await?;
    sqlx::query(
        "ALTER TABLE billing_payment_attempts \
         DROP CONSTRAINT billing_payment_attempts_account_scope_fk",
    )
    .execute(&fixture.database.pool)
    .await?;
    sqlx::query("UPDATE billing_payment_attempts SET gateway_account_id = $2 WHERE id = $1")
        .bind(fixture.command.attempt_id().as_uuid())
        .bind(Uuid::now_v7())
        .execute(&fixture.database.pool)
        .await?;

    assert_application_invalid_state(
        apply_reconciled_subscription_enrollment_gateway_outcome(
            &fixture.database.pool,
            &fixture.coordinator,
            fixture.command.billing_scope_id(),
            fixture.command.attempt_id(),
            &approved_outcome("txn_missing_account"),
        )
        .await
        .expect_err("orphaned attempt must report its missing gateway account"),
        "subscription enrollment gateway account was not found",
    );
    fixture.cleanup().await
}

#[tokio::test]
async fn reconciled_wrapper_reports_invalid_persisted_provider_key() -> Result<(), Box<dyn Error>> {
    let fixture = application_fixture("rec_bad_provider", false, false).await?;
    sqlx::query(
        "ALTER TABLE billing_gateway_accounts \
         DROP CONSTRAINT billing_gateway_accounts_provider_fk",
    )
    .execute(&fixture.database.pool)
    .await?;
    sqlx::query("UPDATE billing_gateway_accounts SET provider_key = 'INVALID!' WHERE id = $1")
        .bind(fixture.gateway_account.gateway_account_id)
        .execute(&fixture.database.pool)
        .await?;

    assert_application_invalid_state(
        apply_reconciled_subscription_enrollment_gateway_outcome(
            &fixture.database.pool,
            &fixture.coordinator,
            fixture.command.billing_scope_id(),
            fixture.command.attempt_id(),
            &approved_outcome("txn_invalid_provider"),
        )
        .await
        .expect_err("invalid provider key must fail reconstruction"),
        "subscription enrollment gateway provider key is invalid",
    );
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
    let reservation =
        match reserve_subscription_recovery_in_transaction(&mut transaction, &command, &resolved)
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

    let error = submit_admitted_subscription_recovery(
        &fixture.database.pool,
        &fixture.coordinator,
        admission,
        &changed_contact,
        &resolved,
    )
    .await
    .expect_err("changed durable contact must invalidate admission");
    assert!(matches!(
        error,
        SubscriptionEnrollmentApplicationError::SubmissionIdentityMismatch
    ));
    assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 0);
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

    let error = submit_admitted_subscription_payment_method_replacement(
        &fixture.database.pool,
        &fixture.coordinator,
        admission,
        &changed_idempotency,
        &resolved,
    )
    .await
    .expect_err("changed durable idempotency key must invalidate admission");
    assert!(matches!(
        error,
        SubscriptionEnrollmentApplicationError::SubmissionIdentityMismatch
    ));
    assert_eq!(gateway.store_calls.load(Ordering::SeqCst), 0);
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
async fn exhausted_attempt_lock_retries_use_the_lock_free_approved_evidence_fallback()
-> Result<(), Box<dyn Error>> {
    let fixture = application_fixture("lock_fallback", false, false).await?;
    let attempt_id = fixture.reservation.identity().attempt_id();
    let mut blocker = fixture.database.pool.begin().await?;
    // `FOR NO KEY UPDATE` blocks the parking path's `FOR UPDATE` while still
    // allowing the lock-free charge insert's foreign-key `KEY SHARE` check.
    sqlx::query("SELECT id FROM billing_payment_attempts WHERE id = $1 FOR NO KEY UPDATE")
        .bind(attempt_id.as_uuid())
        .fetch_one(&mut *blocker)
        .await?;

    let outcome = GatewayPaymentOutcome::new(
        GatewayPaymentStatus::Approved,
        ProcessorEvidence::new(
            Some(GatewayTransactionId::new("txn_lock_free_fallback")?),
            None,
            Some(GatewayDiagnostic::new("approved")),
            Some(GatewayDiagnostic::new("100")),
            None,
            None,
            GatewayPaymentDescriptor::default(),
        ),
    );
    let result = apply_subscription_enrollment_gateway_outcome(
        &fixture.database.pool,
        &fixture.coordinator,
        &fixture.reservation,
        &outcome,
    )
    .await;
    let result = match result {
        Ok(result) => result,
        Err(SubscriptionEnrollmentApplicationError::Sql(error)) => {
            return Err(format!("unexpected parking SQL error: {error:?}").into());
        }
        Err(error) => return Err(format!("unexpected parking error: {error:?}").into()),
    };

    assert_eq!(result.attempt().status(), PaymentAttemptStatus::Pending);
    assert!(result.is_confirmation_pending());
    let charge: (String, String, i32, String) = sqlx::query_as(
        r#"
        SELECT gateway_transaction_id, progression_state, amount_cents, currency
        FROM billing_processor_charges
        WHERE attempt_id = $1
        "#,
    )
    .bind(attempt_id.as_uuid())
    .fetch_one(&fixture.database.pool)
    .await?;
    assert_eq!(
        charge,
        (
            "txn_lock_free_fallback".to_owned(),
            "pending".to_owned(),
            1_000,
            "USD".to_owned(),
        )
    );

    blocker.rollback().await?;
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
