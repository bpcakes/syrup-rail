use std::{error::Error, io};

use chrono::Duration;
use syrup_rail::{
    BillingEventKey, BillingScopeId, CancelSubscription, CancelSubscriptionOutcome,
    PaymentAttemptKind, PlanKey, SubscriberId, SubscriptionStatus,
};

use super::*;
use crate::{
    attempts::LocalAttemptPolicy,
    test_support::{GatewayAccountFixture, TestDatabase, create_gateway_account},
};

struct SubscriptionFixture {
    account: GatewayAccountFixture,
    subscriber_id: Uuid,
    plan_key: PlanKey,
    subscription_id: Uuid,
    payment_method_id: Uuid,
    period_end: DateTime<Utc>,
    initial_transaction_id: String,
}

impl SubscriptionFixture {
    fn command(&self) -> CancelSubscription {
        CancelSubscription::new(
            BillingScopeId::new(self.account.billing_scope_id),
            SubscriberId::new(self.subscriber_id),
            self.plan_key.clone(),
        )
    }
}

#[tokio::test]
async fn cancellation_is_exact_idempotent_and_preserves_the_payment_method()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("sr_cancel_exact").await?;
    let result = async {
        let account = create_gateway_account(&database.pool, "nmi").await?;
        let subscriber_id = Uuid::now_v7();
        let fixture = insert_subscription(
            &database.pool,
            account,
            subscriber_id,
            "plan_a",
            SubscriptionStatus::Active,
        )
        .await?;
        let untouched = insert_subscription(
            &database.pool,
            account,
            subscriber_id,
            "plan_b",
            SubscriptionStatus::Active,
        )
        .await?;
        let method_before =
            payment_method_snapshot(&database.pool, fixture.payment_method_id).await?;

        let mut transaction = database.pool.begin().await?;
        let outcome =
            cancel_subscription_in_transaction(&mut transaction, &fixture.command()).await?;
        let CancelSubscriptionOutcome::Canceled {
            subscription,
            event,
        } = outcome
        else {
            return Err(io::Error::other("active subscription was not canceled").into());
        };
        if subscription.status() != SubscriptionStatus::Canceled
            || event.semantic_key() != BillingEventKey::SubscriptionCanceled(subscription.id())
        {
            return Err(io::Error::other("cancellation result lost canonical state").into());
        }
        transaction.commit().await?;

        let method_after =
            payment_method_snapshot(&database.pool, fixture.payment_method_id).await?;
        if method_after != method_before {
            return Err(io::Error::other("cancellation changed the payment method").into());
        }
        let untouched_status: String =
            sqlx::query_scalar("SELECT status FROM billing_subscriptions WHERE id = $1")
                .bind(untouched.subscription_id)
                .fetch_one(&database.pool)
                .await?;
        if untouched_status != "active" {
            return Err(io::Error::other("cancellation crossed the plan boundary").into());
        }

        let mut transaction = database.pool.begin().await?;
        let replay =
            cancel_subscription_in_transaction(&mut transaction, &fixture.command()).await?;
        transaction.commit().await?;
        if !matches!(replay, CancelSubscriptionOutcome::AlreadyCanceled(_)) {
            return Err(io::Error::other("repeat cancellation was not idempotent").into());
        }

        let wrong_scope = CancelSubscription::new(
            BillingScopeId::new(Uuid::now_v7()),
            SubscriberId::new(subscriber_id),
            fixture.plan_key.clone(),
        );
        let mut transaction = database.pool.begin().await?;
        let outcome = cancel_subscription_in_transaction(&mut transaction, &wrong_scope).await?;
        transaction.commit().await?;
        if outcome != CancelSubscriptionOutcome::NotFound {
            return Err(io::Error::other("scope mismatch was not hidden").into());
        }
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    database.cleanup().await?;
    result
}

#[tokio::test]
async fn cancellation_cleans_only_stale_updates_and_respects_active_blockers()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("sr_cancel_blocks").await?;
    let result = async {
        let account = create_gateway_account(&database.pool, "nmi").await?;
        let subscriber_id = Uuid::now_v7();

        let fresh = insert_subscription(
            &database.pool,
            account,
            subscriber_id,
            "fresh_update",
            SubscriptionStatus::Active,
        )
        .await?;
        insert_payment_method_update(&database.pool, &fresh, Utc::now()).await?;
        assert_outcome(
            &database.pool,
            &fresh.command(),
            CancelSubscriptionOutcome::BlockedByPaymentMethodUpdate,
        )
        .await?;

        let stale = insert_subscription(
            &database.pool,
            account,
            subscriber_id,
            "stale_update",
            SubscriptionStatus::Active,
        )
        .await?;
        let stale_attempt = insert_payment_method_update(
            &database.pool,
            &stale,
            Utc::now()
                - Duration::seconds(
                    LocalAttemptPolicy::for_kind(
                        PaymentAttemptKind::SubscriptionPaymentMethodUpdate,
                    )
                    .stale_after_seconds()
                        + 1,
                ),
        )
        .await?;
        let mut transaction = database.pool.begin().await?;
        let outcome =
            cancel_subscription_in_transaction(&mut transaction, &stale.command()).await?;
        transaction.commit().await?;
        if !matches!(outcome, CancelSubscriptionOutcome::Canceled { .. }) {
            return Err(io::Error::other("stale update still blocked cancellation").into());
        }
        let stale_status: String =
            sqlx::query_scalar("SELECT status FROM billing_payment_attempts WHERE id = $1")
                .bind(stale_attempt)
                .fetch_one(&database.pool)
                .await?;
        if stale_status != "failed" {
            return Err(io::Error::other("stale update was not failed atomically").into());
        }

        let stale_review = insert_subscription(
            &database.pool,
            account,
            subscriber_id,
            "stale_review_update",
            SubscriptionStatus::Active,
        )
        .await?;
        let stale_review_attempt = insert_payment_method_update(
            &database.pool,
            &stale_review,
            Utc::now()
                - Duration::seconds(
                    LocalAttemptPolicy::for_kind(
                        PaymentAttemptKind::SubscriptionPaymentMethodUpdate,
                    )
                    .stale_after_seconds()
                        + 1,
                ),
        )
        .await?;
        sqlx::query("UPDATE billing_payment_attempts SET status = 'review_required' WHERE id = $1")
            .bind(stale_review_attempt)
            .execute(&database.pool)
            .await?;
        let mut transaction = database.pool.begin().await?;
        let outcome =
            cancel_subscription_in_transaction(&mut transaction, &stale_review.command()).await?;
        transaction.commit().await?;
        if !matches!(outcome, CancelSubscriptionOutcome::Canceled { .. }) {
            return Err(
                io::Error::other("stale unsubmitted review still blocked cancellation").into(),
            );
        }
        let stale_review_status: String =
            sqlx::query_scalar("SELECT status FROM billing_payment_attempts WHERE id = $1")
                .bind(stale_review_attempt)
                .fetch_one(&database.pool)
                .await?;
        if stale_review_status != "failed" {
            return Err(
                io::Error::other("stale unsubmitted review was not failed atomically").into(),
            );
        }

        let renewal = insert_subscription(
            &database.pool,
            account,
            subscriber_id,
            "renewal",
            SubscriptionStatus::Active,
        )
        .await?;
        insert_renewal(&database.pool, &renewal, Utc::now()).await?;
        assert_outcome(
            &database.pool,
            &renewal.command(),
            CancelSubscriptionOutcome::BlockedByRenewal,
        )
        .await?;

        let stale_renewal = insert_subscription(
            &database.pool,
            account,
            subscriber_id,
            "stale_renewal",
            SubscriptionStatus::Active,
        )
        .await?;
        let stale_renewal_attempt = insert_renewal(
            &database.pool,
            &stale_renewal,
            Utc::now()
                - Duration::seconds(
                    LocalAttemptPolicy::for_kind(PaymentAttemptKind::SubscriptionRenewal)
                        .stale_after_seconds()
                        + 1,
                ),
        )
        .await?;
        let mut transaction = database.pool.begin().await?;
        let outcome =
            cancel_subscription_in_transaction(&mut transaction, &stale_renewal.command()).await?;
        transaction.commit().await?;
        if !matches!(outcome, CancelSubscriptionOutcome::Canceled { .. }) {
            return Err(io::Error::other("stale renewal still blocked cancellation").into());
        }
        let stale_renewal_status: String =
            sqlx::query_scalar("SELECT status FROM billing_payment_attempts WHERE id = $1")
                .bind(stale_renewal_attempt)
                .fetch_one(&database.pool)
                .await?;
        if stale_renewal_status != "failed" {
            return Err(io::Error::other("stale renewal was not failed atomically").into());
        }

        let past_due = insert_subscription(
            &database.pool,
            account,
            subscriber_id,
            "past_due",
            SubscriptionStatus::PastDue,
        )
        .await?;
        insert_failed_renewal(&database.pool, &past_due).await?;
        let mut transaction = database.pool.begin().await?;
        let outcome =
            cancel_subscription_in_transaction(&mut transaction, &past_due.command()).await?;
        transaction.commit().await?;
        if !matches!(outcome, CancelSubscriptionOutcome::Canceled { .. }) {
            return Err(io::Error::other("past-due cancellation was not applied").into());
        }
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    database.cleanup().await?;
    result
}

#[tokio::test]
async fn caller_rollback_restores_cancellation() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("sr_cancel_rb").await?;
    let result = async {
        let account = create_gateway_account(&database.pool, "nmi").await?;
        let fixture = insert_subscription(
            &database.pool,
            account,
            Uuid::now_v7(),
            "rollback",
            SubscriptionStatus::Active,
        )
        .await?;
        let mut transaction = database.pool.begin().await?;
        let outcome =
            cancel_subscription_in_transaction(&mut transaction, &fixture.command()).await?;
        if !matches!(outcome, CancelSubscriptionOutcome::Canceled { .. }) {
            return Err(io::Error::other("rollback fixture was not canceled").into());
        }
        transaction.rollback().await?;
        let status: String =
            sqlx::query_scalar("SELECT status FROM billing_subscriptions WHERE id = $1")
                .bind(fixture.subscription_id)
                .fetch_one(&database.pool)
                .await?;
        if status != "active" {
            return Err(io::Error::other("caller rollback did not restore subscription").into());
        }
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    database.cleanup().await?;
    result
}

#[tokio::test]
async fn contended_stale_charge_remains_a_cancellation_blocker() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("sr_cn_lock_chg").await?;
    let result = async {
        let account = create_gateway_account(&database.pool, "nmi").await?;
        let fixture = insert_subscription(
            &database.pool,
            account,
            Uuid::now_v7(),
            "locked_stale_renewal",
            SubscriptionStatus::Active,
        )
        .await?;
        let attempt_id = insert_renewal(
            &database.pool,
            &fixture,
            Utc::now()
                - Duration::seconds(
                    LocalAttemptPolicy::for_kind(PaymentAttemptKind::SubscriptionRenewal)
                        .stale_after_seconds()
                        + 1,
                ),
        )
        .await?;

        let mut blocker = database.pool.begin().await?;
        sqlx::query("SELECT id FROM billing_payment_attempts WHERE id = $1 FOR UPDATE")
            .bind(attempt_id)
            .execute(&mut *blocker)
            .await?;

        let mut cancellation = database.pool.begin().await?;
        let outcome =
            cancel_subscription_in_transaction(&mut cancellation, &fixture.command()).await?;
        cancellation.commit().await?;
        if outcome != CancelSubscriptionOutcome::BlockedByRenewal {
            return Err(io::Error::other(format!(
                "contended stale charge was not preserved as a blocker: {outcome:?}"
            ))
            .into());
        }
        let locked_status: String =
            sqlx::query_scalar("SELECT status FROM billing_payment_attempts WHERE id = $1")
                .bind(attempt_id)
                .fetch_one(&database.pool)
                .await?;
        if locked_status != "pending" {
            return Err(io::Error::other("contended stale charge was mutated").into());
        }

        blocker.rollback().await?;
        let mut cancellation = database.pool.begin().await?;
        let outcome =
            cancel_subscription_in_transaction(&mut cancellation, &fixture.command()).await?;
        cancellation.commit().await?;
        if !matches!(outcome, CancelSubscriptionOutcome::Canceled { .. }) {
            return Err(
                io::Error::other("released stale charge still blocked cancellation").into(),
            );
        }
        let released_status: String =
            sqlx::query_scalar("SELECT status FROM billing_payment_attempts WHERE id = $1")
                .bind(attempt_id)
                .fetch_one(&database.pool)
                .await?;
        if released_status != "failed" {
            return Err(io::Error::other("released stale charge was not cleaned up").into());
        }
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    database.cleanup().await?;
    result
}

async fn assert_outcome(
    pool: &sqlx::PgPool,
    command: &CancelSubscription,
    expected: CancelSubscriptionOutcome,
) -> Result<(), Box<dyn Error>> {
    let mut transaction = pool.begin().await?;
    let actual = cancel_subscription_in_transaction(&mut transaction, command).await?;
    transaction.commit().await?;
    if actual != expected {
        return Err(
            io::Error::other(format!("unexpected cancellation outcome: {actual:?}")).into(),
        );
    }
    Ok(())
}

async fn insert_subscription(
    pool: &sqlx::PgPool,
    account: GatewayAccountFixture,
    subscriber_id: Uuid,
    plan_key: &str,
    status: SubscriptionStatus,
) -> Result<SubscriptionFixture, Box<dyn Error>> {
    let payment_method_id = Uuid::now_v7();
    let subscription_id = Uuid::now_v7();
    let suffix = subscription_id.simple();
    let initial_transaction_id = format!("txn_{suffix}");
    sqlx::query(
        r#"
            INSERT INTO billing_payment_methods (
                id, billing_scope_id, subscriber_id, gateway_account_id,
                gateway_payment_method_reference, status
            ) VALUES ($1, $2, $3, $4, $5, 'active')
            "#,
    )
    .bind(payment_method_id)
    .bind(account.billing_scope_id)
    .bind(subscriber_id)
    .bind(account.gateway_account_id)
    .bind(format!("vault_{suffix}"))
    .execute(pool)
    .await?;
    let period_start = Utc::now() - Duration::days(1);
    let period_end = period_start + Duration::days(30);
    sqlx::query(
        r#"
            INSERT INTO billing_subscriptions (
                id, billing_scope_id, subscriber_id, plan_key, status,
                gateway_account_id, payment_method_id, amount_cents, currency,
                current_period_start_at, current_period_end_at, next_renewal_at,
                initial_transaction_id, phase, recurring_period_kind,
                recurring_period_count, dunning_retry_delays_seconds,
                dunning_exhaustion, past_due_access, next_payment_attempt_at
            ) VALUES (
                $1, $2, $3, $4, $5, $6, $7, 5900, 'USD', $8, $9, $9, $10,
                'recurring', 'calendar_months', 1, ARRAY[]::bigint[],
                'remain_past_due', 'suspend_immediately', $9
            )
            "#,
    )
    .bind(subscription_id)
    .bind(account.billing_scope_id)
    .bind(subscriber_id)
    .bind(plan_key)
    .bind(status.as_str())
    .bind(account.gateway_account_id)
    .bind(payment_method_id)
    .bind(period_start)
    .bind(period_end)
    .bind(&initial_transaction_id)
    .execute(pool)
    .await?;
    Ok(SubscriptionFixture {
        account,
        subscriber_id,
        plan_key: PlanKey::new(plan_key)?,
        subscription_id,
        payment_method_id,
        period_end,
        initial_transaction_id,
    })
}

async fn insert_payment_method_update(
    pool: &sqlx::PgPool,
    fixture: &SubscriptionFixture,
    created_at: DateTime<Utc>,
) -> Result<Uuid, sqlx::Error> {
    let attempt_id = Uuid::now_v7();
    sqlx::query(
        r#"
            INSERT INTO billing_payment_attempts (
                id, billing_scope_id, subscriber_id, plan_key, subscription_id,
                payment_method_id, attempt_kind, status, idempotency_key,
                request_fingerprint, amount_cents, currency, gateway_account_id,
                gateway_configuration_id, gateway_order_id,
                payment_method_update_expected_payment_method_id,
                payment_method_update_expected_initial_transaction_id,
                created_at, updated_at
            ) VALUES (
                $1, $2, $3, $4, $5, $6,
                'subscription_payment_method_update', 'pending', $7, $8, 0, 'USD',
                $9, $10, $11, $6, $12, $13, $13
            )
            "#,
    )
    .bind(attempt_id)
    .bind(fixture.account.billing_scope_id)
    .bind(fixture.subscriber_id)
    .bind(fixture.plan_key.as_str())
    .bind(fixture.subscription_id)
    .bind(fixture.payment_method_id)
    .bind(format!("idem_{attempt_id}"))
    .bind(format!("fingerprint_{attempt_id}"))
    .bind(fixture.account.gateway_account_id)
    .bind(fixture.account.gateway_configuration_id)
    .bind(format!("order_{attempt_id}"))
    .bind(&fixture.initial_transaction_id)
    .bind(created_at)
    .execute(pool)
    .await?;
    Ok(attempt_id)
}

async fn insert_renewal(
    pool: &sqlx::PgPool,
    fixture: &SubscriptionFixture,
    created_at: DateTime<Utc>,
) -> Result<Uuid, sqlx::Error> {
    let attempt_id = Uuid::now_v7();
    sqlx::query(
        r#"
            INSERT INTO billing_payment_attempts (
                id, billing_scope_id, subscriber_id, plan_key, subscription_id,
                payment_method_id, attempt_kind, status, idempotency_key,
                request_fingerprint, amount_cents, currency, billing_period_start_at,
                billing_period_end_at, gateway_account_id, gateway_configuration_id,
                gateway_order_id, subscription_expected_payment_method_id,
                subscription_expected_initial_transaction_id, subscription_expected_status,
                created_at, updated_at
            ) VALUES (
                $1, $2, $3, $4, $5, $6, 'subscription_renewal', 'pending',
                $7, $8, 5900, 'USD', $9, $10, $11, $12, $13, $6, $14, 'active',
                $15, $15
            )
            "#,
    )
    .bind(attempt_id)
    .bind(fixture.account.billing_scope_id)
    .bind(fixture.subscriber_id)
    .bind(fixture.plan_key.as_str())
    .bind(fixture.subscription_id)
    .bind(fixture.payment_method_id)
    .bind(format!("idem_{attempt_id}"))
    .bind(format!("fingerprint_{attempt_id}"))
    .bind(fixture.period_end)
    .bind(fixture.period_end + Duration::days(30))
    .bind(fixture.account.gateway_account_id)
    .bind(fixture.account.gateway_configuration_id)
    .bind(format!("order_{attempt_id}"))
    .bind(&fixture.initial_transaction_id)
    .bind(created_at)
    .execute(pool)
    .await?;
    Ok(attempt_id)
}

async fn insert_failed_renewal(
    pool: &sqlx::PgPool,
    fixture: &SubscriptionFixture,
) -> Result<(), sqlx::Error> {
    let attempt_id = Uuid::now_v7();
    sqlx::query(
        r#"
            INSERT INTO billing_payment_attempts (
                id, billing_scope_id, subscriber_id, plan_key, subscription_id,
                payment_method_id, attempt_kind, status, idempotency_key,
                request_fingerprint, amount_cents, currency, billing_period_start_at,
                billing_period_end_at, gateway_account_id, gateway_configuration_id,
                gateway_order_id, subscription_expected_payment_method_id,
                subscription_expected_initial_transaction_id, subscription_expected_status,
                submitted_at, resolved_at
            ) VALUES (
                $1, $2, $3, $4, $5, $6, 'subscription_renewal', 'declined',
                $7, $8, 5900, 'USD', $9, $10, $11, $12, $13, $6, $14, 'active',
                clock_timestamp() - interval '1 minute',
                clock_timestamp() - interval '1 minute'
            )
            "#,
    )
    .bind(attempt_id)
    .bind(fixture.account.billing_scope_id)
    .bind(fixture.subscriber_id)
    .bind(fixture.plan_key.as_str())
    .bind(fixture.subscription_id)
    .bind(fixture.payment_method_id)
    .bind(format!("idem_{attempt_id}"))
    .bind(format!("fingerprint_{attempt_id}"))
    .bind(fixture.period_end)
    .bind(fixture.period_end + Duration::days(30))
    .bind(fixture.account.gateway_account_id)
    .bind(fixture.account.gateway_configuration_id)
    .bind(format!("order_{attempt_id}"))
    .bind(&fixture.initial_transaction_id)
    .execute(pool)
    .await?;
    Ok(())
}

async fn payment_method_snapshot(
    pool: &sqlx::PgPool,
    payment_method_id: Uuid,
) -> Result<String, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT to_jsonb(method)::text FROM billing_payment_methods method WHERE id = $1",
    )
    .bind(payment_method_id)
    .fetch_one(pool)
    .await
}
