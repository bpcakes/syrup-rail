use super::*;

#[test]
fn expected_gateway_identity_requires_every_persisted_component() {
    let account_id = Uuid::from_u128(1);
    let configuration_id = Uuid::from_u128(2);
    let provider_key = GatewayProviderKey::new("nmi").expect("valid provider key");
    let expected = ExpectedGatewayIdentity {
        billing_scope_id: BillingScopeId::new(Uuid::from_u128(3)),
        gateway_account_id: GatewayAccountId::new(account_id),
        gateway_configuration_id: GatewayConfigurationId::new(configuration_id),
        provider_key: &provider_key,
    };

    assert!(expected.matches_row(account_id, configuration_id, "nmi"));
    assert!(!expected.matches_row(Uuid::from_u128(4), configuration_id, "nmi"));
    assert!(!expected.matches_row(account_id, Uuid::from_u128(5), "nmi"));
    assert!(!expected.matches_row(account_id, configuration_id, "other_gateway"));
}

#[tokio::test]
async fn enrollment_reservation_is_token_free_replayable_and_plan_bearing()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("enroll_reserve").await?;
    install_host_offers(&database).await?;
    let account = create_gateway_account(&database.pool, "nmi").await?;
    set_offer(
        &database,
        account.billing_scope_id,
        "base_subscription",
        1_000,
    )
    .await?;
    set_offer(&database, account.billing_scope_id, "premium", 1_000).await?;
    let subscriber_id = Uuid::now_v7();
    let gateway = resolved_gateway(account);
    let mut mismatched_account = account;
    mismatched_account.gateway_configuration_id = Uuid::now_v7();
    let mismatched_command = enrollment_command(
        mismatched_account,
        subscriber_id,
        Uuid::now_v7(),
        "mismatched-gateway",
        full_price("base_subscription", 1_000),
    );
    assert_eq!(
        SubscriptionEnrollmentReservation::from_command(&mismatched_command, &gateway)
            .expect_err("reservation must bind to the resolved configuration"),
        syrup_rail::SubscriptionEnrollmentReservationBuildError::GatewayIdentityMismatch,
    );
    let command = enrollment_command(
        account,
        subscriber_id,
        Uuid::now_v7(),
        "same-key",
        full_price("base_subscription", 1_000),
    );
    let reservation = SubscriptionEnrollmentReservation::from_command(&command, &gateway)?;
    let mut transaction = database.pool.begin().await?;
    let reserved = reserve_subscription_enrollment_in_transaction(
        &mut transaction,
        &TestOfferStore,
        &reservation,
    )
    .await?;
    let attempt = match reserved {
        SubscriptionEnrollmentReservationOutcome::Reserved(attempt) => attempt,
        other => return Err(format!("unexpected reservation outcome: {other:?}").into()),
    };
    assert_eq!(attempt.status(), PaymentAttemptStatus::Pending);
    assert_eq!(
        attempt.request().fingerprint().expose(),
        "subscription_initial:v2:base_subscription:start:recurring_immediately:trial:none:recurring:1000:USD:calendar_months:1:dunning:[]:remain_past_due:suspend_immediately:initial:1000:USD:discount:none"
    );
    assert!(attempt.state().timestamps().submitted_at().is_none());
    transaction.commit().await?;

    let persisted: String = sqlx::query_scalar(
        "SELECT to_jsonb(attempts)::text FROM billing_payment_attempts attempts WHERE id = $1",
    )
    .bind(attempt.identity().attempt_id().as_uuid())
    .fetch_one(&database.pool)
    .await?;
    assert!(!persisted.contains("token-secret"));
    assert!(!format!("{reservation:?}").contains("token-secret"));

    let replay_command = enrollment_command(
        account,
        subscriber_id,
        Uuid::now_v7(),
        "same-key",
        full_price("base_subscription", 1_000),
    );
    let replay_reservation =
        SubscriptionEnrollmentReservation::from_command(&replay_command, &gateway)?;
    let mut transaction = database.pool.begin().await?;
    let replay = reserve_subscription_enrollment_in_transaction(
        &mut transaction,
        &TestOfferStore,
        &replay_reservation,
    )
    .await?;
    assert!(matches!(
        replay,
        SubscriptionEnrollmentReservationOutcome::Replay(ref replayed)
            if replayed.identity().attempt_id() == attempt.identity().attempt_id()
    ));
    transaction.commit().await?;

    let changed_plan_command = enrollment_command(
        account,
        subscriber_id,
        Uuid::now_v7(),
        "same-key",
        full_price("premium", 1_000),
    );
    let changed_plan_reservation =
        SubscriptionEnrollmentReservation::from_command(&changed_plan_command, &gateway)?;
    let mut transaction = database.pool.begin().await?;
    assert_eq!(
        reserve_subscription_enrollment_in_transaction(
            &mut transaction,
            &TestOfferStore,
            &changed_plan_reservation,
        )
        .await?,
        SubscriptionEnrollmentReservationOutcome::IdempotencyConflict,
    );
    transaction.rollback().await?;

    let mut transaction = database.pool.begin().await?;
    let admitted = admit_subscription_enrollment_submission_in_transaction(
        &mut transaction,
        &TestOfferStore,
        &replay_reservation,
    )
    .await?;
    assert!(matches!(
        admitted,
        SubscriptionEnrollmentSubmissionOutcome::Admitted(ref admitted)
            if admitted.identity().attempt_id() == attempt.identity().attempt_id()
                && admitted.state().timestamps().submitted_at().is_some()
    ));
    transaction.commit().await?;
    database.cleanup().await
}

#[tokio::test]
async fn same_key_stale_replay_expires_at_the_exact_boundary_without_a_live_offer()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("enroll_stale").await?;
    install_host_offers(&database).await?;
    let account = create_gateway_account(&database.pool, "nmi").await?;
    let plan_key = "base_subscription";
    set_offer(&database, account.billing_scope_id, plan_key, 1_000).await?;
    let gateway = resolved_gateway(account);

    let before_boundary_subscriber = Uuid::now_v7();
    let before_boundary = SubscriptionEnrollmentReservation::from_command(
        &enrollment_command(
            account,
            before_boundary_subscriber,
            Uuid::now_v7(),
            "before-boundary",
            full_price(plan_key, 1_000),
        ),
        &gateway,
    )?;
    let mut transaction = database.pool.begin().await?;
    let before_boundary_attempt = match reserve_subscription_enrollment_in_transaction(
        &mut transaction,
        &TestOfferStore,
        &before_boundary,
    )
    .await?
    {
        SubscriptionEnrollmentReservationOutcome::Reserved(attempt) => attempt,
        other => return Err(format!("unexpected reservation outcome: {other:?}").into()),
    };
    transaction.commit().await?;
    sqlx::query(
            "UPDATE billing_payment_attempts SET created_at = clock_timestamp() - interval '29 minutes 59 seconds' WHERE id = $1",
        )
        .bind(before_boundary_attempt.identity().attempt_id().as_uuid())
        .execute(&database.pool)
        .await?;
    sqlx::query(
        "DELETE FROM host_subscription_offers WHERE billing_scope_id = $1 AND plan_key = $2",
    )
    .bind(account.billing_scope_id)
    .bind(plan_key)
    .execute(&database.pool)
    .await?;
    let mut transaction = database.pool.begin().await?;
    assert_eq!(
        reserve_subscription_enrollment_in_transaction(
            &mut transaction,
            &TestOfferStore,
            &before_boundary,
        )
        .await?,
        SubscriptionEnrollmentReservationOutcome::Rejected(
            SubscriptionEnrollmentReservationRejection::EnrollmentTermsChanged,
        )
    );
    transaction.rollback().await?;
    let before_boundary_status: String =
        sqlx::query_scalar("SELECT status FROM billing_payment_attempts WHERE id = $1")
            .bind(before_boundary_attempt.identity().attempt_id().as_uuid())
            .fetch_one(&database.pool)
            .await?;
    assert_eq!(before_boundary_status, "pending");

    set_offer(&database, account.billing_scope_id, plan_key, 1_000).await?;
    let boundary_subscriber = Uuid::now_v7();
    let boundary = SubscriptionEnrollmentReservation::from_command(
        &enrollment_command(
            account,
            boundary_subscriber,
            Uuid::now_v7(),
            "at-boundary",
            full_price(plan_key, 1_000),
        ),
        &gateway,
    )?;
    let mut transaction = database.pool.begin().await?;
    let boundary_attempt = match reserve_subscription_enrollment_in_transaction(
        &mut transaction,
        &TestOfferStore,
        &boundary,
    )
    .await?
    {
        SubscriptionEnrollmentReservationOutcome::Reserved(attempt) => attempt,
        other => return Err(format!("unexpected reservation outcome: {other:?}").into()),
    };
    transaction.commit().await?;
    sqlx::query(
            "UPDATE billing_payment_attempts SET created_at = clock_timestamp() - interval '30 minutes' WHERE id = $1",
        )
        .bind(boundary_attempt.identity().attempt_id().as_uuid())
        .execute(&database.pool)
        .await?;
    sqlx::query(
        "DELETE FROM host_subscription_offers WHERE billing_scope_id = $1 AND plan_key = $2",
    )
    .bind(account.billing_scope_id)
    .bind(plan_key)
    .execute(&database.pool)
    .await?;
    let boundary_replay = SubscriptionEnrollmentReservation::from_command(
        &enrollment_command(
            account,
            boundary_subscriber,
            Uuid::now_v7(),
            "at-boundary",
            full_price(plan_key, 1_000),
        ),
        &gateway,
    )?;

    let mut transaction = database.pool.begin().await?;
    let replay = reserve_subscription_enrollment_in_transaction(
        &mut transaction,
        &TestOfferStore,
        &boundary_replay,
    )
    .await?;
    let expired = match replay {
        SubscriptionEnrollmentReservationOutcome::Replay(attempt) => attempt,
        other => return Err(format!("unexpected stale replay outcome: {other:?}").into()),
    };
    assert_eq!(
        expired.identity().attempt_id(),
        boundary_attempt.identity().attempt_id()
    );
    assert_eq!(expired.status(), PaymentAttemptStatus::Failed);
    assert_eq!(
        expired.state().resolution_code(),
        Some(PaymentResolutionCode::SubscriptionInitialPreparedAttemptExpired)
    );
    assert!(expired.state().timestamps().submitted_at().is_none());
    transaction.commit().await?;
    database.cleanup().await
}

#[tokio::test]
async fn saved_discount_survives_repricing_but_not_pre_submission_expiry()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("enroll_discount").await?;
    install_host_offers(&database).await?;
    let account = create_gateway_account(&database.pool, "nmi").await?;
    let plan_key = "base_subscription";
    set_offer(&database, account.billing_scope_id, plan_key, 1_000).await?;
    let code_id = create_discount_code(&database, account.billing_scope_id, plan_key).await?;
    let gateway = resolved_gateway(account);

    let subscriber_a = Uuid::now_v7();
    let claim_a = create_saved_claim(
        &database,
        account.billing_scope_id,
        subscriber_a,
        plan_key,
        code_id,
    )
    .await?;
    let command_a = enrollment_command(
        account,
        subscriber_a,
        Uuid::now_v7(),
        "discount-a",
        discounted_expected(plan_key),
    );
    let reservation_a = SubscriptionEnrollmentReservation::from_command(&command_a, &gateway)?;
    let mut transaction = database.pool.begin().await?;
    let attempt_a = match reserve_subscription_enrollment_in_transaction(
        &mut transaction,
        &TestOfferStore,
        &reservation_a,
    )
    .await?
    {
        SubscriptionEnrollmentReservationOutcome::Reserved(attempt) => attempt,
        other => return Err(format!("unexpected discounted reservation: {other:?}").into()),
    };
    transaction.commit().await?;
    let discount = attempt_a
        .request()
        .target()
        .enrollment_discount()
        .expect("saved discount should be snapshotted");
    assert_eq!(discount.claim_id().as_uuid(), &claim_a);
    assert_eq!(discount.code_id().as_uuid(), &code_id);
    assert_eq!(attempt_a.request().amount().cents(), 800);
    assert_eq!(
        attempt_a.request().fingerprint().expose(),
        format!(
            "subscription_initial:v2:base_subscription:start:recurring_immediately:trial:none:recurring:1000:USD:calendar_months:1:dunning:[]:remain_past_due:suspend_immediately:initial:800:USD:discount:{claim_a}:{code_id}:SAVE20:percent_off:none:2000:USD:1000:800:limited_months:3"
        )
    );
    let debug = format!("{attempt_a:?}");
    assert!(!debug.contains("SAVE20"));
    assert!(!debug.contains("Sensitive campaign"));

    set_offer(&database, account.billing_scope_id, plan_key, 1_400).await?;
    let mut transaction = database.pool.begin().await?;
    assert!(matches!(
        admit_subscription_enrollment_submission_in_transaction(
            &mut transaction,
            &TestOfferStore,
            &reservation_a,
        )
        .await?,
        SubscriptionEnrollmentSubmissionOutcome::Admitted(_)
    ));
    transaction.commit().await?;

    let subscriber_b = Uuid::now_v7();
    let claim_b = create_saved_claim(
        &database,
        account.billing_scope_id,
        subscriber_b,
        plan_key,
        code_id,
    )
    .await?;
    let command_b = enrollment_command(
        account,
        subscriber_b,
        Uuid::now_v7(),
        "discount-b",
        discounted_expected(plan_key),
    );
    let reservation_b = SubscriptionEnrollmentReservation::from_command(&command_b, &gateway)?;
    let mut transaction = database.pool.begin().await?;
    assert!(matches!(
        reserve_subscription_enrollment_in_transaction(
            &mut transaction,
            &TestOfferStore,
            &reservation_b,
        )
        .await?,
        SubscriptionEnrollmentReservationOutcome::Reserved(_)
    ));
    transaction.commit().await?;
    sqlx::query("UPDATE billing_subscription_discount_claims SET status = 'expired' WHERE id = $1")
        .bind(claim_b)
        .execute(&database.pool)
        .await?;
    let mut transaction = database.pool.begin().await?;
    let rejected = admit_subscription_enrollment_submission_in_transaction(
        &mut transaction,
        &TestOfferStore,
        &reservation_b,
    )
    .await?;
    assert!(matches!(
        rejected,
        SubscriptionEnrollmentSubmissionOutcome::Rejected {
            ref attempt,
            reason: SubscriptionEnrollmentSubmissionRejection::EnrollmentTermsChanged,
        } if attempt.status() == PaymentAttemptStatus::Failed
            && attempt.state().timestamps().submitted_at().is_none()
    ));
    transaction.commit().await?;
    database.cleanup().await
}
