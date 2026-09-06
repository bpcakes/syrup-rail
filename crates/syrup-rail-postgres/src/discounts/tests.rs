use std::{error::Error, io};

use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use sqlx::{PgConnection, Row};
use syrup_rail::{
    BillingScopeId, ChargeAmount, CurrencyCode, DiscountClaimId, DiscountCodeId, DunningExhaustion,
    DunningSchedule, LimitedDiscountMonths, PastDueAccessPolicy, PaymentAttemptId,
    PercentOffBasisPoints, PlanKey, PositiveDiscountCents, RecurringSubscriptionTerms,
    RenewalFailurePolicy, SubscriberId, SubscriptionDiscountClaim,
    SubscriptionDiscountClaimOutcome, SubscriptionDiscountClaimState, SubscriptionDiscountCode,
    SubscriptionDiscountCodeCreation, SubscriptionDiscountCodeStatus,
    SubscriptionDiscountCodeUpdate, SubscriptionDiscountDuration, SubscriptionDiscountKind,
    SubscriptionId, SubscriptionOffer, SubscriptionPeriodRule, SubscriptionStart,
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

#[tokio::test]
async fn claim_hydrator_maps_every_flat_lifecycle_shape() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("sr_disc_hydrator").await?;
    let result = async {
        let applied_at = Utc.with_ymd_and_hms(2026, 8, 2, 0, 0, 0).unwrap();
        let superseded_at = Utc.with_ymd_and_hms(2026, 8, 3, 0, 0, 0).unwrap();
        let applied_subscription_id = Uuid::from_u128(105);
        let applied_payment_attempt_id = Uuid::from_u128(106);
        let cases = vec![
            (
                "saved",
                "saved",
                None,
                None,
                None,
                None,
                SubscriptionDiscountClaimState::Saved,
            ),
            (
                "applied",
                "applied",
                Some(applied_at),
                Some(applied_subscription_id),
                Some(applied_payment_attempt_id),
                None,
                SubscriptionDiscountClaimState::Applied {
                    applied_at,
                    subscription_id: SubscriptionId::new(applied_subscription_id),
                    payment_attempt_id: PaymentAttemptId::new(applied_payment_attempt_id),
                },
            ),
            (
                "superseded",
                "superseded",
                None,
                None,
                None,
                Some(superseded_at),
                SubscriptionDiscountClaimState::Superseded { superseded_at },
            ),
            (
                "expired",
                "expired",
                None,
                None,
                None,
                None,
                SubscriptionDiscountClaimState::Expired,
            ),
        ];

        for (
            name,
            status,
            row_applied_at,
            row_subscription_id,
            row_payment_attempt_id,
            row_superseded_at,
            expected_state,
        ) in cases
        {
            let row = sqlx::query(
                r#"
                SELECT
                    $1::uuid AS id,
                    $2::uuid AS billing_scope_id,
                    $3::uuid AS subscriber_id,
                    $4::text AS plan_key,
                    $5::uuid AS discount_code_id,
                    $6::text AS code_snapshot,
                    $7::text AS label_snapshot,
                    $8::text AS discount_kind,
                    $9::integer AS amount_off_cents,
                    $10::integer AS percent_off_bps,
                    $11::text AS currency,
                    $12::text AS duration,
                    $13::integer AS duration_months,
                    $14::integer AS base_amount_cents,
                    $15::integer AS discounted_amount_cents,
                    $16::text AS status,
                    $17::timestamptz AS claimed_at,
                    $18::timestamptz AS applied_at,
                    $19::uuid AS applied_subscription_id,
                    $20::uuid AS applied_payment_attempt_id,
                    $21::timestamptz AS superseded_at
                "#,
            )
            .bind(Uuid::from_u128(101))
            .bind(Uuid::from_u128(102))
            .bind(Uuid::from_u128(103))
            .bind("base_subscription")
            .bind(Uuid::from_u128(104))
            .bind("SAVE10")
            .bind(Some("Launch offer".to_owned()))
            .bind("amount_off")
            .bind(Some(100_i32))
            .bind(None::<i32>)
            .bind("USD")
            .bind("indefinite")
            .bind(None::<i32>)
            .bind(1_000_i32)
            .bind(900_i32)
            .bind(status)
            .bind(Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap())
            .bind(row_applied_at)
            .bind(row_subscription_id)
            .bind(row_payment_attempt_id)
            .bind(row_superseded_at)
            .fetch_one(&database.pool)
            .await?;
            let claim = claim_from_row(&row)?;
            assert_eq!(claim.state(), &expected_state, "{name}");
            assert_eq!(claim.status(), expected_state.status(), "{name}");
        }

        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
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
async fn cancellation_and_discount_workflows_contend_on_the_canonical_subscription_aggregate()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("sr_disc_agg_lock").await?;
    let result = async {
        let subscriber_id = SubscriberId::new(Uuid::now_v7());
        let plan_key = PlanKey::new("base_subscription")?;
        let mut holder = database.pool.begin().await?;
        let cancellation = syrup_rail::CancelSubscription::new(
            BillingScopeId::new(Uuid::now_v7()),
            subscriber_id,
            plan_key.clone(),
        );
        let held = crate::cancel_subscription_in_transaction(&mut holder, &cancellation).await?;
        if held != syrup_rail::CancelSubscriptionOutcome::NotFound {
            return Err(io::Error::other(
                "cancellation workflow did not retain its empty aggregate transaction",
            )
            .into());
        }

        let mut contender = database.pool.begin().await?;
        let error = clear_subscription_discount_in_transaction(
            &mut contender,
            BillingScopeId::new(Uuid::now_v7()),
            subscriber_id,
            &plan_key,
        )
        .await
        .expect_err("canonical aggregate holder must block discount clearing");
        contender.rollback().await?;
        holder.rollback().await?;
        let SubscriptionDiscountOperationError::Sql(sqlx::Error::Database(error)) = error else {
            return Err(
                io::Error::other(format!("expected discount lock timeout, got {error:?}")).into(),
            );
        };
        if error.code().as_deref() != Some("55P03") {
            return Err(io::Error::other(format!(
                "expected discount lock timeout SQLSTATE 55P03, got {:?}",
                error.code()
            ))
            .into());
        }
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn current_subscription_locking_query_preserves_the_complete_view_matrix_and_scope()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("sr_disc_current").await?;
    let result = async {
        let account = create_gateway_account(&database.pool, "test_gateway").await?;
        let scope = BillingScopeId::new(account.billing_scope_id);
        let plan = PlanKey::new("matrix_plan")?;
        let now: chrono::DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
            .fetch_one(&database.pool)
            .await?;
        for (name, status, period_end, expected) in [
            ("active", "active", now + chrono::Duration::hours(1), true),
            (
                "past due",
                "past_due",
                now + chrono::Duration::hours(1),
                true,
            ),
            (
                "paid-through canceled",
                "canceled",
                now + chrono::Duration::hours(1),
                true,
            ),
            (
                "expired canceled",
                "canceled",
                now - chrono::Duration::seconds(1),
                false,
            ),
            ("unpaid", "unpaid", now + chrono::Duration::hours(1), false),
        ] {
            let subscriber = SubscriberId::new(Uuid::now_v7());
            insert_discount_subscription(
                &database.pool,
                account,
                subscriber,
                &plan,
                status,
                period_end,
                now,
            )
            .await?;
            let claim = SubscriptionDiscountClaim::new(
                DiscountClaimId::new(Uuid::now_v7()),
                scope,
                subscriber,
                plan.clone(),
                SubscriptionDiscountCode::new("MATRIX10")?,
            );
            let mut transaction = database.pool.begin().await?;
            assert_eq!(
                current_subscription_exists(&mut transaction, &claim).await?,
                expected,
                "{name}"
            );
            transaction.rollback().await?;
        }

        let subscriber = SubscriberId::new(Uuid::now_v7());
        insert_discount_subscription(
            &database.pool,
            account,
            subscriber,
            &plan,
            "active",
            now + chrono::Duration::hours(1),
            now,
        )
        .await?;
        for (name, claim_scope, claim_plan) in [
            (
                "scope isolation",
                BillingScopeId::new(Uuid::now_v7()),
                plan.clone(),
            ),
            ("plan isolation", scope, PlanKey::new("other_plan")?),
        ] {
            let claim = SubscriptionDiscountClaim::new(
                DiscountClaimId::new(Uuid::now_v7()),
                claim_scope,
                subscriber,
                claim_plan,
                SubscriptionDiscountCode::new("MATRIX10")?,
            );
            let mut transaction = database.pool.begin().await?;
            assert!(
                !current_subscription_exists(&mut transaction, &claim).await?,
                "{name}"
            );
            transaction.rollback().await?;
        }

        let no_subscription = SubscriptionDiscountClaim::new(
            DiscountClaimId::new(Uuid::now_v7()),
            scope,
            SubscriberId::new(Uuid::now_v7()),
            plan,
            SubscriptionDiscountCode::new("MATRIX10")?,
        );
        let mut transaction = database.pool.begin().await?;
        assert!(!current_subscription_exists(&mut transaction, &no_subscription).await?);
        transaction.rollback().await?;
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn current_subscription_query_locks_only_the_highest_ranked_row() -> Result<(), Box<dyn Error>>
{
    let database = TestDatabase::start("sr_disc_lockrow").await?;
    let result = async {
        let account = create_gateway_account(&database.pool, "test_gateway").await?;
        let scope = BillingScopeId::new(account.billing_scope_id);
        let subscriber = SubscriberId::new(Uuid::now_v7());
        let plan = PlanKey::new("footprint_plan")?;
        let now: chrono::DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
            .fetch_one(&database.pool)
            .await?;
        let active_id = insert_discount_subscription(
            &database.pool,
            account,
            subscriber,
            &plan,
            "active",
            now + chrono::Duration::hours(1),
            now - chrono::Duration::minutes(1),
        )
        .await?;
        let canceled_id = insert_discount_subscription(
            &database.pool,
            account,
            subscriber,
            &plan,
            "canceled",
            now + chrono::Duration::hours(1),
            now,
        )
        .await?;
        let claim = SubscriptionDiscountClaim::new(
            DiscountClaimId::new(Uuid::now_v7()),
            scope,
            subscriber,
            plan,
            SubscriptionDiscountCode::new("LOCK10")?,
        );
        let mut selector = database.pool.begin().await?;
        assert!(current_subscription_exists(&mut selector, &claim).await?);

        let mut unchosen_writer = database.pool.begin().await?;
        sqlx::query("SET LOCAL lock_timeout = '100ms'")
            .execute(&mut *unchosen_writer)
            .await?;
        sqlx::query(
            "UPDATE billing_subscriptions SET updated_at = clock_timestamp() WHERE id = $1",
        )
        .bind(canceled_id)
        .execute(&mut *unchosen_writer)
        .await?;
        unchosen_writer.rollback().await?;

        assert_row_update_times_out(&database.pool, active_id).await?;
        selector.rollback().await?;
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn current_subscription_query_rechecks_a_real_concurrent_status_change()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("sr_disc_rankchg").await?;
    let result = async {
        let account = create_gateway_account(&database.pool, "test_gateway").await?;
        let scope = BillingScopeId::new(account.billing_scope_id);
        let subscriber = SubscriberId::new(Uuid::now_v7());
        let plan = PlanKey::new("rank_change_plan")?;
        let now: chrono::DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
            .fetch_one(&database.pool)
            .await?;
        let past_due_id = insert_discount_subscription(
            &database.pool,
            account,
            subscriber,
            &plan,
            "past_due",
            now + chrono::Duration::hours(1),
            now,
        )
        .await?;
        let canceled_id = insert_discount_subscription(
            &database.pool,
            account,
            subscriber,
            &plan,
            "canceled",
            now + chrono::Duration::hours(1),
            now - chrono::Duration::minutes(1),
        )
        .await?;
        let claim = SubscriptionDiscountClaim::new(
            DiscountClaimId::new(Uuid::now_v7()),
            scope,
            subscriber,
            plan,
            SubscriptionDiscountCode::new("RANK10")?,
        );

        let mut writer = database.pool.begin().await?;
        sqlx::query(
            r#"
            UPDATE billing_subscriptions
            SET status = 'unpaid', unpaid_at = clock_timestamp(),
                next_payment_attempt_at = NULL, updated_at = clock_timestamp()
            WHERE id = $1
            "#,
        )
        .bind(past_due_id)
        .execute(&mut *writer)
        .await?;

        let mut selector = database.pool.begin().await?;
        let selected = {
            let selection = current_subscription_exists(&mut selector, &claim);
            tokio::pin!(selection);
            assert!(
                timeout(Duration::from_millis(100), &mut selection)
                    .await
                    .is_err(),
                "selection did not wait for the concurrently changing ranked row"
            );
            writer.commit().await?;
            selection.await?
        };
        assert!(selected);
        assert_row_update_times_out(&database.pool, canceled_id).await?;
        selector.rollback().await?;

        let status: String =
            sqlx::query_scalar("SELECT status FROM billing_subscriptions WHERE id = $1")
                .bind(past_due_id)
                .fetch_one(&database.pool)
                .await?;
        assert_eq!(status, "unpaid");
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
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

include!("tests/claim_lifecycle.rs");
