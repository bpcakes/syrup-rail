use sqlx::PgPool;
use syrup_rail::{BillingScopeId, GatewayAccountId, GatewayAccountReconciliationCandidate};
use uuid::Uuid;

const PAYMENT_METHOD_REPLACEMENT_STALE_AFTER_SECONDS: i64 = 3 * 60;
const RECONCILIATION_PHASE_BATCH_SIZE: i64 = 100;
const STALE_PAYMENT_METHOD_REPLACEMENT_RESPONSE_TEXT: &str =
    "Payment method update was abandoned before gateway submission.";

/// Returns every registered gateway account in deterministic locator order.
///
/// This operation intentionally has no caller-selected or implicit limit.
/// Hosts must either dispatch the complete result or introduce a separately
/// designed durable progress cursor.
pub async fn reconciliation_gateway_accounts(
    pool: &PgPool,
) -> Result<Vec<GatewayAccountReconciliationCandidate>, sqlx::Error> {
    let rows = sqlx::query_as::<_, (Uuid, Uuid)>(
        r#"
        SELECT billing_scope_id, id
        FROM billing_gateway_accounts
        ORDER BY billing_scope_id, id
        "#,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(billing_scope_id, gateway_account_id)| {
            GatewayAccountReconciliationCandidate::new(
                BillingScopeId::new(billing_scope_id),
                GatewayAccountId::new(gateway_account_id),
            )
        })
        .collect())
}

/// Fails one bounded batch of stale payment-method replacements for an account.
///
/// These attempts have never crossed the provider boundary, so expiring them
/// is a local reconciliation phase and performs no gateway I/O.
pub async fn fail_stale_unsubmitted_payment_method_replacements(
    pool: &PgPool,
    gateway_account_id: GatewayAccountId,
) -> Result<u64, sqlx::Error> {
    let mut transaction = pool.begin().await?;
    sqlx::query("SELECT set_config('lock_timeout', '250ms', true)")
        .execute(&mut *transaction)
        .await?;
    let result = sqlx::query(
        r#"
        WITH stale_attempts AS (
            SELECT id
            FROM billing_payment_attempts
            WHERE attempt_kind = 'subscription_payment_method_update'
                AND status = 'pending'
                AND submitted_at IS NULL
                AND created_at <= clock_timestamp()
                    - ($1::bigint * interval '1 second')
                AND gateway_account_id = $2
            ORDER BY created_at, id
            LIMIT $3
            FOR UPDATE SKIP LOCKED
        )
        UPDATE billing_payment_attempts AS attempts
        SET status = 'failed',
            gateway_response_text = COALESCE(gateway_response_text, $4),
            gateway_condition = COALESCE(gateway_condition, 'failed'),
            resolved_at = clock_timestamp(),
            updated_at = clock_timestamp()
        FROM stale_attempts
        WHERE attempts.id = stale_attempts.id
        "#,
    )
    .bind(PAYMENT_METHOD_REPLACEMENT_STALE_AFTER_SECONDS)
    .bind(gateway_account_id.as_uuid())
    .bind(RECONCILIATION_PHASE_BATCH_SIZE)
    .bind(STALE_PAYMENT_METHOD_REPLACEMENT_RESPONSE_TEXT)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use syrup_rail::{
        BillingScopeId, GatewayAccountId, GatewayAccountRegistration, GatewayConfigurationId,
        GatewayProviderKey,
    };
    use uuid::Uuid;

    use super::{
        RECONCILIATION_PHASE_BATCH_SIZE, fail_stale_unsubmitted_payment_method_replacements,
        reconciliation_gateway_accounts,
    };
    use crate::{
        register_gateway_account,
        test_support::{TestDatabase, create_gateway_account},
    };

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
    async fn stale_payment_method_replacement_cleanup_is_bounded_and_account_scoped()
    -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("sr_recon_method").await?;
        let result = async {
            let account = create_gateway_account(&database.pool, "test_gateway").await?;
            let sibling = create_gateway_account(&database.pool, "test_gateway").await?;
            for position in 0..=RECONCILIATION_PHASE_BATCH_SIZE {
                insert_stale_payment_method_replacement(
                    &database.pool,
                    account.billing_scope_id,
                    account.gateway_account_id,
                    account.gateway_configuration_id,
                    position,
                )
                .await?;
            }
            insert_stale_payment_method_replacement(
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
    ) -> Result<(), sqlx::Error> {
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
                initial_transaction_id
            ) SELECT
                $1, $2, $3, 'test_plan', 'active', $4, $5, 100, 'USD',
                observed_at - interval '1 day',
                observed_at + interval '1 day',
                observed_at + interval '1 day', $6
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
        Ok(())
    }
}
