//! The provider's AVS result is stored with the response that carried it.

use syrup_rail::GatewayAvsResponse;

use super::billing_address::{enroll_command, named_contact, service};
use super::*;
use crate::test_support::billing_address as address;

fn avs_rejection(avs: Option<&str>) -> GatewayPaymentOutcome {
    GatewayPaymentOutcome::new(
        GatewayPaymentStatus::Failed,
        ProcessorEvidence::new(
            Some(GatewayTransactionId::new("txn_avs_rejected").unwrap()),
            None,
            Some(GatewayDiagnostic::new("3")),
            Some(GatewayDiagnostic::new("300")),
            Some(GatewayDiagnostic::new("AVS REJECTED")),
            Some(GatewayDiagnostic::new("failed")),
            GatewayPaymentDescriptor::default(),
        )
        .with_avs_response(avs.and_then(GatewayAvsResponse::parse)),
    )
}

async fn stored_avs(
    pool: &sqlx::PgPool,
    attempt_id: PaymentAttemptId,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT gateway_avs_response FROM billing_payment_attempts WHERE id = $1")
        .bind(attempt_id.as_uuid())
        .fetch_one(pool)
        .await
}

async fn reloaded(
    fixture: &ApplicationFixture,
    attempt_id: PaymentAttemptId,
) -> Result<PaymentAttempt, Box<dyn Error>> {
    let mut transaction = fixture.database.pool.begin().await?;
    let attempt = crate::find_payment_attempt_by_id_in_transaction(
        &mut transaction,
        fixture.command.billing_scope_id(),
        attempt_id,
    )
    .await?
    .expect("the attempt is stored");
    transaction.rollback().await?;
    Ok(attempt)
}

#[tokio::test]
async fn a_rejected_sale_stores_its_avs_result_and_reads_it_back() -> Result<(), Box<dyn Error>> {
    let fixture = enrollment_fixture("avs_rejected").build().await?;
    let gateway = Arc::new(ScriptedGateway::new(Ok(avs_rejection(Some("Z")))));
    let result = service(&fixture, &gateway)
        .enroll(enroll_command(
            &fixture,
            "avs-rejected",
            named_contact(Some(address("1 Main St"))),
        ))
        .await?;
    let attempt = result.attempt();
    assert_eq!(attempt.status(), PaymentAttemptStatus::Failed);
    let attempt_id = attempt.identity().attempt_id();
    assert_eq!(
        stored_avs(&fixture.database.pool, attempt_id).await?,
        Some("Z".to_owned())
    );
    let reloaded = reloaded(&fixture, attempt_id).await?;
    assert_eq!(
        reloaded.state().processor_evidence().avs_response(),
        GatewayAvsResponse::parse("Z")
    );
    assert_eq!(
        reloaded
            .state()
            .processor_evidence()
            .response_code()
            .map(GatewayDiagnostic::expose),
        Some("300")
    );
    fixture.cleanup().await
}

#[tokio::test]
async fn a_response_without_an_avs_code_stores_none() -> Result<(), Box<dyn Error>> {
    let fixture = enrollment_fixture("avs_absent").build().await?;
    // The NMI adapter drops a value that is not one letter or digit, so a
    // two-character provider value reaches storage as no result and never
    // fails the varchar(1) write.
    assert_eq!(GatewayAvsResponse::parse("YY"), None);
    let gateway = Arc::new(ScriptedGateway::new(Ok(avs_rejection(Some("YY")))));
    let result = service(&fixture, &gateway)
        .enroll(enroll_command(
            &fixture,
            "avs-absent",
            named_contact(Some(address("2 Main St"))),
        ))
        .await?;
    assert_eq!(result.attempt().status(), PaymentAttemptStatus::Failed);
    let attempt_id = result.attempt().identity().attempt_id();
    assert_eq!(stored_avs(&fixture.database.pool, attempt_id).await?, None);
    assert_eq!(
        reloaded(&fixture, attempt_id)
            .await?
            .state()
            .processor_evidence()
            .avs_response(),
        None
    );
    fixture.cleanup().await
}

#[tokio::test]
async fn a_stored_value_that_is_not_an_avs_code_reads_as_none() -> Result<(), Box<dyn Error>> {
    let fixture = enrollment_fixture("avs_foreign").build().await?;
    let gateway = Arc::new(ScriptedGateway::new(Ok(avs_rejection(Some("N")))));
    let result = service(&fixture, &gateway)
        .enroll(enroll_command(
            &fixture,
            "avs-foreign",
            named_contact(Some(address("3 Main St"))),
        ))
        .await?;
    let attempt_id = result.attempt().identity().attempt_id();
    sqlx::query("UPDATE billing_payment_attempts SET gateway_avs_response = '-' WHERE id = $1")
        .bind(attempt_id.as_uuid())
        .execute(&fixture.database.pool)
        .await?;
    let reloaded = reloaded(&fixture, attempt_id).await?;
    assert_eq!(reloaded.state().processor_evidence().avs_response(), None);
    assert_eq!(reloaded.status(), PaymentAttemptStatus::Failed);
    fixture.cleanup().await
}

fn approved_with_avs(transaction_id: &str, avs: Option<&str>) -> GatewayPaymentOutcome {
    let base = approved_outcome(transaction_id);
    GatewayPaymentOutcome::new(
        GatewayPaymentStatus::Approved,
        base.evidence()
            .clone()
            .with_avs_response(avs.and_then(GatewayAvsResponse::parse)),
    )
}

fn unknown_with_avs(avs: Option<&str>) -> GatewayPaymentOutcome {
    GatewayPaymentOutcome::new(
        GatewayPaymentStatus::Unknown,
        ProcessorEvidence::new(
            None,
            None,
            Some(GatewayDiagnostic::new("3")),
            Some(GatewayDiagnostic::new("420")),
            Some(GatewayDiagnostic::new("Processor communication error")),
            None,
            GatewayPaymentDescriptor::default(),
        )
        .with_avs_response(avs.and_then(GatewayAvsResponse::parse)),
    )
}

#[tokio::test]
async fn an_approved_enrollment_stores_its_avs_result_and_reads_it_back()
-> Result<(), Box<dyn Error>> {
    let fixture = application_fixture("avs_approved").build().await?;
    let attempt_id = fixture.command.attempt_id();
    let applied = apply_subscription_enrollment_gateway_outcome(
        &fixture.database.pool,
        &fixture.coordinator,
        &fixture.reservation,
        &approved_with_avs("txn_avs_approved", Some("Y")),
    )
    .await?;
    assert_eq!(applied.status(), PaymentAttemptStatus::Approved);
    assert_eq!(
        stored_avs(&fixture.database.pool, attempt_id).await?,
        Some("Y".to_owned())
    );
    assert_eq!(
        reloaded(&fixture, attempt_id)
            .await?
            .state()
            .processor_evidence()
            .avs_response(),
        GatewayAvsResponse::parse("Y")
    );
    fixture.cleanup().await
}

#[tokio::test]
async fn parking_a_pending_approval_for_review_stores_its_avs_result() -> Result<(), Box<dyn Error>>
{
    let fixture = application_fixture("avs_parked")
        .with_failing_event_append()
        .build()
        .await?;
    let attempt_id = fixture.command.attempt_id();
    let parked = apply_subscription_enrollment_gateway_outcome(
        &fixture.database.pool,
        &fixture.coordinator,
        &fixture.reservation,
        &approved_with_avs("txn_avs_parked", Some("Y")),
    )
    .await?;
    assert_eq!(parked.status(), PaymentAttemptStatus::ReviewRequired);
    assert_eq!(
        stored_avs(&fixture.database.pool, attempt_id).await?,
        Some("Y".to_owned())
    );
    assert_eq!(
        reloaded(&fixture, attempt_id)
            .await?
            .state()
            .processor_evidence()
            .avs_response(),
        GatewayAvsResponse::parse("Y")
    );
    fixture.cleanup().await
}

#[tokio::test]
async fn a_charge_parked_without_avs_replays_cleanly_when_reconciled_with_avs()
-> Result<(), Box<dyn Error>> {
    let mut fixture = application_fixture("avs_replay")
        .with_failing_event_append()
        .build()
        .await?;
    let attempt_id = fixture.command.attempt_id();
    let without_avs = approved_with_avs("txn_avs_replay", None);
    let with_avs = approved_with_avs("txn_avs_replay", Some("Y"));

    // Simulates 0.6.0 parking an approval before AVS was recorded.
    let parked = apply_subscription_enrollment_gateway_outcome(
        &fixture.database.pool,
        &fixture.coordinator,
        &fixture.reservation,
        &without_avs,
    )
    .await?;
    assert_eq!(parked.status(), PaymentAttemptStatus::ReviewRequired);
    assert_eq!(stored_avs(&fixture.database.pool, attempt_id).await?, None);

    // 0.7.0 reconciles the same response carrying AVS: processor-charge
    // matching excludes AVS, so the charge replays cleanly and the approved
    // attempt records the AVS result.
    fixture.coordinator.fail_event = false;
    let applied = apply_reconciled_subscription_enrollment_gateway_outcome(
        &fixture.database.pool,
        &fixture.coordinator,
        fixture.command.billing_scope_id(),
        attempt_id,
        &with_avs,
    )
    .await?;
    assert_eq!(applied.status(), PaymentAttemptStatus::Approved);
    assert!(applied.subscription().is_some());
    assert_eq!(
        stored_avs(&fixture.database.pool, attempt_id).await?,
        Some("Y".to_owned())
    );

    // Replaying the already-approved attempt either with or without AVS
    // remains idempotent without altering the stored attempt.
    for replay in [&with_avs, &without_avs] {
        let replayed = apply_reconciled_subscription_enrollment_gateway_outcome(
            &fixture.database.pool,
            &fixture.coordinator,
            fixture.command.billing_scope_id(),
            attempt_id,
            replay,
        )
        .await?;
        assert_eq!(replayed.status(), PaymentAttemptStatus::Approved);
        assert_eq!(
            stored_avs(&fixture.database.pool, attempt_id).await?,
            Some("Y".to_owned())
        );
    }
    fixture.cleanup().await
}

#[tokio::test]
async fn reconciled_non_approved_evidence_prefers_observed_avs_and_keeps_stored_fallback()
-> Result<(), Box<dyn Error>> {
    for (name, initial_avs, reconciled_avs, expected) in [
        ("keep", Some("Z"), None, Some("Z")),
        ("replace", Some("Z"), Some("N"), Some("N")),
        ("fill", None, Some("Z"), Some("Z")),
    ] {
        let fixture = application_fixture(&format!("avs_{name}")).build().await?;
        let attempt_id = fixture.command.attempt_id();
        let unknown = apply_subscription_enrollment_gateway_outcome(
            &fixture.database.pool,
            &fixture.coordinator,
            &fixture.reservation,
            &unknown_with_avs(initial_avs),
        )
        .await?;
        assert_eq!(unknown.status(), PaymentAttemptStatus::Unknown, "{name}");
        assert_eq!(
            stored_avs(&fixture.database.pool, attempt_id).await?,
            initial_avs.map(str::to_owned),
            "{name}"
        );

        let failed = apply_reconciled_subscription_enrollment_gateway_outcome(
            &fixture.database.pool,
            &fixture.coordinator,
            fixture.command.billing_scope_id(),
            attempt_id,
            &avs_rejection(reconciled_avs),
        )
        .await?;
        assert_eq!(failed.status(), PaymentAttemptStatus::Failed, "{name}");
        assert_eq!(
            stored_avs(&fixture.database.pool, attempt_id).await?,
            expected.map(str::to_owned),
            "{name}"
        );
        assert_eq!(
            reloaded(&fixture, attempt_id)
                .await?
                .state()
                .processor_evidence()
                .avs_response(),
            expected.and_then(GatewayAvsResponse::parse),
            "{name}"
        );
        fixture.cleanup().await?;
    }
    Ok(())
}

#[tokio::test]
async fn reconciled_approval_without_avs_keeps_stored_avs_and_prefers_newer_code()
-> Result<(), Box<dyn Error>> {
    for (name, reconciled_avs, expected) in
        [("keep", None, Some("Y")), ("replace", Some("A"), Some("A"))]
    {
        let mut fixture = application_fixture(&format!("avs_app_{name}"))
            .with_failing_event_append()
            .build()
            .await?;
        let attempt_id = fixture.command.attempt_id();
        let parked = apply_subscription_enrollment_gateway_outcome(
            &fixture.database.pool,
            &fixture.coordinator,
            &fixture.reservation,
            &approved_with_avs("txn_avs_keep_app", Some("Y")),
        )
        .await?;
        assert_eq!(
            parked.status(),
            PaymentAttemptStatus::ReviewRequired,
            "{name}"
        );
        assert_eq!(
            stored_avs(&fixture.database.pool, attempt_id).await?,
            Some("Y".to_owned()),
            "{name}"
        );

        fixture.coordinator.fail_event = false;
        let applied = apply_reconciled_subscription_enrollment_gateway_outcome(
            &fixture.database.pool,
            &fixture.coordinator,
            fixture.command.billing_scope_id(),
            attempt_id,
            &approved_with_avs("txn_avs_keep_app", reconciled_avs),
        )
        .await?;
        assert_eq!(applied.status(), PaymentAttemptStatus::Approved, "{name}");
        assert_eq!(
            stored_avs(&fixture.database.pool, attempt_id).await?,
            expected.map(str::to_owned),
            "{name}"
        );
        assert_eq!(
            reloaded(&fixture, attempt_id)
                .await?
                .state()
                .processor_evidence()
                .avs_response(),
            expected.and_then(GatewayAvsResponse::parse),
            "{name}"
        );
        fixture.cleanup().await?;
    }
    Ok(())
}

#[tokio::test]
async fn parking_an_unknown_attempt_keeps_its_stored_avs_result() -> Result<(), Box<dyn Error>> {
    let fixture = application_fixture("avs_unk_park")
        .with_failing_event_append()
        .build()
        .await?;
    let attempt_id = fixture.command.attempt_id();
    let unknown = apply_subscription_enrollment_gateway_outcome(
        &fixture.database.pool,
        &fixture.coordinator,
        &fixture.reservation,
        &unknown_with_avs(Some("Z")),
    )
    .await?;
    assert_eq!(unknown.status(), PaymentAttemptStatus::Unknown);

    // A late approval that must be parked leaves an already-observed unknown
    // attempt's evidence (including its AVS code) intact.
    let parked = apply_reconciled_subscription_enrollment_gateway_outcome(
        &fixture.database.pool,
        &fixture.coordinator,
        fixture.command.billing_scope_id(),
        attempt_id,
        &approved_with_avs("txn_avs_unk_park", Some("Y")),
    )
    .await?;
    assert_eq!(parked.status(), PaymentAttemptStatus::ReviewRequired);
    assert_eq!(
        stored_avs(&fixture.database.pool, attempt_id).await?,
        Some("Z".to_owned())
    );
    fixture.cleanup().await
}

#[tokio::test]
async fn payment_method_replacement_and_reconciliation_claim_carry_avs_while_portal_excludes_it()
-> Result<(), Box<dyn Error>> {
    let (fixture, reservation) = reconciled_payment_method_replacement_fixture("avs_pm").await?;
    let attempt_id = reservation.identity().attempt_id();
    let pool = &fixture.database.pool;

    // Unknown foreground outcome stores AVS; exact reconciliation claim reads
    // it back through the explicit schema-v7 column projection.
    let unknown = apply_subscription_payment_method_replacement_gateway_outcome(
        pool,
        &fixture.coordinator,
        &reservation,
        &unknown_with_avs(Some("Z")),
    )
    .await?;
    assert_eq!(unknown.status(), PaymentAttemptStatus::Unknown);
    assert_eq!(stored_avs(pool, attempt_id).await?, Some("Z".to_owned()));

    sqlx::query(
        "UPDATE billing_payment_attempts \
         SET created_at = clock_timestamp() - interval '15 minutes', \
             submitted_at = clock_timestamp() - interval '12 minutes', \
             updated_at = clock_timestamp() - interval '10 minutes' \
         WHERE id = $1",
    )
    .bind(attempt_id.as_uuid())
    .execute(pool)
    .await?;
    let claimed = crate::claim_exact_reconciliation_attempts(
        pool,
        reservation.identity().gateway_account_id(),
    )
    .await?;
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].identity().attempt_id(), attempt_id);
    assert_eq!(
        claimed[0].state().processor_evidence().avs_response(),
        GatewayAvsResponse::parse("Z")
    );

    // Reconciling the replacement as approved without AVS preserves 'Z', and
    // customer billing portal / history reads succeed without exposing AVS.
    let applied = apply_reconciled_subscription_payment_method_replacement_gateway_outcome(
        pool,
        &fixture.coordinator,
        reservation.identity().billing_scope_id(),
        attempt_id,
        &approved_with_avs("txn_avs_pm_approved", None),
    )
    .await?;
    assert_eq!(applied.status(), PaymentAttemptStatus::Approved);
    assert_eq!(stored_avs(pool, attempt_id).await?, Some("Z".to_owned()));

    let portal_query = syrup_rail::SubscriptionBillingPortalQuery::new(
        fixture.command.billing_scope_id(),
        fixture.command.subscriber_id(),
        fixture.command.plan_key().clone(),
    );
    let portal = crate::subscription_billing_portal(pool, &portal_query).await?;
    assert!(portal.entitlement().permits_product_access());
    let history = crate::subscription_payment_history_page(
        pool,
        &portal_query,
        None,
        syrup_rail::SubscriptionPaymentHistoryPageLimit::new(10)?,
    )
    .await?;
    assert!(
        history
            .items()
            .iter()
            .any(|item| item.payment_attempt_id() == attempt_id)
    );
    fixture.cleanup().await
}

#[tokio::test]
async fn renewal_non_approved_outcome_stores_avs_result() -> Result<(), Box<dyn Error>> {
    let fixture = enrollment_fixture("avs_renew").build().await?;
    let pool = &fixture.database.pool;
    let initial_gateway = Arc::new(ScriptedGateway::new(Ok(approved_with_avs(
        "txn_avs_renew_init",
        Some("Y"),
    ))));
    let enrolled = service(&fixture, &initial_gateway)
        .enroll(fixture.command.clone())
        .await?;
    let subscription = enrolled.subscription().expect("active subscription");
    let due_at = *subscription.next_renewal_at() - chrono::Duration::days(60);
    let period_start_at = due_at - chrono::Duration::days(30);
    sqlx::query(
        "UPDATE billing_subscriptions \
         SET current_period_start_at = $2, current_period_end_at = $3, \
             next_renewal_at = $3, next_payment_attempt_at = $3 \
         WHERE id = $1",
    )
    .bind(subscription.id().as_uuid())
    .bind(period_start_at)
    .bind(due_at)
    .execute(pool)
    .await?;

    let decline_gateway = Arc::new(ScriptedGateway::new(Ok(avs_rejection(Some("N")))));
    let renewed = service(&fixture, &decline_gateway)
        .renew(syrup_rail::ChargeRenewal::new(
            fixture.command.billing_scope_id(),
            subscription.id(),
            due_at,
        ))
        .await?;
    let syrup_rail::SubscriptionRenewalOutcome::Payment(payment) = renewed else {
        panic!("expected renewal payment outcome, got {renewed:?}");
    };
    let attempt = payment.attempt();
    assert_eq!(attempt.status(), PaymentAttemptStatus::Failed);
    let attempt_id = attempt.identity().attempt_id();
    assert_eq!(stored_avs(pool, attempt_id).await?, Some("N".to_owned()));
    assert_eq!(
        reloaded(&fixture, attempt_id)
            .await?
            .state()
            .processor_evidence()
            .avs_response(),
        GatewayAvsResponse::parse("N")
    );
    fixture.cleanup().await
}
