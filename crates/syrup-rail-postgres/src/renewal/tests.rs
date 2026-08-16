use std::{collections::HashSet, error::Error, io, time::Duration as StdDuration};

use chrono::Duration;
use uuid::Uuid;

use super::*;
use crate::test_support::{
    TestDatabase, create_gateway_account, explain_plan_root, find_plan_index_node,
    plan_has_node_type,
};

use self::support::*;

mod local_attempts;
mod support;

#[tokio::test]
async fn due_selection_is_provider_keyed_and_has_a_fixed_shared_bound() -> Result<(), Box<dyn Error>>
{
    let database = TestDatabase::start("renew_due").await?;
    let nmi = create_gateway_account(&database.pool, "nmi").await?;
    let other = create_gateway_account(&database.pool, "other-provider").await?;
    let nmi_subscription = insert_due_subscription(&database.pool, nmi, "nmi-plan").await?;
    let other_subscription = insert_due_subscription(&database.pool, other, "other-plan").await?;

    sqlx::query(
        "UPDATE billing_gateway_provider_rate_limits SET rate_limited_until = clock_timestamp() + interval '1 hour' WHERE provider_key = 'nmi'",
    )
    .execute(&database.pool)
    .await?;
    let due = due_renewals(&database.pool).await?;
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].subscription_id().into_uuid(), other_subscription);
    assert_ne!(due[0].subscription_id().into_uuid(), nmi_subscription);
    assert_eq!(syrup_rail::RENEWAL_DISPATCH_LIMIT, 100);

    database.cleanup().await
}

#[tokio::test]
async fn representative_first_and_continuation_plans_are_limit_driven_and_index_ordered()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("renew_plan_v18").await?;
    let account = create_gateway_account(&database.pool, "nmi").await?;
    let due_at = Utc::now() - Duration::minutes(5);
    insert_due_subscription_population(&database.pool, account, due_at, 4_096).await?;
    sqlx::raw_sql(
        r#"
        ANALYZE billing_subscriptions;
        ANALYZE billing_gateway_accounts;
        ANALYZE billing_gateway_provider_rate_limits;
        ANALYZE billing_payment_attempts;
        "#,
    )
    .execute(&database.pool)
    .await?;

    let mut transaction = database.pool.begin().await?;
    let continuation_cursor = RenewalDispatchPageCursor::new(
        due_at,
        due_at - Duration::minutes(8),
        SubscriptionId::new(Uuid::from_u128(u128::MAX / 2)),
    );

    for (page_kind, page_query) in [
        ("first", DueRenewalPageQuery::First(due_at)),
        (
            "continuation",
            DueRenewalPageQuery::Continuation(continuation_cursor),
        ),
    ] {
        let explain_sql = format!(
            "EXPLAIN (GENERIC_PLAN TRUE, FORMAT JSON, COSTS OFF) {}",
            page_query.sql()
        );
        let plan_row = sqlx::raw_sql(&explain_sql)
            .fetch_one(&mut *transaction)
            .await?;
        let plan: serde_json::Value = plan_row.try_get(0)?;
        let root = explain_plan_root(&plan)?;
        let rendered = serde_json::to_string_pretty(root)?;
        if root.get("Node Type").and_then(serde_json::Value::as_str) != Some("Limit") {
            return Err(io::Error::other(format!(
                "{page_kind} representative renewal plan lost its top-level Limit:\n{rendered}"
            ))
            .into());
        }
        if plan_has_node_type(root, "CTE Scan")
            || plan_has_node_type(root, "Sort")
            || plan_has_node_type(root, "Incremental Sort")
        {
            return Err(io::Error::other(format!(
                "{page_kind} representative renewal plan materialized or sorted its due candidates:\n{rendered}"
            ))
            .into());
        }
        let index_node = find_plan_index_node(root, "billing_subscriptions_due_idx").ok_or_else(|| {
            io::Error::other(format!(
                "{page_kind} renewal dispatch plan did not use its ordered due index:\n{rendered}"
            ))
        })?;
        if matches!(page_query, DueRenewalPageQuery::Continuation(_)) {
            let index_condition = index_node
                .get("Index Cond")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            if !index_condition.contains("$13") || !index_condition.contains("$14") {
                return Err(io::Error::other(format!(
                    "continuation keyset was not pushed into the due-index condition:\n{rendered}"
                ))
                .into());
            }
        }
    }
    transaction.rollback().await?;

    database.cleanup().await
}

#[tokio::test]
async fn pages_drain_205_tied_due_subscriptions_once_and_legacy_wrapper_keeps_first_page()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("renew_pages_205").await?;
    let account = create_gateway_account(&database.pool, "nmi").await?;
    let due_at =
        sqlx::query_scalar::<_, DateTime<Utc>>("SELECT clock_timestamp() - interval '5 minutes'")
            .fetch_one(&database.pool)
            .await?;
    let mut expected_ids = Vec::new();
    for value in 1..=205_u128 {
        let subscription_id = Uuid::from_u128(value);
        insert_due_subscription_at(
            &database.pool,
            account,
            "page-plan",
            subscription_id,
            due_at,
        )
        .await?;
        expected_ids.push(subscription_id);
    }
    expected_ids.sort_unstable();

    let first = due_renewals_page(&database.pool, None).await?;
    assert_eq!(first.dispatches().len(), 100);
    assert!(
        first
            .dispatches()
            .iter()
            .all(|dispatch| dispatch.attempt_sequence_count() == 0)
    );
    assert!(
        first
            .dispatches()
            .iter()
            .all(|dispatch| dispatch.period_start_at() == &due_at)
    );
    let first_cursor = first
        .next_cursor()
        .expect("the first 205-row page has another page");

    let second = due_renewals_page(&database.pool, Some(&first_cursor)).await?;
    assert_eq!(second.dispatches().len(), 100);
    let second_cursor = second
        .next_cursor()
        .expect("the second 205-row page has another page");
    assert_eq!(second_cursor.observed_at(), first_cursor.observed_at());

    let third = due_renewals_page(&database.pool, Some(&second_cursor)).await?;
    assert_eq!(third.dispatches().len(), 5);
    assert_eq!(third.next_cursor(), None);

    let mut dispatched_ids = dispatch_ids(&first);
    dispatched_ids.extend(dispatch_ids(&second));
    dispatched_ids.extend(dispatch_ids(&third));
    assert_eq!(dispatched_ids, expected_ids);
    assert_eq!(
        dispatched_ids.iter().copied().collect::<HashSet<_>>().len(),
        205
    );

    let legacy = due_renewals(&database.pool).await?;
    assert_eq!(legacy.len(), 100);
    let legacy_ids = legacy
        .iter()
        .map(|dispatch| dispatch.subscription_id().into_uuid())
        .collect::<Vec<_>>();
    assert_eq!(legacy_ids, expected_ids[..100]);

    database.cleanup().await
}

#[tokio::test]
async fn continuation_excludes_rows_that_become_due_after_its_observed_time()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("renew_pg_observe").await?;
    let account = create_gateway_account(&database.pool, "nmi").await?;
    let due_at = Utc::now() - Duration::minutes(5);
    for value in 1..=101_u128 {
        insert_due_subscription_at(
            &database.pool,
            account,
            "observed-plan",
            Uuid::from_u128(value),
            due_at,
        )
        .await?;
    }
    let later = insert_due_subscription_at(
        &database.pool,
        account,
        "later-plan",
        Uuid::from_u128(1_000),
        Utc::now() + Duration::hours(1),
    )
    .await?;

    let first = due_renewals_page(&database.pool, None).await?;
    assert_eq!(first.dispatches().len(), 100);
    let cursor = first
        .next_cursor()
        .expect("one original due row remains after the first page");
    let newly_due_at = cursor.observed_at() + Duration::milliseconds(1);
    update_due_at(&database.pool, later.subscription_id, newly_due_at).await?;
    tokio::time::sleep(StdDuration::from_millis(10)).await;

    let continuation = due_renewals_page(&database.pool, Some(&cursor)).await?;
    assert_eq!(dispatch_ids(&continuation), vec![Uuid::from_u128(101)]);
    assert_eq!(continuation.next_cursor(), None);

    let next_scan_ids = scan_dispatch_ids(&database.pool).await?;
    assert!(next_scan_ids.contains(&later.subscription_id));
    assert_eq!(next_scan_ids.len(), 102);

    database.cleanup().await
}

#[tokio::test]
async fn continuation_rechecks_current_eligibility_before_dispatching() -> Result<(), Box<dyn Error>>
{
    let database = TestDatabase::start("renew_pg_recheck").await?;
    let account = create_gateway_account(&database.pool, "nmi").await?;
    let due_at = Utc::now() - Duration::minutes(5);
    for value in 1..=100_u128 {
        insert_due_subscription_at(
            &database.pool,
            account,
            "recheck-plan",
            Uuid::from_u128(value),
            due_at,
        )
        .await?;
    }
    let blocked = insert_due_subscription_at(
        &database.pool,
        account,
        "recheck-plan",
        Uuid::from_u128(1_000),
        due_at,
    )
    .await?;

    let first = due_renewals_page(&database.pool, None).await?;
    assert_eq!(first.dispatches().len(), 100);
    let cursor = first
        .next_cursor()
        .expect("the last eligible row is on the continuation");
    insert_renewal_attempt(
        &database.pool,
        account,
        &blocked,
        due_at,
        "subscription_renewal",
        "pending",
        None,
        None,
    )
    .await?;

    let continuation = due_renewals_page(&database.pool, Some(&cursor)).await?;
    assert!(continuation.dispatches().is_empty());
    assert_eq!(continuation.next_cursor(), None);

    database.cleanup().await
}

#[tokio::test]
async fn all_existing_due_renewal_eligibility_gates_remain_effective() -> Result<(), Box<dyn Error>>
{
    let database = TestDatabase::start("renew_pg_gates").await?;
    let account = create_gateway_account(&database.pool, "nmi").await?;
    let due_at = Utc::now() - Duration::minutes(10);
    let included = insert_due_subscription_at(
        &database.pool,
        account,
        "gate-plan",
        Uuid::from_u128(1),
        due_at,
    )
    .await?;
    insert_renewal_attempt(
        &database.pool,
        account,
        &included,
        due_at - Duration::days(32),
        "subscription_renewal",
        "failed",
        None,
        Some(due_at - Duration::days(31)),
    )
    .await?;
    insert_renewal_attempt(
        &database.pool,
        account,
        &included,
        due_at,
        "subscription_renewal",
        "failed",
        None,
        Some(due_at - Duration::minutes(5)),
    )
    .await?;
    let stale_update = insert_due_subscription_at(
        &database.pool,
        account,
        "stale-update-plan",
        Uuid::from_u128(2),
        due_at,
    )
    .await?;
    insert_pending_payment_method_update(
        &database.pool,
        account,
        &stale_update,
        Utc::now()
            - Duration::seconds(
                LocalAttemptPolicy::for_kind(PaymentAttemptKind::SubscriptionPaymentMethodUpdate)
                    .stale_after_seconds()
                    + 1,
            ),
    )
    .await?;
    let stale_review_update = insert_due_subscription_at(
        &database.pool,
        account,
        "stale-review-update-plan",
        Uuid::from_u128(14),
        due_at,
    )
    .await?;
    let stale_review_attempt = insert_pending_payment_method_update(
        &database.pool,
        account,
        &stale_review_update,
        Utc::now()
            - Duration::seconds(
                LocalAttemptPolicy::for_kind(PaymentAttemptKind::SubscriptionPaymentMethodUpdate)
                    .stale_after_seconds()
                    + 1,
            ),
    )
    .await?;
    sqlx::query("UPDATE billing_payment_attempts SET status = 'review_required' WHERE id = $1")
        .bind(stale_review_attempt)
        .execute(&database.pool)
        .await?;
    let past_due = insert_due_subscription_at(
        &database.pool,
        account,
        "past-due-plan",
        Uuid::from_u128(3),
        due_at,
    )
    .await?;
    sqlx::query("UPDATE billing_subscriptions SET status = 'past_due' WHERE id = $1")
        .bind(past_due.subscription_id)
        .execute(&database.pool)
        .await?;

    let future = insert_due_subscription_at(
        &database.pool,
        account,
        "future-plan",
        Uuid::from_u128(4),
        Utc::now() + Duration::hours(1),
    )
    .await?;
    let canceled = insert_due_subscription_at(
        &database.pool,
        account,
        "canceled-plan",
        Uuid::from_u128(5),
        due_at,
    )
    .await?;
    sqlx::query(
        "UPDATE billing_subscriptions SET status = 'canceled', canceled_at = clock_timestamp(), next_payment_attempt_at = NULL WHERE id = $1",
    )
    .bind(canceled.subscription_id)
    .execute(&database.pool)
    .await?;
    let unpaid = insert_due_subscription_at(
        &database.pool,
        account,
        "unpaid-plan",
        Uuid::from_u128(6),
        due_at,
    )
    .await?;
    sqlx::query(
        "UPDATE billing_subscriptions SET status = 'unpaid', unpaid_at = clock_timestamp(), next_payment_attempt_at = NULL WHERE id = $1",
    )
    .bind(unpaid.subscription_id)
    .execute(&database.pool)
    .await?;

    let provider_cooldown_account =
        create_gateway_account(&database.pool, "provider-cooldown").await?;
    sqlx::query(
        "UPDATE billing_gateway_provider_rate_limits SET rate_limited_until = clock_timestamp() + interval '1 hour' WHERE provider_key = 'provider-cooldown'",
    )
    .execute(&database.pool)
    .await?;
    let provider_cooldown = insert_due_subscription_at(
        &database.pool,
        provider_cooldown_account,
        "provider-cooldown-plan",
        Uuid::from_u128(7),
        due_at,
    )
    .await?;

    let account_cooldown_account =
        create_gateway_account(&database.pool, "account-cooldown").await?;
    sqlx::query(
        "UPDATE billing_gateway_accounts SET mutation_rate_limited_until = clock_timestamp() + interval '1 hour' WHERE id = $1",
    )
    .bind(account_cooldown_account.gateway_account_id)
    .execute(&database.pool)
    .await?;
    let account_cooldown = insert_due_subscription_at(
        &database.pool,
        account_cooldown_account,
        "account-cooldown-plan",
        Uuid::from_u128(8),
        due_at,
    )
    .await?;

    let blocking_attempt = insert_due_subscription_at(
        &database.pool,
        account,
        "blocking-attempt-plan",
        Uuid::from_u128(9),
        due_at,
    )
    .await?;
    insert_renewal_attempt(
        &database.pool,
        account,
        &blocking_attempt,
        due_at,
        "subscription_renewal",
        "pending",
        None,
        None,
    )
    .await?;

    let payment_method_update = insert_due_subscription_at(
        &database.pool,
        account,
        "payment-update-plan",
        Uuid::from_u128(10),
        due_at,
    )
    .await?;
    insert_pending_payment_method_update(
        &database.pool,
        account,
        &payment_method_update,
        Utc::now(),
    )
    .await?;

    let infrastructure_limit = insert_due_subscription_at(
        &database.pool,
        account,
        "infrastructure-limit-plan",
        Uuid::from_u128(11),
        due_at,
    )
    .await?;
    for _ in 0..syrup_rail::MAX_RENEWAL_INFRASTRUCTURE_ATTEMPTS_PER_PERIOD_CONFIGURATION {
        insert_renewal_attempt(
            &database.pool,
            account,
            &infrastructure_limit,
            due_at,
            "subscription_renewal",
            "failed",
            Some(PaymentResolutionCode::GatewayAccountMutationCooldownBeforeSubmission.as_str()),
            Some(Utc::now()),
        )
        .await?;
    }

    let infrastructure_pacing = insert_due_subscription_at(
        &database.pool,
        account,
        "infrastructure-pacing-plan",
        Uuid::from_u128(12),
        due_at,
    )
    .await?;
    insert_renewal_attempt(
        &database.pool,
        account,
        &infrastructure_pacing,
        due_at,
        "subscription_renewal",
        "failed",
        Some(PaymentResolutionCode::GatewayLiveReadinessFailedBeforeSubmission.as_str()),
        Some(Utc::now()),
    )
    .await?;

    let provider_rate_pacing = insert_due_subscription_at(
        &database.pool,
        account,
        "provider-rate-pacing-plan",
        Uuid::from_u128(13),
        due_at,
    )
    .await?;
    insert_renewal_attempt(
        &database.pool,
        account,
        &provider_rate_pacing,
        due_at,
        "subscription_renewal",
        "failed",
        Some(PaymentResolutionCode::GatewayProviderRateLimitedBeforeSubmission.as_str()),
        Some(Utc::now()),
    )
    .await?;

    let page = due_renewals_page(&database.pool, None).await?;
    let included_dispatch = page
        .dispatches()
        .iter()
        .find(|dispatch| dispatch.subscription_id().into_uuid() == included.subscription_id)
        .expect("eligible subscription is dispatched");
    assert_eq!(included_dispatch.attempt_sequence_count(), 1);
    let actual = dispatch_ids(&page).into_iter().collect::<HashSet<_>>();
    let expected = [
        included.subscription_id,
        stale_update.subscription_id,
        stale_review_update.subscription_id,
        past_due.subscription_id,
    ]
    .into_iter()
    .collect::<HashSet<_>>();
    assert_eq!(actual, expected);
    assert_eq!(page.next_cursor(), None);

    for excluded in [
        future.subscription_id,
        canceled.subscription_id,
        unpaid.subscription_id,
        provider_cooldown.subscription_id,
        account_cooldown.subscription_id,
        blocking_attempt.subscription_id,
        payment_method_update.subscription_id,
        infrastructure_limit.subscription_id,
        infrastructure_pacing.subscription_id,
        provider_rate_pacing.subscription_id,
    ] {
        assert!(!actual.contains(&excluded));
    }

    database.cleanup().await
}

#[tokio::test]
async fn continuation_keeps_every_clock_dependent_gate_at_the_first_page_time()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("renew_pg_frozen").await?;
    let account = create_gateway_account(&database.pool, "nmi").await?;
    let baseline_due_at = Utc::now() - Duration::minutes(5);
    for value in 1..=101_u128 {
        insert_due_subscription_at(
            &database.pool,
            account,
            "frozen-baseline-plan",
            Uuid::from_u128(value),
            baseline_due_at,
        )
        .await?;
    }
    let first = due_renewals_page(&database.pool, None).await?;
    assert_eq!(first.dispatches().len(), 100);
    let cursor = first
        .next_cursor()
        .expect("one baseline row remains for the continuation");
    let observed_at = cursor.observed_at();
    let window_opens_at = observed_at + Duration::seconds(1);

    insert_due_subscription_at(
        &database.pool,
        account,
        "frozen-due-plan",
        Uuid::from_u128(1_001),
        window_opens_at,
    )
    .await?;

    let provider_cooldown_account =
        create_gateway_account(&database.pool, "frozen-provider").await?;
    sqlx::query(
        "UPDATE billing_gateway_provider_rate_limits SET rate_limited_until = $1 WHERE provider_key = 'frozen-provider'",
    )
    .bind(window_opens_at)
    .execute(&database.pool)
    .await?;
    insert_due_subscription_at(
        &database.pool,
        provider_cooldown_account,
        "frozen-provider-plan",
        Uuid::from_u128(1_002),
        observed_at,
    )
    .await?;

    let account_cooldown_account = create_gateway_account(&database.pool, "frozen-account").await?;
    sqlx::query(
        "UPDATE billing_gateway_accounts SET mutation_rate_limited_until = $2 WHERE id = $1",
    )
    .bind(account_cooldown_account.gateway_account_id)
    .bind(window_opens_at)
    .execute(&database.pool)
    .await?;
    insert_due_subscription_at(
        &database.pool,
        account_cooldown_account,
        "frozen-account-plan",
        Uuid::from_u128(1_003),
        observed_at,
    )
    .await?;

    let stale_update = insert_due_subscription_at(
        &database.pool,
        account,
        "frozen-update-plan",
        Uuid::from_u128(1_004),
        observed_at,
    )
    .await?;
    insert_pending_payment_method_update(
        &database.pool,
        account,
        &stale_update,
        observed_at
            - Duration::seconds(
                LocalAttemptPolicy::for_kind(PaymentAttemptKind::SubscriptionPaymentMethodUpdate)
                    .stale_after_seconds()
                    - 1,
            ),
    )
    .await?;

    let infrastructure_retry = insert_due_subscription_at(
        &database.pool,
        account,
        "frozen-infrastructure-plan",
        Uuid::from_u128(1_005),
        observed_at,
    )
    .await?;
    insert_renewal_attempt(
        &database.pool,
        account,
        &infrastructure_retry,
        observed_at,
        "subscription_renewal",
        "failed",
        Some(PaymentResolutionCode::GatewayLiveReadinessFailedBeforeSubmission.as_str()),
        Some(
            observed_at
                - Duration::seconds(syrup_rail::RENEWAL_INFRASTRUCTURE_RETRY_AFTER_SECONDS - 1),
        ),
    )
    .await?;

    let provider_rate_retry = insert_due_subscription_at(
        &database.pool,
        account,
        "frozen-provider-rate-plan",
        Uuid::from_u128(1_006),
        observed_at,
    )
    .await?;
    insert_renewal_attempt(
        &database.pool,
        account,
        &provider_rate_retry,
        observed_at,
        "subscription_renewal",
        "failed",
        Some(PaymentResolutionCode::GatewayProviderRateLimitedBeforeSubmission.as_str()),
        Some(
            observed_at
                - Duration::seconds(
                    syrup_rail::RENEWAL_PROVIDER_RATE_LIMIT_RETRY_AFTER_SECONDS - 1,
                ),
        ),
    )
    .await?;

    tokio::time::sleep(StdDuration::from_secs(2)).await;
    let now = sqlx::query_scalar::<_, DateTime<Utc>>("SELECT clock_timestamp()")
        .fetch_one(&database.pool)
        .await?;
    assert!(now >= window_opens_at);

    let continuation = due_renewals_page(&database.pool, Some(&cursor)).await?;
    assert_eq!(dispatch_ids(&continuation), vec![Uuid::from_u128(101)]);
    assert_eq!(continuation.next_cursor(), None);

    database.cleanup().await
}

#[tokio::test]
async fn period_state_counts_both_kinds_but_renewal_only_infrastructure()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("renew_state").await?;
    let account = create_gateway_account(&database.pool, "nmi").await?;
    let subscription_id = insert_due_subscription(&database.pool, account, "base-plan").await?;
    let row = sqlx::query(
        "SELECT subscriber_id, payment_method_id, next_renewal_at, initial_transaction_id, status FROM billing_subscriptions WHERE id = $1",
    )
    .bind(subscription_id)
    .fetch_one(&database.pool)
    .await?;
    let subscriber_id: Uuid = row.try_get("subscriber_id")?;
    let payment_method_id: Uuid = row.try_get("payment_method_id")?;
    let period_start_at: DateTime<Utc> = row.try_get("next_renewal_at")?;
    let initial_transaction_id: String = row.try_get("initial_transaction_id")?;
    let subscription_status: String = row.try_get("status")?;
    for (kind, resolution) in [
        (
            "subscription_renewal",
            PaymentResolutionCode::GatewayLiveReadinessFailedBeforeSubmission.as_str(),
        ),
        (
            "subscription_recovery",
            PaymentResolutionCode::GatewayLiveReadinessFailedBeforeSubmission.as_str(),
        ),
    ] {
        let attempt_id = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO billing_payment_attempts (
                id, billing_scope_id, subscriber_id, plan_key, subscription_id,
                payment_method_id, attempt_kind, status, idempotency_key,
                request_fingerprint, amount_cents, currency,
                billing_period_start_at, billing_period_end_at,
                gateway_account_id, gateway_configuration_id, gateway_order_id,
                resolution_code, resolved_at,
                subscription_expected_payment_method_id,
                subscription_expected_initial_transaction_id,
                subscription_expected_status
            ) VALUES (
                $1, $2, $3, 'base-plan', $4, $5, $6, 'failed', $7, $8,
                1900, 'USD', $9, $10, $11, $12, $13, $14, clock_timestamp(),
                $5, $15, $16
            )
            "#,
        )
        .bind(attempt_id)
        .bind(account.billing_scope_id)
        .bind(subscriber_id)
        .bind(subscription_id)
        .bind(payment_method_id)
        .bind(kind)
        .bind(format!("attempt-{}", attempt_id.simple()))
        .bind(format!("fingerprint-{}", attempt_id.simple()))
        .bind(period_start_at)
        .bind(period_start_at + Duration::days(28))
        .bind(account.gateway_account_id)
        .bind(account.gateway_configuration_id)
        .bind(format!("order_{}", attempt_id.simple()))
        .bind(resolution)
        .bind(&initial_transaction_id)
        .bind(&subscription_status)
        .execute(&database.pool)
        .await?;
    }
    let mut transaction = database.pool.begin().await?;
    let state = renewal_attempt_state(
        &mut transaction,
        SubscriptionId::new(subscription_id),
        period_start_at,
        None,
    )
    .await?;
    transaction.rollback().await?;
    assert_eq!(state.attempt_sequence_count, 2);
    assert_eq!(state.automatic_infrastructure_attempt_count, 1);

    database.cleanup().await
}
