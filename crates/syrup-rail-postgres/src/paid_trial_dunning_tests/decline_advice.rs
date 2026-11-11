use std::sync::Mutex as StdMutex;

use syrup_rail::{DunningRetryDelay, RenewalDeclineRetryAdvice};

use super::*;

const STOP_CODE: &str = "261";
const WAIT_CODE: &str = "264";
const WAIT_SECONDS: u32 = 3 * 24 * 60 * 60;

/// A host coordinator that publishes provider retry advice, recording every
/// provider key it is asked about.
struct AdvisingCoordinator {
    inner: TestCoordinator,
    consulted_providers: StdMutex<Vec<String>>,
}

impl AdvisingCoordinator {
    fn new(inner: TestCoordinator) -> Self {
        Self {
            inner,
            consulted_providers: StdMutex::new(Vec::new()),
        }
    }

    fn consulted_providers(&self) -> Vec<String> {
        self.consulted_providers
            .lock()
            .expect("advice log is not poisoned")
            .clone()
    }
}

#[async_trait]
impl BillingTransactionCoordinator for AdvisingCoordinator {
    async fn begin(
        &self,
        subject: BillingEventSubject,
        lock_timeout: Duration,
    ) -> Result<Box<dyn BillingTransaction>, BillingTransactionError> {
        self.inner.begin(subject, lock_timeout).await
    }

    fn renewal_decline_retry_advice(
        &self,
        provider_key: &GatewayProviderKey,
        evidence: &ProcessorEvidence,
    ) -> RenewalDeclineRetryAdvice {
        self.consulted_providers
            .lock()
            .expect("advice log is not poisoned")
            .push(provider_key.as_str().to_owned());
        match evidence.response_code().map(GatewayDiagnostic::expose) {
            Some(STOP_CODE) => RenewalDeclineRetryAdvice::StopAutomaticRetries,
            Some(WAIT_CODE) => RenewalDeclineRetryAdvice::RetryNoEarlierThan(
                DunningRetryDelay::new(WAIT_SECONDS).expect("positive delay"),
            ),
            _ => RenewalDeclineRetryAdvice::FollowSchedule,
        }
    }
}

fn declined_with_code(transaction_id: &str, code: &str) -> GatewayPaymentOutcome {
    GatewayPaymentOutcome::new(
        GatewayPaymentStatus::Declined,
        ProcessorEvidence::new(
            Some(GatewayTransactionId::new(transaction_id).expect("valid transaction ID")),
            None,
            Some(GatewayDiagnostic::new("2")),
            Some(GatewayDiagnostic::new(code)),
            Some(GatewayDiagnostic::new("DECLINE")),
            Some(GatewayDiagnostic::new("declined")),
            GatewayPaymentDescriptor::default(),
        ),
    )
}

async fn due_subscription(
    database: &TestDatabase,
    account: GatewayAccountFixture,
    gateway: &ResolvedGateway,
    coordinator: &dyn BillingTransactionCoordinator,
    policy: RenewalFailurePolicy,
    key: &str,
) -> Result<(syrup_rail::SubscriptionId, DateTime<Utc>), Box<dyn Error>> {
    let offer = paid_trial_offer_with_policy(policy)?;
    let initial = approve_enrollment(
        &database.pool,
        &StaticOfferStore {
            offer: offer.clone(),
        },
        gateway,
        coordinator,
        account,
        SubscriberId::new(Uuid::now_v7()),
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
    let due_at = make_trial_due(&database.pool, subscription_id).await?;
    Ok((subscription_id, due_at))
}

/// The database, gateway account, and resolved gateway shared by a scenario.
struct Renewals<'a> {
    database: &'a TestDatabase,
    account: GatewayAccountFixture,
    gateway: &'a ResolvedGateway,
}

async fn decline_with_code(
    renewals: &Renewals<'_>,
    coordinator: &dyn BillingTransactionCoordinator,
    subscription_id: syrup_rail::SubscriptionId,
    due_at: DateTime<Utc>,
    transaction_id: &str,
    code: &str,
) -> Result<(PaymentAttemptId, DateTime<Utc>), Box<dyn Error>> {
    let Renewals {
        database,
        account,
        gateway,
    } = renewals;
    let reservation = reserve_and_admit_renewal(
        &database.pool,
        gateway,
        ChargeRenewal::new(
            BillingScopeId::new(account.billing_scope_id),
            subscription_id,
            due_at,
        ),
    )
    .await?;
    let result = apply_subscription_renewal_gateway_outcome(
        &database.pool,
        coordinator,
        &reservation,
        &declined_with_code(transaction_id, code),
    )
    .await?;
    let attempt_id = result.attempt().identity().attempt_id();
    Ok((attempt_id, resolved_at(&database.pool, attempt_id).await?))
}

async fn dunning_state(
    pool: &PgPool,
    subscription_id: syrup_rail::SubscriptionId,
) -> Result<(String, Option<DateTime<Utc>>, Option<DateTime<Utc>>), sqlx::Error> {
    sqlx::query_as(
        "SELECT status, next_payment_attempt_at, unpaid_at FROM billing_subscriptions WHERE id = $1",
    )
    .bind(subscription_id.as_uuid())
    .fetch_one(pool)
    .await
}

fn schedule(exhaustion: DunningExhaustion) -> Result<RenewalFailurePolicy, Box<dyn Error>> {
    Ok(RenewalFailurePolicy::new(
        DunningSchedule::from_seconds([60, 300])?,
        exhaustion,
        PastDueAccessPolicy::SuspendImmediately,
    ))
}

#[tokio::test]
async fn stop_advice_ends_dunning_on_the_first_failure_and_replays_as_a_noop()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("pt_decline_stop").await?;
    let account = create_gateway_account(&database.pool, "nmi").await?;
    let gateway = resolved_gateway(account)?;
    let renewals = Renewals {
        database: &database,
        account,
        gateway: &gateway,
    };
    let events = Arc::new(Mutex::new(Vec::new()));
    let coordinator = AdvisingCoordinator::new(TestCoordinator {
        pool: database.pool.clone(),
        events: Arc::clone(&events),
    });

    let (unpaid_id, unpaid_due) = due_subscription(
        &database,
        account,
        &gateway,
        &coordinator,
        schedule(DunningExhaustion::MarkUnpaid)?,
        "stop_unpaid",
    )
    .await?;
    let (unpaid_attempt, unpaid_failed_at) = decline_with_code(
        &renewals,
        &coordinator,
        unpaid_id,
        unpaid_due,
        "stop_unpaid_failure",
        STOP_CODE,
    )
    .await?;
    assert_eq!(
        dunning_state(&database.pool, unpaid_id).await?,
        ("unpaid".to_owned(), None, Some(unpaid_failed_at))
    );
    {
        let recorded = events.lock().await;
        assert!(recorded.iter().any(|event| matches!(
            event,
            BillingEvent::SubscriptionPaymentFailed {
                subscription_id,
                disposition: SubscriptionPaymentFailureDisposition::SubscriptionEnded { ended_at },
                access: SubscriptionPaymentFailureAccess::Ended { access_ended_at },
                ..
            } if *subscription_id == unpaid_id
                && *ended_at == unpaid_failed_at
                && *access_ended_at == unpaid_failed_at
        )));
        assert!(recorded.iter().any(|event| matches!(
            event,
            BillingEvent::SubscriptionEnded {
                subscription_id,
                reason: SubscriptionEndReason::NonPayment,
                ended_at,
                ..
            } if *subscription_id == unpaid_id && *ended_at == unpaid_failed_at
        )));
    }

    // Reconciling an already resolved failure is a no-op: the attempt is no
    // longer resolvable, so no decision is made, no advice is consulted, and
    // the stopped state is unchanged.
    let recorded_before_replay = events.lock().await.len();
    let consulted_before_replay = coordinator.consulted_providers().len();
    apply_reconciled_subscription_renewal_gateway_outcome(
        &database.pool,
        &coordinator,
        BillingScopeId::new(account.billing_scope_id),
        unpaid_attempt,
        &declined_with_code("stop_unpaid_failure", STOP_CODE),
    )
    .await?;
    assert_eq!(events.lock().await.len(), recorded_before_replay);
    assert_eq!(
        coordinator.consulted_providers().len(),
        consulted_before_replay
    );
    assert_eq!(
        dunning_state(&database.pool, unpaid_id).await?,
        ("unpaid".to_owned(), None, Some(unpaid_failed_at))
    );

    let (past_due_id, past_due_due) = due_subscription(
        &database,
        account,
        &gateway,
        &coordinator,
        schedule(DunningExhaustion::RemainPastDue)?,
        "stop_past_due",
    )
    .await?;
    let (_, past_due_failed_at) = decline_with_code(
        &renewals,
        &coordinator,
        past_due_id,
        past_due_due,
        "stop_past_due_failure",
        STOP_CODE,
    )
    .await?;
    assert_eq!(
        dunning_state(&database.pool, past_due_id).await?,
        ("past_due".to_owned(), None, None)
    );
    assert!(events.lock().await.iter().any(|event| matches!(
        event,
        BillingEvent::SubscriptionPaymentFailed {
            subscription_id,
            disposition: SubscriptionPaymentFailureDisposition::DunningExhausted { exhausted_at },
            ..
        } if *subscription_id == past_due_id && *exhausted_at == past_due_failed_at
    )));
    assert!(
        due_renewals(&database.pool)
            .await?
            .iter()
            .all(|dispatch| dispatch.subscription_id() != past_due_id),
        "an exhausted subscription is not dispatched again"
    );

    let consulted = coordinator.consulted_providers();
    assert!(!consulted.is_empty());
    assert!(consulted.iter().all(|provider| provider == "nmi"));

    database.cleanup().await
}

#[tokio::test]
async fn retry_floor_postpones_only_an_earlier_scheduled_retry() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("pt_decline_floor").await?;
    let account = create_gateway_account(&database.pool, "nmi").await?;
    let gateway = resolved_gateway(account)?;
    let renewals = Renewals {
        database: &database,
        account,
        gateway: &gateway,
    };
    let coordinator = AdvisingCoordinator::new(TestCoordinator {
        pool: database.pool.clone(),
        events: Arc::new(Mutex::new(Vec::new())),
    });
    let (subscription_id, due_at) = due_subscription(
        &database,
        account,
        &gateway,
        &coordinator,
        schedule(DunningExhaustion::MarkUnpaid)?,
        "floor",
    )
    .await?;

    let (_, first_failed_at) = decline_with_code(
        &renewals,
        &coordinator,
        subscription_id,
        due_at,
        "floor_failure_1",
        WAIT_CODE,
    )
    .await?;
    assert_eq!(
        dunning_state(&database.pool, subscription_id).await?,
        (
            "past_due".to_owned(),
            Some(first_failed_at + ChronoDuration::seconds(i64::from(WAIT_SECONDS))),
            None
        )
    );

    // The postponed retry stays the schedule's second attempt: a later
    // generic decline uses the schedule's next step, and advice never adds an
    // attempt past it.
    make_retry_due(&database.pool, subscription_id).await?;
    let (_, second_failed_at) = decline_with_code(
        &renewals,
        &coordinator,
        subscription_id,
        due_at,
        "floor_failure_2",
        "201",
    )
    .await?;
    assert_eq!(
        dunning_state(&database.pool, subscription_id).await?,
        (
            "past_due".to_owned(),
            Some(second_failed_at + ChronoDuration::seconds(300)),
            None
        )
    );

    make_retry_due(&database.pool, subscription_id).await?;
    let (_, final_failed_at) = decline_with_code(
        &renewals,
        &coordinator,
        subscription_id,
        due_at,
        "floor_failure_3",
        WAIT_CODE,
    )
    .await?;
    assert_eq!(
        dunning_state(&database.pool, subscription_id).await?,
        ("unpaid".to_owned(), None, Some(final_failed_at))
    );

    database.cleanup().await
}

#[tokio::test]
async fn reconciliation_that_first_resolves_a_decline_applies_advice() -> Result<(), Box<dyn Error>>
{
    let database = TestDatabase::start("pt_decl_reconcil").await?;
    let account = create_gateway_account(&database.pool, "nmi").await?;
    let gateway = resolved_gateway(account)?;
    let events = Arc::new(Mutex::new(Vec::new()));
    let coordinator = AdvisingCoordinator::new(TestCoordinator {
        pool: database.pool.clone(),
        events: Arc::clone(&events),
    });

    for (key, code) in [("reconcile_stop", STOP_CODE), ("reconcile_wait", WAIT_CODE)] {
        let (subscription_id, due_at) = due_subscription(
            &database,
            account,
            &gateway,
            &coordinator,
            schedule(DunningExhaustion::MarkUnpaid)?,
            key,
        )
        .await?;
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
        let attempt_id = reservation.identity().attempt_id();

        // The foreground outcome is indeterminate: nothing is resolved, so no
        // advice is consulted and dunning has not started.
        let consulted_before = coordinator.consulted_providers().len();
        let foreground = apply_subscription_renewal_gateway_outcome(
            &database.pool,
            &coordinator,
            &reservation,
            &unknown_outcome(),
        )
        .await?;
        assert_eq!(
            foreground.attempt().status(),
            syrup_rail::PaymentAttemptStatus::Unknown,
            "{key}"
        );
        assert_eq!(
            coordinator.consulted_providers().len(),
            consulted_before,
            "{key}"
        );
        assert_eq!(
            dunning_state(&database.pool, subscription_id).await?,
            ("active".to_owned(), Some(due_at), None),
            "{key}"
        );

        // Reconciliation is the first to resolve the decline and must apply
        // the coordinator's advice for the attempt's provider.
        let reconciled = apply_reconciled_subscription_renewal_gateway_outcome(
            &database.pool,
            &coordinator,
            BillingScopeId::new(account.billing_scope_id),
            attempt_id,
            &declined_with_code(&format!("{key}_failure"), code),
        )
        .await?;
        assert_eq!(
            reconciled.attempt().status(),
            syrup_rail::PaymentAttemptStatus::Declined,
            "{key}"
        );
        assert_eq!(
            coordinator.consulted_providers()[consulted_before..],
            ["nmi".to_owned()],
            "{key}"
        );
        let failed_at = resolved_at(&database.pool, attempt_id).await?;
        let expected = if code == STOP_CODE {
            ("unpaid".to_owned(), None, Some(failed_at))
        } else {
            // The schedule's 60-second first retry is postponed to the floor.
            (
                "past_due".to_owned(),
                Some(failed_at + ChronoDuration::seconds(i64::from(WAIT_SECONDS))),
                None,
            )
        };
        assert_eq!(
            dunning_state(&database.pool, subscription_id).await?,
            expected,
            "{key}"
        );
    }

    database.cleanup().await
}

#[tokio::test]
async fn a_retry_scheduled_before_advice_admits_a_later_stop() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("pt_decl_overlap").await?;
    let account = create_gateway_account(&database.pool, "nmi").await?;
    let gateway = resolved_gateway(account)?;
    let renewals = Renewals {
        database: &database,
        account,
        gateway: &gateway,
    };
    let events = Arc::new(Mutex::new(Vec::new()));
    let schedule_only = TestCoordinator {
        pool: database.pool.clone(),
        events: Arc::clone(&events),
    };
    let advising = AdvisingCoordinator::new(schedule_only.clone());
    let (subscription_id, due_at) = due_subscription(
        &database,
        account,
        &gateway,
        &schedule_only,
        schedule(DunningExhaustion::MarkUnpaid)?,
        "overlap",
    )
    .await?;

    // A host release without advice scheduled a retry after a hard decline.
    let (_, first_failed_at) = decline_with_code(
        &renewals,
        &schedule_only,
        subscription_id,
        due_at,
        "overlap_failure_1",
        STOP_CODE,
    )
    .await?;
    assert_eq!(
        dunning_state(&database.pool, subscription_id).await?,
        (
            "past_due".to_owned(),
            Some(first_failed_at + ChronoDuration::seconds(60)),
            None
        )
    );

    // The advising release must still admit that earlier history and stop.
    make_retry_due(&database.pool, subscription_id).await?;
    let (_, second_failed_at) = decline_with_code(
        &renewals,
        &advising,
        subscription_id,
        due_at,
        "overlap_failure_2",
        STOP_CODE,
    )
    .await?;
    assert_eq!(
        dunning_state(&database.pool, subscription_id).await?,
        ("unpaid".to_owned(), None, Some(second_failed_at))
    );
    assert!(events.lock().await.iter().any(|event| matches!(
        event,
        BillingEvent::SubscriptionEnded {
            subscription_id: ended,
            ended_at,
            ..
        } if *ended == subscription_id && *ended_at == second_failed_at
    )));

    database.cleanup().await
}
