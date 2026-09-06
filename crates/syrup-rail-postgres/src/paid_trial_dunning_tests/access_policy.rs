use super::*;

async fn policy_subscription(
    database: &TestDatabase,
    account: GatewayAccountFixture,
    gateway: &ResolvedGateway,
    coordinator: &TestCoordinator,
    policy: RenewalFailurePolicy,
    key: &str,
) -> Result<(SubscriberId, ChargeRenewal), Box<dyn Error>> {
    let offer = paid_trial_offer_with_policy(policy)?;
    let subscriber_id = SubscriberId::new(Uuid::now_v7());
    let initial = approve_enrollment(
        &database.pool,
        &StaticOfferStore {
            offer: offer.clone(),
        },
        gateway,
        coordinator,
        account,
        subscriber_id,
        key,
        SubscriptionEnrollmentExpectedTerms::full_price(offer),
        &format!("{key}_initial"),
        &format!("vault_{key}"),
    )
    .await?;
    let subscription_id = initial
        .subscription()
        .expect("approved trial creates subscription")
        .id();
    let renewal = force_due_renewal(
        &database.pool,
        BillingScopeId::new(account.billing_scope_id),
        subscription_id,
    )
    .await?;
    Ok((subscriber_id, renewal))
}

fn query(account: GatewayAccountFixture, subscriber_id: SubscriberId) -> EntitlementQuery {
    EntitlementQuery::new(
        BillingScopeId::new(account.billing_scope_id),
        subscriber_id,
        PlanKey::new("identity_pro").expect("valid plan key"),
    )
}

#[tokio::test]
async fn access_policies_drive_terminal_and_cancellation_timestamps_without_reinterpreting_terms()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("pt_access").await?;
    let account = create_gateway_account(&database.pool, "nmi").await?;
    let gateway = resolved_gateway(account)?;
    let events = Arc::new(Mutex::new(Vec::new()));
    let coordinator = TestCoordinator {
        pool: database.pool.clone(),
        events: Arc::clone(&events),
    };
    let scope = BillingScopeId::new(account.billing_scope_id);

    let suspend_policy = RenewalFailurePolicy::new(
        DunningSchedule::from_seconds([60])?,
        DunningExhaustion::MarkUnpaid,
        PastDueAccessPolicy::SuspendImmediately,
    );
    let (suspended_subscriber, suspended_renewal) = policy_subscription(
        &database,
        account,
        &gateway,
        &coordinator,
        suspend_policy.clone(),
        "suspend_terminal",
    )
    .await?;
    let suspended_id = suspended_renewal.subscription_id();
    let (_, first_suspended_at) = decline_due_renewal(
        &database.pool,
        &gateway,
        &coordinator,
        suspended_renewal,
        "suspend_terminal_failure_1",
    )
    .await?;
    assert!(events.lock().await.iter().any(|event| matches!(
        &event.event,
        BillingEvent::SubscriptionPaymentFailed {
            subscription_id,
            outcome: SubscriptionPaymentFailureOutcome::RetryScheduled {
                access: SubscriptionPaymentFailureAccess::Ended { access_ended_at },
                ..
            },
            ..
        } if *subscription_id == suspended_id && *access_ended_at == first_suspended_at
    )));
    assert!(matches!(
        entitlement(&database.pool, &query(account, suspended_subscriber)).await?,
        Entitlement::PastDue {
            access: syrup_rail::PastDueAccess::Suspended,
            ..
        }
    ));
    assert!(matches!(
        require_entitlement_for_update(
            EntitlementWriteTransaction::begin(&database.pool).await?,
            &EntitlementGuard::new(scope, suspended_subscriber, PlanKey::new("identity_pro")?,),
        )
        .await,
        Err(crate::EntitlementGuardError::PastDue)
    ));
    make_retry_due(&database.pool, suspended_renewal).await?;
    let (_, final_suspended_at) = decline_due_renewal(
        &database.pool,
        &gateway,
        &coordinator,
        suspended_renewal,
        "suspend_terminal_failure_2",
    )
    .await?;
    let status: String =
        sqlx::query_scalar("SELECT status FROM billing_subscriptions WHERE id = $1")
            .bind(suspended_id.as_uuid())
            .fetch_one(&database.pool)
            .await?;
    assert_eq!(status, "unpaid");
    let emitted = events.lock().await.clone();
    assert!(emitted.iter().any(|event| matches!(
        &event.event,
        BillingEvent::SubscriptionPaymentFailed {
            subscription_id,
            outcome: SubscriptionPaymentFailureOutcome::SubscriptionEnded {
                ended_at,
                access_ended_at,
            },
            ..
        } if *subscription_id == suspended_id
            && *ended_at == final_suspended_at
            && *access_ended_at == first_suspended_at
    )));
    assert!(emitted.iter().any(|event| matches!(
        &event.event,
        BillingEvent::SubscriptionEnded {
            subscription_id,
            reason: SubscriptionEndReason::NonPayment,
            ended_at,
            access_ends_at,
            ..
        } if *subscription_id == suspended_id
            && *ended_at == final_suspended_at
            && *access_ends_at == first_suspended_at
    )));

    let remain_policy = RenewalFailurePolicy::new(
        DunningSchedule::from_seconds([60])?,
        DunningExhaustion::RemainPastDue,
        PastDueAccessPolicy::ContinueUntilDunningExhausted,
    );
    let (remain_subscriber, remain_renewal) = policy_subscription(
        &database,
        account,
        &gateway,
        &coordinator,
        remain_policy,
        "remain_exhausted",
    )
    .await?;
    let remain_id = remain_renewal.subscription_id();
    decline_due_renewal(
        &database.pool,
        &gateway,
        &coordinator,
        remain_renewal,
        "remain_failure_1",
    )
    .await?;
    assert!(events.lock().await.iter().any(|event| matches!(
        &event.event,
        BillingEvent::SubscriptionPaymentFailed {
            subscription_id,
            outcome: SubscriptionPaymentFailureOutcome::RetryScheduled {
                access: SubscriptionPaymentFailureAccess::ContinuesDuringDunning,
                ..
            },
            ..
        } if *subscription_id == remain_id
    )));
    assert!(matches!(
        entitlement(&database.pool, &query(account, remain_subscriber)).await?,
        Entitlement::PastDue {
            access: syrup_rail::PastDueAccess::AllowedDuringDunning,
            ..
        }
    ));
    make_retry_due(&database.pool, remain_renewal).await?;
    let (_, remain_exhausted_at) = decline_due_renewal(
        &database.pool,
        &gateway,
        &coordinator,
        remain_renewal,
        "remain_failure_2",
    )
    .await?;
    let (status, next_payment): (String, Option<DateTime<Utc>>) = sqlx::query_as(
        "SELECT status, next_payment_attempt_at FROM billing_subscriptions WHERE id = $1",
    )
    .bind(remain_id.as_uuid())
    .fetch_one(&database.pool)
    .await?;
    assert_eq!((status.as_str(), next_payment), ("past_due", None));
    assert!(matches!(
        entitlement(&database.pool, &query(account, remain_subscriber)).await?,
        Entitlement::PastDue {
            access: syrup_rail::PastDueAccess::Suspended,
            ..
        }
    ));
    assert!(events.lock().await.iter().any(|event| matches!(
        &event.event,
        BillingEvent::SubscriptionPaymentFailed {
            subscription_id,
            outcome: SubscriptionPaymentFailureOutcome::DunningExhausted {
                exhausted_at,
                access_ended_at,
            },
            ..
        } if *subscription_id == remain_id
            && *exhausted_at == remain_exhausted_at
            && *access_ended_at == remain_exhausted_at
    )));
    let remain_cancel =
        CancelSubscription::new(scope, remain_subscriber, PlanKey::new("identity_pro")?);
    let mut transaction = database.pool.begin().await?;
    let outcome = cancel_subscription_in_transaction(&mut transaction, &remain_cancel).await?;
    transaction.commit().await?;
    assert!(matches!(
        outcome,
        CancelSubscriptionOutcome::Canceled {
            event: BillingEvent::SubscriptionCanceled { access_ends_at, .. },
            ..
        } if access_ends_at == remain_exhausted_at
    ));

    let (cancel_subscriber, cancel_renewal) = policy_subscription(
        &database,
        account,
        &gateway,
        &coordinator,
        suspend_policy,
        "suspend_cancel",
    )
    .await?;
    let (_, suspended_at) = decline_due_renewal(
        &database.pool,
        &gateway,
        &coordinator,
        cancel_renewal,
        "suspend_cancel_failure",
    )
    .await?;
    let cancel = CancelSubscription::new(scope, cancel_subscriber, PlanKey::new("identity_pro")?);
    let mut transaction = database.pool.begin().await?;
    let outcome = cancel_subscription_in_transaction(&mut transaction, &cancel).await?;
    transaction.commit().await?;
    assert!(matches!(
        outcome,
        CancelSubscriptionOutcome::Canceled {
            event: BillingEvent::SubscriptionCanceled { access_ends_at, .. },
            ..
        } if access_ends_at == suspended_at
    ));

    database.cleanup().await
}
