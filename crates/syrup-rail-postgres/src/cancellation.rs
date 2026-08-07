use chrono::{DateTime, Utc};
use sqlx::{Postgres, Row, Transaction, postgres::PgRow};
use syrup_rail::{
    BillingEvent, BillingPeriod, CancelSubscription, CancelSubscriptionOutcome, ChargeAmount,
    CurrencyCode, PaymentMethodId, PlanKey, Subscription, SubscriptionId, SubscriptionStatus,
};
use thiserror::Error;
use uuid::Uuid;

const BILLING_ROW_LOCK_TIMEOUT: &str = "250ms";
const CURRENT_SUBSCRIPTION_LOCK_MAX_ATTEMPTS: usize = 2;
const PAYMENT_METHOD_UPDATE_UNSUBMITTED_STALE_AFTER_SECONDS: i64 = 3 * 60;
const UNSUBMITTED_PAYMENT_METHOD_UPDATE_FAILED_RESPONSE_TEXT: &str =
    "Payment method update was abandoned before gateway submission.";
const INVALID_SUBSCRIPTION_STATE: &str = "canonical subscription state is invalid";
const UNSTABLE_CURRENT_SUBSCRIPTION: &str =
    "current subscription ranking did not stabilize while acquiring the row lock";

#[derive(Debug, Error)]
pub enum SubscriptionCancellationError {
    #[error("subscription cancellation failed")]
    Sql(#[from] sqlx::Error),
    #[error("{0}")]
    InvalidState(&'static str),
}

/// Cancels one exact scope/subscriber/plan subscription inside the caller's transaction.
///
/// The caller must acquire any host recipient lock before invoking this operation and append the
/// returned event before committing. Cancellation never changes the stored payment method.
pub async fn cancel_subscription_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    command: &CancelSubscription,
) -> Result<CancelSubscriptionOutcome, SubscriptionCancellationError> {
    set_lock_timeout(transaction).await?;
    lock_subscription_aggregate(
        transaction,
        command.subscriber_id().as_uuid(),
        command.plan_key(),
    )
    .await?;

    let Some(subscription) = current_subscription(transaction, command).await? else {
        return Ok(CancelSubscriptionOutcome::NotFound);
    };
    match subscription.status() {
        SubscriptionStatus::Canceled => {
            Ok(CancelSubscriptionOutcome::AlreadyCanceled(subscription))
        }
        SubscriptionStatus::PastDue => Ok(CancelSubscriptionOutcome::BlockedByPastDue),
        SubscriptionStatus::Active => {
            if has_blocking_renewal(transaction, &subscription).await? {
                return Ok(CancelSubscriptionOutcome::BlockedByRenewal);
            }
            expire_stale_payment_method_updates(transaction, subscription.id()).await?;
            if has_blocking_payment_method_update(transaction, subscription.id()).await? {
                return Ok(CancelSubscriptionOutcome::BlockedByPaymentMethodUpdate);
            }

            let subscription = cancel_active_subscription(transaction, command, subscription.id())
                .await?
                .ok_or(SubscriptionCancellationError::InvalidState(
                    INVALID_SUBSCRIPTION_STATE,
                ))?;
            let event = BillingEvent::SubscriptionCanceled {
                subscription_id: subscription.id(),
                plan_key: subscription.plan_key().clone(),
                access_ends_at: *subscription.current_period().end_at(),
            };
            Ok(CancelSubscriptionOutcome::Canceled {
                subscription,
                event,
            })
        }
    }
}

async fn set_lock_timeout(transaction: &mut Transaction<'_, Postgres>) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT set_config('lock_timeout', $1, true)")
        .bind(BILLING_ROW_LOCK_TIMEOUT)
        .execute(&mut **transaction)
        .await?;
    Ok(())
}

async fn lock_subscription_aggregate(
    transaction: &mut Transaction<'_, Postgres>,
    subscriber_id: &Uuid,
    plan_key: &PlanKey,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::uuid::text || ':' || $2, 0))")
        .bind(subscriber_id)
        .bind(plan_key.as_str())
        .execute(&mut **transaction)
        .await?;
    Ok(())
}

async fn current_subscription(
    transaction: &mut Transaction<'_, Postgres>,
    command: &CancelSubscription,
) -> Result<Option<Subscription>, SubscriptionCancellationError> {
    for attempt in 0..CURRENT_SUBSCRIPTION_LOCK_MAX_ATTEMPTS {
        let Some(candidate_id) = current_subscription_id(transaction, command).await? else {
            return Ok(None);
        };
        let row = sqlx::query(
            r#"
            SELECT id, plan_key, status, payment_method_id, amount_cents, currency,
                current_period_start_at, current_period_end_at, next_renewal_at
            FROM billing_subscriptions
            WHERE id = $1
                AND billing_scope_id = $2
                AND subscriber_id = $3
                AND plan_key = $4
            FOR NO KEY UPDATE
            "#,
        )
        .bind(candidate_id)
        .bind(command.billing_scope_id().as_uuid())
        .bind(command.subscriber_id().as_uuid())
        .bind(command.plan_key().as_str())
        .fetch_optional(&mut **transaction)
        .await?;
        let Some(row) = row else {
            require_stabilization_retry(attempt)?;
            continue;
        };
        match current_subscription_id(transaction, command).await? {
            Some(current_id) if current_id == candidate_id => {
                return subscription_from_row(&row).map(Some);
            }
            Some(_) => require_stabilization_retry(attempt)?,
            None => return Ok(None),
        }
    }
    Err(SubscriptionCancellationError::InvalidState(
        UNSTABLE_CURRENT_SUBSCRIPTION,
    ))
}

fn require_stabilization_retry(attempt: usize) -> Result<(), SubscriptionCancellationError> {
    if attempt + 1 < CURRENT_SUBSCRIPTION_LOCK_MAX_ATTEMPTS {
        Ok(())
    } else {
        Err(SubscriptionCancellationError::InvalidState(
            UNSTABLE_CURRENT_SUBSCRIPTION,
        ))
    }
}

async fn current_subscription_id(
    transaction: &mut Transaction<'_, Postgres>,
    command: &CancelSubscription,
) -> Result<Option<Uuid>, sqlx::Error> {
    sqlx::query_scalar(
        r#"
        SELECT id
        FROM billing_current_subscriptions
        WHERE billing_scope_id = $1
            AND subscriber_id = $2
            AND plan_key = $3
        ORDER BY current_subscription_rank, updated_at DESC, id DESC
        LIMIT 1
        "#,
    )
    .bind(command.billing_scope_id().as_uuid())
    .bind(command.subscriber_id().as_uuid())
    .bind(command.plan_key().as_str())
    .fetch_optional(&mut **transaction)
    .await
}

async fn has_blocking_renewal(
    transaction: &mut Transaction<'_, Postgres>,
    subscription: &Subscription,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM billing_payment_attempts
            WHERE subscription_id = $1
                AND attempt_kind IN ('subscription_renewal', 'subscription_recovery')
                AND billing_period_start_at = $2
                AND status IN ('pending', 'unknown', 'review_required', 'approved')
        )
        "#,
    )
    .bind(subscription.id().as_uuid())
    .bind(subscription.next_renewal_at())
    .fetch_one(&mut **transaction)
    .await
}

async fn expire_stale_payment_method_updates(
    transaction: &mut Transaction<'_, Postgres>,
    subscription_id: SubscriptionId,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        WITH stale_attempts AS (
            SELECT id
            FROM billing_payment_attempts
            WHERE subscription_id = $1
                AND attempt_kind = 'subscription_payment_method_update'
                AND status = 'pending'
                AND submitted_at IS NULL
                AND created_at <= now() - ($2::bigint * interval '1 second')
            FOR UPDATE SKIP LOCKED
        )
        UPDATE billing_payment_attempts attempts
        SET status = 'failed',
            gateway_response_text = COALESCE(gateway_response_text, $3),
            gateway_condition = COALESCE(gateway_condition, 'failed'),
            resolved_at = now(),
            updated_at = now()
        FROM stale_attempts
        WHERE attempts.id = stale_attempts.id
        "#,
    )
    .bind(subscription_id.as_uuid())
    .bind(PAYMENT_METHOD_UPDATE_UNSUBMITTED_STALE_AFTER_SECONDS)
    .bind(UNSUBMITTED_PAYMENT_METHOD_UPDATE_FAILED_RESPONSE_TEXT)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn has_blocking_payment_method_update(
    transaction: &mut Transaction<'_, Postgres>,
    subscription_id: SubscriptionId,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM billing_payment_attempts
            WHERE subscription_id = $1
                AND attempt_kind = 'subscription_payment_method_update'
                AND status IN ('pending', 'unknown', 'review_required')
        )
        "#,
    )
    .bind(subscription_id.as_uuid())
    .fetch_one(&mut **transaction)
    .await
}

async fn cancel_active_subscription(
    transaction: &mut Transaction<'_, Postgres>,
    command: &CancelSubscription,
    subscription_id: SubscriptionId,
) -> Result<Option<Subscription>, SubscriptionCancellationError> {
    let row = sqlx::query(
        r#"
        UPDATE billing_subscriptions
        SET status = 'canceled',
            canceled_at = now(),
            updated_at = now()
        WHERE id = $1
            AND billing_scope_id = $2
            AND subscriber_id = $3
            AND plan_key = $4
            AND status = 'active'
        RETURNING id, plan_key, status, payment_method_id, amount_cents, currency,
            current_period_start_at, current_period_end_at, next_renewal_at
        "#,
    )
    .bind(subscription_id.as_uuid())
    .bind(command.billing_scope_id().as_uuid())
    .bind(command.subscriber_id().as_uuid())
    .bind(command.plan_key().as_str())
    .fetch_optional(&mut **transaction)
    .await?;
    row.as_ref().map(subscription_from_row).transpose()
}

fn subscription_from_row(row: &PgRow) -> Result<Subscription, SubscriptionCancellationError> {
    let plan_key = PlanKey::new(row.try_get::<String, _>("plan_key")?)
        .map_err(|_| SubscriptionCancellationError::InvalidState(INVALID_SUBSCRIPTION_STATE))?;
    let status = row
        .try_get::<String, _>("status")?
        .parse::<SubscriptionStatus>()
        .map_err(|_| SubscriptionCancellationError::InvalidState(INVALID_SUBSCRIPTION_STATE))?;
    let currency = CurrencyCode::new(&row.try_get::<String, _>("currency")?)
        .map_err(|_| SubscriptionCancellationError::InvalidState(INVALID_SUBSCRIPTION_STATE))?;
    let charge = ChargeAmount::new(row.try_get("amount_cents")?, currency)
        .map_err(|_| SubscriptionCancellationError::InvalidState(INVALID_SUBSCRIPTION_STATE))?;
    let period = BillingPeriod::new(
        row.try_get::<DateTime<Utc>, _>("current_period_start_at")?,
        row.try_get::<DateTime<Utc>, _>("current_period_end_at")?,
    )
    .map_err(|_| SubscriptionCancellationError::InvalidState(INVALID_SUBSCRIPTION_STATE))?;
    Ok(Subscription::new(
        SubscriptionId::new(row.try_get("id")?),
        plan_key,
        status,
        PaymentMethodId::new(row.try_get("payment_method_id")?),
        charge,
        period,
        row.try_get("next_renewal_at")?,
    ))
}

#[cfg(test)]
mod tests {
    use std::{error::Error, io};

    use chrono::Duration;
    use syrup_rail::{
        BillingEventKey, BillingScopeId, CancelSubscription, CancelSubscriptionOutcome, PlanKey,
        SubscriberId, SubscriptionStatus,
    };

    use super::*;
    use crate::test_support::{GatewayAccountFixture, TestDatabase, create_gateway_account};

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
            let outcome =
                cancel_subscription_in_transaction(&mut transaction, &wrong_scope).await?;
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
                    - Duration::seconds(PAYMENT_METHOD_UPDATE_UNSUBMITTED_STALE_AFTER_SECONDS + 1),
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

            let renewal = insert_subscription(
                &database.pool,
                account,
                subscriber_id,
                "renewal",
                SubscriptionStatus::Active,
            )
            .await?;
            insert_renewal(&database.pool, &renewal).await?;
            assert_outcome(
                &database.pool,
                &renewal.command(),
                CancelSubscriptionOutcome::BlockedByRenewal,
            )
            .await?;

            let past_due = insert_subscription(
                &database.pool,
                account,
                subscriber_id,
                "past_due",
                SubscriptionStatus::PastDue,
            )
            .await?;
            assert_outcome(
                &database.pool,
                &past_due.command(),
                CancelSubscriptionOutcome::BlockedByPastDue,
            )
            .await?;
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
                return Err(
                    io::Error::other("caller rollback did not restore subscription").into(),
                );
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
                initial_transaction_id
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, 5900, 'USD', $8, $9, $9, $10)
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
                subscription_expected_initial_transaction_id, subscription_expected_status
            ) VALUES (
                $1, $2, $3, $4, $5, $6, 'subscription_renewal', 'pending',
                $7, $8, 5900, 'USD', $9, $10, $11, $12, $13, $6, $14, 'active'
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
}
