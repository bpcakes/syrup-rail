use chrono::{Duration, Utc};
use sqlx::PgConnection;
use syrup_rail::{BillingScopeId, DeletionBlockerQuery, PaymentAttemptKind, SubscriberId};

use super::*;
use crate::{attempts::LocalAttemptPolicy, billing_deletion_blockers};

struct BlockerFixture {
    account: GatewayAccountFixture,
    subscriber_id: Uuid,
    payment_method_id: Uuid,
    subscription_id: Uuid,
}

struct AttemptCase {
    label: &'static str,
    status: &'static str,
    age_seconds: i64,
    submitted: bool,
    expected_blocked: bool,
}

#[tokio::test]
async fn deletion_blockers_cover_every_attempt_kind_and_submission_boundary()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("delete_matrix").await?;
    let result = async {
        let fixture = blocker_fixture(&database.pool).await?;
        let kinds = PaymentAttemptKind::ALL.map(|kind| {
            (
                kind.as_str(),
                LocalAttemptPolicy::for_kind(kind).stale_after_seconds(),
            )
        });
        let mut connection = database.pool.acquire().await?;
        for (kind, stale_after) in kinds {
            let cases = [
                AttemptCase {
                    label: "fresh prepared",
                    status: "pending",
                    age_seconds: stale_after - 10,
                    submitted: false,
                    expected_blocked: true,
                },
                AttemptCase {
                    label: "stale prepared",
                    status: "pending",
                    age_seconds: stale_after + 10,
                    submitted: false,
                    expected_blocked: false,
                },
                AttemptCase {
                    label: "stale legacy review",
                    status: "review_required",
                    age_seconds: stale_after + 10,
                    submitted: false,
                    expected_blocked: false,
                },
                AttemptCase {
                    label: "stale unknown",
                    status: "unknown",
                    age_seconds: stale_after + 10,
                    submitted: false,
                    expected_blocked: true,
                },
                AttemptCase {
                    label: "stale submitted pending",
                    status: "pending",
                    age_seconds: stale_after + 10,
                    submitted: true,
                    expected_blocked: true,
                },
                AttemptCase {
                    label: "stale submitted review",
                    status: "review_required",
                    age_seconds: stale_after + 10,
                    submitted: true,
                    expected_blocked: true,
                },
            ];
            for case in cases {
                let attempt_id = insert_attempt(&mut connection, &fixture, kind, &case).await?;
                let blockers = billing_deletion_blockers(
                    &mut connection,
                    DeletionBlockerQuery::new(
                        BillingScopeId::new(fixture.account.billing_scope_id),
                        SubscriberId::new(fixture.subscriber_id),
                    ),
                )
                .await?;
                assert!(!blockers.active_subscription(), "{kind}: {}", case.label);
                assert_eq!(
                    blockers.unresolved_payment(),
                    case.expected_blocked,
                    "{kind}: {}",
                    case.label,
                );
                sqlx::query("DELETE FROM billing_payment_attempts WHERE id = $1")
                    .bind(attempt_id)
                    .execute(&mut *connection)
                    .await?;
            }
        }
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

async fn blocker_fixture(pool: &PgPool) -> Result<BlockerFixture, sqlx::Error> {
    let account = create_gateway_account(pool, "deletion_matrix").await?;
    let subscriber_id = Uuid::now_v7();
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
    .bind(subscriber_id)
    .bind(account.gateway_account_id)
    .bind(format!("vault_{}", payment_method_id.simple()))
    .execute(pool)
    .await?;

    let subscription_id = Uuid::now_v7();
    let period_start = Utc::now() - Duration::days(1);
    let period_end = period_start + Duration::days(30);
    sqlx::query(
        r#"
        INSERT INTO billing_subscriptions (
            id, billing_scope_id, subscriber_id, plan_key, status,
            gateway_account_id, payment_method_id, amount_cents, currency,
            current_period_start_at, current_period_end_at, next_renewal_at,
            initial_transaction_id, phase, recurring_period_kind,
            recurring_period_count, dunning_retry_delays_seconds,
            dunning_exhaustion, past_due_access, next_payment_attempt_at
        ) VALUES (
            $1, $2, $3, 'matrix_plan', 'active', $4, $5, 100, 'USD',
            $6, $7, $7, 'txn_matrix_initial', 'recurring',
            'calendar_months', 1, ARRAY[]::bigint[], 'remain_past_due',
            'suspend_immediately', $7
        )
        "#,
    )
    .bind(subscription_id)
    .bind(account.billing_scope_id)
    .bind(subscriber_id)
    .bind(account.gateway_account_id)
    .bind(payment_method_id)
    .bind(period_start)
    .bind(period_end)
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
        UPDATE billing_subscriptions
        SET status = 'canceled', canceled_at = clock_timestamp(),
            next_payment_attempt_at = NULL
        WHERE id = $1
        "#,
    )
    .bind(subscription_id)
    .execute(pool)
    .await?;
    Ok(BlockerFixture {
        account,
        subscriber_id,
        payment_method_id,
        subscription_id,
    })
}

async fn insert_attempt(
    connection: &mut PgConnection,
    fixture: &BlockerFixture,
    kind: &str,
    case: &AttemptCase,
) -> Result<Uuid, sqlx::Error> {
    let attempt_id = Uuid::now_v7();
    let host_charge = kind == "host_charge";
    let initial = kind == "subscription_initial";
    let subscription_charge = matches!(kind, "subscription_renewal" | "subscription_recovery");
    let method_update = kind == "subscription_payment_method_update";
    let initial_requires_method = initial && case.status == "review_required";
    let created_at = Utc::now() - Duration::seconds(case.age_seconds);
    let submitted_at = case.submitted.then_some(created_at + Duration::seconds(1));
    let review_required_at =
        (case.status == "review_required").then_some(created_at + Duration::seconds(2));
    let period_start = Utc::now() - Duration::days(1);
    let period_end = period_start + Duration::days(30);
    let host_target_id = host_charge.then(Uuid::now_v7);
    let payment_method_id = (subscription_charge || method_update || initial_requires_method)
        .then_some(fixture.payment_method_id);
    let subscription_id = (subscription_charge || method_update).then_some(fixture.subscription_id);
    let initial_terms_version = initial.then_some(2_i16);

    sqlx::query(
        r#"
        INSERT INTO billing_payment_attempts (
            id, billing_scope_id, subscriber_id, plan_key,
            host_charge_target_id, subscription_id, payment_method_id,
            attempt_kind, status, idempotency_key, request_fingerprint,
            amount_cents, currency, billing_period_start_at,
            billing_period_end_at, gateway_account_id,
            gateway_configuration_id, gateway_order_id, submitted_at,
            review_required_at,
            payment_method_update_expected_payment_method_id,
            payment_method_update_expected_initial_transaction_id,
            subscription_expected_payment_method_id,
            subscription_expected_initial_transaction_id,
            subscription_expected_status, subscription_initial_terms_version,
            subscription_initial_start_kind,
            subscription_initial_recurring_base_amount_cents,
            subscription_initial_recurring_period_kind,
            subscription_initial_recurring_period_count,
            subscription_initial_dunning_retry_delays_seconds,
            subscription_initial_dunning_exhaustion,
            subscription_initial_past_due_access, created_at, updated_at
        ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11,
            $12, 'USD', $13, $14, $15, $16, $17, $18, $19,
            $20, $21, $22, $23, $24, $25, $26, $27, $28, $29,
            $30, $31, $32, $33, $33
        )
        "#,
    )
    .bind(attempt_id)
    .bind(fixture.account.billing_scope_id)
    .bind(fixture.subscriber_id)
    .bind((!host_charge).then_some("matrix_plan"))
    .bind(host_target_id)
    .bind(subscription_id)
    .bind(payment_method_id)
    .bind(kind)
    .bind(case.status)
    .bind(format!("matrix_{}", attempt_id.simple()))
    .bind(format!("matrix:{kind}:{}", attempt_id.simple()))
    .bind(if method_update { 0 } else { 100 })
    .bind(subscription_charge.then_some(period_start))
    .bind(subscription_charge.then_some(period_end))
    .bind(fixture.account.gateway_account_id)
    .bind(fixture.account.gateway_configuration_id)
    .bind(format!("matrix_order_{}", attempt_id.simple()))
    .bind(submitted_at)
    .bind(review_required_at)
    .bind(method_update.then_some(fixture.payment_method_id))
    .bind(method_update.then_some("txn_matrix_initial"))
    .bind(subscription_charge.then_some(fixture.payment_method_id))
    .bind(subscription_charge.then_some("txn_matrix_initial"))
    .bind(subscription_charge.then_some("active"))
    .bind(initial_terms_version)
    .bind(initial.then_some("recurring_immediately"))
    .bind(initial.then_some(100_i32))
    .bind(initial.then_some("calendar_months"))
    .bind(initial.then_some(1_i32))
    .bind(initial.then_some(Vec::<i64>::new()))
    .bind(initial.then_some("remain_past_due"))
    .bind(initial.then_some("suspend_immediately"))
    .bind(created_at)
    .execute(connection)
    .await?;
    Ok(attempt_id)
}
