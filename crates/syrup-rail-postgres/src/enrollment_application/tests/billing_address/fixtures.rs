use super::*;

pub(super) async fn method_address(
    pool: &sqlx::PgPool,
    payment_method_id: PaymentMethodId,
) -> Result<AddressColumns, sqlx::Error> {
    checked_query_as!(
        r#"
        SELECT billing_address_line1, billing_address_line2, billing_address_city,
            billing_address_region, billing_address_postal_code, billing_address_country
        FROM billing_payment_methods
        WHERE id = $1
        "#,
        payment_method_id.as_uuid(),
    )
    .fetch_one(pool)
    .await
}

pub(super) async fn current_method(
    pool: &sqlx::PgPool,
    subscription_id: SubscriptionId,
) -> Result<PaymentMethodId, sqlx::Error> {
    sqlx::query_scalar!(
        "SELECT payment_method_id FROM billing_subscriptions WHERE id = $1",
        subscription_id.as_uuid(),
    )
    .fetch_one(pool)
    .await
    .map(PaymentMethodId::new)
}
