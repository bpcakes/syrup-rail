use super::*;
use syrup_rail::{
    ActorId, ExternalReversalHostChargeRelease, ExternalReversalKind, ExternalReversalReason,
    GatewayTransactionId, ProcessorChargeId,
};

use crate::{
    ExternalReversalAttestationOutcome, ExternalReversalHostStore, ExternalReversalHostStoreError,
    ExternalReversalHostTransitionOutcome, attest_external_reversal,
};

struct NoHostTarget;

#[async_trait]
impl ExternalReversalHostStore for NoHostTarget {
    async fn release(
        &self,
        _connection: &mut PgConnection,
        _release: ExternalReversalHostChargeRelease,
    ) -> Result<ExternalReversalHostTransitionOutcome, ExternalReversalHostStoreError> {
        Ok(ExternalReversalHostTransitionOutcome::Unchanged)
    }
}

#[tokio::test]
async fn late_approval_and_reversal_preserve_terminal_failure_history_and_cancellation()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("pt_reclassify").await?;
    let account = create_gateway_account(&database.pool, "nmi").await?;
    let gateway = resolved_gateway(account)?;
    let offer = paid_trial_offer_with_policy(RenewalFailurePolicy::new(
        DunningSchedule::from_seconds([60, 180])?,
        DunningExhaustion::MarkUnpaid,
        PastDueAccessPolicy::SuspendImmediately,
    ))?;
    let offers = StaticOfferStore {
        offer: offer.clone(),
    };
    let events = Arc::new(Mutex::new(Vec::new()));
    let coordinator = TestCoordinator {
        pool: database.pool.clone(),
        events,
    };
    let subscriber_id = SubscriberId::new(Uuid::now_v7());
    let initial = approve_enrollment(
        &database.pool,
        &offers,
        &gateway,
        &coordinator,
        account,
        subscriber_id,
        "reclassification-enrollment",
        SubscriptionEnrollmentExpectedTerms::full_price(offer),
        "reclassification_initial",
        "vault_reclassification",
    )
    .await?;
    let subscription_id = initial
        .subscription()
        .expect("approved trial creates a subscription")
        .id();
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

    let renewal_command = ChargeRenewal::new(
        BillingScopeId::new(account.billing_scope_id),
        subscription_id,
        due_at,
    );
    let first_reservation =
        reserve_and_admit_renewal(&database.pool, &gateway, renewal_command).await?;
    let first = apply_subscription_renewal_gateway_outcome(
        &database.pool,
        &coordinator,
        &first_reservation,
        &declined_outcome("reclassification_decline"),
    )
    .await?;
    let first_attempt_id = first.attempt().identity().attempt_id();
    let first_failed_at = resolved_at(&database.pool, first_attempt_id).await?;

    let late_transaction_id = "reclassification_late_approval";
    let late = apply_subscription_renewal_gateway_outcome(
        &database.pool,
        &coordinator,
        &first_reservation,
        &approved_outcome_with_reference(late_transaction_id, "vault_late_reclassification"),
    )
    .await?;
    assert!(late.subscription().is_none());
    let (attempt_status, resolution_code): (String, Option<String>) = sqlx::query_as(
        "SELECT status, resolution_code FROM billing_payment_attempts WHERE id = $1",
    )
    .bind(first_attempt_id.as_uuid())
    .fetch_one(&database.pool)
    .await?;
    assert_eq!(attempt_status, "declined");
    assert_eq!(resolution_code, None);

    let charge_id: Uuid = sqlx::query_scalar(
        "SELECT id FROM billing_processor_charges WHERE gateway_transaction_id = $1",
    )
    .bind(late_transaction_id)
    .fetch_one(&database.pool)
    .await?;
    let expected_transaction_id = GatewayTransactionId::new(late_transaction_id)?;
    assert!(matches!(
        attest_external_reversal(
            &database.pool,
            &NoHostTarget,
            ProcessorChargeId::new(charge_id),
            ActorId::new(Uuid::now_v7()),
            ExternalReversalKind::Refund,
            &expected_transaction_id,
            &ExternalReversalReason::new("late renewal approval was refunded")?,
        )
        .await?,
        ExternalReversalAttestationOutcome::Attested { .. }
    ));
    let (attempt_status, resolution_code): (String, Option<String>) = sqlx::query_as(
        "SELECT status, resolution_code FROM billing_payment_attempts WHERE id = $1",
    )
    .bind(first_attempt_id.as_uuid())
    .fetch_one(&database.pool)
    .await?;
    assert_eq!(attempt_status, "declined");
    assert_eq!(resolution_code, None);

    sqlx::query(
        r#"
        UPDATE billing_subscriptions
        SET next_payment_attempt_at = clock_timestamp() - interval '1 second'
        WHERE id = $1
        "#,
    )
    .bind(subscription_id.as_uuid())
    .execute(&database.pool)
    .await?;
    let second_reservation =
        reserve_and_admit_renewal(&database.pool, &gateway, renewal_command).await?;
    let second = apply_subscription_renewal_gateway_outcome(
        &database.pool,
        &coordinator,
        &second_reservation,
        &declined_outcome("reclassification_decline_2"),
    )
    .await?;
    let second_failed_at =
        resolved_at(&database.pool, second.attempt().identity().attempt_id()).await?;
    let next_payment_attempt_at: Option<DateTime<Utc>> = sqlx::query_scalar(
        "SELECT next_payment_attempt_at FROM billing_subscriptions WHERE id = $1",
    )
    .bind(subscription_id.as_uuid())
    .fetch_one(&database.pool)
    .await?;
    assert_eq!(
        next_payment_attempt_at,
        Some(second_failed_at + ChronoDuration::seconds(180))
    );
    let failure_count: i64 = sqlx::query_scalar(
        r#"
        SELECT count(*)
        FROM billing_payment_attempts
        WHERE subscription_id = $1
            AND billing_period_start_at = $2
            AND attempt_kind = 'subscription_renewal'
            AND submitted_at IS NOT NULL
            AND status IN ('declined', 'failed')
            AND resolution_code IS NULL
        "#,
    )
    .bind(subscription_id.as_uuid())
    .bind(due_at)
    .fetch_one(&database.pool)
    .await?;
    assert_eq!(failure_count, 2);

    let command = CancelSubscription::new(
        BillingScopeId::new(account.billing_scope_id),
        subscriber_id,
        PlanKey::new("identity_pro")?,
    );
    let mut transaction = database.pool.begin().await?;
    let canceled = cancel_subscription_in_transaction(&mut transaction, &command).await?;
    transaction.commit().await?;
    let CancelSubscriptionOutcome::Canceled { event, .. } = canceled else {
        return Err("past-due subscription was not canceled".into());
    };
    assert!(matches!(
        event,
        BillingEvent::SubscriptionCanceled { access_ends_at, .. }
            if access_ends_at == first_failed_at
    ));

    database.cleanup().await
}
