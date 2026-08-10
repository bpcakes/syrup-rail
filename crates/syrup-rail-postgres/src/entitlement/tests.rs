use std::{error::Error, io};

use chrono::{DateTime, TimeZone, Utc};
use sqlx::PgPool;
use syrup_rail::{
    BillingScopeId, Entitlement, EntitlementGuard, EntitlementQuery, PlanKey, SubscriberId,
};
use uuid::Uuid;

use super::{
    EntitlementGuardError, EntitlementQueryError, GuardAccess, classify_guard_access, entitlement,
    require_entitlement_for_update,
};
use crate::test_support::{TestDatabase, create_gateway_account};

fn guard_subscription(
    status: &str,
    period_end_at: DateTime<Utc>,
) -> (String, DateTime<Utc>, String, Option<DateTime<Utc>>) {
    (
        status.to_owned(),
        period_end_at,
        "suspend_immediately".to_owned(),
        None,
    )
}

#[test]
fn protected_write_policy_covers_paid_grant_boundaries_and_invalid_overlap() {
    let at = Utc.with_ymd_and_hms(2026, 8, 7, 18, 0, 0).unwrap();
    let before = at - chrono::Duration::hours(1);
    let after = at + chrono::Duration::hours(1);

    assert_eq!(classify_guard_access(&[], &[], at), GuardAccess::Missing);
    assert_eq!(
        classify_guard_access(&[guard_subscription("active", after)], &[], at),
        GuardAccess::Paid
    );
    assert_eq!(
        classify_guard_access(&[guard_subscription("past_due", after)], &[], at),
        GuardAccess::PastDue
    );
    assert_eq!(
        classify_guard_access(&[guard_subscription("canceled", after)], &[], at),
        GuardAccess::PaidThroughCancellation
    );
    assert_eq!(
        classify_guard_access(&[guard_subscription("canceled", at)], &[], at),
        GuardAccess::Missing
    );
    assert_eq!(
        classify_guard_access(&[], &[(before, after, None)], at),
        GuardAccess::Granted
    );
    assert_eq!(
        classify_guard_access(&[], &[(before, at, None)], at),
        GuardAccess::Missing
    );
    assert_eq!(
        classify_guard_access(&[], &[(before, after, Some(at))], at),
        GuardAccess::Missing
    );
    assert_eq!(
        classify_guard_access(
            &[guard_subscription("active", after)],
            &[(before, after, None)],
            at,
        ),
        GuardAccess::Invalid
    );
    assert_eq!(
        classify_guard_access(
            &[
                guard_subscription("canceled", after),
                guard_subscription("canceled", after),
            ],
            &[],
            at,
        ),
        GuardAccess::Invalid
    );
    assert_eq!(
        classify_guard_access(&[], &[(before, after, None), (before, after, None)], at,),
        GuardAccess::Invalid
    );
}

#[tokio::test]
async fn protected_write_guard_uses_database_state_and_restores_the_host_timeout()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("sr_guard_state").await?;
    let result = async {
            let scope = Uuid::now_v7();
            let subscriber = Uuid::now_v7();
            let entitlement_guard = guard(scope, subscriber)?;

            let mut transaction = database.pool.begin().await?;
            sqlx::query("SELECT set_config('lock_timeout', '3s', true)")
                .execute(&mut *transaction)
                .await?;
            if !matches!(
                require_entitlement_for_update(&mut transaction, &entitlement_guard).await,
                Err(EntitlementGuardError::Required)
            ) {
                return Err(io::Error::other("missing entitlement was not rejected").into());
            }
            let restored: String = sqlx::query_scalar("SELECT current_setting('lock_timeout')")
                .fetch_one(&mut *transaction)
                .await?;
            if restored != "3s" {
                return Err(io::Error::other("guard did not restore the host lock timeout").into());
            }
            transaction.rollback().await?;

            let first_grant = insert_grant(&database.pool, scope, subscriber).await?;
            require_guard(&database.pool, &entitlement_guard).await?;
            let second_grant = insert_grant(&database.pool, scope, subscriber).await?;
            if !matches!(
                guard_result(&database.pool, &entitlement_guard).await?,
                Err(EntitlementGuardError::InvalidState(_))
            ) {
                return Err(io::Error::other("duplicate active grants did not fail closed").into());
            }
            sqlx::query("DELETE FROM billing_subscription_grants WHERE id IN ($1, $2)")
                .bind(first_grant)
                .bind(second_grant)
                .execute(&database.pool)
                .await?;

            let (paid_scope, paid_subscriber, subscription) =
                insert_paid_subscription(&database.pool, "active").await?;
            let paid_guard = guard(paid_scope, paid_subscriber)?;
            require_guard(&database.pool, &paid_guard).await?;
            sqlx::query("UPDATE billing_subscriptions SET status = 'past_due' WHERE id = $1")
                .bind(subscription)
                .execute(&database.pool)
                .await?;
            if !matches!(
                guard_result(&database.pool, &paid_guard).await?,
                Err(EntitlementGuardError::PastDue)
            ) {
                return Err(io::Error::other("past-due entitlement lost its distinct result").into());
            }
            sqlx::query(
                "UPDATE billing_subscriptions SET status = 'canceled', canceled_at = clock_timestamp(), next_payment_attempt_at = NULL WHERE id = $1",
            )
            .bind(subscription)
            .execute(&database.pool)
            .await?;
            require_guard(&database.pool, &paid_guard).await?;
            sqlx::query(
                r#"
                UPDATE billing_subscriptions
                SET current_period_end_at = boundary.at,
                    next_renewal_at = boundary.at,
                    updated_at = boundary.at
                FROM (SELECT clock_timestamp() AS at) boundary
                WHERE id = $1
                "#,
            )
            .bind(subscription)
            .execute(&database.pool)
            .await?;
            if !matches!(
                guard_result(&database.pool, &paid_guard).await?,
                Err(EntitlementGuardError::Required)
            ) {
                return Err(io::Error::other("paid-through boundary did not end access").into());
            }
            Ok::<_, Box<dyn Error>>(())
        }
        .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn protected_write_guard_holds_the_aggregate_and_entitlement_rows_until_caller_end()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("sr_guard_locks").await?;
    let result = async {
        let scope = Uuid::now_v7();
        let subscriber = Uuid::now_v7();
        let grant_id = insert_grant(&database.pool, scope, subscriber).await?;
        let guard = guard(scope, subscriber)?;
        let mut holder = database.pool.begin().await?;
        require_entitlement_for_update(&mut holder, &guard).await?;

        let mut aggregate_contender = database.pool.begin().await?;
        sqlx::query("SET LOCAL lock_timeout = '100ms'")
            .execute(&mut *aggregate_contender)
            .await?;
        let aggregate_error = sqlx::query(
            "SELECT pg_advisory_xact_lock(hashtextextended($1::uuid::text || ':' || $2, 0))",
        )
        .bind(subscriber)
        .bind("base_subscription")
        .execute(&mut *aggregate_contender)
        .await
        .expect_err("guard must hold the shared subscription aggregate domain");
        assert_lock_timeout(&aggregate_error)?;
        aggregate_contender.rollback().await?;

        let mut row_contender = database.pool.begin().await?;
        sqlx::query("SET LOCAL lock_timeout = '100ms'")
            .execute(&mut *row_contender)
            .await?;
        let row_error =
            sqlx::query("SELECT id FROM billing_subscription_grants WHERE id = $1 FOR UPDATE")
                .bind(grant_id)
                .execute(&mut *row_contender)
                .await
                .expect_err("guard must hold its accepted grant row through the host mutation");
        assert_lock_timeout(&row_error)?;
        row_contender.rollback().await?;
        holder.rollback().await?;
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn entitlement_is_exact_lossless_and_rejects_overlapping_owners() -> Result<(), Box<dyn Error>>
{
    let database = TestDatabase::start("sr_entitle_v1").await?;
    let result = async {
        let scope = Uuid::now_v7();
        let subscriber = Uuid::now_v7();
        let plan = PlanKey::new("base_subscription")?;
        let query = EntitlementQuery::new(
            BillingScopeId::new(scope),
            SubscriberId::new(subscriber),
            plan,
        );

        if !matches!(
            entitlement(&database.pool, &query).await?,
            Entitlement::Missing {
                next_action: syrup_rail::MissingSubscriptionAction::StartSubscription,
                saved_discount: None,
            }
        ) {
            return Err(io::Error::other("empty aggregate did not require enrollment").into());
        }

        let discount_code = Uuid::now_v7();
        let discount_claim = Uuid::now_v7();
        sqlx::query(
            r#"
                INSERT INTO billing_subscription_discount_codes (
                    id, billing_scope_id, plan_key, code_normalized,
                    display_code, label, status, discount_kind,
                    amount_off_cents, currency, duration
                ) VALUES (
                    $1, $2, 'base_subscription', 'WELCOME10', 'WELCOME10',
                    'Welcome discount', 'active', 'amount_off', 10, 'USD',
                    'indefinite'
                )
                "#,
        )
        .bind(discount_code)
        .bind(scope)
        .execute(&database.pool)
        .await?;
        sqlx::query(
            r#"
                INSERT INTO billing_subscription_discount_claims (
                    id, billing_scope_id, subscriber_id, plan_key,
                    discount_code_id, code_snapshot, label_snapshot,
                    discount_kind, amount_off_cents, currency, duration,
                    base_amount_cents, discounted_amount_cents, status
                ) VALUES (
                    $1, $2, $3, 'base_subscription', $4, 'WELCOME10',
                    'Welcome discount', 'amount_off', 10, 'USD',
                    'indefinite', 100, 90, 'saved'
                )
                "#,
        )
        .bind(discount_claim)
        .bind(scope)
        .bind(subscriber)
        .bind(discount_code)
        .execute(&database.pool)
        .await?;
        match entitlement(&database.pool, &query).await? {
            Entitlement::Missing {
                saved_discount: Some(discount),
                ..
            } if discount.snapshot().label() == Some("Welcome discount") => {}
            _ => {
                return Err(io::Error::other(
                    "saved discount snapshot was not projected losslessly",
                )
                .into());
            }
        }

        let grant_id = Uuid::now_v7();
        sqlx::query(
            r#"
                INSERT INTO billing_subscription_grants (
                    id, billing_scope_id, subscriber_id, plan_key, grant_kind,
                    reason, starts_at, ends_at, granted_by_actor_id
                ) VALUES (
                    $1, $2, $3, 'base_subscription', 'promotion', 'launch',
                    clock_timestamp() - interval '1 minute',
                    clock_timestamp() + interval '1 day', $4
                )
                "#,
        )
        .bind(grant_id)
        .bind(scope)
        .bind(subscriber)
        .bind(Uuid::now_v7())
        .execute(&database.pool)
        .await?;
        if !matches!(
            entitlement(&database.pool, &query).await?,
            Entitlement::Granted { grant } if grant.id().into_uuid() == grant_id
        ) {
            return Err(io::Error::other("active grant did not own entitlement").into());
        }
        sqlx::query("DELETE FROM billing_subscription_grants WHERE id = $1")
            .bind(grant_id)
            .execute(&database.pool)
            .await?;

        let provider = "test_gateway";
        let account = Uuid::now_v7();
        let configuration = Uuid::now_v7();
        sqlx::query("INSERT INTO billing_gateway_provider_rate_limits (provider_key) VALUES ($1)")
            .bind(provider)
            .execute(&database.pool)
            .await?;
        sqlx::query(
            r#"
                INSERT INTO billing_gateway_accounts (
                    id, billing_scope_id, provider_key, gateway_configuration_id
                ) VALUES ($1, $2, $3, $4)
                "#,
        )
        .bind(account)
        .bind(scope)
        .bind(provider)
        .bind(configuration)
        .execute(&database.pool)
        .await?;
        let method = Uuid::now_v7();
        sqlx::query(
            r#"
                INSERT INTO billing_payment_methods (
                    id, billing_scope_id, subscriber_id, gateway_account_id,
                    gateway_payment_method_reference, status
                ) VALUES ($1, $2, $3, $4, $5, 'active')
                "#,
        )
        .bind(method)
        .bind(scope)
        .bind(subscriber)
        .bind(account)
        .bind(format!("vault_{}", method.simple()))
        .execute(&database.pool)
        .await?;
        let subscription = Uuid::now_v7();
        sqlx::query(
            r#"
                INSERT INTO billing_subscriptions (
                    id, billing_scope_id, subscriber_id, plan_key, status,
                    gateway_account_id, payment_method_id, amount_cents,
                    currency, current_period_start_at, current_period_end_at,
                    next_renewal_at, initial_transaction_id, phase,
                    recurring_period_kind, recurring_period_count,
                    dunning_retry_delays_seconds, dunning_exhaustion,
                    past_due_access, next_payment_attempt_at
                ) SELECT
                    $1, $2, $3, 'base_subscription', 'active', $4, $5, 100,
                    'USD', observed_at - interval '1 day',
                    observed_at + interval '1 day',
                    observed_at + interval '1 day', $6, 'recurring',
                    'calendar_months', 1, ARRAY[]::bigint[],
                    'remain_past_due', 'suspend_immediately',
                    observed_at + interval '1 day'
                FROM (SELECT clock_timestamp() AS observed_at) clock
                "#,
        )
        .bind(subscription)
        .bind(scope)
        .bind(subscriber)
        .bind(account)
        .bind(method)
        .bind(format!("txn_{}", subscription.simple()))
        .execute(&database.pool)
        .await?;
        sqlx::query(
            r#"
                INSERT INTO billing_subscription_discounts (
                    subscription_id, billing_scope_id, subscriber_id,
                    plan_key, code_snapshot, label_snapshot, discount_kind,
                    amount_off_cents, currency, duration, base_amount_cents,
                    discounted_amount_cents, periods_applied, status
                ) VALUES (
                    $1, $2, $3, 'base_subscription', 'WELCOME10',
                    'Welcome discount', 'amount_off', 10, 'USD', 'indefinite',
                    100, 90, 1, 'active'
                )
                "#,
        )
        .bind(subscription)
        .bind(scope)
        .bind(subscriber)
        .execute(&database.pool)
        .await?;
        match entitlement(&database.pool, &query).await? {
            Entitlement::PaidActive {
                subscription: paid,
                applied_discount: Some(discount),
            } if paid.id().into_uuid() == subscription
                && discount.snapshot().label() == Some("Welcome discount") => {}
            _ => {
                return Err(io::Error::other(
                    "paid entitlement or applied discount was not projected",
                )
                .into());
            }
        }

        let overlapping_grant = Uuid::now_v7();
        sqlx::query(
            r#"
                INSERT INTO billing_subscription_grants (
                    id, billing_scope_id, subscriber_id, plan_key, grant_kind,
                    reason, starts_at, ends_at, granted_by_actor_id
                ) VALUES (
                    $1, $2, $3, 'base_subscription', 'testing', 'overlap',
                    clock_timestamp() - interval '1 minute',
                    clock_timestamp() + interval '1 day', $4
                )
                "#,
        )
        .bind(overlapping_grant)
        .bind(scope)
        .bind(subscriber)
        .bind(Uuid::now_v7())
        .execute(&database.pool)
        .await?;
        if !matches!(
            entitlement(&database.pool, &query).await,
            Err(EntitlementQueryError::InvalidState(_))
        ) {
            return Err(io::Error::other("paid/grant overlap did not fail closed").into());
        }
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

fn guard(scope: Uuid, subscriber: Uuid) -> Result<EntitlementGuard, Box<dyn Error>> {
    Ok(EntitlementGuard::new(
        BillingScopeId::new(scope),
        SubscriberId::new(subscriber),
        PlanKey::new("base_subscription")?,
    ))
}

async fn guard_result(
    pool: &PgPool,
    guard: &EntitlementGuard,
) -> Result<Result<(), EntitlementGuardError>, sqlx::Error> {
    let mut transaction = pool.begin().await?;
    let result = require_entitlement_for_update(&mut transaction, guard).await;
    transaction.rollback().await?;
    Ok(result)
}

async fn require_guard(pool: &PgPool, guard: &EntitlementGuard) -> Result<(), Box<dyn Error>> {
    guard_result(pool, guard).await??;
    Ok(())
}

async fn insert_grant(pool: &PgPool, scope: Uuid, subscriber: Uuid) -> Result<Uuid, sqlx::Error> {
    let grant_id = Uuid::now_v7();
    sqlx::query(
        r#"
            INSERT INTO billing_subscription_grants (
                id, billing_scope_id, subscriber_id, plan_key, grant_kind,
                reason, starts_at, ends_at, granted_by_actor_id
            ) VALUES (
                $1, $2, $3, 'base_subscription', 'testing', 'guard fixture',
                clock_timestamp() - interval '1 minute',
                clock_timestamp() + interval '1 day', $4
            )
            "#,
    )
    .bind(grant_id)
    .bind(scope)
    .bind(subscriber)
    .bind(Uuid::now_v7())
    .execute(pool)
    .await?;
    Ok(grant_id)
}

async fn insert_paid_subscription(
    pool: &PgPool,
    status: &str,
) -> Result<(Uuid, Uuid, Uuid), sqlx::Error> {
    let account = create_gateway_account(pool, "guard_gateway").await?;
    let subscriber = Uuid::now_v7();
    let method = Uuid::now_v7();
    sqlx::query(
        r#"
            INSERT INTO billing_payment_methods (
                id, billing_scope_id, subscriber_id, gateway_account_id,
                gateway_payment_method_reference, status
            ) VALUES ($1, $2, $3, $4, $5, 'active')
            "#,
    )
    .bind(method)
    .bind(account.billing_scope_id)
    .bind(subscriber)
    .bind(account.gateway_account_id)
    .bind(format!("vault_{}", method.simple()))
    .execute(pool)
    .await?;
    let subscription = Uuid::now_v7();
    sqlx::query(
        r#"
            INSERT INTO billing_subscriptions (
                id, billing_scope_id, subscriber_id, plan_key, status,
                gateway_account_id, payment_method_id, amount_cents,
                currency, current_period_start_at, current_period_end_at,
                next_renewal_at, initial_transaction_id, canceled_at, phase,
                recurring_period_kind, recurring_period_count,
                dunning_retry_delays_seconds, dunning_exhaustion,
                past_due_access, next_payment_attempt_at
            ) SELECT
                $1, $2, $3, 'base_subscription', $4, $5, $6, 100, 'USD',
                observed_at - interval '1 day', observed_at + interval '1 day',
                observed_at + interval '1 day', $7,
                CASE WHEN $4 = 'canceled' THEN observed_at ELSE NULL END,
                'recurring', 'calendar_months', 1, ARRAY[]::bigint[],
                'remain_past_due', 'suspend_immediately',
                CASE WHEN $4 = 'canceled' THEN NULL
                    ELSE observed_at + interval '1 day' END
            FROM (SELECT clock_timestamp() AS observed_at) clock
            "#,
    )
    .bind(subscription)
    .bind(account.billing_scope_id)
    .bind(subscriber)
    .bind(status)
    .bind(account.gateway_account_id)
    .bind(method)
    .bind(format!("txn_{}", subscription.simple()))
    .execute(pool)
    .await?;
    Ok((account.billing_scope_id, subscriber, subscription))
}

fn assert_lock_timeout(error: &sqlx::Error) -> Result<(), io::Error> {
    let code = error
        .as_database_error()
        .and_then(|error| error.code())
        .map(|code| code.into_owned());
    if code.as_deref() == Some("55P03") {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "expected PostgreSQL lock timeout, got {error}"
        )))
    }
}
