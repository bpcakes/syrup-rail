use super::*;

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
