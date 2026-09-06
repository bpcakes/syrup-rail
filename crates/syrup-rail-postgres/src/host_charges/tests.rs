use std::error::Error;

use uuid::Uuid;

use super::*;
use crate::test_support::{TestDatabase, create_gateway_account};

#[tokio::test]
async fn admission_distinguishes_safe_contender_and_unsafe_modes() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("rail_host_admit").await?;
    let result = async {
        let gateway = create_gateway_account(&database.pool, "test_gateway").await?;
        let subscriber_id = SubscriberId::new(Uuid::now_v7());
        let target_id = HostChargeTargetId::new(Uuid::now_v7());
        let idempotency_key = IdempotencyKey::new("host-charge-test")?;

        let mut connection = database.pool.acquire().await?;
        let reserve = HostChargeLedgerAdmissionQuery::new(
            BillingScopeId::new(gateway.billing_scope_id),
            subscriber_id,
            target_id,
            HostChargeLedgerAdmissionMode::Reserve {
                idempotency_key: idempotency_key.clone(),
            },
        );
        assert_eq!(
            host_charge_ledger_admission(&mut connection, &reserve).await?,
            HostChargeLedgerAdmission::Safe
        );

        let attempt_id = PaymentAttemptId::new(Uuid::now_v7());
        sqlx::query(
            r#"
            INSERT INTO billing_payment_attempts (
                id, billing_scope_id, subscriber_id, host_charge_target_id,
                attempt_kind, status, idempotency_key, request_fingerprint,
                amount_cents, currency, gateway_account_id,
                gateway_configuration_id, gateway_order_id,
                required_gateway_account_mode
            ) VALUES (
                $1, $2, $3, $4, 'host_charge', 'pending', $5, $6,
                100, 'USD', $7, $8, 'host-charge-test-order', 'live'
            )
            "#,
        )
        .bind(attempt_id.as_uuid())
        .bind(gateway.billing_scope_id)
        .bind(subscriber_id.as_uuid())
        .bind(target_id.as_uuid())
        .bind(idempotency_key.expose())
        .bind(format!("host_charge:{}:100:USD", target_id.as_uuid()))
        .bind(gateway.gateway_account_id)
        .bind(gateway.gateway_configuration_id)
        .execute(&mut *connection)
        .await?;

        assert_eq!(
            host_charge_ledger_admission(&mut connection, &reserve).await?,
            HostChargeLedgerAdmission::IdempotentContender
        );
        let submit = HostChargeLedgerAdmissionQuery::new(
            BillingScopeId::new(gateway.billing_scope_id),
            subscriber_id,
            target_id,
            HostChargeLedgerAdmissionMode::Submit { attempt_id },
        );
        assert_eq!(
            host_charge_ledger_admission(&mut connection, &submit).await?,
            HostChargeLedgerAdmission::Safe
        );
        sqlx::query(
            "UPDATE billing_payment_attempts SET gateway_approval_evidence = 'text_only' WHERE id = $1",
        )
        .bind(attempt_id.as_uuid())
        .execute(&mut *connection)
        .await?;
        assert_eq!(
            host_charge_ledger_admission(&mut connection, &submit).await?,
            HostChargeLedgerAdmission::Unsafe
        );
        sqlx::query(
            "UPDATE billing_payment_attempts SET status = 'failed', resolved_at = clock_timestamp() WHERE id = $1",
        )
        .bind(attempt_id.as_uuid())
        .execute(&mut *connection)
        .await?;
        let new_reserve = HostChargeLedgerAdmissionQuery::new(
            BillingScopeId::new(gateway.billing_scope_id),
            subscriber_id,
            target_id,
            HostChargeLedgerAdmissionMode::Reserve {
                idempotency_key: IdempotencyKey::new("host-charge-new")?,
            },
        );
        assert_eq!(
            host_charge_ledger_admission(&mut connection, &new_reserve).await?,
            HostChargeLedgerAdmission::Unsafe
        );
        let release = HostChargeLedgerAdmissionQuery::new(
            BillingScopeId::new(gateway.billing_scope_id),
            subscriber_id,
            target_id,
            HostChargeLedgerAdmissionMode::Release,
        );
        assert_eq!(
            host_charge_ledger_admission(&mut connection, &release).await?,
            HostChargeLedgerAdmission::Unsafe
        );
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}
