use std::{error::Error, io};

use chrono::{DateTime, Duration, TimeZone, Utc};
use sqlx::{PgPool, Row};
use syrup_rail::{
    BillingScopeId, Entitlement, PastDueAccess, PaymentAttemptId, PaymentCardBrand, PlanKey,
    ScrubSubscriberBillingData, SubscriberId, SubscriptionBillingPortalQuery,
    SubscriptionPaymentHistoryCursor, SubscriptionPaymentHistoryPageLimit, SubscriptionPhase,
    SubscriptionStatus,
};
use uuid::Uuid;

use super::{
    SubscriptionPaymentHistoryPageQuery, subscription_billing_portal,
    subscription_payment_history_page,
};
use crate::{
    scrub_subscriber_billing_data,
    test_support::{
        GatewayAccountFixture, TestDatabase, create_gateway_account, explain_plan_root,
        find_plan_index_node, plan_has_node_type,
    },
};

struct PortalFixture {
    account: GatewayAccountFixture,
    subscriber_id: Uuid,
    plan_key: PlanKey,
    subscription_id: Uuid,
    payment_method_id: Uuid,
    initial_transaction_id: String,
    period_end: DateTime<Utc>,
}

impl PortalFixture {
    fn query(&self) -> SubscriptionBillingPortalQuery {
        SubscriptionBillingPortalQuery::new(
            BillingScopeId::new(self.account.billing_scope_id),
            SubscriberId::new(self.subscriber_id),
            self.plan_key.clone(),
        )
    }
}

#[tokio::test]
async fn billing_portal_projects_empty_active_trial_dunning_cancellation_and_grant()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("sr_portal_proj").await?;
    let result = async {
        let account = create_gateway_account(&database.pool, "portal_gateway").await?;
        let subscriber = Uuid::now_v7();

        let empty = portal_query(account.billing_scope_id, subscriber, "empty_plan")?;
        let empty_snapshot = subscription_billing_portal(&database.pool, &empty).await?;
        if !matches!(
            empty_snapshot.entitlement(),
            Entitlement::Missing {
                saved_discount: None,
                ..
            }
        ) || empty_snapshot.payment_method_display().is_some()
        {
            return Err(io::Error::other("empty portal projection was not exact").into());
        }
        let empty_history = subscription_payment_history_page(
            &database.pool,
            &empty,
            None,
            SubscriptionPaymentHistoryPageLimit::new(1)?,
        )
        .await?;
        if !empty_history.items().is_empty() || empty_history.next_cursor().is_some() {
            return Err(io::Error::other("empty portal history was not empty").into());
        }

        let active = insert_subscription(
            &database.pool,
            account,
            subscriber,
            "active_plan",
            SubscriptionStatus::Active,
            SubscriptionPhase::Recurring,
        )
        .await?;
        insert_applied_discount(&database.pool, &active).await?;
        let active_snapshot = subscription_billing_portal(&database.pool, &active.query()).await?;
        match active_snapshot.entitlement() {
            Entitlement::PaidActive {
                subscription,
                applied_discount: Some(discount),
            } if subscription.phase() == SubscriptionPhase::Recurring
                && discount.snapshot().label() == Some("Portal promotion") => {}
            _ => {
                return Err(io::Error::other(
                    "active portal projection lost entitlement or applied discount",
                )
                .into());
            }
        }
        let active_display = active_snapshot
            .payment_method_display()
            .ok_or_else(|| io::Error::other("active portal projection lost card display"))?;
        if active_display.card_brand() != Some(PaymentCardBrand::Visa)
            || active_display.card_last_four() != Some("4242")
            || active_display.card_expiration_month() != Some(12)
            || active_display.card_expiration_year() != Some(2032)
        {
            return Err(io::Error::other("active portal card display was not lossless").into());
        }

        let saved = portal_query(account.billing_scope_id, subscriber, "saved_plan")?;
        insert_saved_discount(&database.pool, &saved).await?;
        match subscription_billing_portal(&database.pool, &saved)
            .await?
            .entitlement()
        {
            Entitlement::Missing {
                saved_discount: Some(discount),
                ..
            } if discount.snapshot().label() == Some("Saved portal promotion") => {}
            _ => {
                return Err(io::Error::other(
                    "missing portal projection lost saved discount state",
                )
                .into());
            }
        }

        let trial = insert_subscription(
            &database.pool,
            account,
            Uuid::now_v7(),
            "trial_plan",
            SubscriptionStatus::Active,
            SubscriptionPhase::PaidTrial,
        )
        .await?;
        let trial_snapshot = subscription_billing_portal(&database.pool, &trial.query()).await?;
        match trial_snapshot.entitlement() {
            Entitlement::PaidActive { subscription, .. }
                if subscription.phase() == SubscriptionPhase::PaidTrial => {}
            _ => return Err(io::Error::other("paid trial was not projected").into()),
        }
        if trial_snapshot.payment_method_display().is_none() {
            return Err(io::Error::other("paid trial lost its masked card display").into());
        }

        let dunning = insert_subscription(
            &database.pool,
            account,
            Uuid::now_v7(),
            "dunning_plan",
            SubscriptionStatus::PastDue,
            SubscriptionPhase::Recurring,
        )
        .await?;
        sqlx::query(
            "UPDATE billing_subscriptions SET past_due_access = 'continue_until_dunning_exhausted' WHERE id = $1",
        )
        .bind(dunning.subscription_id)
        .execute(&database.pool)
        .await?;
        let dunning_snapshot = subscription_billing_portal(&database.pool, &dunning.query()).await?;
        match dunning_snapshot.entitlement() {
            Entitlement::PastDue {
                access: PastDueAccess::AllowedDuringDunning,
                subscription,
                ..
            } if subscription.next_payment_attempt_at().is_some() => {}
            _ => return Err(io::Error::other("dunning entitlement was not preserved").into()),
        }
        if dunning_snapshot.payment_method_display().is_none() {
            return Err(io::Error::other("dunning subscription lost its masked card display").into());
        }

        let canceled = insert_subscription(
            &database.pool,
            account,
            Uuid::now_v7(),
            "canceled_plan",
            SubscriptionStatus::Canceled,
            SubscriptionPhase::Recurring,
        )
        .await?;
        let canceled_snapshot = subscription_billing_portal(&database.pool, &canceled.query()).await?;
        if !matches!(
            canceled_snapshot.entitlement(),
            Entitlement::PaidThroughCancellation { .. }
        ) || canceled_snapshot.payment_method_display().is_none()
        {
            return Err(io::Error::other("paid-through cancellation was not projected").into());
        }

        let grant_query = portal_query(account.billing_scope_id, Uuid::now_v7(), "grant_plan")?;
        let grant_id = insert_grant(&database.pool, &grant_query).await?;
        let grant_snapshot = subscription_billing_portal(&database.pool, &grant_query).await?;
        match grant_snapshot.entitlement() {
            Entitlement::Granted { grant } if grant.id().into_uuid() == grant_id => {}
            _ => return Err(io::Error::other("grant-only entitlement was not projected").into()),
        }
        if grant_snapshot.payment_method_display().is_some() {
            return Err(io::Error::other("grant-only entitlement exposed a payment method").into());
        }

        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn billing_portal_is_exact_and_hides_scrubbed_or_sensitive_method_data()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("sr_portal_redact").await?;
    let result = async {
        let account = create_gateway_account(&database.pool, "portal_redaction_gateway").await?;
        let fixture = insert_subscription(
            &database.pool,
            account,
            Uuid::now_v7(),
            "private_plan",
            SubscriptionStatus::Active,
            SubscriptionPhase::Recurring,
        )
        .await?;
        sqlx::query(
            r#"
            UPDATE billing_payment_methods
            SET gateway_payment_method_reference = 'portal-method-reference-secret',
                billing_name = 'Portal Private Name',
                billing_email = 'portal-private@example.test'
            WHERE id = $1
            "#,
        )
        .bind(fixture.payment_method_id)
        .execute(&database.pool)
        .await?;

        let snapshot = subscription_billing_portal(&database.pool, &fixture.query()).await?;
        let debug = format!("{snapshot:?}");
        for secret in [
            "Visa",
            "4242",
            "2032",
            "portal-method-reference-secret",
            "Portal Private Name",
            "portal-private@example.test",
        ] {
            if debug.contains(secret) {
                return Err(io::Error::other(format!(
                    "portal snapshot Debug leaked protected display data: {secret}"
                ))
                .into());
            }
        }

        sqlx::query(
            r#"
            UPDATE billing_payment_methods
            SET card_brand = '   ',
                card_last4 = NULL,
                card_exp_month = NULL,
                card_exp_year = NULL
            WHERE id = $1
            "#,
        )
        .bind(fixture.payment_method_id)
        .execute(&database.pool)
        .await?;
        let normalized_empty_snapshot =
            subscription_billing_portal(&database.pool, &fixture.query()).await?;
        if normalized_empty_snapshot.payment_method_display().is_some() {
            return Err(io::Error::other(
                "normalized empty card metadata produced a portal display",
            )
            .into());
        }

        for wrong_query in [
            portal_query(Uuid::now_v7(), fixture.subscriber_id, "private_plan")?,
            portal_query(
                fixture.account.billing_scope_id,
                fixture.subscriber_id,
                "other_plan",
            )?,
            portal_query(
                fixture.account.billing_scope_id,
                Uuid::now_v7(),
                "private_plan",
            )?,
        ] {
            let wrong_snapshot = subscription_billing_portal(&database.pool, &wrong_query).await?;
            if !matches!(wrong_snapshot.entitlement(), Entitlement::Missing { .. })
                || wrong_snapshot.payment_method_display().is_some()
            {
                return Err(
                    io::Error::other("portal query crossed an exact identity boundary").into(),
                );
            }
        }

        let mut transaction = database.pool.begin().await?;
        let scrubbed = scrub_subscriber_billing_data(
            &mut transaction,
            ScrubSubscriberBillingData::new(
                BillingScopeId::new(fixture.account.billing_scope_id),
                SubscriberId::new(fixture.subscriber_id),
            ),
        )
        .await?;
        transaction.commit().await?;
        if scrubbed.payment_methods() != 1 {
            return Err(
                io::Error::other("scrub fixture did not disable its payment method").into(),
            );
        }
        let scrubbed_snapshot =
            subscription_billing_portal(&database.pool, &fixture.query()).await?;
        if scrubbed_snapshot.payment_method_display().is_some() {
            return Err(
                io::Error::other("scrubbed payment method retained a portal display").into(),
            );
        }

        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn payment_history_first_and_continuation_plans_use_the_exact_ordered_index()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("sr_hist_plan_v18").await?;
    let result = async {
        let account = create_gateway_account(&database.pool, "portal_history_plan_gateway").await?;
        let fixture = insert_subscription(
            &database.pool,
            account,
            Uuid::now_v7(),
            "history_plan_shape",
            SubscriptionStatus::Active,
            SubscriptionPhase::Recurring,
        )
        .await?;
        let newest_at = Utc.with_ymd_and_hms(2026, 8, 12, 12, 0, 0).unwrap();
        insert_payment_history_population(&database.pool, &fixture, newest_at, 4_096).await?;
        sqlx::query("ANALYZE billing_payment_attempts")
            .execute(&database.pool)
            .await?;

        let cursor = SubscriptionPaymentHistoryCursor::new(
            newest_at - Duration::minutes(8),
            PaymentAttemptId::new(Uuid::from_u128(u128::MAX / 2)),
        );
        for (page_kind, page_query) in [
            ("first", SubscriptionPaymentHistoryPageQuery::First),
            (
                "continuation",
                SubscriptionPaymentHistoryPageQuery::Continuation(cursor),
            ),
        ] {
            let explain_sql = format!(
                "EXPLAIN (GENERIC_PLAN TRUE, FORMAT JSON, COSTS OFF) {}",
                page_query.sql()
            );
            let plan_row = sqlx::raw_sql(&explain_sql)
                .fetch_one(&database.pool)
                .await?;
            let plan: serde_json::Value = plan_row.try_get(0)?;
            let root = explain_plan_root(&plan)?;
            let rendered = serde_json::to_string_pretty(root)?;
            if root.get("Node Type").and_then(serde_json::Value::as_str) != Some("Limit")
                || plan_has_node_type(root, "Sort")
                || plan_has_node_type(root, "Incremental Sort")
            {
                return Err(io::Error::other(format!(
                    "{page_kind} payment-history plan lost its limit-driven index order:\n{rendered}"
                ))
                .into());
            }
            let index_node = find_plan_index_node(
                root,
                "billing_payment_attempts_subscription_history_idx",
            )
            .ok_or_else(|| {
                io::Error::other(format!(
                    "{page_kind} payment-history plan did not use its ordered index:\n{rendered}"
                ))
            })?;
            if matches!(
                page_query,
                SubscriptionPaymentHistoryPageQuery::Continuation(_)
            ) {
                let index_condition = index_node
                    .get("Index Cond")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                if !index_condition.contains("$4") || !index_condition.contains("$5") {
                    return Err(io::Error::other(format!(
                        "payment-history continuation keyset was not pushed into the index condition:\n{rendered}"
                    ))
                    .into());
                }
            }
        }
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn payment_history_is_exact_keyset_paginated_and_redacted() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("sr_payment_hist").await?;
    let result = async {
        let account = create_gateway_account(&database.pool, "portal_history_gateway").await?;
        let fixture = insert_subscription(
            &database.pool,
            account,
            Uuid::now_v7(),
            "history_plan",
            SubscriptionStatus::Active,
            SubscriptionPhase::Recurring,
        )
        .await?;
        let tied_at = Utc.with_ymd_and_hms(2026, 8, 11, 9, 0, 0).unwrap();
        let tied_ids = [
            Uuid::from_u128(101),
            Uuid::from_u128(104),
            Uuid::from_u128(102),
            Uuid::from_u128(105),
            Uuid::from_u128(103),
        ];
        for attempt_id in tied_ids {
            insert_payment_method_update_attempt(&database.pool, &fixture, attempt_id, tied_at)
                .await?;
        }
        let renewal_id = Uuid::from_u128(106);
        insert_renewal_attempt(
            &database.pool,
            &fixture,
            renewal_id,
            tied_at + Duration::seconds(1),
        )
        .await?;
        insert_host_charge(
            &database.pool,
            &fixture,
            Uuid::from_u128(107),
            tied_at + Duration::seconds(2),
        )
        .await?;

        let limit = SubscriptionPaymentHistoryPageLimit::new(2)?;
        let first =
            subscription_payment_history_page(&database.pool, &fixture.query(), None, limit)
                .await?;
        let second_cursor = first
            .next_cursor()
            .ok_or_else(|| io::Error::other("first payment-history page had no cursor"))?;
        let second = subscription_payment_history_page(
            &database.pool,
            &fixture.query(),
            Some(&second_cursor),
            limit,
        )
        .await?;
        let third_cursor = second
            .next_cursor()
            .ok_or_else(|| io::Error::other("second payment-history page had no cursor"))?;
        let third = subscription_payment_history_page(
            &database.pool,
            &fixture.query(),
            Some(&third_cursor),
            limit,
        )
        .await?;
        if third.next_cursor().is_some() {
            return Err(io::Error::other("terminal payment-history page emitted a cursor").into());
        }

        let mut expected = vec![(renewal_id, tied_at + Duration::seconds(1))];
        expected.extend(tied_ids.into_iter().map(|id| (id, tied_at)));
        expected.sort_by(|(left_id, left_at), (right_id, right_at)| {
            right_at.cmp(left_at).then_with(|| right_id.cmp(left_id))
        });
        let actual = [&first, &second, &third]
            .into_iter()
            .flat_map(|page| {
                page.items()
                    .iter()
                    .map(|item| (item.payment_attempt_id().into_uuid(), item.created_at()))
            })
            .collect::<Vec<_>>();
        if actual != expected {
            return Err(io::Error::other(format!(
                "payment-history pages were not strict descending keyset pages: {actual:?}"
            ))
            .into());
        }
        if first.items()[0].payment_attempt_id().into_uuid() != renewal_id
            || first.items()[0].billing_period().is_none()
            || first.items()[0].amount().cents() != 5900
            || first.items()[1].amount().cents() != 0
        {
            return Err(io::Error::other("payment-history item facts were not projected").into());
        }
        let debug = format!("{first:?}{second:?}{third:?}");
        for secret in [
            "history-method-reference-secret",
            "history-transaction-secret",
            "history-response-secret",
            "History Private Name",
            "history-private@example.test",
        ] {
            if debug.contains(secret) {
                return Err(io::Error::other(format!(
                    "payment-history Debug leaked protected data: {secret}"
                ))
                .into());
            }
        }

        for wrong_query in [
            portal_query(Uuid::now_v7(), fixture.subscriber_id, "history_plan")?,
            portal_query(
                fixture.account.billing_scope_id,
                fixture.subscriber_id,
                "other_history_plan",
            )?,
            portal_query(
                fixture.account.billing_scope_id,
                Uuid::now_v7(),
                "history_plan",
            )?,
        ] {
            let page = subscription_payment_history_page(&database.pool, &wrong_query, None, limit)
                .await?;
            if !page.items().is_empty() || page.next_cursor().is_some() {
                return Err(
                    io::Error::other("payment history crossed an identity boundary").into(),
                );
            }
        }

        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

fn portal_query(
    scope: Uuid,
    subscriber: Uuid,
    plan_key: &str,
) -> Result<SubscriptionBillingPortalQuery, syrup_rail::SlugError> {
    Ok(SubscriptionBillingPortalQuery::new(
        BillingScopeId::new(scope),
        SubscriberId::new(subscriber),
        PlanKey::new(plan_key)?,
    ))
}

async fn insert_subscription(
    pool: &PgPool,
    account: GatewayAccountFixture,
    subscriber_id: Uuid,
    plan_key: &str,
    status: SubscriptionStatus,
    phase: SubscriptionPhase,
) -> Result<PortalFixture, Box<dyn Error>> {
    let payment_method_id = Uuid::now_v7();
    let subscription_id = Uuid::now_v7();
    let initial_transaction_id = format!("txn_{}", subscription_id.simple());
    sqlx::query(
        r#"
        INSERT INTO billing_payment_methods (
            id, billing_scope_id, subscriber_id, gateway_account_id,
            gateway_payment_method_reference, status, card_brand, card_last4,
            card_exp_month, card_exp_year
        ) VALUES ($1, $2, $3, $4, $5, 'active', 'Visa', '4242', 12, 2032)
        "#,
    )
    .bind(payment_method_id)
    .bind(account.billing_scope_id)
    .bind(subscriber_id)
    .bind(account.gateway_account_id)
    .bind(format!("vault_{}", payment_method_id.simple()))
    .execute(pool)
    .await?;

    let period_start = Utc::now() - Duration::days(1);
    let period_end = period_start + Duration::days(30);
    let (canceled_at, next_payment_attempt_at) = match status {
        SubscriptionStatus::Active | SubscriptionStatus::PastDue => (None, Some(period_end)),
        SubscriptionStatus::Canceled => (Some(Utc::now()), None),
        SubscriptionStatus::Unpaid => (None, None),
    };
    let (trial_amount_cents, trial_period_kind, trial_period_count) = match phase {
        SubscriptionPhase::PaidTrial => (Some(99_i32), Some("calendar_months"), Some(1_i32)),
        SubscriptionPhase::Recurring => (None, None, None),
    };
    sqlx::query(
        r#"
        INSERT INTO billing_subscriptions (
            required_gateway_account_mode,
            id, billing_scope_id, subscriber_id, plan_key, status,
            gateway_account_id, payment_method_id, amount_cents, currency,
            current_period_start_at, current_period_end_at, next_renewal_at,
            initial_transaction_id, canceled_at, phase, recurring_period_kind,
            recurring_period_count, trial_amount_cents, trial_period_kind,
            trial_period_count, dunning_retry_delays_seconds, dunning_exhaustion,
            past_due_access, next_payment_attempt_at
        ) VALUES (
            'live',
            $1, $2, $3, $4, $5, $6, $7, 5900, 'USD', $8, $9, $9, $10,
            $11, $12, 'calendar_months', 1, $13, $14, $15, ARRAY[]::bigint[],
            'remain_past_due', 'suspend_immediately', $16
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
    .bind(canceled_at)
    .bind(phase.as_str())
    .bind(trial_amount_cents)
    .bind(trial_period_kind)
    .bind(trial_period_count)
    .bind(next_payment_attempt_at)
    .execute(pool)
    .await?;
    Ok(PortalFixture {
        account,
        subscriber_id,
        plan_key: PlanKey::new(plan_key)?,
        subscription_id,
        payment_method_id,
        initial_transaction_id,
        period_end,
    })
}

async fn insert_applied_discount(
    pool: &PgPool,
    fixture: &PortalFixture,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO billing_subscription_discounts (
            subscription_id, billing_scope_id, subscriber_id, plan_key,
            code_snapshot, label_snapshot, discount_kind, amount_off_cents,
            currency, duration, base_amount_cents, discounted_amount_cents,
            periods_applied, status
        ) VALUES (
            $1, $2, $3, $4, 'PORTAL10', 'Portal promotion', 'amount_off', 10,
            'USD', 'indefinite', 5900, 5890, 1, 'active'
        )
        "#,
    )
    .bind(fixture.subscription_id)
    .bind(fixture.account.billing_scope_id)
    .bind(fixture.subscriber_id)
    .bind(fixture.plan_key.as_str())
    .execute(pool)
    .await?;
    Ok(())
}

async fn insert_saved_discount(
    pool: &PgPool,
    query: &SubscriptionBillingPortalQuery,
) -> Result<(), sqlx::Error> {
    let discount_code_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO billing_subscription_discount_codes (
            id, billing_scope_id, plan_key, code_normalized, display_code,
            label, status, discount_kind, amount_off_cents, currency, duration
        ) VALUES (
            $1, $2, $3, 'SAVED10', 'SAVED10', 'Saved portal promotion',
            'active', 'amount_off', 10, 'USD', 'indefinite'
        )
        "#,
    )
    .bind(discount_code_id)
    .bind(query.billing_scope_id().as_uuid())
    .bind(query.plan_key().as_str())
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
        INSERT INTO billing_subscription_discount_claims (
            id, billing_scope_id, subscriber_id, plan_key, discount_code_id,
            code_snapshot, label_snapshot, discount_kind, amount_off_cents,
            currency, duration, base_amount_cents, discounted_amount_cents, status
        ) VALUES (
            $1, $2, $3, $4, $5, 'SAVED10', 'Saved portal promotion',
            'amount_off', 10, 'USD', 'indefinite', 5900, 5890, 'saved'
        )
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(query.billing_scope_id().as_uuid())
    .bind(query.subscriber_id().as_uuid())
    .bind(query.plan_key().as_str())
    .bind(discount_code_id)
    .execute(pool)
    .await?;
    Ok(())
}

async fn insert_grant(
    pool: &PgPool,
    query: &SubscriptionBillingPortalQuery,
) -> Result<Uuid, sqlx::Error> {
    let grant_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO billing_subscription_grants (
            id, billing_scope_id, subscriber_id, plan_key, grant_kind, reason,
            starts_at, ends_at, granted_by_actor_id
        ) VALUES (
            $1, $2, $3, $4, 'promotion', 'billing portal fixture',
            clock_timestamp() - interval '1 minute',
            clock_timestamp() + interval '1 day', $5
        )
        "#,
    )
    .bind(grant_id)
    .bind(query.billing_scope_id().as_uuid())
    .bind(query.subscriber_id().as_uuid())
    .bind(query.plan_key().as_str())
    .bind(Uuid::now_v7())
    .execute(pool)
    .await?;
    Ok(grant_id)
}

async fn insert_payment_method_update_attempt(
    pool: &PgPool,
    fixture: &PortalFixture,
    attempt_id: Uuid,
    created_at: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO billing_payment_attempts (
            required_gateway_account_mode,
            id, billing_scope_id, subscriber_id, plan_key, subscription_id,
            payment_method_id, attempt_kind, status, idempotency_key,
            request_fingerprint, amount_cents, currency, gateway_account_id,
            gateway_configuration_id, gateway_order_id, gateway_transaction_id,
            gateway_payment_method_reference, gateway_response, gateway_response_code,
            gateway_response_text, gateway_condition,
            billing_first_name, billing_last_name, billing_email,
            payment_method_update_expected_payment_method_id,
            payment_method_update_expected_initial_transaction_id, submitted_at,
            resolved_at, created_at, updated_at
        ) VALUES (
            'live',
            $1, $2, $3, $4, $5, $6,
            'subscription_payment_method_update', 'failed', $7, $8, 0, 'USD',
            $9, $10, $11, 'history-transaction-secret-' || $1::text,
            'history-method-reference-secret', 'history-response-secret',
            'history-response-code-secret', 'history-response-text-secret',
            'history-condition-secret', 'History', 'Private Name',
            'history-private@example.test', $6, $12, $13, $13, $13, $13
        )
        "#,
    )
    .bind(attempt_id)
    .bind(fixture.account.billing_scope_id)
    .bind(fixture.subscriber_id)
    .bind(fixture.plan_key.as_str())
    .bind(fixture.subscription_id)
    .bind(fixture.payment_method_id)
    .bind(format!("history_idempotency_{attempt_id}"))
    .bind(format!("history_fingerprint_{attempt_id}"))
    .bind(fixture.account.gateway_account_id)
    .bind(fixture.account.gateway_configuration_id)
    .bind(format!("history_order_{attempt_id}"))
    .bind(&fixture.initial_transaction_id)
    .bind(created_at)
    .execute(pool)
    .await?;
    Ok(())
}

async fn insert_payment_history_population(
    pool: &PgPool,
    fixture: &PortalFixture,
    newest_at: DateTime<Utc>,
    population: i32,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO billing_payment_attempts (
            required_gateway_account_mode,
            id, billing_scope_id, subscriber_id, plan_key, subscription_id,
            payment_method_id, attempt_kind, status, idempotency_key,
            request_fingerprint, amount_cents, currency, gateway_account_id,
            gateway_configuration_id, gateway_order_id,
            payment_method_update_expected_payment_method_id,
            payment_method_update_expected_initial_transaction_id,
            resolved_at, created_at, updated_at
        )
        SELECT
            'live',
            md5('payment-history-plan-attempt-' || value)::uuid,
            $1,
            $2,
            $3,
            $4,
            $5,
            'subscription_payment_method_update',
            'failed',
            'payment-history-plan-idempotency-' || value,
            'payment-history-plan-fingerprint-' || value,
            0,
            'USD',
            $6,
            $7,
            'payment-history-plan-order-' || value,
            $5,
            $8,
            observed_at,
            observed_at,
            observed_at
        FROM generate_series(1, $10::integer) AS fixture(value)
        CROSS JOIN LATERAL (
            SELECT $9::timestamptz
                - ((value - 1) / 4) * interval '1 second' AS observed_at
        ) AS clock
        "#,
    )
    .bind(fixture.account.billing_scope_id)
    .bind(fixture.subscriber_id)
    .bind(fixture.plan_key.as_str())
    .bind(fixture.subscription_id)
    .bind(fixture.payment_method_id)
    .bind(fixture.account.gateway_account_id)
    .bind(fixture.account.gateway_configuration_id)
    .bind(&fixture.initial_transaction_id)
    .bind(newest_at)
    .bind(population)
    .execute(pool)
    .await?;
    Ok(())
}

async fn insert_renewal_attempt(
    pool: &PgPool,
    fixture: &PortalFixture,
    attempt_id: Uuid,
    created_at: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO billing_payment_attempts (
            required_gateway_account_mode,
            id, billing_scope_id, subscriber_id, plan_key, subscription_id,
            payment_method_id, attempt_kind, status, idempotency_key,
            request_fingerprint, amount_cents, currency, billing_period_start_at,
            billing_period_end_at, gateway_account_id, gateway_configuration_id,
            gateway_order_id, subscription_expected_payment_method_id,
            subscription_expected_initial_transaction_id, subscription_expected_status,
            submitted_at, resolved_at, created_at, updated_at
        ) VALUES (
            'live',
            $1, $2, $3, $4, $5, $6, 'subscription_renewal', 'declined',
            $7, $8, 5900, 'USD', $9, $10, $11, $12, $13, $6, $14, 'active',
            $15, $15, $15, $15
        )
        "#,
    )
    .bind(attempt_id)
    .bind(fixture.account.billing_scope_id)
    .bind(fixture.subscriber_id)
    .bind(fixture.plan_key.as_str())
    .bind(fixture.subscription_id)
    .bind(fixture.payment_method_id)
    .bind(format!("renewal_idempotency_{attempt_id}"))
    .bind(format!("renewal_fingerprint_{attempt_id}"))
    .bind(fixture.period_end)
    .bind(fixture.period_end + Duration::days(30))
    .bind(fixture.account.gateway_account_id)
    .bind(fixture.account.gateway_configuration_id)
    .bind(format!("renewal_order_{attempt_id}"))
    .bind(&fixture.initial_transaction_id)
    .bind(created_at)
    .execute(pool)
    .await?;
    Ok(())
}

async fn insert_host_charge(
    pool: &PgPool,
    fixture: &PortalFixture,
    attempt_id: Uuid,
    created_at: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO billing_payment_attempts (
            required_gateway_account_mode,
            id, billing_scope_id, subscriber_id, host_charge_target_id,
            attempt_kind, status, idempotency_key, request_fingerprint,
            amount_cents, currency, gateway_account_id, gateway_configuration_id,
            gateway_order_id, created_at, updated_at
        ) VALUES (
            'live',
            $1, $2, $3, $4, 'host_charge', 'pending', $5, $6, 1200, 'USD',
            $7, $8, $9, $10, $10
        )
        "#,
    )
    .bind(attempt_id)
    .bind(fixture.account.billing_scope_id)
    .bind(fixture.subscriber_id)
    .bind(Uuid::now_v7())
    .bind(format!("host_idempotency_{attempt_id}"))
    .bind(format!("host_fingerprint_{attempt_id}"))
    .bind(fixture.account.gateway_account_id)
    .bind(fixture.account.gateway_configuration_id)
    .bind(format!("host_order_{attempt_id}"))
    .bind(created_at)
    .execute(pool)
    .await?;
    Ok(())
}
