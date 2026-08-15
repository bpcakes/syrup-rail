use super::*;

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
        SubscriptionBillingServiceError::GatewayMutationCooldown {
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
