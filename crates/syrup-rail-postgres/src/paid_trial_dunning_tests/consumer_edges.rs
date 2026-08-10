use super::*;
use syrup_rail::{
    ActorId, DeletionBlockerQuery, ScrubSubscriberBillingData, SubscriptionGrantCreation,
    SubscriptionGrantCreationOutcome, SubscriptionGrantId, SubscriptionGrantKind,
    SubscriptionGrantReason,
};

use crate::{billing_deletion_blockers, create_subscription_grant, scrub_subscriber_billing_data};

#[tokio::test]
async fn newer_unpaid_history_allows_grants_and_deletion_scrub_without_becoming_canceled()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("pt_consumers").await?;
    let account = create_gateway_account(&database.pool, "nmi").await?;
    let gateway = resolved_gateway(account)?;
    let offer = paid_trial_offer()?;
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
        "consumer-history",
        SubscriptionEnrollmentExpectedTerms::full_price(offer),
        "consumer_history_initial",
        "vault_consumer_history",
    )
    .await?;
    let subscription = initial
        .subscription()
        .expect("approved trial creates subscription");
    let older_canceled_id = subscription.id();
    let payment_method_id = subscription.payment_method_id();
    let command = CancelSubscription::new(
        BillingScopeId::new(account.billing_scope_id),
        subscriber_id,
        PlanKey::new("identity_pro")?,
    );
    let mut transaction = database.pool.begin().await?;
    assert!(matches!(
        cancel_subscription_in_transaction(&mut transaction, &command).await?,
        CancelSubscriptionOutcome::Canceled { .. }
    ));
    transaction.commit().await?;

    sqlx::query(
        r#"
        UPDATE billing_subscriptions
        SET current_period_start_at = boundary.at - interval '2 days',
            current_period_end_at = boundary.at - interval '1 day',
            next_renewal_at = boundary.at - interval '1 day'
        FROM (SELECT clock_timestamp() AS at) boundary
        WHERE id = $1
        "#,
    )
    .bind(older_canceled_id.as_uuid())
    .execute(&database.pool)
    .await?;
    let unpaid_id = syrup_rail::SubscriptionId::new(Uuid::now_v7());
    let unpaid_created_at: DateTime<Utc> = sqlx::query_scalar(
        r#"
        WITH clock AS MATERIALIZED (
            SELECT clock_timestamp() + interval '1 second' AS observed_at
        )
        INSERT INTO billing_subscriptions (
            id, billing_scope_id, subscriber_id, plan_key, status,
            gateway_account_id, payment_method_id, amount_cents, currency,
            current_period_start_at, current_period_end_at, next_renewal_at,
            initial_transaction_id, created_at, updated_at, phase,
            recurring_period_kind, recurring_period_count,
            dunning_retry_delays_seconds, dunning_exhaustion,
            past_due_access, next_payment_attempt_at, unpaid_at
        ) SELECT
            $1, $2, $3, 'identity_pro', 'unpaid', $4, $5, 2900, 'USD',
            observed_at - interval '2 months', observed_at - interval '1 month',
            observed_at - interval '1 month', $6, observed_at, observed_at,
            'recurring', 'calendar_months', 1, ARRAY[]::bigint[],
            'mark_unpaid', 'continue_until_dunning_exhausted', NULL, observed_at
        FROM clock
        RETURNING created_at
        "#,
    )
    .bind(unpaid_id.as_uuid())
    .bind(account.billing_scope_id)
    .bind(subscriber_id.as_uuid())
    .bind(account.gateway_account_id)
    .bind(payment_method_id.as_uuid())
    .bind(format!("unpaid_{}", unpaid_id.as_uuid().simple()))
    .fetch_one(&database.pool)
    .await?;
    sqlx::query(
        "UPDATE billing_subscriptions SET updated_at = $2 + interval '1 day' WHERE id = $1",
    )
    .bind(older_canceled_id.as_uuid())
    .bind(unpaid_created_at)
    .execute(&database.pool)
    .await?;

    let mut transaction = database.pool.begin().await?;
    let outcome = cancel_subscription_in_transaction(&mut transaction, &command).await?;
    transaction.commit().await?;
    assert_eq!(outcome, CancelSubscriptionOutcome::NotFound);

    let grant_id = SubscriptionGrantId::new(Uuid::now_v7());
    let creation = SubscriptionGrantCreation::new(
        grant_id,
        BillingScopeId::new(account.billing_scope_id),
        subscriber_id,
        PlanKey::new("identity_pro")?,
        SubscriptionGrantKind::Promotion,
        SubscriptionGrantReason::new("terminal subscriber accommodation")?,
        Utc::now() + ChronoDuration::days(30),
        ActorId::new(Uuid::now_v7()),
    );
    let mut transaction = database.pool.begin().await?;
    let grant = create_subscription_grant(&mut transaction, &creation).await?;
    transaction.commit().await?;
    assert!(matches!(
        grant,
        SubscriptionGrantCreationOutcome::Created(_)
    ));
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
        Entitlement::Granted { grant } if grant.id() == grant_id
    ));

    let mut transaction = database.pool.begin().await?;
    let blockers = billing_deletion_blockers(
        &mut transaction,
        DeletionBlockerQuery::new(BillingScopeId::new(account.billing_scope_id), subscriber_id),
    )
    .await?;
    assert!(blockers.is_empty());
    let scrubbed = scrub_subscriber_billing_data(
        &mut transaction,
        ScrubSubscriberBillingData::new(
            BillingScopeId::new(account.billing_scope_id),
            subscriber_id,
        ),
    )
    .await?;
    assert!(scrubbed.payment_attempts() > 0);
    assert!(scrubbed.payment_methods() > 0);
    transaction.commit().await?;
    let unpaid_status: String =
        sqlx::query_scalar("SELECT status FROM billing_subscriptions WHERE id = $1")
            .bind(unpaid_id.as_uuid())
            .fetch_one(&database.pool)
            .await?;
    assert_eq!(unpaid_status, "unpaid");
    let method: (String, String) = sqlx::query_as(
        "SELECT status, gateway_payment_method_reference FROM billing_payment_methods WHERE id = $1",
    )
    .bind(payment_method_id.as_uuid())
    .fetch_one(&database.pool)
    .await?;
    assert_eq!(method.0, "disabled");
    assert_eq!(method.1, format!("erased:{}", payment_method_id.as_uuid()));

    database.cleanup().await
}
