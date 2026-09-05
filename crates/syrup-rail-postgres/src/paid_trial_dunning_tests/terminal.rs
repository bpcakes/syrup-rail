use super::*;

#[tokio::test]
async fn indeterminate_diagnostics_keep_terminal_renewal_statuses_out_of_dunning()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("pt_indet_term").await?;
    let account = create_gateway_account(&database.pool, "nmi").await?;
    let gateway = resolved_gateway(account)?;
    let offer = paid_trial_offer()?;
    let offers = StaticOfferStore {
        offer: offer.clone(),
    };
    let events = Arc::new(Mutex::new(Vec::new()));
    let coordinator = TestCoordinator {
        pool: database.pool.clone(),
        events: Arc::clone(&events),
    };

    for (provider_status, suffix) in [
        (GatewayPaymentStatus::Declined, "declined"),
        (GatewayPaymentStatus::Failed, "failed"),
    ] {
        let subscriber_id = SubscriberId::new(Uuid::now_v7());
        let enrollment = approve_enrollment(
            &database.pool,
            &offers,
            &gateway,
            &coordinator,
            account,
            subscriber_id,
            &format!("indeterminate-terminal-{suffix}"),
            SubscriptionEnrollmentExpectedTerms::full_price(offer.clone()),
            &format!("indeterminate_initial_{suffix}"),
            &format!("vault_indeterminate_{suffix}"),
        )
        .await?;
        let subscription_id = enrollment
            .subscription()
            .expect("approved enrollment creates a subscription")
            .id();
        let due_at = make_trial_due(&database.pool, subscription_id).await?;
        let reservation = reserve_and_admit_renewal(
            &database.pool,
            &gateway,
            ChargeRenewal::new(
                BillingScopeId::new(account.billing_scope_id),
                subscription_id,
                due_at,
            ),
        )
        .await?;
        let outcome = GatewayPaymentOutcome::new(
            provider_status,
            ProcessorEvidence::new(
                syrup_rail::ProcessorApprovalEvidence::Unclassified,
                Some(
                    GatewayTransactionId::new(format!("indeterminate_{suffix}_txn"))
                        .expect("valid transaction ID"),
                ),
                None,
                Some(GatewayDiagnostic::new("3")),
                Some(GatewayDiagnostic::new("400")),
                Some(GatewayDiagnostic::new("Processor error")),
                None,
                GatewayPaymentDescriptor::default(),
            ),
        )
        .with_diagnostics(vec![GatewayPaymentDiagnostic::IndeterminatePaymentOutcome]);
        assert_eq!(outcome.status(), GatewayPaymentStatus::Unknown, "{suffix}");

        let result = apply_subscription_renewal_gateway_outcome(
            &database.pool,
            &coordinator,
            &reservation,
            &outcome,
        )
        .await?;
        assert_eq!(
            result.attempt().status(),
            syrup_rail::PaymentAttemptStatus::Unknown,
            "{suffix}"
        );
        assert_eq!(
            result.observation_diagnostics(),
            &[GatewayPaymentDiagnostic::IndeterminatePaymentOutcome],
            "{suffix}"
        );

        let projection: (String, DateTime<Utc>, DateTime<Utc>, Option<DateTime<Utc>>) =
            sqlx::query_as(
                "SELECT status, next_renewal_at, next_payment_attempt_at, unpaid_at \
                 FROM billing_subscriptions WHERE id = $1",
            )
            .bind(subscription_id.as_uuid())
            .fetch_one(&database.pool)
            .await?;
        assert_eq!(
            projection,
            ("active".to_owned(), due_at, due_at, None),
            "{suffix}"
        );
        let qualifying_failure_count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM billing_payment_attempts \
             WHERE subscription_id = $1 \
               AND attempt_kind = 'subscription_renewal' \
               AND status IN ('declined', 'failed') \
               AND submitted_at IS NOT NULL \
               AND resolution_code IS NULL",
        )
        .bind(subscription_id.as_uuid())
        .fetch_one(&database.pool)
        .await?;
        assert_eq!(qualifying_failure_count, 0, "{suffix}");
    }

    assert_eq!(events.lock().await.len(), 2);
    database.cleanup().await
}

#[tokio::test]
async fn reconciled_renewal_outcomes_preserve_durable_identity_and_do_not_start_dunning()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("pt_renew_id").await?;
    let account = create_gateway_account(&database.pool, "nmi").await?;
    let gateway = resolved_gateway(account)?;
    let offer = paid_trial_offer()?;
    let offers = StaticOfferStore {
        offer: offer.clone(),
    };
    let events = Arc::new(Mutex::new(Vec::new()));
    let coordinator = TestCoordinator {
        pool: database.pool.clone(),
        events: Arc::clone(&events),
    };
    let subscriber_id = SubscriberId::new(Uuid::now_v7());
    let enrollment = approve_enrollment(
        &database.pool,
        &offers,
        &gateway,
        &coordinator,
        account,
        subscriber_id,
        "renewal-identity-enrollment",
        SubscriptionEnrollmentExpectedTerms::full_price(offer),
        "txn_renewal_identity_initial",
        "vault_renewal_identity_initial",
    )
    .await?;
    let subscription_id = enrollment
        .subscription()
        .expect("approved enrollment creates a subscription")
        .id();
    let due_at = make_trial_due(&database.pool, subscription_id).await?;
    let reservation = reserve_and_admit_renewal(
        &database.pool,
        &gateway,
        ChargeRenewal::new(
            BillingScopeId::new(account.billing_scope_id),
            subscription_id,
            due_at,
        ),
    )
    .await?;
    let unknown = GatewayPaymentOutcome::new(
        GatewayPaymentStatus::Unknown,
        ProcessorEvidence::new(
            syrup_rail::ProcessorApprovalEvidence::Unclassified,
            Some(GatewayTransactionId::new("txn_renewal_durable")?),
            None,
            Some(GatewayDiagnostic::new("3")),
            Some(GatewayDiagnostic::new("400")),
            Some(GatewayDiagnostic::new("Processor outcome unknown")),
            Some(GatewayDiagnostic::new("unknown")),
            GatewayPaymentDescriptor::default(),
        ),
    );
    let durable = apply_subscription_renewal_gateway_outcome(
        &database.pool,
        &coordinator,
        &reservation,
        &unknown,
    )
    .await?;
    assert_eq!(
        durable.attempt().status(),
        syrup_rail::PaymentAttemptStatus::Unknown
    );

    let conflicting_decline = apply_reconciled_subscription_renewal_gateway_outcome(
        &database.pool,
        &coordinator,
        BillingScopeId::new(account.billing_scope_id),
        reservation.identity().attempt_id(),
        &declined_outcome("txn_renewal_conflict"),
    )
    .await?;
    assert_eq!(
        conflicting_decline.attempt().status(),
        syrup_rail::PaymentAttemptStatus::Unknown
    );
    assert_eq!(
        conflicting_decline.observation_diagnostics(),
        &[GatewayPaymentDiagnostic::InvalidOrConflictingTransactionIdentifier]
    );

    let conflicting_approval = apply_reconciled_subscription_renewal_gateway_outcome(
        &database.pool,
        &coordinator,
        BillingScopeId::new(account.billing_scope_id),
        reservation.identity().attempt_id(),
        &approved_outcome_with_reference("txn_renewal_conflict", "vault_renewal_conflict"),
    )
    .await?;
    assert_eq!(
        conflicting_approval.attempt().status(),
        syrup_rail::PaymentAttemptStatus::Unknown
    );
    assert_eq!(
        conflicting_approval.observation_diagnostics(),
        &[GatewayPaymentDiagnostic::InvalidOrConflictingTransactionIdentifier]
    );
    let durable_transaction_id: Option<String> = sqlx::query_scalar(
        "SELECT gateway_transaction_id FROM billing_payment_attempts WHERE id = $1",
    )
    .bind(reservation.identity().attempt_id().as_uuid())
    .fetch_one(&database.pool)
    .await?;
    assert_eq!(
        durable_transaction_id.as_deref(),
        Some("txn_renewal_durable")
    );
    let conflicting_charge: (String, String) = sqlx::query_as(
        "SELECT gateway_transaction_id, progression_state \
         FROM billing_processor_charges WHERE attempt_id = $1",
    )
    .bind(reservation.identity().attempt_id().as_uuid())
    .fetch_one(&database.pool)
    .await?;
    assert_eq!(
        conflicting_charge,
        (
            "txn_renewal_conflict".to_owned(),
            "reconciliation_required".to_owned(),
        )
    );
    let subscription_state: (String, DateTime<Utc>, Option<DateTime<Utc>>) = sqlx::query_as(
        "SELECT status, next_renewal_at, unpaid_at FROM billing_subscriptions WHERE id = $1",
    )
    .bind(subscription_id.as_uuid())
    .fetch_one(&database.pool)
    .await?;
    assert_eq!(subscription_state, ("active".to_owned(), due_at, None));
    assert_eq!(events.lock().await.len(), 1);
    database.cleanup().await
}

#[tokio::test]
async fn paid_trial_dunning_transitions_to_unpaid_once_with_exact_schedule_and_events()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("pt_dunning").await?;
    let account = create_gateway_account(&database.pool, "nmi").await?;
    let gateway = resolved_gateway(account)?;
    let offer = paid_trial_offer()?;
    let offers = StaticOfferStore {
        offer: offer.clone(),
    };
    let subscriber_id = SubscriberId::new(Uuid::now_v7());
    let command = syrup_rail::EnrollSubscription::new(
        syrup_rail::SubscriptionPaymentContext::new(
            PaymentAttemptId::new(Uuid::now_v7()),
            BillingScopeId::new(account.billing_scope_id),
            subscriber_id,
            GatewayConfigurationId::new(account.gateway_configuration_id),
            IdempotencyKey::new("paid-trial-primary")?,
            PaymentToken::new("opaque-paid-trial-token")?,
            BillingContact::new(None, None, Some("trial@example.test".to_owned()))?,
        ),
        SubscriptionEnrollmentExpectedTerms::full_price(offer),
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
        &approved_outcome("trial_initial_txn"),
    )
    .await?;
    let subscription = initial
        .subscription()
        .expect("approved trial creates a subscription");
    assert_eq!(initial.status(), syrup_rail::PaymentAttemptStatus::Approved);
    assert_eq!(subscription.status(), SubscriptionStatus::Active);
    assert_eq!(subscription.phase(), SubscriptionPhase::PaidTrial);
    assert_eq!(subscription.recurring_charge().cents(), 2_900);
    assert_eq!(
        *subscription.current_period().end_at() - subscription.current_period().start_at(),
        ChronoDuration::days(7)
    );
    assert_eq!(
        subscription.next_payment_attempt_at(),
        Some(subscription.current_period().end_at())
    );
    let subscription_id = subscription.id();
    let trial_start = *subscription.current_period().start_at();
    let trial_end = *subscription.current_period().end_at();
    let started_events = events.lock().await.clone();
    assert_eq!(started_events.len(), 1);
    assert!(matches!(
        &started_events[0].event,
        BillingEvent::SubscriptionStarted {
            charge,
            period,
            phase: SubscriptionPhase::PaidTrial,
            ..
        } if charge.cents() == 100
            && period == &BillingPeriod::new(trial_start, trial_end).expect("valid trial period")
    ));

    let renewal_command = force_due_renewal(
        &database.pool,
        BillingScopeId::new(account.billing_scope_id),
        subscription_id,
    )
    .await?;
    let due_at = *renewal_command.period_start_at();
    let (forced_start_at, forced_end_at): (DateTime<Utc>, DateTime<Utc>) = sqlx::query_as(
        "SELECT current_period_start_at, current_period_end_at FROM billing_subscriptions WHERE id = $1",
    )
    .bind(subscription_id.as_uuid())
    .fetch_one(&database.pool)
    .await?;
    assert_eq!(forced_end_at - forced_start_at, ChronoDuration::days(7));
    let first_reservation =
        reserve_and_admit_renewal(&database.pool, &gateway, renewal_command).await?;
    assert_eq!(first_reservation.request().amount().cents(), 2_900);
    assert_eq!(first_reservation.period().start_at(), &due_at);
    let first_outcome = declined_outcome("trial_renewal_decline_1");
    let (first, concurrent_replay) = tokio::join!(
        apply_subscription_renewal_gateway_outcome(
            &database.pool,
            &coordinator,
            &first_reservation,
            &first_outcome,
        ),
        apply_subscription_renewal_gateway_outcome(
            &database.pool,
            &coordinator,
            &first_reservation,
            &first_outcome,
        ),
    );
    let first = first?;
    let concurrent_replay = concurrent_replay?;
    assert_eq!(concurrent_replay, first);
    assert_eq!(events.lock().await.len(), 2);
    let duplicate_replay = apply_reconciled_subscription_renewal_gateway_outcome(
        &database.pool,
        &coordinator,
        BillingScopeId::new(account.billing_scope_id),
        first.attempt().identity().attempt_id(),
        &processor_duplicate_outcome(),
    )
    .await?;
    assert_eq!(duplicate_replay, first);
    assert_eq!(
        duplicate_replay.attempt().status(),
        syrup_rail::PaymentAttemptStatus::Declined
    );
    assert_eq!(
        duplicate_replay.observation_diagnostics(),
        &[GatewayPaymentDiagnostic::ProcessorReportedDuplicate],
        "the latest duplicate observation cannot reclassify a durable decline"
    );
    assert_eq!(events.lock().await.len(), 2);
    let failure_one_at =
        resolved_at(&database.pool, first.attempt().identity().attempt_id()).await?;
    let (status, economic_anchor, retry_at, unpaid_at): SubscriptionDunningProjection =
        sqlx::query_as(
        "SELECT status, next_renewal_at, next_payment_attempt_at, unpaid_at FROM billing_subscriptions WHERE id = $1",
    )
    .bind(subscription_id.as_uuid())
    .fetch_one(&database.pool)
    .await?;
    assert_eq!(status, "past_due");
    assert_eq!(economic_anchor, due_at);
    assert_eq!(retry_at, Some(failure_one_at + ChronoDuration::days(1)));
    assert_eq!(unpaid_at, None);
    assert!(matches!(
        entitlement(
            &database.pool,
            &EntitlementQuery::new(
                BillingScopeId::new(account.billing_scope_id),
                subscriber_id,
                PlanKey::new("identity_pro")?,
            ),
        )
        .await?,
        Entitlement::PastDue {
            access: syrup_rail::PastDueAccess::AllowedDuringDunning,
            ..
        }
    ));

    let resolver = Arc::new(CountingResolver {
        gateway: gateway.clone(),
        calls: AtomicUsize::new(0),
    });
    let service = SubscriptionBillingService::new(
        database.pool.clone(),
        Arc::new(offers.clone()),
        resolver.clone(),
        Arc::new(PermitAdmission),
        Arc::new(coordinator.clone()),
    );
    assert!(matches!(
        service.renew(renewal_command).await?,
        SubscriptionRenewalOutcome::Noop
    ));
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 0);

    make_retry_due(&database.pool, renewal_command).await?;
    let second_reservation =
        reserve_and_admit_renewal(&database.pool, &gateway, renewal_command).await?;
    let second_outcome = declined_outcome("trial_renewal_decline_2");
    let second = apply_reconciled_subscription_renewal_gateway_outcome(
        &database.pool,
        &coordinator,
        BillingScopeId::new(account.billing_scope_id),
        second_reservation.identity().attempt_id(),
        &second_outcome,
    )
    .await?;
    assert!(second.observation_diagnostics().is_empty());
    let failure_two_at =
        resolved_at(&database.pool, second.attempt().identity().attempt_id()).await?;
    let retry_at: Option<DateTime<Utc>> = sqlx::query_scalar(
        "SELECT next_payment_attempt_at FROM billing_subscriptions WHERE id = $1",
    )
    .bind(subscription_id.as_uuid())
    .fetch_one(&database.pool)
    .await?;
    assert_eq!(retry_at, Some(failure_two_at + ChronoDuration::days(3)));

    make_retry_due(&database.pool, renewal_command).await?;
    let final_reservation =
        reserve_and_admit_renewal(&database.pool, &gateway, renewal_command).await?;
    let final_outcome = declined_outcome("trial_renewal_decline_3");
    let final_result = apply_subscription_renewal_gateway_outcome(
        &database.pool,
        &coordinator,
        &final_reservation,
        &final_outcome,
    )
    .await?;
    assert!(final_result.observation_diagnostics().is_empty());
    let failure_three_at = resolved_at(
        &database.pool,
        final_result.attempt().identity().attempt_id(),
    )
    .await?;
    let (status, retry_at, unpaid_at): (String, Option<DateTime<Utc>>, Option<DateTime<Utc>>) =
        sqlx::query_as(
            "SELECT status, next_payment_attempt_at, unpaid_at FROM billing_subscriptions WHERE id = $1",
        )
        .bind(subscription_id.as_uuid())
        .fetch_one(&database.pool)
        .await?;
    assert_eq!(status, "unpaid");
    assert_eq!(retry_at, None);
    assert_eq!(unpaid_at, Some(failure_three_at));

    let final_events = events.lock().await.clone();
    assert_eq!(final_events.len(), 5);
    assert!(matches!(
        &final_events[1].event,
        BillingEvent::SubscriptionPaymentFailed {
            outcome: SubscriptionPaymentFailureOutcome::RetryScheduled { retry_at, .. },
            ..
        } if *retry_at == failure_one_at + ChronoDuration::days(1)
    ));
    assert!(matches!(
        &final_events[2].event,
        BillingEvent::SubscriptionPaymentFailed {
            outcome: SubscriptionPaymentFailureOutcome::RetryScheduled { retry_at, .. },
            ..
        } if *retry_at == failure_two_at + ChronoDuration::days(3)
    ));
    assert!(matches!(
        &final_events[3].event,
        BillingEvent::SubscriptionPaymentFailed {
            outcome: SubscriptionPaymentFailureOutcome::SubscriptionEnded { ended_at, .. },
            ..
        } if *ended_at == failure_three_at
    ));
    assert!(matches!(
        &final_events[4].event,
        BillingEvent::SubscriptionEnded {
            reason: SubscriptionEndReason::NonPayment,
            ended_at,
            access_ends_at,
            ..
        } if *ended_at == failure_three_at && *access_ends_at == failure_three_at
    ));

    apply_subscription_renewal_gateway_outcome(
        &database.pool,
        &coordinator,
        &final_reservation,
        &final_outcome,
    )
    .await?;
    assert_eq!(events.lock().await.len(), 5);

    let mut transaction = database.pool.begin().await?;
    let latest_attempt = find_payment_attempt_by_id_in_transaction(
        &mut transaction,
        BillingScopeId::new(account.billing_scope_id),
        final_result.attempt().identity().attempt_id(),
    )
    .await?
    .expect("final attempt remains durable");
    assert_eq!(
        apply_resolved_automatic_renewal_failure(&mut transaction, &latest_attempt).await?,
        RenewalFailureApplication::Noop
    );
    let older_attempt = find_payment_attempt_by_id_in_transaction(
        &mut transaction,
        BillingScopeId::new(account.billing_scope_id),
        second.attempt().identity().attempt_id(),
    )
    .await?
    .expect("older attempt remains durable");
    assert_eq!(
        apply_resolved_automatic_renewal_failure(&mut transaction, &older_attempt).await?,
        RenewalFailureApplication::Noop
    );
    transaction.rollback().await?;

    let mut transaction = database.pool.begin().await?;
    sqlx::query(
        r#"
        UPDATE billing_subscriptions
        SET status = 'past_due', unpaid_at = NULL,
            next_payment_attempt_at = clock_timestamp() + interval '1 day'
        WHERE id = $1
        "#,
    )
    .bind(subscription_id.as_uuid())
    .execute(&mut *transaction)
    .await?;
    let latest_attempt = find_payment_attempt_by_id_in_transaction(
        &mut transaction,
        BillingScopeId::new(account.billing_scope_id),
        final_result.attempt().identity().attempt_id(),
    )
    .await?
    .expect("final attempt remains durable");
    assert!(
        apply_resolved_automatic_renewal_failure(&mut transaction, &latest_attempt)
            .await
            .is_err()
    );
    transaction.rollback().await?;

    assert!(due_renewals(&database.pool).await?.is_empty());
    assert!(matches!(
        service.renew(renewal_command).await?,
        SubscriptionRenewalOutcome::Noop
    ));
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 0);
    assert!(matches!(
        entitlement(
            &database.pool,
            &EntitlementQuery::new(
                BillingScopeId::new(account.billing_scope_id),
                subscriber_id,
                PlanKey::new("identity_pro")?,
            ),
        )
        .await?,
        Entitlement::Missing { .. }
    ));
    let protected = require_entitlement_for_update(
        EntitlementWriteTransaction::begin(&database.pool).await?,
        &EntitlementGuard::new(
            BillingScopeId::new(account.billing_scope_id),
            subscriber_id,
            PlanKey::new("identity_pro")?,
        ),
    )
    .await;
    assert!(matches!(
        protected,
        Err(crate::EntitlementGuardError::Required)
    ));

    let late = apply_subscription_renewal_gateway_outcome(
        &database.pool,
        &coordinator,
        &final_reservation,
        &approved_outcome_with_reference("late_after_unpaid", "vault_late"),
    )
    .await?;
    assert!(late.subscription().is_none());
    let terminal_status: String =
        sqlx::query_scalar("SELECT status FROM billing_subscriptions WHERE id = $1")
            .bind(subscription_id.as_uuid())
            .fetch_one(&database.pool)
            .await?;
    assert_eq!(terminal_status, "unpaid");
    let late_progression: String = sqlx::query_scalar(
        "SELECT progression_state FROM billing_processor_charges WHERE gateway_transaction_id = 'late_after_unpaid'",
    )
    .fetch_one(&database.pool)
    .await?;
    assert_eq!(late_progression, "external_reversal_required");
    assert_eq!(events.lock().await.len(), 5);

    database.cleanup().await
}
