//! The renewal intent's first-charge-after-paid-trial flag and the resolved
//! gateway's statement-descriptor policy, observed at the provider boundary.

use syrup_rail::{
    GatewaySaleIntent, StatementDescriptor, StatementDescriptorPolicy, SubscriptionId,
    SubscriptionPhase,
};

use super::billing_address::{enroll_command, named_contact, service};
use super::*;

async fn phase(
    pool: &sqlx::PgPool,
    subscription_id: SubscriptionId,
) -> Result<String, sqlx::Error> {
    sqlx::query_scalar("SELECT phase FROM billing_subscriptions WHERE id = $1")
        .bind(subscription_id.as_uuid())
        .fetch_one(pool)
        .await
}

/// The enrollment fixture activates an immediate-recurring offer. Moving the
/// row back into the paid-trial phase recreates the state a paid-trial
/// enrollment leaves behind until its first renewal is approved.
async fn mark_paid_trial(
    pool: &sqlx::PgPool,
    subscription_id: SubscriptionId,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        UPDATE billing_subscriptions
        SET phase = 'paid_trial', trial_amount_cents = 100,
            trial_period_kind = 'fixed_days', trial_period_count = 7,
            updated_at = clock_timestamp()
        WHERE id = $1
        "#,
    )
    .bind(subscription_id.as_uuid())
    .execute(pool)
    .await?;
    Ok(())
}

/// Makes the next renewal due `minutes_ago` minutes in the past and returns
/// its period start. Consecutive renewals of one subscription need distinct
/// period starts: the renewal idempotency key has second resolution and an
/// approved attempt for the same period would be replayed instead of charged.
async fn make_renewal_due_minutes_ago(
    pool: &sqlx::PgPool,
    subscription_id: SubscriptionId,
    minutes_ago: i32,
) -> Result<DateTime<Utc>, sqlx::Error> {
    sqlx::query_scalar(
        r#"
        UPDATE billing_subscriptions
        SET current_period_start_at = now() - interval '1 month' - make_interval(mins => $2),
            current_period_end_at = now() - make_interval(mins => $2),
            next_renewal_at = now() - make_interval(mins => $2),
            next_payment_attempt_at = now() - make_interval(mins => $2),
            updated_at = clock_timestamp()
        WHERE id = $1
        RETURNING next_renewal_at
        "#,
    )
    .bind(subscription_id.as_uuid())
    .bind(minutes_ago)
    .fetch_one(pool)
    .await
}

async fn enroll(fixture: &ApplicationFixture, key: &str) -> Result<SubscriptionId, Box<dyn Error>> {
    let gateway = Arc::new(ScriptedGateway::new(Ok(approved_outcome_with_reference(
        Some(&format!("txn_{key}")),
        &format!("vault_{key}"),
    ))));
    let result = service(fixture, &gateway)
        .enroll(enroll_command(fixture, key, named_contact(None)))
        .await?;
    assert_eq!(result.attempt().status(), PaymentAttemptStatus::Approved);
    Ok(result
        .subscription()
        .expect("approved enrollment creates a subscription")
        .id())
}

async fn renew_with(
    fixture: &ApplicationFixture,
    subscription_id: SubscriptionId,
    transaction_id: &str,
    due_minutes_ago: i32,
    policy: Option<StatementDescriptorPolicy>,
) -> Result<GatewaySaleRequest, Box<dyn Error>> {
    let period_start_at =
        make_renewal_due_minutes_ago(&fixture.database.pool, subscription_id, due_minutes_ago)
            .await?;
    let (request, status) = submit_renewal(
        fixture,
        subscription_id,
        period_start_at,
        approved_outcome(transaction_id),
        policy,
    )
    .await?;
    assert_eq!(status, PaymentAttemptStatus::Approved);
    Ok(request)
}

async fn submit_renewal(
    fixture: &ApplicationFixture,
    subscription_id: SubscriptionId,
    period_start_at: DateTime<Utc>,
    outcome: GatewayPaymentOutcome,
    policy: Option<StatementDescriptorPolicy>,
) -> Result<(GatewaySaleRequest, PaymentAttemptStatus), Box<dyn Error>> {
    let gateway = Arc::new(ScriptedGateway::new(Ok(outcome)));
    let mut resolved = scripted_resolved_gateway(fixture.gateway_account, Arc::clone(&gateway));
    if let Some(policy) = policy {
        resolved = resolved.with_statement_descriptors(policy);
    }
    let service = SubscriptionBillingService::new(
        fixture.database.pool.clone(),
        Arc::new(TestOfferStore),
        Arc::new(StaticResolver {
            gateway: resolved,
            calls: AtomicUsize::new(0),
        }),
        Arc::new(PermitAdmission {
            calls: AtomicUsize::new(0),
        }),
        Arc::new(fixture.coordinator.clone()),
    );
    let outcome = service
        .renew(ChargeRenewal::new(
            fixture.command.billing_scope_id(),
            subscription_id,
            period_start_at,
        ))
        .await?;
    let SubscriptionRenewalOutcome::Payment(payment) = outcome else {
        return Err(format!("due renewal must submit: {outcome:?}").into());
    };
    let requests = gateway.sale_requests.lock().await.clone();
    assert_eq!(requests.len(), 1);
    Ok((
        requests.into_iter().next().unwrap(),
        payment.attempt().status(),
    ))
}

fn first_charge_after_paid_trial(request: &GatewaySaleRequest) -> bool {
    match request.intent() {
        GatewaySaleIntent::RecurringStoredCredential {
            first_charge_after_paid_trial,
            ..
        } => *first_charge_after_paid_trial,
        other => panic!("renewal must submit a recurring stored credential: {other:?}"),
    }
}

fn policy() -> StatementDescriptorPolicy {
    StatementDescriptorPolicy::new(
        StatementDescriptor::new("IDENTITYPRO.COM")
            .unwrap()
            .with_phone(Some("800-555-0199".to_owned()))
            .unwrap(),
    )
    .with_first_charge_after_paid_trial(StatementDescriptor::new("IDENTITYPRO TRIAL").unwrap())
}

#[tokio::test]
async fn first_renewal_after_a_paid_trial_is_flagged_until_its_approval_is_applied()
-> Result<(), Box<dyn Error>> {
    let fixture = enrollment_fixture("desc_trial", false, false, false).await?;
    let subscription_id = enroll(&fixture, "descriptor-trial-enrollment").await?;
    mark_paid_trial(&fixture.database.pool, subscription_id).await?;
    assert_eq!(
        phase(&fixture.database.pool, subscription_id).await?,
        SubscriptionPhase::PaidTrial.as_str()
    );

    // The first post-trial renewal is flagged and gets the trial wording.
    let first = renew_with(
        &fixture,
        subscription_id,
        "txn_descriptor_first_renewal",
        60,
        Some(policy()),
    )
    .await?;
    assert!(first_charge_after_paid_trial(&first));
    assert_eq!(
        first.descriptor().map(StatementDescriptor::name),
        Some("IDENTITYPRO TRIAL")
    );
    assert_eq!(
        phase(&fixture.database.pool, subscription_id).await?,
        SubscriptionPhase::Recurring.as_str(),
        "an applied renewal approval moves the subscription into recurring"
    );

    // Later renewals are ordinary recurring charges with the default wording.
    let second = renew_with(
        &fixture,
        subscription_id,
        "txn_descriptor_second_renewal",
        30,
        Some(policy()),
    )
    .await?;
    assert!(!first_charge_after_paid_trial(&second));
    let descriptor = second.descriptor().expect("policy applies to every sale");
    assert_eq!(descriptor.name(), "IDENTITYPRO.COM");
    assert_eq!(descriptor.phone(), Some("800-555-0199"));
    fixture.cleanup().await
}

#[tokio::test]
async fn dunning_retry_keeps_trial_descriptor_until_the_same_period_is_approved()
-> Result<(), Box<dyn Error>> {
    let fixture = enrollment_fixture("desc_retry", false, false, false).await?;
    let subscription_id = enroll(&fixture, "descriptor-retry-enrollment").await?;
    mark_paid_trial(&fixture.database.pool, subscription_id).await?;
    sqlx::query(
        "UPDATE billing_subscriptions SET dunning_retry_delays_seconds = ARRAY[60]::bigint[] WHERE id = $1",
    )
    .bind(subscription_id.as_uuid())
    .execute(&fixture.database.pool)
    .await?;
    let period_start_at =
        make_renewal_due_minutes_ago(&fixture.database.pool, subscription_id, 60).await?;
    let decline = GatewayPaymentOutcome::new(
        GatewayPaymentStatus::Declined,
        ProcessorEvidence::new(
            Some(GatewayTransactionId::new("txn_descriptor_decline")?),
            None,
            Some(GatewayDiagnostic::new("2")),
            Some(GatewayDiagnostic::new("200")),
            Some(GatewayDiagnostic::new("Declined")),
            Some(GatewayDiagnostic::new("declined")),
            GatewayPaymentDescriptor::default(),
        ),
    );
    let (first, status) = submit_renewal(
        &fixture,
        subscription_id,
        period_start_at,
        decline,
        Some(policy()),
    )
    .await?;
    assert_eq!(status, PaymentAttemptStatus::Declined);
    assert!(first_charge_after_paid_trial(&first));
    assert_eq!(
        first.descriptor().map(StatementDescriptor::name),
        Some("IDENTITYPRO TRIAL")
    );
    assert_eq!(
        phase(&fixture.database.pool, subscription_id).await?,
        "paid_trial"
    );
    let (status, due_period, retry_at): (String, DateTime<Utc>, Option<DateTime<Utc>>) =
        sqlx::query_as(
            "SELECT status, next_renewal_at, next_payment_attempt_at FROM billing_subscriptions WHERE id = $1",
        )
        .bind(subscription_id.as_uuid())
        .fetch_one(&fixture.database.pool)
        .await?;
    assert_eq!(status, "past_due");
    assert_eq!(due_period, period_start_at);
    assert!(
        retry_at.is_some(),
        "decline must schedule an automatic retry"
    );

    // Advance only dispatch eligibility: this is a retry of the same economic period.
    sqlx::query(
        "UPDATE billing_subscriptions SET next_payment_attempt_at = clock_timestamp() - interval '1 second' WHERE id = $1",
    )
    .bind(subscription_id.as_uuid())
    .execute(&fixture.database.pool)
    .await?;
    let (retry, status) = submit_renewal(
        &fixture,
        subscription_id,
        period_start_at,
        approved_outcome("txn_descriptor_retry"),
        Some(policy()),
    )
    .await?;
    assert_eq!(status, PaymentAttemptStatus::Approved);
    assert_ne!(
        retry.order_id(),
        first.order_id(),
        "retry must submit a new attempt"
    );
    assert!(first_charge_after_paid_trial(&retry));
    assert_eq!(
        retry.descriptor().map(StatementDescriptor::name),
        Some("IDENTITYPRO TRIAL")
    );
    assert_eq!(
        phase(&fixture.database.pool, subscription_id).await?,
        "recurring"
    );

    let next = renew_with(
        &fixture,
        subscription_id,
        "txn_descriptor_after_retry",
        30,
        Some(policy()),
    )
    .await?;
    assert!(!first_charge_after_paid_trial(&next));
    assert_eq!(next.descriptor(), Some(policy().default_descriptor()));
    fixture.cleanup().await
}

#[tokio::test]
async fn immediate_recurring_renewals_are_never_flagged_and_send_no_descriptor_without_a_policy()
-> Result<(), Box<dyn Error>> {
    let fixture = enrollment_fixture("desc_immediate", false, false, false).await?;
    let subscription_id = enroll(&fixture, "descriptor-immediate-enrollment").await?;
    assert_eq!(
        phase(&fixture.database.pool, subscription_id).await?,
        SubscriptionPhase::Recurring.as_str()
    );
    let request = renew_with(
        &fixture,
        subscription_id,
        "txn_descriptor_immediate_renewal",
        60,
        None,
    )
    .await?;
    assert!(!first_charge_after_paid_trial(&request));
    assert!(request.descriptor().is_none(), "no policy, no descriptor");
    fixture.cleanup().await
}
