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
async fn claim_transitions_return_closed_lifecycle_states() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("sr_disc_trans").await?;
    let result = async {
        create_offer_table(&database.pool).await?;
        let scope = BillingScopeId::new(Uuid::now_v7());
        let subscriber = SubscriberId::new(Uuid::now_v7());
        let plan = PlanKey::new("plan_a")?;
        insert_offer(&database.pool, scope, &plan, 5_900).await?;
        let first_code = SubscriptionDiscountCode::new("FIRST10")?;
        let second_code = SubscriptionDiscountCode::new("SECOND10")?;
        for (id, code) in [
            (DiscountCodeId::new(Uuid::now_v7()), first_code.clone()),
            (DiscountCodeId::new(Uuid::now_v7()), second_code.clone()),
        ] {
            create_subscription_discount_code(
                &database.pool,
                &TestOfferStore,
                &SubscriptionDiscountCodeCreation::new(
                    id,
                    scope,
                    plan.clone(),
                    code,
                    None,
                    SubscriptionDiscountKind::AmountOffCents(PositiveDiscountCents::new(500)?),
                    CurrencyCode::new("USD")?,
                    SubscriptionDiscountDuration::Indefinite,
                )?,
            )
            .await?;
        }

        let first_id = DiscountClaimId::new(Uuid::now_v7());
        let first = claim_subscription_discount(
            &database.pool,
            &TestOfferStore,
            &SubscriptionDiscountClaim::new(first_id, scope, subscriber, plan.clone(), first_code),
        )
        .await?;
        let SubscriptionDiscountClaimOutcome::Saved(first) = first else {
            return Err(io::Error::other("first claim was not saved").into());
        };
        assert_eq!(first.state(), &SubscriptionDiscountClaimState::Saved);

        let second = claim_subscription_discount(
            &database.pool,
            &TestOfferStore,
            &SubscriptionDiscountClaim::new(
                DiscountClaimId::new(Uuid::now_v7()),
                scope,
                subscriber,
                plan.clone(),
                second_code,
            ),
        )
        .await?;
        let SubscriptionDiscountClaimOutcome::Saved(second) = second else {
            return Err(io::Error::other("second claim was not saved").into());
        };
        assert_eq!(second.state(), &SubscriptionDiscountClaimState::Saved);

        let row = sqlx::query(
            r#"
            SELECT id, billing_scope_id, subscriber_id, plan_key,
                discount_code_id, code_snapshot, label_snapshot, discount_kind,
                amount_off_cents, percent_off_bps, currency, duration,
                duration_months, base_amount_cents, discounted_amount_cents,
                status, claimed_at, applied_at, applied_subscription_id,
                applied_payment_attempt_id, superseded_at
            FROM billing_subscription_discount_claims
            WHERE id = $1
            "#,
        )
        .bind(first_id.as_uuid())
        .fetch_one(&database.pool)
        .await?;
        let superseded = claim_from_row(&row)?;
        assert!(matches!(
            superseded.state(),
            SubscriptionDiscountClaimState::Superseded { .. }
        ));
        assert_eq!(
            superseded.status(),
            syrup_rail::SubscriptionDiscountClaimStatus::Superseded
        );

        let cleared = clear_subscription_discount(&database.pool, scope, subscriber, &plan).await?;
        let SubscriptionDiscountClearOutcome::Cleared(cleared) = cleared else {
            return Err(io::Error::other("second saved claim was not cleared").into());
        };
        assert_eq!(cleared.state(), &SubscriptionDiscountClaimState::Expired);

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

async fn insert_discount_subscription(
    pool: &PgPool,
    account: crate::test_support::GatewayAccountFixture,
    subscriber: SubscriberId,
    plan: &PlanKey,
    status: &str,
    period_end: chrono::DateTime<Utc>,
    updated_at: chrono::DateTime<Utc>,
) -> Result<Uuid, sqlx::Error> {
    let payment_method_id = Uuid::now_v7();
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
    .bind(subscriber.as_uuid())
    .bind(account.gateway_account_id)
    .bind(format!("discount-method-{payment_method_id}"))
    .execute(pool)
    .await?;

    let subscription_id = Uuid::now_v7();
    let canceled_at = (status == "canceled").then_some(updated_at);
    let unpaid_at = (status == "unpaid").then_some(updated_at);
    let next_payment_attempt_at = matches!(status, "active" | "past_due").then_some(period_end);
    sqlx::query(
        r#"
        INSERT INTO billing_subscriptions (
            required_gateway_account_mode,
            id, billing_scope_id, subscriber_id, plan_key, status,
            gateway_account_id, payment_method_id, amount_cents, currency,
            current_period_start_at, current_period_end_at, next_renewal_at,
            initial_transaction_id, canceled_at, created_at, updated_at,
            phase, recurring_period_kind, recurring_period_count,
            dunning_retry_delays_seconds, dunning_exhaustion,
            past_due_access, next_payment_attempt_at, unpaid_at
        ) VALUES (
            'live', $1, $2, $3, $4, $5, $6, $7, 1000, 'USD',
            $8, $9, $9, $10, $11, $12, $13, 'recurring',
            'calendar_months', 1, ARRAY[]::bigint[], 'remain_past_due',
            'suspend_immediately', $14, $15
        )
        "#,
    )
    .bind(subscription_id)
    .bind(account.billing_scope_id)
    .bind(subscriber.as_uuid())
    .bind(plan.as_str())
    .bind(status)
    .bind(account.gateway_account_id)
    .bind(payment_method_id)
    .bind(period_end - chrono::Duration::days(30))
    .bind(period_end)
    .bind(format!("discount-transaction-{subscription_id}"))
    .bind(canceled_at)
    .bind(updated_at - chrono::Duration::days(1))
    .bind(updated_at)
    .bind(next_payment_attempt_at)
    .bind(unpaid_at)
    .execute(pool)
    .await?;
    Ok(subscription_id)
}

async fn assert_row_update_times_out(
    pool: &PgPool,
    subscription_id: Uuid,
) -> Result<(), Box<dyn Error>> {
    let mut contender = pool.begin().await?;
    sqlx::query("SET LOCAL lock_timeout = '100ms'")
        .execute(&mut *contender)
        .await?;
    let error = sqlx::query(
        "UPDATE billing_subscriptions SET updated_at = clock_timestamp() WHERE id = $1",
    )
    .bind(subscription_id)
    .execute(&mut *contender)
    .await
    .expect_err("selected subscription row must remain locked");
    let code = error
        .as_database_error()
        .and_then(|database_error| database_error.code())
        .map(|code| code.into_owned());
    contender.rollback().await?;
    if code.as_deref() != Some("55P03") {
        return Err(
            io::Error::other(format!("expected selected-row lock timeout, got {error}")).into(),
        );
    }
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
                required_gateway_account_mode,
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
                'live',
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
