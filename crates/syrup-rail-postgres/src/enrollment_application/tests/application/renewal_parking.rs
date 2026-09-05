use super::*;

#[tokio::test]
async fn exhausted_renewal_attempt_lock_retries_refuse_lock_free_approved_evidence()
-> Result<(), Box<dyn Error>> {
    let fixture = application_fixture("renew_lock", false, false).await?;
    let result = async {
        let initial = apply_subscription_enrollment_gateway_outcome(
            &fixture.database.pool,
            &fixture.coordinator,
            &fixture.reservation,
            &approved_outcome("txn_renew_lock_initial"),
        )
        .await?;
        let subscription_id = initial.subscription().expect("approved enrollment").id();
        let due_at: DateTime<Utc> = sqlx::query_scalar(
            r#"
            UPDATE billing_subscriptions
            SET current_period_start_at = $2 - interval '1 month',
                current_period_end_at = $2,
                next_renewal_at = $2,
                next_payment_attempt_at = $2,
                updated_at = clock_timestamp()
            WHERE id = $1
            RETURNING next_renewal_at
            "#,
        )
        .bind(subscription_id.as_uuid())
        .bind(Utc::now() - ChronoDuration::hours(1))
        .fetch_one(&fixture.database.pool)
        .await?;
        let resolved =
            scripted_resolved_gateway(fixture.gateway_account, Arc::new(NeverCalledGateway));
        let command =
            ChargeRenewal::new(fixture.command.billing_scope_id(), subscription_id, due_at);
        let mut transaction = fixture.database.pool.begin().await?;
        let reservation = match reserve_subscription_renewal_in_transaction(
            &mut transaction,
            command,
            &resolved,
            GatewayAccountMode::Live,
        )
        .await?
        {
            SubscriptionRenewalReservationOutcome::Reserved(reservation, _) => *reservation,
            other => return Err(format!("unexpected renewal reservation: {other:?}").into()),
        };
        transaction.commit().await?;
        let admission =
            crate::admit_subscription_renewal_submission(&fixture.database.pool, &reservation)
                .await?;
        assert!(matches!(
            admission,
            crate::SubscriptionRenewalAdmissionOutcome::Admitted(_)
        ));

        let attempt_id = reservation.identity().attempt_id();
        let mut blocker = fixture.database.pool.begin().await?;
        // Block application and evidence retries while permitting the KEY SHARE
        // foreign-key check that a lock-free charge insert would acquire.
        sqlx::query("SELECT id FROM billing_payment_attempts WHERE id = $1 FOR NO KEY UPDATE")
            .bind(attempt_id.as_uuid())
            .execute(&mut *blocker)
            .await?;
        let approved = approved_outcome("txn_renew_lock_approved");
        let application = tokio::time::timeout(
            Duration::from_secs(10),
            apply_subscription_renewal_gateway_outcome(
                &fixture.database.pool,
                &fixture.coordinator,
                &reservation,
                &approved,
            ),
        )
        .await?;
        assert!(matches!(
            application,
            Err(SubscriptionEnrollmentApplicationError::ApprovedEvidenceNotDurable)
        ));
        let (status, charge_count): (String, i64) = sqlx::query_as(
            r#"
            SELECT status,
                (SELECT count(*) FROM billing_processor_charges WHERE attempt_id = $1)
            FROM billing_payment_attempts WHERE id = $1
            "#,
        )
        .bind(attempt_id.as_uuid())
        .fetch_one(&fixture.database.pool)
        .await?;
        assert_eq!(status, "pending");
        assert_eq!(
            charge_count, 0,
            "renewal evidence must wait for the attempt lock"
        );
        blocker.rollback().await?;

        let applied = apply_subscription_renewal_gateway_outcome(
            &fixture.database.pool,
            &fixture.coordinator,
            &reservation,
            &approved,
        )
        .await?;
        assert_eq!(applied.status(), PaymentAttemptStatus::Approved);
        assert!(applied.subscription().is_some());
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = fixture.cleanup().await;
    result?;
    cleanup
}
