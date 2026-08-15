use super::*;

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
