use std::{
    error::Error,
    fmt,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use super::*;
use crate::test_support::{TestDatabase, create_gateway_account};
use crate::transactions::{BillingTransaction, BillingTransactionSubjectState};
use tokio::sync::Mutex;

#[derive(Debug)]
struct InjectedTestError;

impl fmt::Display for InjectedTestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("injected test error")
    }
}

impl Error for InjectedTestError {}

#[derive(Clone)]
struct TestCoordinator {
    pool: PgPool,
    events: Arc<Mutex<Vec<BillingEvent>>>,
    fail_event: bool,
}

#[async_trait]
impl BillingTransactionCoordinator for TestCoordinator {
    async fn begin(
        &self,
        _subject: BillingEventSubject,
        _lock_timeout: Duration,
    ) -> Result<Box<dyn BillingTransaction>, BillingTransactionError> {
        Ok(Box::new(TestTransaction {
            transaction: Some(
                self.pool
                    .begin()
                    .await
                    .map_err(BillingTransactionError::new)?,
            ),
            events: Arc::clone(&self.events),
            fail_event: self.fail_event,
        }))
    }
}

struct TestTransaction {
    transaction: Option<Transaction<'static, Postgres>>,
    events: Arc<Mutex<Vec<BillingEvent>>>,
    fail_event: bool,
}

#[async_trait]
impl BillingTransaction for TestTransaction {
    fn connection(&mut self) -> &mut PgConnection {
        &mut *self.transaction.as_mut().expect("active test transaction")
    }

    fn subject_state(&self) -> BillingTransactionSubjectState {
        BillingTransactionSubjectState::LiveRecipient
    }

    async fn append_event(&mut self, event: &BillingEvent) -> Result<(), BillingEventWriteError> {
        if self.fail_event {
            return Err(BillingEventWriteError::new(InjectedTestError));
        }
        self.events.lock().await.push(event.clone());
        Ok(())
    }

    async fn commit(mut self: Box<Self>) -> Result<(), BillingTransactionError> {
        self.transaction
            .take()
            .expect("active test transaction")
            .commit()
            .await
            .map_err(BillingTransactionError::new)
    }

    async fn rollback(mut self: Box<Self>) -> Result<(), BillingTransactionError> {
        self.transaction
            .take()
            .expect("active test transaction")
            .rollback()
            .await
            .map_err(BillingTransactionError::new)
    }
}

struct ExactManualFailureHost;

#[async_trait]
impl ManualAttemptFailureHostStore for ExactManualFailureHost {
    async fn lock_payment_failure_target(
        &self,
        connection: &mut PgConnection,
        charge: ManualFailureHostCharge,
    ) -> Result<(), ManualAttemptFailureHostStoreError> {
        crate::test_support::fixture_query_scalar::<_, Uuid>(
                "SELECT id FROM manual_failure_host_targets WHERE id = $1 AND billing_scope_id = $2 AND subscriber_id = $3 FOR UPDATE",
            )
            .bind(charge.target_id().as_uuid())
            .bind(charge.billing_scope_id().as_uuid())
            .bind(charge.subscriber_id().as_uuid())
            .fetch_optional(connection)
            .await
            .map_err(ManualAttemptFailureHostStoreError::new)?;
        Ok(())
    }

    async fn mark_payment_failed(
        &self,
        connection: &mut PgConnection,
        charge: ManualFailureHostCharge,
    ) -> Result<ManualAttemptFailureHostTransitionOutcome, ManualAttemptFailureHostStoreError> {
        let result = crate::test_support::fixture_query(
                "UPDATE manual_failure_host_targets SET status = 'payment_failed' WHERE id = $1 AND billing_scope_id = $2 AND subscriber_id = $3 AND status = 'pending'",
            )
            .bind(charge.target_id().as_uuid())
            .bind(charge.billing_scope_id().as_uuid())
            .bind(charge.subscriber_id().as_uuid())
            .execute(connection)
            .await
            .map_err(ManualAttemptFailureHostStoreError::new)?;
        Ok(if result.rows_affected() == 1 {
            ManualAttemptFailureHostTransitionOutcome::Changed
        } else {
            ManualAttemptFailureHostTransitionOutcome::Unchanged
        })
    }
}

async fn insert_review_renewal(
    database: &TestDatabase,
    account: &crate::test_support::GatewayAccountFixture,
    subscriber_id: Uuid,
    suffix: &str,
) -> Result<(Uuid, Uuid), Box<dyn Error>> {
    let payment_method_id = Uuid::now_v7();
    let subscription_id = Uuid::now_v7();
    let attempt_id = Uuid::now_v7();
    let initial_transaction_id = format!("txn-initial-{suffix}");
    sqlx::query!(
        r#"
            INSERT INTO billing_payment_methods (
                id, billing_scope_id, subscriber_id, gateway_account_id,
                gateway_payment_method_reference, status
            ) VALUES ($1, $2, $3, $4, $5, 'active')
            "#,
        payment_method_id,
        account.billing_scope_id,
        subscriber_id,
        account.gateway_account_id,
        format!("method-{suffix}"),
    )
    .execute(&database.pool)
    .await?;
    checked_query!(
        r#"
            WITH clock AS MATERIALIZED (SELECT clock_timestamp() AS observed_at)
            INSERT INTO billing_subscriptions (
            required_gateway_account_mode,
                id, billing_scope_id, subscriber_id, plan_key, status,
                gateway_account_id, payment_method_id, amount_cents, currency,
                current_period_start_at, current_period_end_at, next_renewal_at,
                initial_transaction_id, phase, recurring_period_kind,
                recurring_period_count, dunning_retry_delays_seconds,
                dunning_exhaustion, past_due_access, next_payment_attempt_at
            ) SELECT
                'live', $1, $2, $3, 'test_plan', 'active', $4, $5, 500, 'USD',
                observed_at - interval '1 month', observed_at, observed_at,
                $6, 'recurring', 'calendar_months', 1, ARRAY[]::bigint[],
                'remain_past_due', 'suspend_immediately', observed_at
            FROM clock
            "#,
        subscription_id,
        account.billing_scope_id,
        subscriber_id,
        account.gateway_account_id,
        payment_method_id,
        &initial_transaction_id,
    )
    .execute(&database.pool)
    .await?;
    checked_query!(
        r#"
            WITH clock AS MATERIALIZED (SELECT clock_timestamp() AS observed_at)
            INSERT INTO billing_payment_attempts (
                id, billing_scope_id, subscriber_id, plan_key,
                subscription_id, payment_method_id, attempt_kind, status,
                idempotency_key, request_fingerprint, amount_cents, currency,
                billing_period_start_at, billing_period_end_at,
                gateway_account_id, gateway_configuration_id, gateway_order_id,
                submitted_at, review_required_at,
                subscription_expected_payment_method_id,
                subscription_expected_initial_transaction_id,
                subscription_expected_status,
                required_gateway_account_mode
            ) SELECT
                $1, $2, $3, 'test_plan', $4, $5, 'subscription_renewal',
                'review_required', $6, $7, 500, 'USD', subscriptions.next_renewal_at,
                subscriptions.next_renewal_at + interval '1 month', $8, $9, $10,
                observed_at, observed_at, $5, $11, 'active', 'live'
            FROM clock
            CROSS JOIN billing_subscriptions AS subscriptions
            WHERE subscriptions.id = $4
            "#,
        attempt_id,
        account.billing_scope_id,
        subscriber_id,
        subscription_id,
        payment_method_id,
        format!("idem-{suffix}"),
        format!("fingerprint-{suffix}"),
        account.gateway_account_id,
        account.gateway_configuration_id,
        format!("order-{suffix}"),
        initial_transaction_id,
    )
    .execute(&database.pool)
    .await?;
    Ok((subscription_id, attempt_id))
}

mod external_reversal;
mod manual_failure;

mod renewal_retry;
