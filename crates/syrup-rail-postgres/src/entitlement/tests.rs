use std::{error::Error, io, time::Duration};

use chrono::{DateTime, TimeZone, Utc};
use sqlx::{PgPool, Postgres, Transaction};
use syrup_rail::{
    BillingScopeId, Entitlement, EntitlementGuard, EntitlementQuery, PlanKey, SubscriberId,
};
use uuid::Uuid;

use super::{
    EntitlementGuardError, EntitlementQueryError, EntitlementWriteTransaction, GuardAccess,
    classify_guard_access, entitlement, require_entitlement_for_update,
    require_entitlement_for_update_with_lock_timeout,
};
use crate::test_support::{TestDatabase, create_gateway_account};

mod projection;
mod stale_attempts;

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

            create_host_guard_probe_table(&database.pool).await?;
            let probe_id = Uuid::now_v7();
            let mut transaction = EntitlementWriteTransaction::begin(&database.pool).await?;
            let transaction_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                .fetch_one(transaction.connection())
                .await?;
            sqlx::query("INSERT INTO host_entitlement_guard_probes (id) VALUES ($1)")
                .bind(probe_id)
                .execute(transaction.connection())
                .await?;
            let mut observer = database.pool.acquire().await?;
            if !matches!(
                require_entitlement_for_update(transaction, &entitlement_guard).await,
                Err(EntitlementGuardError::Required)
            ) {
                return Err(io::Error::other("missing entitlement was not rejected").into());
            }
            assert_backend_transaction_ended(&mut observer, transaction_pid).await?;
            assert_host_guard_probe_absent(&database.pool, probe_id).await?;
            drop(observer);

            let first_grant = insert_grant(&database.pool, scope, subscriber).await?;
            let mut transaction = EntitlementWriteTransaction::begin(&database.pool).await?;
            sqlx::query("SELECT set_config('lock_timeout', '3s', true)")
                .execute(transaction.connection())
                .await?;
            let mut transaction =
                require_entitlement_for_update(transaction, &entitlement_guard).await?;
            let restored: String = sqlx::query_scalar("SELECT current_setting('lock_timeout')")
                .fetch_one(transaction.connection())
                .await?;
            if restored != "3s" {
                return Err(io::Error::other("guard did not restore the host lock timeout").into());
            }
            transaction.rollback().await?;

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
async fn admitted_protected_write_commits_preparatory_and_protected_host_mutations()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("sr_guard_commit").await?;
    let result = async {
        let scope = Uuid::now_v7();
        let subscriber = Uuid::now_v7();
        let entitlement_guard = guard(scope, subscriber)?;
        insert_grant(&database.pool, scope, subscriber).await?;
        create_host_guard_probe_table(&database.pool).await?;
        let preparatory_probe_id = Uuid::now_v7();
        let protected_probe_id = Uuid::now_v7();

        let mut transaction = EntitlementWriteTransaction::begin(&database.pool).await?;
        sqlx::query("INSERT INTO host_entitlement_guard_probes (id) VALUES ($1)")
            .bind(preparatory_probe_id)
            .execute(transaction.connection())
            .await?;
        let mut transaction =
            require_entitlement_for_update(transaction, &entitlement_guard).await?;
        sqlx::query("INSERT INTO host_entitlement_guard_probes (id) VALUES ($1)")
            .bind(protected_probe_id)
            .execute(transaction.connection())
            .await?;

        assert_host_guard_probe_count(
            &database.pool,
            &[preparatory_probe_id, protected_probe_id],
            0,
        )
        .await?;
        transaction.commit().await?;
        assert_host_guard_probe_count(
            &database.pool,
            &[preparatory_probe_id, protected_probe_id],
            2,
        )
        .await?;
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn protected_write_guard_rolls_back_after_a_client_decode_error() -> Result<(), Box<dyn Error>>
{
    let database = TestDatabase::start("sr_guard_decode").await?;
    let result = async {
        let scope = Uuid::now_v7();
        let subscriber = Uuid::now_v7();
        let entitlement_guard = guard(scope, subscriber)?;
        create_host_guard_probe_table(&database.pool).await?;
        let probe_id = Uuid::now_v7();
        let mut transaction = EntitlementWriteTransaction::begin(&database.pool).await?;
        let transaction_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(transaction.connection())
            .await?;
        let mut observer = database.pool.acquire().await?;

        sqlx::query("INSERT INTO host_entitlement_guard_probes (id) VALUES ($1)")
            .bind(probe_id)
            .execute(transaction.connection())
            .await?;
        sqlx::query(
            r#"
            CREATE TEMPORARY TABLE billing_subscriptions (
                id uuid NOT NULL,
                billing_scope_id uuid NOT NULL,
                subscriber_id uuid NOT NULL,
                plan_key text NOT NULL,
                status text NOT NULL,
                current_period_end_at text NOT NULL,
                past_due_access text NOT NULL,
                next_payment_attempt_at timestamptz
            ) ON COMMIT DROP
            "#,
        )
        .execute(transaction.connection())
        .await?;
        sqlx::query(
            r#"
            INSERT INTO billing_subscriptions (
                id, billing_scope_id, subscriber_id, plan_key, status,
                current_period_end_at, past_due_access, next_payment_attempt_at
            ) VALUES ($1, $2, $3, 'base_subscription', 'active',
                'not-a-timestamp', 'suspend_immediately', NULL)
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(scope)
        .bind(subscriber)
        .execute(transaction.connection())
        .await?;

        let error = require_entitlement_for_update(transaction, &entitlement_guard)
            .await
            .expect_err("the incompatible temporary row must fail client-side decoding");
        if !matches!(
            error,
            EntitlementGuardError::Sql(sqlx::Error::ColumnDecode { .. })
        ) {
            return Err(io::Error::other(format!(
                "expected a client-side column decode error, got {error}"
            ))
            .into());
        }
        assert_backend_transaction_ended(&mut observer, transaction_pid).await?;
        assert_host_guard_probe_absent(&database.pool, probe_id).await?;
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn protected_write_guard_rolls_back_after_a_database_lock_timeout()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("sr_guard_timeout").await?;
    let result = async {
        let scope = Uuid::now_v7();
        let subscriber = Uuid::now_v7();
        let entitlement_guard = guard(scope, subscriber)?;
        create_host_guard_probe_table(&database.pool).await?;
        let probe_id = Uuid::now_v7();

        let mut holder = database.pool.begin().await?;
        lock_entitlement_aggregate(&mut holder, subscriber).await?;

        let mut caller = EntitlementWriteTransaction::begin(&database.pool).await?;
        let caller_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(caller.connection())
            .await?;
        let mut observer = database.pool.acquire().await?;
        sqlx::query("INSERT INTO host_entitlement_guard_probes (id) VALUES ($1)")
            .bind(probe_id)
            .execute(caller.connection())
            .await?;
        let error = require_entitlement_for_update(caller, &entitlement_guard)
            .await
            .expect_err("the held aggregate lock must trigger the guard timeout");
        let EntitlementGuardError::Sql(error) = error else {
            return Err(io::Error::other(format!(
                "expected a SQL lock timeout from the guard, got {error}"
            ))
            .into());
        };
        assert_lock_timeout(&error)?;
        assert_backend_transaction_ended(&mut observer, caller_pid).await?;
        holder.rollback().await?;
        assert_host_guard_probe_absent(&database.pool, probe_id).await?;
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn canceling_a_blocked_protected_write_guard_rolls_back_the_transaction()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("sr_guard_cancel").await?;
    let result = async {
        let scope = Uuid::now_v7();
        let subscriber = Uuid::now_v7();
        let entitlement_guard = guard(scope, subscriber)?;
        create_host_guard_probe_table(&database.pool).await?;
        let probe_id = Uuid::now_v7();

        let mut holder = database.pool.begin().await?;
        lock_entitlement_aggregate(&mut holder, subscriber).await?;

        let mut caller = EntitlementWriteTransaction::begin(&database.pool).await?;
        sqlx::query("INSERT INTO host_entitlement_guard_probes (id) VALUES ($1)")
            .bind(probe_id)
            .execute(caller.connection())
            .await?;
        let caller_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(caller.connection())
            .await?;
        let mut observer = database.pool.acquire().await?;

        let wait_until_blocked = async {
            loop {
                let waiting_on_lock: Option<bool> = sqlx::query_scalar(
                    r#"
                    SELECT wait_event_type = 'Lock'
                    FROM pg_catalog.pg_stat_activity
                    WHERE pid = $1
                    "#,
                )
                .bind(caller_pid)
                .fetch_one(&mut *observer)
                .await?;
                if waiting_on_lock.unwrap_or(false) {
                    return Ok::<_, sqlx::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        let mut guard_future = Box::pin(require_entitlement_for_update_with_lock_timeout(
            caller,
            &entitlement_guard,
            "30s",
        ));
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::select! {
                guard_result = &mut guard_future => Err(io::Error::other(format!(
                    "guard completed instead of blocking before cancellation: {guard_result:?}"
                ))),
                observation = wait_until_blocked => observation.map_err(io::Error::other),
            }
        })
        .await
        .map_err(|_| io::Error::other("guard did not reach its advisory-lock wait"))??;
        drop(guard_future);
        holder.rollback().await?;
        assert_backend_transaction_ended(&mut observer, caller_pid).await?;

        let mut contender = database.pool.begin().await?;
        let aggregate_unlocked: bool = sqlx::query_scalar(
            "SELECT pg_try_advisory_xact_lock(hashtextextended($1::uuid::text || ':' || $2, 0))",
        )
        .bind(subscriber)
        .bind("base_subscription")
        .fetch_one(&mut *contender)
        .await?;
        if !aggregate_unlocked {
            return Err(
                io::Error::other("canceled guard retained its aggregate advisory lock").into(),
            );
        }
        contender.rollback().await?;
        assert_host_guard_probe_absent(&database.pool, probe_id).await?;
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
        let holder = EntitlementWriteTransaction::begin(&database.pool).await?;
        let holder = require_entitlement_for_update(holder, &guard).await?;

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
    let transaction = EntitlementWriteTransaction::begin(pool).await?;
    match require_entitlement_for_update(transaction, guard).await {
        Ok(transaction) => {
            transaction.rollback().await?;
            Ok(Ok(()))
        }
        Err(error) => Ok(Err(error)),
    }
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

async fn create_host_guard_probe_table(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        CREATE TABLE host_entitlement_guard_probes (
            id uuid PRIMARY KEY
        )
        "#,
    )
    .execute(pool)
    .await?;
    Ok(())
}

async fn assert_host_guard_probe_absent(pool: &PgPool, probe_id: Uuid) -> Result<(), io::Error> {
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM host_entitlement_guard_probes WHERE id = $1)",
    )
    .bind(probe_id)
    .fetch_one(pool)
    .await
    .map_err(io::Error::other)?;
    if exists {
        Err(io::Error::other(
            "failed protected-write admission committed an earlier host mutation",
        ))
    } else {
        Ok(())
    }
}

async fn assert_host_guard_probe_count(
    pool: &PgPool,
    probe_ids: &[Uuid],
    expected: i64,
) -> Result<(), io::Error> {
    let actual: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM host_entitlement_guard_probes WHERE id = ANY($1::uuid[])",
    )
    .bind(probe_ids)
    .fetch_one(pool)
    .await
    .map_err(io::Error::other)?;
    if actual == expected {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "expected {expected} committed host guard probes, found {actual}"
        )))
    }
}

async fn assert_backend_transaction_ended(
    observer: &mut sqlx::pool::PoolConnection<Postgres>,
    backend_pid: i32,
) -> Result<(), io::Error> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let state = sqlx::query_as::<_, (String, bool)>(
                r#"
                SELECT state, xact_start IS NULL
                FROM pg_catalog.pg_stat_activity
                WHERE pid = $1
                "#,
            )
            .bind(backend_pid)
            .fetch_optional(&mut **observer)
            .await
            .map_err(io::Error::other)?;
            if match state {
                None => true,
                Some((state, transaction_ended)) => state == "idle" && transaction_ended,
            } {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .map_err(|_| io::Error::other("protected-write backend did not finish rolling back"))?
}

async fn lock_entitlement_aggregate(
    transaction: &mut Transaction<'_, Postgres>,
    subscriber: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::uuid::text || ':' || $2, 0))")
        .bind(subscriber)
        .bind("base_subscription")
        .execute(&mut **transaction)
        .await?;
    Ok(())
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
