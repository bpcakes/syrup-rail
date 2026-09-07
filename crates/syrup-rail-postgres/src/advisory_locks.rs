use sqlx::PgConnection;
use syrup_rail::{GatewayAccountId, SubscriberId};

pub(crate) async fn lock_payment_method_domain(
    connection: &mut PgConnection,
    subscriber_id: SubscriberId,
    gateway_account_id: GatewayAccountId,
) -> Result<(), sqlx::Error> {
    // This is the deployed pre-v5 approval writer key. Keep its bytes stable
    // with those writers. Pre-0.6 scrub callers used a different key and must
    // be stopped for the v5 cutover; they do not share this exclusion domain.
    // A gateway-account ID already identifies its billing scope; adding the scope or a namespace prefix would split the
    // lock domain from older writers.
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtextextended($1::uuid::text || ':' || $2::uuid::text, 0))",
    )
    .bind(gateway_account_id.as_uuid())
    .bind(subscriber_id.as_uuid())
    .execute(connection)
    .await?;
    Ok(())
}

pub(crate) async fn lock_payment_method_domains(
    connection: &mut PgConnection,
    subscriber_id: SubscriberId,
    gateway_account_ids: impl IntoIterator<Item = GatewayAccountId>,
) -> Result<(), sqlx::Error> {
    for gateway_account_id in ordered_payment_method_domains(gateway_account_ids) {
        lock_payment_method_domain(connection, subscriber_id, gateway_account_id).await?;
    }
    Ok(())
}

fn ordered_payment_method_domains(
    gateway_account_ids: impl IntoIterator<Item = GatewayAccountId>,
) -> Vec<GatewayAccountId> {
    let mut gateway_account_ids = gateway_account_ids.into_iter().collect::<Vec<_>>();
    gateway_account_ids.sort_unstable();
    gateway_account_ids.dedup();
    gateway_account_ids
}

#[cfg(test)]
mod tests {
    use syrup_rail::GatewayAccountId;
    use uuid::Uuid;

    use super::ordered_payment_method_domains;

    #[test]
    fn payment_method_domains_are_deduplicated_in_uuid_order() {
        let first = GatewayAccountId::new(Uuid::from_u128(1));
        let middle = GatewayAccountId::new(Uuid::from_u128(2));
        let last = GatewayAccountId::new(Uuid::from_u128(3));

        assert_eq!(
            ordered_payment_method_domains([last, first, middle, first]),
            vec![first, middle, last]
        );
    }
}
