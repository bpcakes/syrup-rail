use super::*;

#[tokio::test]
async fn paid_trial_recovery_collects_discounted_recurring_period_and_invalidates_queued_retry()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("pt_recovery").await?;
    let account = create_gateway_account(&database.pool, "nmi").await?;
    let gateway = resolved_gateway(account)?;
    let offer = paid_trial_offer()?;
    let offers = StaticOfferStore {
        offer: offer.clone(),
    };
    let subscriber_id = SubscriberId::new(Uuid::now_v7());
    let code_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO billing_subscription_discount_codes (
            id, billing_scope_id, plan_key, code_normalized, display_code,
            status, discount_kind, percent_off_bps, currency,
            duration, duration_months
        ) VALUES (
            $1, $2, 'identity_pro', 'SAVE20', 'SAVE20',
            'active', 'percent_off', 2000, 'USD', 'limited_months', 3
        )
        "#,
    )
    .bind(code_id)
    .bind(account.billing_scope_id)
    .execute(&database.pool)
    .await?;
    sqlx::query(
        r#"
        INSERT INTO billing_subscription_discount_claims (
            id, billing_scope_id, subscriber_id, plan_key,
            discount_code_id, code_snapshot, discount_kind,
            percent_off_bps, currency, duration, duration_months,
            base_amount_cents, discounted_amount_cents, status
        ) VALUES (
            $1, $2, $3, 'identity_pro', $4, 'SAVE20', 'percent_off',
            2000, 'USD', 'limited_months', 3, 2900, 2320, 'saved'
        )
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(account.billing_scope_id)
    .bind(subscriber_id.as_uuid())
    .bind(code_id)
    .execute(&database.pool)
    .await?;
    let discount = SubscriptionDiscountSnapshot::new(
        SubscriptionDiscountCode::new("SAVE20")?,
        None,
        SubscriptionDiscountKind::PercentOffBasisPoints(PercentOffBasisPoints::new(2_000)?),
        SubscriptionDiscountDuration::LimitedMonths(LimitedDiscountMonths::new(3)?),
        ChargeAmount::new(2_900, CurrencyCode::new("USD")?)?,
        ChargeAmount::new(2_320, CurrencyCode::new("USD")?)?,
    )?;
    let command = syrup_rail::EnrollSubscription::new(
        syrup_rail::SubscriptionPaymentContext::new(
            PaymentAttemptId::new(Uuid::now_v7()),
            BillingScopeId::new(account.billing_scope_id),
            subscriber_id,
            GatewayConfigurationId::new(account.gateway_configuration_id),
            IdempotencyKey::new("paid-trial-recovery")?,
            PaymentToken::new("opaque-paid-trial-token")?,
            BillingContact::new(None, None, Some("discount@example.test".to_owned()))?,
        ),
        SubscriptionEnrollmentExpectedTerms::discounted(offer, discount)?,
    );
    let enrollment = SubscriptionEnrollmentReservation::from_command(
        &command,
        &gateway,
        GatewayAccountMode::Live,
    )?;
    let mut transaction = database.pool.begin().await?;
    assert!(matches!(
        reserve_subscription_enrollment_in_transaction(&mut transaction, &offers, &enrollment)
            .await?,
        SubscriptionEnrollmentReservationOutcome::Reserved(_)
    ));
    transaction.commit().await?;
    assert!(matches!(
        admit_subscription_enrollment_submission(&database.pool, &offers, &enrollment).await?,
        SubscriptionEnrollmentAdmissionOutcome::Admitted(_)
    ));
    let events = Arc::new(Mutex::new(Vec::new()));
    let coordinator = TestCoordinator {
        pool: database.pool.clone(),
        events: Arc::clone(&events),
    };
    let initial = apply_subscription_enrollment_gateway_outcome(
        &database.pool,
        &coordinator,
        &enrollment,
        &approved_outcome("discounted_trial_initial"),
    )
    .await?;
    let initial_subscription = initial
        .subscription()
        .expect("approved trial creates subscription");
    let subscription_id = initial_subscription.id();
    let original_payment_method_id = initial_subscription.payment_method_id();
    let initial_discount: (i32, i32, String, i32) = sqlx::query_as(
        r#"
        SELECT periods_applied, periods_total, discounts.status, subscriptions.amount_cents
        FROM billing_subscription_discounts AS discounts
        JOIN billing_subscriptions AS subscriptions ON subscriptions.id = discounts.subscription_id
        WHERE discounts.subscription_id = $1
        "#,
    )
    .bind(subscription_id.as_uuid())
    .fetch_one(&database.pool)
    .await?;
    assert_eq!(initial_discount, (0, 3, "active".to_owned(), 2_320));

    let unpaid_history_id = Uuid::now_v7();
    sqlx::query(
        r#"
        WITH clock AS MATERIALIZED (SELECT clock_timestamp() AS observed_at)
        INSERT INTO billing_subscriptions (
            required_gateway_account_mode,
            id, billing_scope_id, subscriber_id, plan_key, status,
            gateway_account_id, payment_method_id, amount_cents, currency,
            current_period_start_at, current_period_end_at, next_renewal_at,
            initial_transaction_id, phase, recurring_period_kind,
            recurring_period_count, dunning_retry_delays_seconds,
            dunning_exhaustion, past_due_access, next_payment_attempt_at, unpaid_at
        ) SELECT
            'live', $1, $2, $3, 'identity_pro', 'unpaid', $4, $5, 2900, 'USD',
            observed_at - interval '2 months', observed_at - interval '1 month',
            observed_at - interval '1 month', $6, 'recurring',
            'calendar_months', 1, ARRAY[]::bigint[], 'mark_unpaid',
            'continue_until_dunning_exhausted', NULL, observed_at
        FROM clock
        "#,
    )
    .bind(unpaid_history_id)
    .bind(account.billing_scope_id)
    .bind(subscriber_id.as_uuid())
    .bind(account.gateway_account_id)
    .bind(original_payment_method_id.as_uuid())
    .bind(format!("unpaid_history_{}", unpaid_history_id.simple()))
    .execute(&database.pool)
    .await?;

    let due_at: DateTime<Utc> =
        sqlx::query_scalar("SELECT clock_timestamp() - interval '1 second'")
            .fetch_one(&database.pool)
            .await?;
    sqlx::query(
        r#"
        UPDATE billing_subscriptions
        SET current_period_start_at = $2 - interval '7 days',
            current_period_end_at = $2,
            next_renewal_at = $2,
            next_payment_attempt_at = $2
        WHERE id = $1
        "#,
    )
    .bind(subscription_id.as_uuid())
    .bind(due_at)
    .execute(&database.pool)
    .await?;
    let queued_renewal = ChargeRenewal::new(
        BillingScopeId::new(account.billing_scope_id),
        subscription_id,
        due_at,
    );
    let renewal = reserve_and_admit_renewal(&database.pool, &gateway, queued_renewal).await?;
    assert_eq!(renewal.request().amount().cents(), 2_320);
    apply_subscription_renewal_gateway_outcome(
        &database.pool,
        &coordinator,
        &renewal,
        &declined_outcome("discounted_trial_decline"),
    )
    .await?;

    let scheduled_retry: Option<DateTime<Utc>> = sqlx::query_scalar(
        "SELECT next_payment_attempt_at FROM billing_subscriptions WHERE id = $1",
    )
    .bind(subscription_id.as_uuid())
    .fetch_one(&database.pool)
    .await?;
    let failed_recovery_command = RecoverSubscriptionPayment::new(
        syrup_rail::SubscriptionPaymentContext::new(
            PaymentAttemptId::new(Uuid::now_v7()),
            BillingScopeId::new(account.billing_scope_id),
            subscriber_id,
            GatewayConfigurationId::new(account.gateway_configuration_id),
            IdempotencyKey::new("paid-trial-failed-recovery")?,
            PaymentToken::new("opaque-failed-recovery-token")?,
            BillingContact::new(None, None, Some("failed-recovery@example.test".to_owned()))?,
        ),
        PlanKey::new("identity_pro")?,
    );
    let failed_recovery =
        reserve_and_admit_recovery(&database.pool, &gateway, &failed_recovery_command).await?;
    let failed_recovery_result = apply_subscription_recovery_gateway_outcome(
        &database.pool,
        &coordinator,
        &failed_recovery,
        &declined_outcome("discounted_failed_recovery"),
    )
    .await?;
    assert_eq!(
        failed_recovery_result.attempt().status(),
        syrup_rail::PaymentAttemptStatus::Declined
    );
    let retry_after_failed_recovery: Option<DateTime<Utc>> = sqlx::query_scalar(
        "SELECT next_payment_attempt_at FROM billing_subscriptions WHERE id = $1",
    )
    .bind(subscription_id.as_uuid())
    .fetch_one(&database.pool)
    .await?;
    assert_eq!(retry_after_failed_recovery, scheduled_retry);

    let recovery_command = RecoverSubscriptionPayment::new(
        syrup_rail::SubscriptionPaymentContext::new(
            PaymentAttemptId::new(Uuid::now_v7()),
            BillingScopeId::new(account.billing_scope_id),
            subscriber_id,
            GatewayConfigurationId::new(account.gateway_configuration_id),
            IdempotencyKey::new("paid-trial-user-recovery")?,
            PaymentToken::new("opaque-recovery-token")?,
            BillingContact::new(None, None, Some("recovery@example.test".to_owned()))?,
        ),
        PlanKey::new("identity_pro")?,
    );
    let recovery = reserve_and_admit_recovery(&database.pool, &gateway, &recovery_command).await?;
    assert_eq!(recovery.request().amount().cents(), 2_320);
    assert_eq!(recovery.period().start_at(), &due_at);
    let unknown = apply_subscription_recovery_gateway_outcome(
        &database.pool,
        &coordinator,
        &recovery,
        &unknown_outcome(),
    )
    .await?;
    assert_eq!(unknown.status(), syrup_rail::PaymentAttemptStatus::Unknown);
    let retry_after_unknown: Option<DateTime<Utc>> = sqlx::query_scalar(
        "SELECT next_payment_attempt_at FROM billing_subscriptions WHERE id = $1",
    )
    .bind(subscription_id.as_uuid())
    .fetch_one(&database.pool)
    .await?;
    assert_eq!(retry_after_unknown, scheduled_retry);

    let blocked_recovery = RecoverSubscriptionPayment::new(
        syrup_rail::SubscriptionPaymentContext::new(
            PaymentAttemptId::new(Uuid::now_v7()),
            BillingScopeId::new(account.billing_scope_id),
            subscriber_id,
            GatewayConfigurationId::new(account.gateway_configuration_id),
            IdempotencyKey::new("paid-trial-blocked-recovery")?,
            PaymentToken::new("opaque-blocked-recovery-token")?,
            BillingContact::new(None, None, Some("blocked@example.test".to_owned()))?,
        ),
        PlanKey::new("identity_pro")?,
    );
    let mut transaction = database.pool.begin().await?;
    let blocked = reserve_subscription_recovery_in_transaction(
        &mut transaction,
        &blocked_recovery,
        &gateway,
        GatewayAccountMode::Live,
    )
    .await?;
    transaction.rollback().await?;
    assert_eq!(
        blocked,
        syrup_rail::SubscriptionRecoveryReservationOutcome::Rejected(
            syrup_rail::SubscriptionRecoveryReservationRejection::AttemptInProgress,
        )
    );

    let recovered = apply_reconciled_subscription_recovery_gateway_outcome(
        &database.pool,
        &coordinator,
        BillingScopeId::new(account.billing_scope_id),
        recovery.identity().attempt_id(),
        &approved_outcome_with_reference("discounted_trial_recovery", "vault_recovery"),
    )
    .await?;
    let subscription = recovered
        .subscription()
        .expect("approved recovery restores subscription");
    assert_eq!(subscription.status(), SubscriptionStatus::Active);
    assert_eq!(subscription.phase(), SubscriptionPhase::Recurring);
    assert_eq!(subscription.current_period().start_at(), &due_at);
    assert_eq!(
        subscription.next_payment_attempt_at(),
        Some(subscription.current_period().end_at())
    );
    assert_eq!(
        subscription.next_renewal_at(),
        subscription.current_period().end_at()
    );
    let recovered_discount: (i32, String, i32) = sqlx::query_as(
        r#"
        SELECT discounts.periods_applied, discounts.status, subscriptions.amount_cents
        FROM billing_subscription_discounts AS discounts
        JOIN billing_subscriptions AS subscriptions ON subscriptions.id = discounts.subscription_id
        WHERE discounts.subscription_id = $1
        "#,
    )
    .bind(subscription_id.as_uuid())
    .fetch_one(&database.pool)
    .await?;
    assert_eq!(recovered_discount, (1, "active".to_owned(), 2_320));
    let original_method_status: String =
        sqlx::query_scalar("SELECT status FROM billing_payment_methods WHERE id = $1")
            .bind(original_payment_method_id.as_uuid())
            .fetch_one(&database.pool)
            .await?;
    assert_eq!(original_method_status, "disabled");

    let mut transaction = database.pool.begin().await?;
    let stale = reserve_subscription_renewal_in_transaction(
        &mut transaction,
        queued_renewal,
        &gateway,
        GatewayAccountMode::Live,
    )
    .await?;
    transaction.rollback().await?;
    assert_eq!(
        stale,
        syrup_rail::SubscriptionRenewalReservationOutcome::Rejected(
            syrup_rail::SubscriptionRenewalReservationRejection::PaymentNotDue,
        )
    );
    let emitted = events.lock().await.clone();
    assert_eq!(emitted.len(), 3);
    assert!(matches!(
        emitted[0],
        BillingEvent::SubscriptionStarted { .. }
    ));
    assert!(matches!(
        emitted[1],
        BillingEvent::SubscriptionPaymentFailed { .. }
    ));
    assert!(matches!(
        emitted[2],
        BillingEvent::SubscriptionRenewed { .. }
    ));

    database.cleanup().await
}
