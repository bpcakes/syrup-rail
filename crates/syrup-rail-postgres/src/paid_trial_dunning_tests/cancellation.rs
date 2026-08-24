use super::*;

#[tokio::test]
async fn paid_trial_and_scheduled_dunning_cancellation_preserve_exact_access_and_stop_collection()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("pt_cancel").await?;
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

    let trial_subscriber = SubscriberId::new(Uuid::now_v7());
    let initial = approve_enrollment(
        &database.pool,
        &offers,
        &gateway,
        &coordinator,
        account,
        trial_subscriber,
        "trial-cancel",
        SubscriptionEnrollmentExpectedTerms::full_price(offer.clone()),
        "trial_cancel_initial",
        "vault_trial_cancel",
    )
    .await?;
    let trial = initial
        .subscription()
        .expect("approved trial creates subscription");
    let trial_end = *trial.current_period().end_at();
    let trial_command = CancelSubscription::new(
        BillingScopeId::new(account.billing_scope_id),
        trial_subscriber,
        PlanKey::new("identity_pro")?,
    );
    let mut transaction = database.pool.begin().await?;
    let canceled_trial =
        cancel_subscription_in_transaction(&mut transaction, &trial_command).await?;
    transaction.commit().await?;
    let CancelSubscriptionOutcome::Canceled {
        subscription,
        event,
    } = canceled_trial
    else {
        return Err("active trial was not canceled".into());
    };
    assert_eq!(subscription.status(), SubscriptionStatus::Canceled);
    assert_eq!(subscription.phase(), SubscriptionPhase::PaidTrial);
    assert_eq!(subscription.next_payment_attempt_at(), None);
    assert!(matches!(
        event,
        BillingEvent::SubscriptionCanceled { access_ends_at, .. } if access_ends_at == trial_end
    ));
    assert!(matches!(
        entitlement(
            &database.pool,
            &EntitlementQuery::new(
                BillingScopeId::new(account.billing_scope_id),
                trial_subscriber,
                PlanKey::new("identity_pro")?,
            ),
        )
        .await?,
        Entitlement::PaidThroughCancellation { .. }
    ));
    let stale_trial_job = ChargeRenewal::new(
        BillingScopeId::new(account.billing_scope_id),
        subscription.id(),
        trial_end,
    );
    let mut transaction = database.pool.begin().await?;
    let stale =
        reserve_subscription_renewal_in_transaction(&mut transaction, stale_trial_job, &gateway)
            .await?;
    transaction.rollback().await?;
    assert_eq!(
        stale,
        syrup_rail::SubscriptionRenewalReservationOutcome::Rejected(
            syrup_rail::SubscriptionRenewalReservationRejection::PaymentNotDue,
        )
    );

    let dunning_subscriber = SubscriberId::new(Uuid::now_v7());
    let initial = approve_enrollment(
        &database.pool,
        &offers,
        &gateway,
        &coordinator,
        account,
        dunning_subscriber,
        "dunning-cancel",
        SubscriptionEnrollmentExpectedTerms::full_price(offer),
        "dunning_cancel_initial",
        "vault_dunning_cancel",
    )
    .await?;
    let subscription_id = initial
        .subscription()
        .expect("approved trial creates subscription")
        .id();
    let queued_dunning_job = force_due_renewal(
        &database.pool,
        BillingScopeId::new(account.billing_scope_id),
        subscription_id,
    )
    .await?;
    let renewal = reserve_and_admit_renewal(&database.pool, &gateway, queued_dunning_job).await?;
    apply_subscription_renewal_gateway_outcome(
        &database.pool,
        &coordinator,
        &renewal,
        &declined_outcome("dunning_cancel_decline"),
    )
    .await?;
    let cancel_dunning = CancelSubscription::new(
        BillingScopeId::new(account.billing_scope_id),
        dunning_subscriber,
        PlanKey::new("identity_pro")?,
    );
    let mut transaction = database.pool.begin().await?;
    let canceled = cancel_subscription_in_transaction(&mut transaction, &cancel_dunning).await?;
    transaction.commit().await?;
    let CancelSubscriptionOutcome::Canceled { event, .. } = canceled else {
        return Err("scheduled dunning was not canceled".into());
    };
    let (status, canceled_at, next_payment_attempt_at): (
        String,
        Option<DateTime<Utc>>,
        Option<DateTime<Utc>>,
    ) = sqlx::query_as(
        "SELECT status, canceled_at, next_payment_attempt_at FROM billing_subscriptions WHERE id = $1",
    )
    .bind(subscription_id.as_uuid())
    .fetch_one(&database.pool)
    .await?;
    assert_eq!(status, "canceled");
    assert_eq!(next_payment_attempt_at, None);
    let canceled_at = canceled_at.expect("cancellation timestamp");
    assert!(matches!(
        event,
        BillingEvent::SubscriptionCanceled { access_ends_at, .. } if access_ends_at == canceled_at
    ));

    let mut transaction = database.pool.begin().await?;
    let replay = cancel_subscription_in_transaction(&mut transaction, &cancel_dunning).await?;
    transaction.commit().await?;
    assert!(matches!(
        replay,
        CancelSubscriptionOutcome::AlreadyCanceled(_)
    ));
    let mut transaction = database.pool.begin().await?;
    let stale =
        reserve_subscription_renewal_in_transaction(&mut transaction, queued_dunning_job, &gateway)
            .await?;
    transaction.rollback().await?;
    assert_eq!(
        stale,
        syrup_rail::SubscriptionRenewalReservationOutcome::Rejected(
            syrup_rail::SubscriptionRenewalReservationRejection::PaymentNotDue,
        )
    );

    database.cleanup().await
}
