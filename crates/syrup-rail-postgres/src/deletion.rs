use sqlx::PgConnection;
use syrup_rail::{BillingDeletionBlockers, DeletionBlockerQuery};

/// Reports the canonical financial rows that prevent host account deletion.
///
/// The query is subscriber-wide across plans and uses the caller's transaction.
/// Host-specific orders and fulfillment blockers remain the host's responsibility.
pub async fn billing_deletion_blockers(
    connection: &mut PgConnection,
    query: DeletionBlockerQuery,
) -> Result<BillingDeletionBlockers, sqlx::Error> {
    let billing_scope_id = query.billing_scope_id().into_uuid();
    let subscriber_id = query.subscriber_id().into_uuid();
    let row = sqlx::query!(
        r#"
        SELECT
            EXISTS (
                SELECT 1
                FROM billing_subscriptions
                WHERE billing_scope_id = $1
                    AND subscriber_id = $2
                    AND status IN ('active', 'past_due')
            ) AS "active_subscription!",
            EXISTS (
                SELECT 1
                FROM billing_payment_attempts
                WHERE billing_scope_id = $1
                    AND subscriber_id = $2
                    AND status IN ('pending', 'unknown', 'review_required')
            ) AS "unresolved_payment!"
        "#,
        billing_scope_id,
        subscriber_id,
    )
    .fetch_one(connection)
    .await?;
    Ok(BillingDeletionBlockers::new(
        row.active_subscription,
        row.unresolved_payment,
    ))
}

#[cfg(test)]
mod tests {
    use std::{error::Error, io};

    use syrup_rail::{BillingScopeId, DeletionBlockerQuery, SubscriberId};
    use uuid::Uuid;

    use super::billing_deletion_blockers;
    use crate::test_support::TestDatabase;

    #[tokio::test]
    async fn deletion_blockers_are_scoped_subscriber_wide_and_transaction_local()
    -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("sr_deletion_v1").await?;
        let result = async {
            let scope = Uuid::now_v7();
            let subscriber = Uuid::now_v7();
            let provider = "test_gateway";
            let account = Uuid::now_v7();
            let configuration = Uuid::now_v7();
            sqlx::query(
                "INSERT INTO billing_gateway_provider_rate_limits (provider_key) VALUES ($1)",
            )
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
                WITH clock AS MATERIALIZED (
                    SELECT clock_timestamp() AS observed_at
                )
                INSERT INTO billing_subscriptions (
                    id, billing_scope_id, subscriber_id, plan_key, status,
                    gateway_account_id, payment_method_id, amount_cents,
                    currency, current_period_start_at, current_period_end_at,
                    next_renewal_at, initial_transaction_id
                ) SELECT
                    $1, $2, $3, 'plan_a', 'active', $4, $5, 100, 'USD',
                    observed_at - interval '1 day',
                    observed_at + interval '1 day',
                    observed_at + interval '1 day', $6
                FROM clock
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
            let attempt = Uuid::now_v7();
            let target = Uuid::now_v7();
            sqlx::query(
                r#"
                INSERT INTO billing_payment_attempts (
                    id, billing_scope_id, subscriber_id, host_charge_target_id,
                    attempt_kind, status, idempotency_key, request_fingerprint,
                    amount_cents, currency, gateway_account_id,
                    gateway_configuration_id, gateway_order_id
                ) VALUES (
                    $1, $2, $3, $4, 'host_charge', 'pending', $5, $6, 100,
                    'USD', $7, $8, $9
                )
                "#,
            )
            .bind(attempt)
            .bind(scope)
            .bind(subscriber)
            .bind(target)
            .bind(format!("delete-{}", attempt.simple()))
            .bind(format!("host_charge:{target}:100:USD"))
            .bind(account)
            .bind(configuration)
            .bind(format!("delete-order-{}", attempt.simple()))
            .execute(&database.pool)
            .await?;

            let query = DeletionBlockerQuery::new(
                BillingScopeId::new(scope),
                SubscriberId::new(subscriber),
            );
            let mut transaction = database.pool.begin().await?;
            let blockers = billing_deletion_blockers(&mut transaction, query).await?;
            if !blockers.active_subscription() || !blockers.unresolved_payment() {
                return Err(io::Error::other("canonical blockers were not reported").into());
            }
            let other_scope = billing_deletion_blockers(
                &mut transaction,
                DeletionBlockerQuery::new(
                    BillingScopeId::new(Uuid::now_v7()),
                    SubscriberId::new(subscriber),
                ),
            )
            .await?;
            if !other_scope.is_empty() {
                return Err(io::Error::other("blockers crossed billing scopes").into());
            }
            transaction.rollback().await?;

            sqlx::query(
                "UPDATE billing_subscriptions SET status = 'canceled', canceled_at = clock_timestamp() WHERE id = $1",
            )
            .bind(subscription)
            .execute(&database.pool)
            .await?;
            sqlx::query(
                "UPDATE billing_payment_attempts SET status = 'failed', resolved_at = clock_timestamp() WHERE id = $1",
            )
            .bind(attempt)
            .execute(&database.pool)
            .await?;
            let mut transaction = database.pool.begin().await?;
            let blockers = billing_deletion_blockers(&mut transaction, query).await?;
            transaction.rollback().await?;
            if !blockers.is_empty() {
                return Err(io::Error::other(
                    "terminal financial history incorrectly blocked deletion",
                )
                .into());
            }
            Ok::<_, Box<dyn Error>>(())
        }
        .await;
        let cleanup = database.cleanup().await;
        result?;
        cleanup
    }
}
