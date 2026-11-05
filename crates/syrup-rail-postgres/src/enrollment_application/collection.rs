//! Revalidate collection authority before applying renewal or recovery.
use super::*;

pub(super) async fn recovery_subscription_matches(
    connection: &mut PgConnection,
    reservation: &SubscriptionRecoveryReservation,
) -> Result<bool, SubscriptionEnrollmentApplicationError> {
    let identity = reservation.identity();
    let expected = reservation.expected_state();
    let row = sqlx::query(
        r#"
        SELECT status, payment_method_id, initial_transaction_id, next_renewal_at, next_payment_attempt_at,
            required_gateway_account_mode
        FROM billing_subscriptions
        WHERE id = $1 AND billing_scope_id = $2 AND subscriber_id = $3
            AND gateway_account_id = $4 AND plan_key = $5
        FOR UPDATE
        "#,
    )
    .bind(reservation.subscription_id().as_uuid())
    .bind(identity.billing_scope_id().as_uuid())
    .bind(identity.subscriber_id().as_uuid())
    .bind(identity.gateway_account_id().as_uuid())
    .bind(reservation.plan_key().as_str())
    .fetch_optional(&mut *connection)
    .await?;
    let Some(row) = row else {
        return Ok(false);
    };
    let collection = crate::subscription_persistence::collection_state_from_row(&row)
        .map_err(map_subscription_persistence_error)?;
    let status = collection.status();
    let payment_method_id: Uuid = row.try_get("payment_method_id")?;
    let initial_transaction_id: String = row.try_get("initial_transaction_id")?;
    let next_renewal_at: DateTime<Utc> = row.try_get("next_renewal_at")?;
    Ok(status == expected.status()
        && collection.accepts_payment()
        && payment_method_id == expected.payment_method_id().into_uuid()
        && syrup_rail::canonical_gateway_transaction_ids_equal(
            &initial_transaction_id,
            expected.initial_transaction_id().expose(),
        )
        && row.try_get::<String, _>("required_gateway_account_mode")?
            == identity.required_gateway_account_mode().as_str()
        && next_renewal_at == *reservation.period().start_at())
}

pub(super) async fn renewal_subscription_matches(
    connection: &mut PgConnection,
    reservation: &SubscriptionRenewalReservation,
) -> Result<bool, SubscriptionEnrollmentApplicationError> {
    let identity = reservation.identity();
    let expected = reservation.expected_state();
    let row = sqlx::query(
        r#"
        SELECT status, payment_method_id, initial_transaction_id,
            amount_cents, currency, next_renewal_at, next_payment_attempt_at, required_gateway_account_mode
        FROM billing_subscriptions
        WHERE id = $1 AND billing_scope_id = $2 AND subscriber_id = $3
            AND gateway_account_id = $4 AND plan_key = $5
        FOR UPDATE
        "#,
    )
    .bind(reservation.subscription_id().as_uuid())
    .bind(identity.billing_scope_id().as_uuid())
    .bind(identity.subscriber_id().as_uuid())
    .bind(identity.gateway_account_id().as_uuid())
    .bind(reservation.plan_key().as_str())
    .fetch_optional(&mut *connection)
    .await?;
    let Some(row) = row else {
        return Ok(false);
    };
    let collection = crate::subscription_persistence::collection_state_from_row(&row)
        .map_err(map_subscription_persistence_error)?;
    let status = collection.status();
    let initial_transaction_id: String = row.try_get("initial_transaction_id")?;
    Ok(status == expected.status()
        && collection.accepts_payment()
        && row.try_get::<Uuid, _>("payment_method_id")? == expected.payment_method_id().into_uuid()
        && syrup_rail::canonical_gateway_transaction_ids_equal(
            &initial_transaction_id,
            expected.initial_transaction_id().expose(),
        )
        && row.try_get::<i32, _>("amount_cents")? == reservation.request().amount().cents()
        && row.try_get::<String, _>("currency")?
            == reservation.request().amount().currency().as_str()
        && row.try_get::<String, _>("required_gateway_account_mode")?
            == identity.required_gateway_account_mode().as_str()
        && row.try_get::<DateTime<Utc>, _>("next_renewal_at")? == *reservation.period().start_at())
}
