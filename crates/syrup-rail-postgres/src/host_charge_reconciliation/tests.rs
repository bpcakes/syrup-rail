use std::error::Error;

use async_trait::async_trait;
use chrono::{Duration, Utc};
use sqlx::PgConnection;
use syrup_rail::{
    BillingScopeId, GatewayAccountId, HostChargeTargetNoChange, HostChargeTargetTransition,
    HostChargeTargetTransitionKind, HostChargeTargetTransitionOutcome, SubscriberId,
};
use uuid::Uuid;

use super::fail_stale_unsubmitted_host_charges;
use crate::{
    HostChargeLedgerAdmission, HostChargeLedgerAdmissionMode, HostChargeLedgerAdmissionQuery,
    HostChargeReservationDecision, HostChargeSubmissionAdmission, HostChargeSubmissionDecision,
    HostChargeTargetError, HostChargeTargetReservation, HostChargeTargetStore,
    host_charge_ledger_admission,
    test_support::{GatewayAccountFixture, TestDatabase, create_gateway_account},
};

struct ReconciliationTargets;

#[async_trait]
impl HostChargeTargetStore for ReconciliationTargets {
    async fn preflight_target(
        &self,
        _connection: &mut PgConnection,
        _reservation: &HostChargeTargetReservation,
    ) -> Result<HostChargeReservationDecision, HostChargeTargetError> {
        unreachable!("host-charge cleanup never performs reservation preflight")
    }

    async fn reserve_target(
        &self,
        _connection: &mut PgConnection,
        _reservation: &HostChargeTargetReservation,
    ) -> Result<HostChargeReservationDecision, HostChargeTargetError> {
        unreachable!("host-charge cleanup never reserves a target")
    }

    async fn admit_submission(
        &self,
        _connection: &mut PgConnection,
        _admission: &HostChargeSubmissionAdmission,
    ) -> Result<HostChargeSubmissionDecision, HostChargeTargetError> {
        unreachable!("host-charge cleanup never admits submission")
    }

    async fn apply_transition(
        &self,
        connection: &mut PgConnection,
        transition: HostChargeTargetTransition,
    ) -> Result<HostChargeTargetTransitionOutcome, HostChargeTargetError> {
        let current = sqlx::query_as::<_, (Uuid, String)>(
            r#"
            SELECT attempt_id, status
            FROM host_reconciliation_targets
            WHERE id = $1 AND billing_scope_id = $2 AND subscriber_id = $3
            FOR UPDATE
            "#,
        )
        .bind(transition.target_id().as_uuid())
        .bind(transition.billing_scope_id().as_uuid())
        .bind(transition.subscriber_id().as_uuid())
        .fetch_optional(&mut *connection)
        .await
        .map_err(HostChargeTargetError::new)?;
        let Some((attempt_id, status)) = current else {
            return Ok(HostChargeTargetTransitionOutcome::StaleTarget);
        };
        if attempt_id != transition.attempt_id().into_uuid()
            || transition.kind() != HostChargeTargetTransitionKind::PaymentFailed
        {
            return Ok(HostChargeTargetTransitionOutcome::StaleTarget);
        }
        match status.as_str() {
            "reserved" => {
                sqlx::query(
                    "UPDATE host_reconciliation_targets SET status = 'released' WHERE id = $1",
                )
                .bind(transition.target_id().as_uuid())
                .execute(&mut *connection)
                .await
                .map_err(HostChargeTargetError::new)?;
                Ok(HostChargeTargetTransitionOutcome::Applied)
            }
            "released" => Ok(HostChargeTargetTransitionOutcome::ExactReplay),
            _ => Ok(HostChargeTargetTransitionOutcome::Unchanged {
                reason: HostChargeTargetNoChange::InapplicableState,
            }),
        }
    }
}

#[tokio::test]
async fn stale_host_charge_cleanup_releases_target_without_gateway_io() -> Result<(), Box<dyn Error>>
{
    let database = TestDatabase::start("host_stale").await?;
    let result = async {
        sqlx::query(
            r#"
            CREATE TABLE host_reconciliation_targets (
                id uuid PRIMARY KEY,
                billing_scope_id uuid NOT NULL,
                subscriber_id uuid NOT NULL,
                attempt_id uuid NOT NULL,
                status text NOT NULL
            )
            "#,
        )
        .execute(&database.pool)
        .await?;
        let account = create_gateway_account(&database.pool, "host_reconciliation").await?;
        let sibling = create_gateway_account(&database.pool, "host_reconciliation").await?;
        let stale =
            insert_host_charge(&database.pool, account, Utc::now() - Duration::minutes(31)).await?;
        let fresh = insert_host_charge(&database.pool, account, Utc::now()).await?;
        let sibling_stale =
            insert_host_charge(&database.pool, sibling, Utc::now() - Duration::minutes(31)).await?;

        assert_eq!(
            fail_stale_unsubmitted_host_charges(
                &database.pool,
                &ReconciliationTargets,
                GatewayAccountId::new(account.gateway_account_id),
            )
            .await?,
            1
        );
        assert_eq!(attempt_status(&database.pool, stale.0).await?, "failed");
        assert_eq!(target_status(&database.pool, stale.1).await?, "released");
        assert_eq!(attempt_status(&database.pool, fresh.0).await?, "pending");
        assert_eq!(target_status(&database.pool, fresh.1).await?, "reserved");
        assert_eq!(
            attempt_status(&database.pool, sibling_stale.0).await?,
            "pending"
        );
        assert_eq!(
            target_status(&database.pool, sibling_stale.1).await?,
            "reserved"
        );

        let mut connection = database.pool.acquire().await?;
        assert_eq!(
            host_charge_ledger_admission(
                &mut connection,
                &HostChargeLedgerAdmissionQuery::new(
                    BillingScopeId::new(account.billing_scope_id),
                    SubscriberId::new(stale.2),
                    syrup_rail::HostChargeTargetId::new(stale.1),
                    HostChargeLedgerAdmissionMode::Release,
                ),
            )
            .await?,
            HostChargeLedgerAdmission::Safe
        );
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

async fn insert_host_charge(
    pool: &sqlx::PgPool,
    account: GatewayAccountFixture,
    created_at: chrono::DateTime<Utc>,
) -> Result<(Uuid, Uuid, Uuid), sqlx::Error> {
    let attempt_id = Uuid::now_v7();
    let target_id = Uuid::now_v7();
    let subscriber_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO host_reconciliation_targets (
            id, billing_scope_id, subscriber_id, attempt_id, status
        ) VALUES ($1, $2, $3, $4, 'reserved')
        "#,
    )
    .bind(target_id)
    .bind(account.billing_scope_id)
    .bind(subscriber_id)
    .bind(attempt_id)
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
        INSERT INTO billing_payment_attempts (
            id, billing_scope_id, subscriber_id, host_charge_target_id,
            attempt_kind, status, idempotency_key, request_fingerprint,
            amount_cents, currency, gateway_account_id,
            gateway_configuration_id, gateway_order_id, created_at, updated_at
        ) VALUES (
            $1, $2, $3, $4, 'host_charge', 'pending', $5, $6,
            100, 'USD', $7, $8, $9, $10, $10
        )
        "#,
    )
    .bind(attempt_id)
    .bind(account.billing_scope_id)
    .bind(subscriber_id)
    .bind(target_id)
    .bind(format!("idem_{}", attempt_id.simple()))
    .bind(format!("host_charge:{target_id}:100:USD"))
    .bind(account.gateway_account_id)
    .bind(account.gateway_configuration_id)
    .bind(format!("order_{}", attempt_id.simple()))
    .bind(created_at)
    .execute(pool)
    .await?;
    Ok((attempt_id, target_id, subscriber_id))
}

async fn attempt_status(pool: &sqlx::PgPool, attempt_id: Uuid) -> Result<String, sqlx::Error> {
    sqlx::query_scalar("SELECT status FROM billing_payment_attempts WHERE id = $1")
        .bind(attempt_id)
        .fetch_one(pool)
        .await
}

async fn target_status(pool: &sqlx::PgPool, target_id: Uuid) -> Result<String, sqlx::Error> {
    sqlx::query_scalar("SELECT status FROM host_reconciliation_targets WHERE id = $1")
        .bind(target_id)
        .fetch_one(pool)
        .await
}
