use sqlx::PgPool;
use syrup_rail::{BillingScopeId, GatewayAccountId, GatewayAccountReconciliationCandidate};
use uuid::Uuid;

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

#[cfg(test)]
mod tests {
    use std::error::Error;

    use syrup_rail::{
        BillingScopeId, GatewayAccountId, GatewayAccountRegistration, GatewayConfigurationId,
        GatewayProviderKey,
    };
    use uuid::Uuid;

    use super::reconciliation_gateway_accounts;
    use crate::{register_gateway_account, test_support::TestDatabase};

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
}
