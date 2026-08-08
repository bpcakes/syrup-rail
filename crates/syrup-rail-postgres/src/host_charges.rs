use sqlx::PgConnection;
use syrup_rail::{
    BillingScopeId, HostChargeTargetId, IdempotencyKey, PaymentAttemptId, SubscriberId,
};
use thiserror::Error;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HostChargeLedgerAdmissionMode {
    Reserve { idempotency_key: IdempotencyKey },
    Submit { attempt_id: PaymentAttemptId },
    Release,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostChargeLedgerAdmissionQuery {
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    target_id: HostChargeTargetId,
    mode: HostChargeLedgerAdmissionMode,
}

impl HostChargeLedgerAdmissionQuery {
    pub const fn new(
        billing_scope_id: BillingScopeId,
        subscriber_id: SubscriberId,
        target_id: HostChargeTargetId,
        mode: HostChargeLedgerAdmissionMode,
    ) -> Self {
        Self {
            billing_scope_id,
            subscriber_id,
            target_id,
            mode,
        }
    }

    pub const fn billing_scope_id(&self) -> BillingScopeId {
        self.billing_scope_id
    }

    pub const fn subscriber_id(&self) -> SubscriberId {
        self.subscriber_id
    }

    pub const fn target_id(&self) -> HostChargeTargetId {
        self.target_id
    }

    pub const fn mode(&self) -> &HostChargeLedgerAdmissionMode {
        &self.mode
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostChargeLedgerAdmission {
    Safe,
    IdempotentContender,
    Unsafe,
}

#[derive(Debug, Error)]
pub enum HostChargeLedgerAdmissionError {
    #[error("host charge ledger admission query failed")]
    Sql(#[from] sqlx::Error),
    #[error("host charge ledger admission returned an invalid result")]
    InvalidResult,
}

pub async fn host_charge_ledger_admission(
    connection: &mut PgConnection,
    query: &HostChargeLedgerAdmissionQuery,
) -> Result<HostChargeLedgerAdmission, HostChargeLedgerAdmissionError> {
    let (mode, idempotency_key, attempt_id) = match query.mode() {
        HostChargeLedgerAdmissionMode::Reserve { idempotency_key } => {
            ("reserve", Some(idempotency_key.expose()), None)
        }
        HostChargeLedgerAdmissionMode::Submit { attempt_id } => {
            ("submit", None, Some(attempt_id.into_uuid()))
        }
        HostChargeLedgerAdmissionMode::Release => ("release", None, None),
    };
    let result: String = sqlx::query_scalar(
        r#"
        SELECT billing_host_charge_ledger_admission($1, $2, $3, $4, $5, $6)
        "#,
    )
    .bind(query.billing_scope_id().into_uuid())
    .bind(query.subscriber_id().into_uuid())
    .bind(query.target_id().into_uuid())
    .bind(mode)
    .bind(idempotency_key)
    .bind(attempt_id)
    .fetch_one(connection)
    .await?;

    match result.as_str() {
        "safe" => Ok(HostChargeLedgerAdmission::Safe),
        "idempotent_contender" => Ok(HostChargeLedgerAdmission::IdempotentContender),
        "unsafe" => Ok(HostChargeLedgerAdmission::Unsafe),
        _ => Err(HostChargeLedgerAdmissionError::InvalidResult),
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use uuid::Uuid;

    use super::*;
    use crate::test_support::{TestDatabase, create_gateway_account};

    #[tokio::test]
    async fn admission_distinguishes_safe_contender_and_unsafe_modes() -> Result<(), Box<dyn Error>>
    {
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
                    gateway_configuration_id, gateway_order_id
                ) VALUES (
                    $1, $2, $3, $4, 'host_charge', 'pending', $5, $6,
                    100, 'USD', $7, $8, 'host-charge-test-order'
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
}
