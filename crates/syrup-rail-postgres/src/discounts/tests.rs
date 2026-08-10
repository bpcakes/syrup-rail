use std::{error::Error, io};

use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use sqlx::{PgConnection, Row};
use syrup_rail::{
    BillingScopeId, ChargeAmount, CurrencyCode, DiscountClaimId, DiscountCodeId, DunningExhaustion,
    DunningSchedule, LimitedDiscountMonths, PastDueAccessPolicy, PercentOffBasisPoints, PlanKey,
    PositiveDiscountCents, RecurringSubscriptionTerms, RenewalFailurePolicy, SubscriberId,
    SubscriptionDiscountClaim, SubscriptionDiscountClaimOutcome, SubscriptionDiscountCode,
    SubscriptionDiscountCodeCreation, SubscriptionDiscountCodeStatus,
    SubscriptionDiscountCodeUpdate, SubscriptionDiscountDuration, SubscriptionDiscountKind,
    SubscriptionOffer, SubscriptionPeriodRule, SubscriptionStart,
};
use tokio::time::{Duration, timeout};
use uuid::Uuid;

use super::*;
use crate::test_support::{TestDatabase, create_gateway_account};

struct TestOfferStore;

#[test]
fn limited_discount_cadence_failure_has_a_dedicated_error() {
    let usd = CurrencyCode::new("USD").expect("valid test currency");
    let offer = SubscriptionOffer::new(
        PlanKey::new("fixed_plan").expect("valid test plan"),
        RecurringSubscriptionTerms::new(
            ChargeAmount::new(5_900, usd).expect("valid test charge"),
            SubscriptionPeriodRule::fixed_days(30).expect("valid test cadence"),
        ),
        SubscriptionStart::RecurringImmediately,
        RenewalFailurePolicy::new(
            DunningSchedule::default(),
            DunningExhaustion::RemainPastDue,
            PastDueAccessPolicy::SuspendImmediately,
        ),
    )
    .expect("valid fixed-period offer");
    let duration = SubscriptionDiscountDuration::LimitedMonths(
        LimitedDiscountMonths::new(3).expect("valid limited duration"),
    );

    assert!(matches!(
        validate_discount_cadence(duration, &offer),
        Err(SubscriptionDiscountOperationError::LimitedDiscountCadence)
    ));
    let now = Utc
        .with_ymd_and_hms(2026, 8, 9, 0, 0, 0)
        .single()
        .expect("valid test time");
    let code = SubscriptionDiscountCodeRecord::new(
        DiscountCodeId::new(Uuid::from_u128(1)),
        BillingScopeId::new(Uuid::from_u128(2)),
        PlanKey::new("fixed_plan").expect("valid test plan"),
        SubscriptionDiscountCode::new("SAVE10").expect("valid test code"),
        "SAVE10".to_owned(),
        None,
        SubscriptionDiscountCodeStatus::Active,
        SubscriptionDiscountKind::AmountOffCents(
            PositiveDiscountCents::new(100).expect("valid test discount"),
        ),
        usd,
        duration,
        now,
        now,
    )
    .expect("valid discount code record");
    assert!(matches!(
        quote_for_offer(code, &offer),
        Err(SubscriptionDiscountOperationError::LimitedDiscountCadence)
    ));
    assert!(validate_discount_cadence(SubscriptionDiscountDuration::Indefinite, &offer).is_ok());
}

#[async_trait]
impl SubscriptionOfferStore for TestOfferStore {
    async fn lock_current_offer(
        &self,
        connection: &mut PgConnection,
        billing_scope_id: BillingScopeId,
        plan_key: &PlanKey,
    ) -> Result<Option<SubscriptionOffer>, sqlx::Error> {
        let row = sqlx::query(
            r#"
                SELECT amount_cents, currency, recurring_period_kind,
                    recurring_period_count
                FROM test_subscription_offers
                WHERE billing_scope_id = $1 AND plan_key = $2
                FOR NO KEY UPDATE
                "#,
        )
        .bind(billing_scope_id.as_uuid())
        .bind(plan_key.as_str())
        .fetch_optional(connection)
        .await?;
        row.map(|row| {
            let currency = CurrencyCode::new(row.try_get::<String, _>("currency")?.as_str())
                .map_err(|error| sqlx::Error::Decode(Box::new(error)))?;
            let charge = ChargeAmount::new(row.try_get("amount_cents")?, currency)
                .map_err(|error| sqlx::Error::Decode(Box::new(error)))?;
            let count =
                u16::try_from(row.try_get::<i32, _>("recurring_period_count")?).map_err(|_| {
                    sqlx::Error::Decode(Box::new(io::Error::other(
                        "invalid test offer period count",
                    )))
                })?;
            let period = match row.try_get::<String, _>("recurring_period_kind")?.as_str() {
                "calendar_months" => SubscriptionPeriodRule::calendar_months(count),
                "fixed_days" => SubscriptionPeriodRule::fixed_days(count),
                _ => {
                    return Err(sqlx::Error::Decode(Box::new(io::Error::other(
                        "invalid test offer period kind",
                    ))));
                }
            }
            .map_err(|error| sqlx::Error::Decode(Box::new(error)))?;
            SubscriptionOffer::new(
                plan_key.clone(),
                RecurringSubscriptionTerms::new(charge, period),
                SubscriptionStart::RecurringImmediately,
                RenewalFailurePolicy::new(
                    DunningSchedule::default(),
                    DunningExhaustion::RemainPastDue,
                    PastDueAccessPolicy::SuspendImmediately,
                ),
            )
            .map_err(|error| sqlx::Error::Decode(Box::new(error)))
        })
        .transpose()
    }
}

#[tokio::test]
async fn quote_validation_uses_the_callers_connection_and_blocks_price_updates()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("sr_disc_lock").await?;
    let result = async {
            create_offer_table(&database.pool).await?;
            let scope = BillingScopeId::new(Uuid::now_v7());
            let plan = PlanKey::new("plan_a")?;
            insert_offer(&database.pool, scope, &plan, 5_900).await?;

            let mut transaction = database.pool.begin().await?;
            let quote = validate_subscription_discount_code_in_transaction(
                &mut transaction,
                &TestOfferStore,
                scope,
                &plan,
                &SubscriptionDiscountCode::new("MISSING")?,
            )
            .await?;
            if quote.is_some() {
                return Err(io::Error::other("missing code unexpectedly produced a quote").into());
            }

            let pool = database.pool.clone();
            let plan_for_update = plan.clone();
            let mut update = tokio::spawn(async move {
                sqlx::query(
                    "UPDATE test_subscription_offers SET amount_cents = 6900 WHERE billing_scope_id = $1 AND plan_key = $2",
                )
                .bind(scope.as_uuid())
                .bind(plan_for_update.as_str())
                .execute(&pool)
                .await
            });
            if timeout(Duration::from_millis(100), &mut update).await.is_ok() {
                return Err(io::Error::other("price update escaped the offer row lock").into());
            }
            transaction.commit().await?;
            update.await??;

            let amount: i32 = sqlx::query_scalar(
                "SELECT amount_cents FROM test_subscription_offers WHERE billing_scope_id = $1 AND plan_key = $2",
            )
            .bind(scope.as_uuid())
            .bind(plan.as_str())
            .fetch_one(&database.pool)
            .await?;
            if amount != 6_900 {
                return Err(io::Error::other("blocked price update did not resume").into());
            }
            Ok::<_, Box<dyn Error>>(())
        }
        .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn discount_code_and_claim_policy_is_exact_plan_scoped_and_snapshot_preserving()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("sr_disc_life").await?;
    let result = async {
        create_offer_table(&database.pool).await?;
        let scope = BillingScopeId::new(Uuid::now_v7());
        let subscriber = SubscriberId::new(Uuid::now_v7());
        let plan_a = PlanKey::new("plan_a")?;
        let plan_b = PlanKey::new("plan_b")?;
        insert_offer(&database.pool, scope, &plan_a, 5_900).await?;
        insert_offer(&database.pool, scope, &plan_b, 9_900).await?;
        let usd = CurrencyCode::new("USD")?;

        let invalid_id = DiscountCodeId::new(Uuid::now_v7());
        let invalid = SubscriptionDiscountCodeCreation::new(
            invalid_id,
            scope,
            plan_a.clone(),
            SubscriptionDiscountCode::new("TOOLARGE")?,
            None,
            SubscriptionDiscountKind::AmountOffCents(PositiveDiscountCents::new(6_000)?),
            usd,
            SubscriptionDiscountDuration::Indefinite,
        )?;
        if !matches!(
            create_subscription_discount_code(&database.pool, &TestOfferStore, &invalid).await,
            Err(SubscriptionDiscountOperationError::InvalidConfiguration)
        ) {
            return Err(io::Error::other("invalid offer-relative terms were accepted").into());
        }
        let invalid_count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM billing_subscription_discount_codes WHERE id = $1",
        )
        .bind(invalid_id.as_uuid())
        .fetch_one(&database.pool)
        .await?;
        if invalid_count != 0 {
            return Err(io::Error::other("invalid code escaped its caller transaction").into());
        }

        let code_a_id = DiscountCodeId::new(Uuid::now_v7());
        let code = SubscriptionDiscountCode::new("SAVE25")?;
        let creation = SubscriptionDiscountCodeCreation::new(
            code_a_id,
            scope,
            plan_a.clone(),
            code.clone(),
            Some("  Launch offer  ".into()),
            SubscriptionDiscountKind::PercentOffBasisPoints(PercentOffBasisPoints::new(2_500)?),
            usd,
            SubscriptionDiscountDuration::Indefinite,
        )?;
        let created =
            create_subscription_discount_code(&database.pool, &TestOfferStore, &creation).await?;
        if created.label() != Some("Launch offer") {
            return Err(io::Error::other("created record lost canonical terms").into());
        }
        let created_quote = validate_subscription_discount_code(
            &database.pool,
            &TestOfferStore,
            scope,
            &plan_a,
            &code,
        )
        .await?
        .ok_or_else(|| io::Error::other("created code was not quoteable"))?;
        if created_quote.base_charge().cents() != 5_900
            || created_quote.discounted_charge().cents() != 4_425
        {
            return Err(io::Error::other("created code quote lost offer terms").into());
        }

        let first_claim_id = DiscountClaimId::new(Uuid::now_v7());
        let first_claim = SubscriptionDiscountClaim::new(
            first_claim_id,
            scope,
            subscriber,
            plan_a.clone(),
            code.clone(),
        );
        let first =
            claim_subscription_discount(&database.pool, &TestOfferStore, &first_claim).await?;
        let SubscriptionDiscountClaimOutcome::Saved(first) = first else {
            return Err(io::Error::other("first claim was not saved").into());
        };
        if first.snapshot().discounted_charge().cents() != 4_425 {
            return Err(io::Error::other("claim did not snapshot locked offer").into());
        }

        let update = SubscriptionDiscountCodeUpdate::new(
            code_a_id,
            scope,
            plan_a.clone(),
            Some("Changed terms".into()),
            SubscriptionDiscountCodeStatus::Active,
            SubscriptionDiscountKind::AmountOffCents(PositiveDiscountCents::new(900)?),
            usd,
            SubscriptionDiscountDuration::Indefinite,
        )?;
        update_subscription_discount_code(&database.pool, &TestOfferStore, &update)
            .await?
            .ok_or_else(|| io::Error::other("updated code disappeared"))?;

        let replay = SubscriptionDiscountClaim::new(
            DiscountClaimId::new(Uuid::now_v7()),
            scope,
            subscriber,
            plan_a.clone(),
            code.clone(),
        );
        let replay = claim_subscription_discount(&database.pool, &TestOfferStore, &replay).await?;
        let SubscriptionDiscountClaimOutcome::Existing(replay) = replay else {
            return Err(io::Error::other("same canonical code did not replay").into());
        };
        if replay.id() != first_claim_id
            || replay.snapshot().discounted_charge().cents() != 4_425
            || replay.snapshot().label() != Some("Launch offer")
        {
            return Err(io::Error::other("same-code replay reinterpreted the snapshot").into());
        }

        let plan_b_creation = SubscriptionDiscountCodeCreation::new(
            DiscountCodeId::new(Uuid::now_v7()),
            scope,
            plan_b.clone(),
            code.clone(),
            None,
            SubscriptionDiscountKind::AmountOffCents(PositiveDiscountCents::new(900)?),
            usd,
            SubscriptionDiscountDuration::Indefinite,
        )?;
        create_subscription_discount_code(&database.pool, &TestOfferStore, &plan_b_creation)
            .await?;
        let plan_b_claim = SubscriptionDiscountClaim::new(
            DiscountClaimId::new(Uuid::now_v7()),
            scope,
            subscriber,
            plan_b.clone(),
            code,
        );
        let plan_b_saved =
            claim_subscription_discount(&database.pool, &TestOfferStore, &plan_b_claim).await?;
        if !matches!(plan_b_saved, SubscriptionDiscountClaimOutcome::Saved(_)) {
            return Err(io::Error::other("another plan did not own an independent claim").into());
        }

        disable_subscription_discount_code(&database.pool, scope, &plan_a, code_a_id)
            .await?
            .ok_or_else(|| io::Error::other("disabled code disappeared"))?;
        if saved_subscription_discount_claim(&database.pool, scope, subscriber, &plan_a)
            .await?
            .is_some()
            || saved_subscription_discount_claim(&database.pool, scope, subscriber, &plan_b)
                .await?
                .is_none()
        {
            return Err(io::Error::other("disable crossed the exact plan boundary").into());
        }
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn historical_code_records_remain_listable_and_disableable_after_cadence_drift()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("sr_disc_drift").await?;
    let result = async {
        create_offer_table(&database.pool).await?;
        let scope = BillingScopeId::new(Uuid::now_v7());
        let plan = PlanKey::new("monthly_plan")?;
        insert_offer(&database.pool, scope, &plan, 5_900).await?;
        let code_id = DiscountCodeId::new(Uuid::now_v7());
        let code = SubscriptionDiscountCode::new("MONTHS10")?;
        let creation = SubscriptionDiscountCodeCreation::new(
            code_id,
            scope,
            plan.clone(),
            code.clone(),
            Some("Historical monthly offer".into()),
            SubscriptionDiscountKind::AmountOffCents(PositiveDiscountCents::new(1_000)?),
            CurrencyCode::new("USD")?,
            SubscriptionDiscountDuration::LimitedMonths(LimitedDiscountMonths::new(3)?),
        )?;
        create_subscription_discount_code(&database.pool, &TestOfferStore, &creation).await?;

        sqlx::query(
            r#"
                UPDATE test_subscription_offers
                SET recurring_period_kind = 'fixed_days', recurring_period_count = 30
                WHERE billing_scope_id = $1 AND plan_key = $2
                "#,
        )
        .bind(scope.as_uuid())
        .bind(plan.as_str())
        .execute(&database.pool)
        .await?;

        assert!(matches!(
            validate_subscription_discount_code(
                &database.pool,
                &TestOfferStore,
                scope,
                &plan,
                &code,
            )
            .await,
            Err(SubscriptionDiscountOperationError::LimitedDiscountCadence)
        ));
        let listed = list_subscription_discount_codes(&database.pool, scope, &plan).await?;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id(), code_id);
        assert_eq!(listed[0].status(), SubscriptionDiscountCodeStatus::Active);

        let disabled_update = SubscriptionDiscountCodeUpdate::new(
            code_id,
            scope,
            plan.clone(),
            Some("Historical monthly offer".into()),
            SubscriptionDiscountCodeStatus::Disabled,
            SubscriptionDiscountKind::AmountOffCents(PositiveDiscountCents::new(1_000)?),
            CurrencyCode::new("USD")?,
            SubscriptionDiscountDuration::LimitedMonths(LimitedDiscountMonths::new(3)?),
        )?;
        let updated =
            update_subscription_discount_code(&database.pool, &TestOfferStore, &disabled_update)
                .await?
                .ok_or_else(|| io::Error::other("historical code disappeared while updating"))?;
        assert_eq!(updated.status(), SubscriptionDiscountCodeStatus::Disabled);

        let disabled = disable_subscription_discount_code(&database.pool, scope, &plan, code_id)
            .await?
            .ok_or_else(|| io::Error::other("historical code disappeared while disabling"))?;
        assert_eq!(disabled.status(), SubscriptionDiscountCodeStatus::Disabled);
        assert!(matches!(
            disabled.duration(),
            SubscriptionDiscountDuration::LimitedMonths(_)
        ));
        let listed = list_subscription_discount_codes(&database.pool, scope, &plan).await?;
        assert_eq!(listed, vec![disabled]);
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn clear_is_blocked_by_an_exact_plan_initial_attempt() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("sr_disc_clear").await?;
    let result = async {
        create_offer_table(&database.pool).await?;
        let gateway = create_gateway_account(&database.pool, "test_gateway").await?;
        let scope = BillingScopeId::new(gateway.billing_scope_id);
        let subscriber = SubscriberId::new(Uuid::now_v7());
        let plan = PlanKey::new("plan_a")?;
        insert_offer(&database.pool, scope, &plan, 5_900).await?;
        let creation = SubscriptionDiscountCodeCreation::new(
            DiscountCodeId::new(Uuid::now_v7()),
            scope,
            plan.clone(),
            SubscriptionDiscountCode::new("CLEAR10")?,
            None,
            SubscriptionDiscountKind::AmountOffCents(PositiveDiscountCents::new(500)?),
            CurrencyCode::new("USD")?,
            SubscriptionDiscountDuration::Indefinite,
        )?;
        create_subscription_discount_code(&database.pool, &TestOfferStore, &creation).await?;
        let claim = SubscriptionDiscountClaim::new(
            DiscountClaimId::new(Uuid::now_v7()),
            scope,
            subscriber,
            plan.clone(),
            SubscriptionDiscountCode::new("CLEAR10")?,
        );
        claim_subscription_discount(&database.pool, &TestOfferStore, &claim).await?;
        insert_pending_initial_attempt(
            &database.pool,
            gateway.billing_scope_id,
            subscriber.into_uuid(),
            plan.as_str(),
            gateway.gateway_account_id,
            gateway.gateway_configuration_id,
        )
        .await?;

        let blocked = clear_subscription_discount(&database.pool, scope, subscriber, &plan).await?;
        if blocked != SubscriptionDiscountClearOutcome::BlockedByInitialAttempt {
            return Err(io::Error::other("initial checkout did not block clear").into());
        }
        if saved_subscription_discount_claim(&database.pool, scope, subscriber, &plan)
            .await?
            .is_none()
        {
            return Err(io::Error::other("blocked clear expired the saved claim").into());
        }
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

async fn create_offer_table(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
            CREATE TABLE test_subscription_offers (
                billing_scope_id uuid NOT NULL,
                plan_key text NOT NULL,
                amount_cents integer NOT NULL,
                currency text NOT NULL,
                recurring_period_kind text NOT NULL DEFAULT 'calendar_months',
                recurring_period_count integer NOT NULL DEFAULT 1,
                PRIMARY KEY (billing_scope_id, plan_key)
            )
            "#,
    )
    .execute(pool)
    .await?;
    Ok(())
}

async fn insert_offer(
    pool: &PgPool,
    scope: BillingScopeId,
    plan: &PlanKey,
    amount_cents: i32,
) -> Result<(), sqlx::Error> {
    sqlx::query(
            "INSERT INTO test_subscription_offers (billing_scope_id, plan_key, amount_cents, currency) VALUES ($1, $2, $3, 'USD')",
        )
        .bind(scope.as_uuid())
        .bind(plan.as_str())
        .bind(amount_cents)
        .execute(pool)
        .await?;
    Ok(())
}

async fn insert_pending_initial_attempt(
    pool: &PgPool,
    scope: Uuid,
    subscriber: Uuid,
    plan: &str,
    gateway_account: Uuid,
    gateway_configuration: Uuid,
) -> Result<(), sqlx::Error> {
    let attempt = Uuid::now_v7();
    sqlx::query(
        r#"
            INSERT INTO billing_payment_attempts (
                id, billing_scope_id, subscriber_id, plan_key, attempt_kind,
                status, idempotency_key, request_fingerprint, amount_cents,
                currency, gateway_account_id, gateway_configuration_id,
                gateway_order_id, subscription_initial_terms_version,
                subscription_initial_start_kind,
                subscription_initial_recurring_base_amount_cents,
                subscription_initial_recurring_period_kind,
                subscription_initial_recurring_period_count,
                subscription_initial_dunning_retry_delays_seconds,
                subscription_initial_dunning_exhaustion,
                subscription_initial_past_due_access
            ) VALUES (
                $1, $2, $3, $4, 'subscription_initial', 'pending', $5, $6,
                100, 'USD', $7, $8, $9, 2, 'recurring_immediately', 100,
                'calendar_months', 1, ARRAY[]::bigint[],
                'remain_past_due', 'suspend_immediately'
            )
            "#,
    )
    .bind(attempt)
    .bind(scope)
    .bind(subscriber)
    .bind(plan)
    .bind(format!("discount-{}", attempt.simple()))
    .bind(format!("initial:{plan}:100:USD"))
    .bind(gateway_account)
    .bind(gateway_configuration)
    .bind(format!("discount-order-{}", attempt.simple()))
    .execute(pool)
    .await?;
    Ok(())
}
