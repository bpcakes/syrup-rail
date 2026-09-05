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
            required_gateway_account_mode,
            id, billing_scope_id, subscriber_id, host_charge_target_id,
            attempt_kind, status, idempotency_key, request_fingerprint,
            amount_cents, currency, gateway_account_id,
            gateway_configuration_id, gateway_order_id
        ) VALUES (
            'live', $1, $2, $3, $4, 'host_charge', 'pending', $5, $6,
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
            syrup_rail::ProcessorApprovalEvidence::Unclassified,
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
