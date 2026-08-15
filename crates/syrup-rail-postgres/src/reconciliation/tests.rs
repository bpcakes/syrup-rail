use std::error::Error;

use syrup_rail::{
    BillingScopeId, GatewayAccountId, GatewayAccountRegistration, GatewayConfigurationId,
    GatewayProviderKey, PaymentAttemptKind,
};
use uuid::Uuid;

use super::{
    ExactQueryObservation, RECONCILIATION_PHASE_BATCH_SIZE, apply_exact_query_observation,
    claim_exact_reconciliation_attempts, classify_pending_processor_charges,
    fail_stale_unsubmitted_payment_method_replacements,
    fail_stale_unsubmitted_subscription_charges, fail_stale_unsubmitted_subscription_enrollments,
    reconciliation_gateway_accounts,
};
use crate::{
    find_payment_attempt_by_id_in_transaction, register_gateway_account,
    test_support::{TestDatabase, create_gateway_account},
};

mod local_attempts;

#[tokio::test]
async fn reconciliation_candidate_scan_is_complete_unbounded_and_deterministic()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("sr_recon_scan").await?;
    let result = async {
        let provider = GatewayProviderKey::new("test_gateway")?;
        let mut expected = Vec::new();
        let mut transaction = database.pool.begin().await?;
        for position in (0_u128..101).rev() {
            let scope = BillingScopeId::new(Uuid::from_u128(1 + position));
            let account = GatewayAccountId::new(Uuid::from_u128(2_000 - position));
            register_gateway_account(
                &mut transaction,
                &GatewayAccountRegistration::new(
                    scope,
                    account,
                    provider.clone(),
                    GatewayConfigurationId::new(Uuid::from_u128(2_000 + position)),
                ),
            )
            .await?;
            expected.push((scope, account));
        }
        transaction.commit().await?;
        expected.sort_unstable();

        let candidates = reconciliation_gateway_accounts(&database.pool).await?;
        let actual: Vec<_> = candidates
            .into_iter()
            .map(|candidate| (candidate.billing_scope_id(), candidate.gateway_account_id()))
            .collect();

        assert_eq!(actual.len(), 101);
        assert_eq!(actual, expected);
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn exact_attempt_claim_is_canonical_bounded_and_account_scoped() -> Result<(), Box<dyn Error>>
{
    let database = TestDatabase::start("sr_exact_claim").await?;
    let result = async {
        let account = create_gateway_account(&database.pool, "test_gateway").await?;
        let sibling = create_gateway_account(&database.pool, "test_gateway").await?;
        let subscriber_id = Uuid::now_v7();
        let first =
            insert_stale_enrollment(&database.pool, account, subscriber_id, "plan_a").await?;
        let second =
            insert_stale_enrollment(&database.pool, account, subscriber_id, "plan_b").await?;
        sqlx::query(
            r#"
                UPDATE billing_payment_attempts
                SET status = 'review_required', review_required_at = created_at
                WHERE id = $1
                "#,
        )
        .bind(first)
        .execute(&database.pool)
        .await?;
        for position in 2..=RECONCILIATION_PHASE_BATCH_SIZE {
            insert_stale_enrollment(
                &database.pool,
                account,
                Uuid::now_v7(),
                &format!("plan_{position}"),
            )
            .await?;
        }
        let sibling_attempt =
            insert_stale_enrollment(&database.pool, sibling, Uuid::now_v7(), "sibling_plan")
                .await?;
        sqlx::query(
            r#"
            UPDATE billing_payment_attempts
            SET submitted_at = created_at
            WHERE gateway_account_id IN ($1, $2)
            "#,
        )
        .bind(account.gateway_account_id)
        .bind(sibling.gateway_account_id)
        .execute(&database.pool)
        .await?;
        let local_unsubmitted =
            insert_stale_enrollment(&database.pool, account, Uuid::now_v7(), "local_only").await?;

        let claimed = claim_exact_reconciliation_attempts(
            &database.pool,
            GatewayAccountId::new(account.gateway_account_id),
        )
        .await?;
        assert_eq!(claimed.len(), RECONCILIATION_PHASE_BATCH_SIZE as usize);
        assert_eq!(*claimed[0].identity().attempt_id().as_uuid(), first);
        assert_eq!(*claimed[1].identity().attempt_id().as_uuid(), second);
        assert_eq!(claimed[0].kind(), PaymentAttemptKind::SubscriptionInitial);
        assert_eq!(
            claimed[0]
                .request()
                .target()
                .plan_key()
                .map(|key| key.as_str()),
            Some("plan_a"),
        );
        assert!(claimed.iter().all(|attempt| {
            *attempt.identity().gateway_account_id().as_uuid() == account.gateway_account_id
        }));
        assert!(
            !apply_exact_query_observation(
                &database.pool,
                &claimed[0],
                ExactQueryObservation::NoTransaction,
            )
            .await?
        );
        assert_eq!(
            attempt_status(&database.pool, first).await?,
            "review_required"
        );
        assert!(
            apply_exact_query_observation(
                &database.pool,
                &claimed[1],
                ExactQueryObservation::NoTransaction,
            )
            .await?
        );
        assert_eq!(
            attempt_status(&database.pool, second).await?,
            "review_required"
        );
        assert!(
            apply_exact_query_observation(
                &database.pool,
                &claimed[2],
                ExactQueryObservation::MalformedResponse,
            )
            .await?
        );

        let remainder = claim_exact_reconciliation_attempts(
            &database.pool,
            GatewayAccountId::new(account.gateway_account_id),
        )
        .await?;
        assert_eq!(remainder.len(), 1);
        assert_ne!(
            *remainder[0].identity().attempt_id().as_uuid(),
            local_unsubmitted
        );
        assert_eq!(
            *claim_exact_reconciliation_attempts(
                &database.pool,
                GatewayAccountId::new(sibling.gateway_account_id),
            )
            .await?[0]
                .identity()
                .attempt_id()
                .as_uuid(),
            sibling_attempt,
        );
        let mut transaction = database.pool.begin().await?;
        let local_attempt = find_payment_attempt_by_id_in_transaction(
            &mut transaction,
            BillingScopeId::new(account.billing_scope_id),
            syrup_rail::PaymentAttemptId::new(local_unsubmitted),
        )
        .await?
        .expect("local attempt exists");
        transaction.commit().await?;
        assert!(
            !apply_exact_query_observation(
                &database.pool,
                &local_attempt,
                ExactQueryObservation::NoTransaction,
            )
            .await?
        );
        assert_eq!(
            attempt_status(&database.pool, local_unsubmitted).await?,
            "pending"
        );
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn stale_payment_method_replacement_cleanup_is_bounded_and_account_scoped()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("sr_recon_method").await?;
    let result = async {
        let account = create_gateway_account(&database.pool, "test_gateway").await?;
        let sibling = create_gateway_account(&database.pool, "test_gateway").await?;
        let unsubmitted_review = insert_stale_payment_method_replacement(
            &database.pool,
            account.billing_scope_id,
            account.gateway_account_id,
            account.gateway_configuration_id,
            0,
        )
        .await?;
        sqlx::query("UPDATE billing_payment_attempts SET status = 'review_required' WHERE id = $1")
            .bind(unsubmitted_review)
            .execute(&database.pool)
            .await?;
        for position in 1..=RECONCILIATION_PHASE_BATCH_SIZE {
            let _ = insert_stale_payment_method_replacement(
                &database.pool,
                account.billing_scope_id,
                account.gateway_account_id,
                account.gateway_configuration_id,
                position,
            )
            .await?;
        }
        let _ = insert_stale_payment_method_replacement(
            &database.pool,
            sibling.billing_scope_id,
            sibling.gateway_account_id,
            sibling.gateway_configuration_id,
            10_000,
        )
        .await?;

        assert_eq!(
            fail_stale_unsubmitted_payment_method_replacements(
                &database.pool,
                GatewayAccountId::new(account.gateway_account_id),
            )
            .await?,
            RECONCILIATION_PHASE_BATCH_SIZE as u64,
        );
        assert_eq!(
            fail_stale_unsubmitted_payment_method_replacements(
                &database.pool,
                GatewayAccountId::new(account.gateway_account_id),
            )
            .await?,
            1,
        );
        let sibling_status: String = sqlx::query_scalar(
            "SELECT status FROM billing_payment_attempts WHERE gateway_account_id = $1",
        )
        .bind(sibling.gateway_account_id)
        .fetch_one(&database.pool)
        .await?;
        assert_eq!(sibling_status, "pending");
        assert_eq!(
            attempt_status(&database.pool, unsubmitted_review).await?,
            "failed"
        );
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

async fn insert_stale_payment_method_replacement(
    pool: &sqlx::PgPool,
    billing_scope_id: Uuid,
    gateway_account_id: Uuid,
    gateway_configuration_id: Uuid,
    position: i64,
) -> Result<Uuid, sqlx::Error> {
    let subscriber_id = Uuid::now_v7();
    let payment_method_id = Uuid::now_v7();
    let subscription_id = Uuid::now_v7();
    let attempt_id = Uuid::now_v7();
    let transaction_id = format!("txn{position}ref");
    sqlx::query(
        r#"
            INSERT INTO billing_payment_methods (
                id, billing_scope_id, subscriber_id, gateway_account_id,
                gateway_payment_method_reference, status
            ) VALUES ($1, $2, $3, $4, $5, 'active')
            "#,
    )
    .bind(payment_method_id)
    .bind(billing_scope_id)
    .bind(subscriber_id)
    .bind(gateway_account_id)
    .bind(format!("method{position}ref"))
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
            WITH clock AS MATERIALIZED (
                SELECT clock_timestamp() AS observed_at
            )
            INSERT INTO billing_subscriptions (
                id, billing_scope_id, subscriber_id, plan_key, status,
                gateway_account_id, payment_method_id, amount_cents, currency,
                current_period_start_at, current_period_end_at, next_renewal_at,
                initial_transaction_id, phase, recurring_period_kind,
                recurring_period_count, dunning_retry_delays_seconds,
                dunning_exhaustion, past_due_access, next_payment_attempt_at
            ) SELECT
                $1, $2, $3, 'test_plan', 'active', $4, $5, 100, 'USD',
                observed_at - interval '1 day',
                observed_at + interval '1 day',
                observed_at + interval '1 day', $6, 'recurring',
                'calendar_months', 1, ARRAY[]::bigint[],
                'remain_past_due', 'suspend_immediately',
                observed_at + interval '1 day'
            FROM clock
            "#,
    )
    .bind(subscription_id)
    .bind(billing_scope_id)
    .bind(subscriber_id)
    .bind(gateway_account_id)
    .bind(payment_method_id)
    .bind(&transaction_id)
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
            INSERT INTO billing_payment_attempts (
                id, billing_scope_id, subscriber_id, plan_key, subscription_id,
                payment_method_id, attempt_kind, status, idempotency_key,
                request_fingerprint, amount_cents, currency, gateway_account_id,
                gateway_configuration_id, gateway_order_id,
                payment_method_update_expected_payment_method_id,
                payment_method_update_expected_initial_transaction_id, created_at,
                updated_at
            ) VALUES (
                $1, $2, $3, 'test_plan', $4, $5,
                'subscription_payment_method_update', 'pending', $6, $7, 0,
                'USD', $8, $9, $10, $5, $11,
                clock_timestamp() - interval '4 minutes',
                clock_timestamp() - interval '4 minutes'
            )
            "#,
    )
    .bind(attempt_id)
    .bind(billing_scope_id)
    .bind(subscriber_id)
    .bind(subscription_id)
    .bind(payment_method_id)
    .bind(format!("idem{position}"))
    .bind(format!("fingerprint{position}"))
    .bind(gateway_account_id)
    .bind(gateway_configuration_id)
    .bind(format!("order{position}ref"))
    .bind(transaction_id)
    .execute(pool)
    .await?;
    Ok(attempt_id)
}

#[tokio::test]
async fn stale_enrollment_cleanup_uses_the_persisted_plan_lock_and_account_scope()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("sr_recon_initial").await?;
    let result = async {
        let account = create_gateway_account(&database.pool, "test_gateway").await?;
        let sibling = create_gateway_account(&database.pool, "test_gateway").await?;
        let subscriber_id = Uuid::now_v7();
        let plan_a = "plan_a";
        let plan_b = "plan_b";
        let plan_a_attempt =
            insert_stale_enrollment(&database.pool, account, subscriber_id, plan_a).await?;
        let plan_b_attempt =
            insert_stale_enrollment(&database.pool, account, subscriber_id, plan_b).await?;
        let sibling_attempt =
            insert_stale_enrollment(&database.pool, sibling, Uuid::now_v7(), "plan_c").await?;

        let mut lock_holder = database.pool.begin().await?;
        sqlx::query(
            "SELECT pg_advisory_xact_lock(hashtextextended($1::uuid::text || ':' || $2, 0))",
        )
        .bind(subscriber_id)
        .bind(plan_a)
        .execute(&mut *lock_holder)
        .await?;

        assert_eq!(
            fail_stale_unsubmitted_subscription_enrollments(
                &database.pool,
                GatewayAccountId::new(account.gateway_account_id),
            )
            .await?,
            1,
        );
        assert_eq!(
            attempt_status(&database.pool, plan_a_attempt).await?,
            "pending"
        );
        assert_eq!(
            attempt_status(&database.pool, plan_b_attempt).await?,
            "failed"
        );
        assert_eq!(
            attempt_status(&database.pool, sibling_attempt).await?,
            "pending"
        );

        lock_holder.rollback().await?;
        assert_eq!(
            fail_stale_unsubmitted_subscription_enrollments(
                &database.pool,
                GatewayAccountId::new(account.gateway_account_id),
            )
            .await?,
            1,
        );
        assert_eq!(
            attempt_status(&database.pool, plan_a_attempt).await?,
            "failed"
        );
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn pending_charge_classification_skips_a_busy_persisted_plan_without_starvation()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("sr_charge_lock").await?;
    let result = async {
        let account = create_gateway_account(&database.pool, "test_gateway").await?;
        let sibling = create_gateway_account(&database.pool, "test_gateway").await?;
        let subscriber_id = Uuid::now_v7();
        let plan_a_charge =
            insert_pending_processor_charge(&database.pool, account, subscriber_id, "plan_a", 30)
                .await?;
        let plan_b_charge =
            insert_pending_processor_charge(&database.pool, account, subscriber_id, "plan_b", 20)
                .await?;
        let sibling_charge =
            insert_pending_processor_charge(&database.pool, sibling, Uuid::now_v7(), "plan_c", 10)
                .await?;

        let mut lock_holder = database.pool.begin().await?;
        sqlx::query(
            "SELECT pg_advisory_xact_lock(hashtextextended($1::uuid::text || ':' || $2, 0))",
        )
        .bind(subscriber_id)
        .bind("plan_a")
        .execute(&mut *lock_holder)
        .await?;

        let summary = classify_pending_processor_charges(
            &database.pool,
            GatewayAccountId::new(account.gateway_account_id),
            1,
        )
        .await?;
        assert_eq!(summary.transitioned(), 1);
        assert_eq!(summary.skipped_locked(), 1);
        assert_eq!(summary.remaining_pending(), 1);
        assert_eq!(
            charge_progression(&database.pool, plan_a_charge).await?,
            "pending"
        );
        assert_eq!(
            charge_progression(&database.pool, plan_b_charge).await?,
            "reconciliation_required"
        );
        assert_eq!(
            charge_progression(&database.pool, sibling_charge).await?,
            "pending"
        );

        lock_holder.rollback().await?;
        let summary = classify_pending_processor_charges(
            &database.pool,
            GatewayAccountId::new(account.gateway_account_id),
            1,
        )
        .await?;
        assert_eq!(summary.transitioned(), 1);
        assert_eq!(summary.skipped_locked(), 0);
        assert_eq!(summary.remaining_pending(), 0);
        assert_eq!(
            charge_progression(&database.pool, plan_a_charge).await?,
            "reconciliation_required"
        );
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn pending_charge_classification_caps_each_account_pass_at_one_hundred()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("sr_charge_bound").await?;
    let result = async {
        let account = create_gateway_account(&database.pool, "test_gateway").await?;
        let sibling = create_gateway_account(&database.pool, "test_gateway").await?;
        for position in 0..=RECONCILIATION_PHASE_BATCH_SIZE {
            insert_pending_processor_charge(
                &database.pool,
                account,
                Uuid::now_v7(),
                "test_plan",
                position,
            )
            .await?;
        }
        let sibling_charge = insert_pending_processor_charge(
            &database.pool,
            sibling,
            Uuid::now_v7(),
            "test_plan",
            10_000,
        )
        .await?;

        let first = classify_pending_processor_charges(
            &database.pool,
            GatewayAccountId::new(account.gateway_account_id),
            u64::MAX,
        )
        .await?;
        assert_eq!(first.transitioned(), RECONCILIATION_PHASE_BATCH_SIZE as u64);
        assert_eq!(first.remaining_pending(), 1);
        let second = classify_pending_processor_charges(
            &database.pool,
            GatewayAccountId::new(account.gateway_account_id),
            u64::MAX,
        )
        .await?;
        assert_eq!(second.transitioned(), 1);
        assert_eq!(second.remaining_pending(), 0);
        assert_eq!(
            charge_progression(&database.pool, sibling_charge).await?,
            "pending"
        );
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

async fn insert_pending_processor_charge(
    pool: &sqlx::PgPool,
    account: crate::test_support::GatewayAccountFixture,
    subscriber_id: Uuid,
    plan_key: &str,
    age_seconds: i64,
) -> Result<Uuid, sqlx::Error> {
    let attempt_id = Uuid::now_v7();
    let charge_id = Uuid::now_v7();
    let order_id = format!("order-{attempt_id}");
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
    .bind(attempt_id)
    .bind(account.billing_scope_id)
    .bind(subscriber_id)
    .bind(plan_key)
    .bind(format!("idem-{attempt_id}"))
    .bind(format!("fingerprint-{attempt_id}"))
    .bind(account.gateway_account_id)
    .bind(account.gateway_configuration_id)
    .bind(&order_id)
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
            INSERT INTO billing_processor_charges (
                id, attempt_id, billing_scope_id, gateway_account_id,
                gateway_order_id, gateway_transaction_id, gateway_response,
                gateway_response_code, gateway_response_text,
                gateway_condition, charge_role, progression_state,
                observed_at, attempt_kind, plan_key, amount_cents, currency
            ) VALUES (
                $1, $2, $3, $4, $5, $6, '1', '100', 'Approved',
                'complete', 'primary', 'pending',
                clock_timestamp() - ($7::bigint * interval '1 second'),
                'subscription_initial', $8, 100, 'USD'
            )
            "#,
    )
    .bind(charge_id)
    .bind(attempt_id)
    .bind(account.billing_scope_id)
    .bind(account.gateway_account_id)
    .bind(order_id)
    .bind(format!("transaction-{attempt_id}"))
    .bind(age_seconds)
    .bind(plan_key)
    .execute(pool)
    .await?;
    Ok(charge_id)
}

async fn charge_progression(pool: &sqlx::PgPool, charge_id: Uuid) -> Result<String, sqlx::Error> {
    sqlx::query_scalar("SELECT progression_state FROM billing_processor_charges WHERE id = $1")
        .bind(charge_id)
        .fetch_one(pool)
        .await
}

async fn insert_stale_enrollment(
    pool: &sqlx::PgPool,
    account: crate::test_support::GatewayAccountFixture,
    subscriber_id: Uuid,
    plan_key: &str,
) -> Result<Uuid, sqlx::Error> {
    let attempt_id = Uuid::now_v7();
    sqlx::query(
        r#"
            INSERT INTO billing_payment_attempts (
                id, billing_scope_id, subscriber_id, plan_key, attempt_kind,
                status, idempotency_key, request_fingerprint, amount_cents,
                currency, gateway_account_id, gateway_configuration_id,
                gateway_order_id, created_at, updated_at,
                subscription_initial_terms_version,
                subscription_initial_start_kind,
                subscription_initial_recurring_base_amount_cents,
                subscription_initial_recurring_period_kind,
                subscription_initial_recurring_period_count,
                subscription_initial_dunning_retry_delays_seconds,
                subscription_initial_dunning_exhaustion,
                subscription_initial_past_due_access
            ) VALUES (
                $1, $2, $3, $4, 'subscription_initial', 'pending', $5, $6,
                100, 'USD', $7, $8, $9,
                clock_timestamp() - interval '31 minutes',
                clock_timestamp() - interval '31 minutes',
                2, 'recurring_immediately', 100, 'calendar_months', 1,
                ARRAY[]::bigint[], 'remain_past_due', 'suspend_immediately'
            )
            "#,
    )
    .bind(attempt_id)
    .bind(account.billing_scope_id)
    .bind(subscriber_id)
    .bind(plan_key)
    .bind(format!("idem-{attempt_id}"))
    .bind(format!("fingerprint-{attempt_id}"))
    .bind(account.gateway_account_id)
    .bind(account.gateway_configuration_id)
    .bind(format!("order_{}", attempt_id.simple()))
    .execute(pool)
    .await?;
    Ok(attempt_id)
}

async fn attempt_status(pool: &sqlx::PgPool, attempt_id: Uuid) -> Result<String, sqlx::Error> {
    sqlx::query_scalar("SELECT status FROM billing_payment_attempts WHERE id = $1")
        .bind(attempt_id)
        .fetch_one(pool)
        .await
}
