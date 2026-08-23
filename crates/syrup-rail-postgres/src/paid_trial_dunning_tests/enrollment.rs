use super::*;
use syrup_rail::PositiveDiscountCents;

#[derive(Clone, Debug, Eq, PartialEq)]
struct EnrollmentOfferLockObservation {
    stage: crate::SubscriptionEnrollmentOfferStage,
    attempt_id: PaymentAttemptId,
    idempotency_key: IdempotencyKey,
    other_initial_attempts: i64,
    in_flight_attempts: i64,
}

#[derive(Clone)]
struct AttemptHistoryOfferStore {
    offer: syrup_rail::SubscriptionOffer,
    observations: Arc<Mutex<Vec<EnrollmentOfferLockObservation>>>,
}

#[async_trait]
impl SubscriptionOfferStore for AttemptHistoryOfferStore {
    async fn lock_current_offer(
        &self,
        _connection: &mut PgConnection,
        _billing_scope_id: BillingScopeId,
        plan_key: &PlanKey,
    ) -> Result<Option<syrup_rail::SubscriptionOffer>, sqlx::Error> {
        Ok((self.offer.plan_key() == plan_key).then(|| self.offer.clone()))
    }

    async fn lock_enrollment_offer(
        &self,
        connection: &mut PgConnection,
        context: crate::SubscriptionEnrollmentOfferContext<'_>,
    ) -> Result<Option<syrup_rail::SubscriptionOffer>, sqlx::Error> {
        let (other_initial_attempts, in_flight_attempts): (i64, i64) = sqlx::query_as(
            r#"
            SELECT
                count(*) FILTER (WHERE id <> $4),
                count(*) FILTER (WHERE id = $4)
            FROM billing_payment_attempts
            WHERE billing_scope_id = $1
                AND subscriber_id = $2
                AND plan_key = $3
                AND attempt_kind = 'subscription_initial'
            "#,
        )
        .bind(context.billing_scope_id().as_uuid())
        .bind(context.subscriber_id().as_uuid())
        .bind(context.plan_key().as_str())
        .bind(context.attempt_id().as_uuid())
        .fetch_one(connection)
        .await?;
        self.observations
            .lock()
            .await
            .push(EnrollmentOfferLockObservation {
                stage: context.stage(),
                attempt_id: context.attempt_id(),
                idempotency_key: context.idempotency_key().clone(),
                other_initial_attempts,
                in_flight_attempts,
            });
        Ok(
            (self.offer.plan_key() == context.plan_key() && other_initial_attempts == 0)
                .then(|| self.offer.clone()),
        )
    }
}

#[tokio::test]
async fn enrollment_offer_hook_excludes_one_in_flight_identity_across_both_stages()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("pt_offer_context").await?;
    let account = create_gateway_account(&database.pool, "nmi").await?;
    let gateway = resolved_gateway(account)?;
    let offer = paid_trial_offer()?;
    let observations = Arc::new(Mutex::new(Vec::new()));
    let offers = AttemptHistoryOfferStore {
        offer: offer.clone(),
        observations: Arc::clone(&observations),
    };
    let subscriber_id = SubscriberId::new(Uuid::now_v7());
    let command = syrup_rail::EnrollSubscription::new(
        syrup_rail::SubscriptionPaymentContext::new(
            PaymentAttemptId::new(Uuid::now_v7()),
            BillingScopeId::new(account.billing_scope_id),
            subscriber_id,
            GatewayConfigurationId::new(account.gateway_configuration_id),
            IdempotencyKey::new("trial-history-context")?,
            PaymentToken::new("opaque-history-token")?,
            BillingContact::new(None, None, Some("history@example.test".to_owned()))?,
        ),
        SubscriptionEnrollmentExpectedTerms::full_price(offer),
    );
    let reservation = SubscriptionEnrollmentReservation::from_command(&command, &gateway)?;
    let context_debug = format!(
        "{:?}",
        crate::SubscriptionEnrollmentOfferContext::from_reservation(
            &reservation,
            crate::SubscriptionEnrollmentOfferStage::Reservation,
        )
    );
    assert!(!context_debug.contains(reservation.idempotency_key().expose()));
    assert!(context_debug.contains("has_idempotency_key"));

    let mut transaction = database.pool.begin().await?;
    assert!(matches!(
        reserve_subscription_enrollment_in_transaction(&mut transaction, &offers, &reservation)
            .await?,
        SubscriptionEnrollmentReservationOutcome::Reserved(_)
    ));
    transaction.commit().await?;
    assert!(matches!(
        admit_subscription_enrollment_submission(&database.pool, &offers, &reservation).await?,
        SubscriptionEnrollmentAdmissionOutcome::Admitted(_)
    ));

    let first_observations = observations.lock().await.clone();
    assert_eq!(first_observations.len(), 2);
    assert_eq!(
        first_observations[0],
        EnrollmentOfferLockObservation {
            stage: crate::SubscriptionEnrollmentOfferStage::Reservation,
            attempt_id: reservation.identity().attempt_id(),
            idempotency_key: reservation.idempotency_key().clone(),
            other_initial_attempts: 0,
            in_flight_attempts: 0,
        }
    );
    assert_eq!(
        first_observations[1],
        EnrollmentOfferLockObservation {
            stage: crate::SubscriptionEnrollmentOfferStage::SubmissionAdmission,
            attempt_id: reservation.identity().attempt_id(),
            idempotency_key: reservation.idempotency_key().clone(),
            other_initial_attempts: 0,
            in_flight_attempts: 1,
        }
    );

    let later_command = syrup_rail::EnrollSubscription::new(
        syrup_rail::SubscriptionPaymentContext::new(
            PaymentAttemptId::new(Uuid::now_v7()),
            BillingScopeId::new(account.billing_scope_id),
            subscriber_id,
            GatewayConfigurationId::new(account.gateway_configuration_id),
            IdempotencyKey::new("trial-history-later")?,
            PaymentToken::new("opaque-later-token")?,
            BillingContact::new(None, None, Some("history@example.test".to_owned()))?,
        ),
        SubscriptionEnrollmentExpectedTerms::full_price(paid_trial_offer()?),
    );
    let later_reservation =
        SubscriptionEnrollmentReservation::from_command(&later_command, &gateway)?;
    let mut transaction = database.pool.begin().await?;
    assert_eq!(
        reserve_subscription_enrollment_in_transaction(
            &mut transaction,
            &offers,
            &later_reservation,
        )
        .await?,
        SubscriptionEnrollmentReservationOutcome::Rejected(
            syrup_rail::SubscriptionEnrollmentReservationRejection::EnrollmentTermsChanged,
        )
    );
    transaction.rollback().await?;
    let observations = observations.lock().await.clone();
    assert_eq!(observations.len(), 3);
    assert_eq!(
        observations[2],
        EnrollmentOfferLockObservation {
            stage: crate::SubscriptionEnrollmentOfferStage::Reservation,
            attempt_id: later_reservation.identity().attempt_id(),
            idempotency_key: later_reservation.idempotency_key().clone(),
            other_initial_attempts: 1,
            in_flight_attempts: 0,
        }
    );

    database.cleanup().await
}

#[tokio::test]
async fn fixed_day_recurring_cadence_persists_and_drives_the_next_renewal()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("pt_fixed_recur").await?;
    let account = create_gateway_account(&database.pool, "nmi").await?;
    let gateway = resolved_gateway(account)?;
    let recurring_period = SubscriptionPeriodRule::fixed_days(14)?;
    let offer = syrup_rail::SubscriptionOffer::new(
        PlanKey::new("identity_pro")?,
        RecurringSubscriptionTerms::new(
            ChargeAmount::new(2_900, CurrencyCode::new("USD")?)?,
            recurring_period,
        ),
        syrup_rail::SubscriptionStart::RecurringImmediately,
        RenewalFailurePolicy::new(
            DunningSchedule::default(),
            DunningExhaustion::RemainPastDue,
            PastDueAccessPolicy::SuspendImmediately,
        ),
    )?;
    let offers = StaticOfferStore {
        offer: offer.clone(),
    };
    let events = Arc::new(Mutex::new(Vec::new()));
    let coordinator = TestCoordinator {
        pool: database.pool.clone(),
        events: Arc::clone(&events),
    };
    let enrollment = approve_enrollment(
        &database.pool,
        &offers,
        &gateway,
        &coordinator,
        account,
        SubscriberId::new(Uuid::now_v7()),
        "fixed-day-enrollment",
        SubscriptionEnrollmentExpectedTerms::full_price(offer),
        "fixed_day_initial",
        "vault_fixed_day",
    )
    .await?;
    let subscription = enrollment
        .subscription()
        .expect("fixed-day enrollment creates subscription");
    assert_eq!(subscription.recurring_period(), recurring_period);
    assert_eq!(
        *subscription.current_period().end_at() - subscription.current_period().start_at(),
        ChronoDuration::days(14)
    );
    let subscription_id = subscription.id();

    let due_at: DateTime<Utc> =
        sqlx::query_scalar("SELECT clock_timestamp() - interval '1 second'")
            .fetch_one(&database.pool)
            .await?;
    sqlx::query(
        r#"
        UPDATE billing_subscriptions
        SET current_period_start_at = $2 - interval '14 days',
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

    let renewal = reserve_and_admit_renewal(
        &database.pool,
        &gateway,
        ChargeRenewal::new(
            BillingScopeId::new(account.billing_scope_id),
            subscription_id,
            due_at,
        ),
    )
    .await?;
    assert_eq!(renewal.period().start_at(), &due_at);
    assert_eq!(
        *renewal.period().end_at() - renewal.period().start_at(),
        ChronoDuration::days(14)
    );
    let renewed = apply_subscription_renewal_gateway_outcome(
        &database.pool,
        &coordinator,
        &renewal,
        &approved_outcome("fixed_day_renewal"),
    )
    .await?;
    let subscription = renewed
        .subscription()
        .expect("renewal advances subscription");
    assert_eq!(subscription.recurring_period(), recurring_period);
    assert_eq!(subscription.current_period(), renewal.period());
    assert_eq!(events.lock().await.len(), 2);

    database.cleanup().await
}

#[tokio::test]
async fn enrollment_compatibility_decline_reconciliation_replay_and_term_mismatch_are_durable()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("pt_enroll_paths").await?;
    let account = create_gateway_account(&database.pool, "nmi").await?;
    let gateway = resolved_gateway(account)?;
    let events = Arc::new(Mutex::new(Vec::new()));
    let coordinator = TestCoordinator {
        pool: database.pool.clone(),
        events: Arc::clone(&events),
    };
    let recurring = RecurringSubscriptionTerms::new(
        ChargeAmount::new(2_900, CurrencyCode::new("USD")?)?,
        SubscriptionPeriodRule::calendar_months(1)?,
    );
    let immediate_offer = syrup_rail::SubscriptionOffer::new(
        PlanKey::new("identity_pro")?,
        recurring,
        syrup_rail::SubscriptionStart::RecurringImmediately,
        RenewalFailurePolicy::new(
            DunningSchedule::from_seconds([86_400, 259_200])?,
            DunningExhaustion::MarkUnpaid,
            PastDueAccessPolicy::ContinueUntilDunningExhausted,
        ),
    )?;
    let immediate_store = StaticOfferStore {
        offer: immediate_offer.clone(),
    };
    let immediate = approve_enrollment(
        &database.pool,
        &immediate_store,
        &gateway,
        &coordinator,
        account,
        SubscriberId::new(Uuid::now_v7()),
        "immediate-compat",
        SubscriptionEnrollmentExpectedTerms::full_price(immediate_offer),
        "immediate_compat_txn",
        "vault_immediate_compat",
    )
    .await?;
    let immediate_subscription = immediate
        .subscription()
        .expect("immediate recurring approval creates subscription");
    assert_eq!(immediate.attempt().request().amount().cents(), 2_900);
    assert_eq!(immediate_subscription.phase(), SubscriptionPhase::Recurring);
    assert_eq!(
        immediate_subscription.current_period(),
        &syrup_rail::next_billing_period(
            *immediate_subscription.current_period().start_at(),
            SubscriptionPeriodRule::calendar_months(1)?,
        )?
    );
    assert!(matches!(
        &events.lock().await[0].event,
        BillingEvent::SubscriptionStarted {
            charge,
            phase: SubscriptionPhase::Recurring,
            ..
        } if charge.cents() == 2_900
    ));

    let trial_offer = paid_trial_offer()?;
    let trial_store = StaticOfferStore {
        offer: trial_offer.clone(),
    };
    let declined_subscriber = SubscriberId::new(Uuid::now_v7());
    let declined_command = syrup_rail::EnrollSubscription::new(
        syrup_rail::SubscriptionPaymentContext::new(
            PaymentAttemptId::new(Uuid::now_v7()),
            BillingScopeId::new(account.billing_scope_id),
            declined_subscriber,
            GatewayConfigurationId::new(account.gateway_configuration_id),
            IdempotencyKey::new("trial-declined")?,
            PaymentToken::new("opaque-declined-token")?,
            BillingContact::new(None, None, Some("declined@example.test".to_owned()))?,
        ),
        SubscriptionEnrollmentExpectedTerms::full_price(trial_offer.clone()),
    );
    let declined_reservation =
        SubscriptionEnrollmentReservation::from_command(&declined_command, &gateway)?;
    let mut transaction = database.pool.begin().await?;
    assert!(matches!(
        reserve_subscription_enrollment_in_transaction(
            &mut transaction,
            &trial_store,
            &declined_reservation,
        )
        .await?,
        SubscriptionEnrollmentReservationOutcome::Reserved(_)
    ));
    transaction.commit().await?;
    assert!(matches!(
        admit_subscription_enrollment_submission(
            &database.pool,
            &trial_store,
            &declined_reservation,
        )
        .await?,
        SubscriptionEnrollmentAdmissionOutcome::Admitted(_)
    ));
    let declined = apply_subscription_enrollment_gateway_outcome(
        &database.pool,
        &coordinator,
        &declined_reservation,
        &declined_outcome("trial_initial_declined"),
    )
    .await?;
    assert_eq!(
        declined.attempt().status(),
        syrup_rail::PaymentAttemptStatus::Declined
    );
    assert!(declined.subscription().is_none());
    let declined_subscriptions: i64 =
        sqlx::query_scalar("SELECT count(*) FROM billing_subscriptions WHERE subscriber_id = $1")
            .bind(declined_subscriber.as_uuid())
            .fetch_one(&database.pool)
            .await?;
    assert_eq!(declined_subscriptions, 0);
    assert_eq!(events.lock().await.len(), 1);

    let reconciled_subscriber = SubscriberId::new(Uuid::now_v7());
    let reconciled_attempt_id = PaymentAttemptId::new(Uuid::now_v7());
    let reconciled_command = syrup_rail::EnrollSubscription::new(
        syrup_rail::SubscriptionPaymentContext::new(
            reconciled_attempt_id,
            BillingScopeId::new(account.billing_scope_id),
            reconciled_subscriber,
            GatewayConfigurationId::new(account.gateway_configuration_id),
            IdempotencyKey::new("trial-reconciled")?,
            PaymentToken::new("opaque-reconciled-token")?,
            BillingContact::new(None, None, Some("reconciled@example.test".to_owned()))?,
        ),
        SubscriptionEnrollmentExpectedTerms::full_price(trial_offer.clone()),
    );
    let reconciled_reservation =
        SubscriptionEnrollmentReservation::from_command(&reconciled_command, &gateway)?;
    let mut transaction = database.pool.begin().await?;
    assert!(matches!(
        reserve_subscription_enrollment_in_transaction(
            &mut transaction,
            &trial_store,
            &reconciled_reservation,
        )
        .await?,
        SubscriptionEnrollmentReservationOutcome::Reserved(_)
    ));
    let durable = find_payment_attempt_by_id_in_transaction(
        &mut transaction,
        BillingScopeId::new(account.billing_scope_id),
        reconciled_attempt_id,
    )
    .await?
    .expect("paid-trial attempt is durable");
    let reconstructed =
        SubscriptionEnrollmentReservation::from_attempt(&durable, GatewayProviderKey::new("nmi")?)?;
    assert_eq!(
        reconstructed.expected_terms(),
        reconciled_reservation.expected_terms()
    );
    assert_eq!(
        reconstructed.gateway_order_id(),
        reconciled_reservation.gateway_order_id()
    );
    transaction.commit().await?;
    assert!(matches!(
        admit_subscription_enrollment_submission(
            &database.pool,
            &trial_store,
            &reconciled_reservation,
        )
        .await?,
        SubscriptionEnrollmentAdmissionOutcome::Admitted(_)
    ));
    let reconciled_outcome =
        approved_outcome_with_reference("trial_reconciled_approved", "vault_trial_reconciled");
    let reconciled = apply_reconciled_subscription_enrollment_gateway_outcome(
        &database.pool,
        &coordinator,
        BillingScopeId::new(account.billing_scope_id),
        reconciled_attempt_id,
        &reconciled_outcome,
    )
    .await?;
    assert_eq!(
        reconciled
            .subscription()
            .expect("reconciled subscription")
            .phase(),
        SubscriptionPhase::PaidTrial
    );
    let replay = apply_reconciled_subscription_enrollment_gateway_outcome(
        &database.pool,
        &coordinator,
        BillingScopeId::new(account.billing_scope_id),
        reconciled_attempt_id,
        &reconciled_outcome,
    )
    .await?;
    assert_eq!(replay, reconciled);
    assert_eq!(events.lock().await.len(), 2);

    let changed_offer = syrup_rail::SubscriptionOffer::new(
        PlanKey::new("identity_pro")?,
        recurring,
        syrup_rail::SubscriptionStart::PaidTrial(PaidTrialTerms::new(
            ChargeAmount::new(100, CurrencyCode::new("USD")?)?,
            SubscriptionPeriodRule::fixed_days(8)?,
        )),
        trial_offer.renewal_failure().clone(),
    )?;
    let mismatch_subscriber = SubscriberId::new(Uuid::now_v7());
    let mismatch_command = syrup_rail::EnrollSubscription::new(
        syrup_rail::SubscriptionPaymentContext::new(
            PaymentAttemptId::new(Uuid::now_v7()),
            BillingScopeId::new(account.billing_scope_id),
            mismatch_subscriber,
            GatewayConfigurationId::new(account.gateway_configuration_id),
            IdempotencyKey::new("trial-term-mismatch")?,
            PaymentToken::new("opaque-mismatch-token")?,
            BillingContact::new(None, None, Some("mismatch@example.test".to_owned()))?,
        ),
        SubscriptionEnrollmentExpectedTerms::full_price(trial_offer),
    );
    let mismatch_reservation =
        SubscriptionEnrollmentReservation::from_command(&mismatch_command, &gateway)?;
    let mut transaction = database.pool.begin().await?;
    let mismatch = reserve_subscription_enrollment_in_transaction(
        &mut transaction,
        &StaticOfferStore {
            offer: changed_offer,
        },
        &mismatch_reservation,
    )
    .await?;
    transaction.commit().await?;
    assert_eq!(
        mismatch,
        SubscriptionEnrollmentReservationOutcome::Rejected(
            syrup_rail::SubscriptionEnrollmentReservationRejection::EnrollmentTermsChanged,
        )
    );
    let mismatch_attempts: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM billing_payment_attempts WHERE subscriber_id = $1",
    )
    .bind(mismatch_subscriber.as_uuid())
    .fetch_one(&database.pool)
    .await?;
    assert_eq!(mismatch_attempts, 0);

    database.cleanup().await
}

#[tokio::test]
async fn indefinite_discounted_paid_trial_starts_with_zero_recurring_periods_applied()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("pt_indef_disc").await?;
    let account = create_gateway_account(&database.pool, "nmi").await?;
    let gateway = resolved_gateway(account)?;
    let offer = paid_trial_offer()?;
    let subscriber_id = SubscriberId::new(Uuid::now_v7());
    let code_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO billing_subscription_discount_codes (
            id, billing_scope_id, plan_key, code_normalized, display_code,
            status, discount_kind, amount_off_cents, currency, duration
        ) VALUES (
            $1, $2, 'identity_pro', 'FOREVER5', 'FOREVER5', 'active',
            'amount_off', 500, 'USD', 'indefinite'
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
            amount_off_cents, currency, duration,
            base_amount_cents, discounted_amount_cents, status
        ) VALUES (
            $1, $2, $3, 'identity_pro', $4, 'FOREVER5', 'amount_off',
            500, 'USD', 'indefinite', 2900, 2400, 'saved'
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
        SubscriptionDiscountCode::new("FOREVER5")?,
        None,
        SubscriptionDiscountKind::AmountOffCents(PositiveDiscountCents::new(500)?),
        SubscriptionDiscountDuration::Indefinite,
        ChargeAmount::new(2_900, CurrencyCode::new("USD")?)?,
        ChargeAmount::new(2_400, CurrencyCode::new("USD")?)?,
    )?;
    let events = Arc::new(Mutex::new(Vec::new()));
    let coordinator = TestCoordinator {
        pool: database.pool.clone(),
        events: Arc::clone(&events),
    };
    let result = approve_enrollment(
        &database.pool,
        &StaticOfferStore {
            offer: offer.clone(),
        },
        &gateway,
        &coordinator,
        account,
        subscriber_id,
        "indefinite-trial",
        SubscriptionEnrollmentExpectedTerms::discounted(offer, discount)?,
        "indefinite_trial_initial",
        "vault_indefinite_trial",
    )
    .await?;
    let subscription = result.subscription().expect("paid trial subscription");
    assert_eq!(subscription.phase(), SubscriptionPhase::PaidTrial);
    assert_eq!(subscription.recurring_charge().cents(), 2_400);
    let discount_state: (i32, Option<i32>, String) = sqlx::query_as(
        "SELECT periods_applied, periods_total, status FROM billing_subscription_discounts WHERE subscription_id = $1",
    )
    .bind(subscription.id().as_uuid())
    .fetch_one(&database.pool)
    .await?;
    assert_eq!(discount_state, (0, None, "active".to_owned()));
    assert!(matches!(
        &events.lock().await[0].event,
        BillingEvent::SubscriptionStarted { charge, .. } if charge.cents() == 100
    ));

    database.cleanup().await
}
