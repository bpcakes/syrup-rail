use sqlx::PgConnection;
use syrup_rail::{BillingScopeId, ScrubSubscriberBillingData, ScrubbedBillingRows, SubscriberId};

mod blockers;
pub use blockers::billing_deletion_blockers;

/// Removes the canonical mutable billing PII for one subscriber.
///
/// The caller owns deletion admission, must stabilize creation of new billing
/// rows for the subscriber, and must commit or roll back the surrounding host
/// deletion transaction. Following the [`crate::BillingTransactionCoordinator`]
/// contract, the host takes its own subject lock before calling this function
/// and holds no Syrup Rail aggregate or billing row locks. The scrub then
/// enters each affected gateway account's scrub domain and approval domain, in
/// ascending account order, before updating any row, so a concurrent approval
/// or renewal reservation cannot restore contact or address data mid-scrub.
/// Names, email, billing addresses, provider references, response text, and
/// card display are cleared; closed decision codes such as
/// `gateway_response_code` and `gateway_avs_response` are retained, and
/// immutable processor-charge observations are never selected or mutated.
pub async fn scrub_subscriber_billing_data(
    connection: &mut PgConnection,
    command: ScrubSubscriberBillingData,
) -> Result<ScrubbedBillingRows, sqlx::Error> {
    let billing_scope_id = command.billing_scope_id().into_uuid();
    let subscriber_id = command.subscriber_id().into_uuid();

    // Payment methods are shared across plan aggregates. Enter every affected
    // subscriber/account mutation domain in deterministic account order before
    // locking either the attempts or methods that carry its mutable projection.
    let gateway_account_ids = sqlx::query_scalar::<_, uuid::Uuid>(
        r#"
        SELECT gateway_account_id
        FROM (
            SELECT gateway_account_id
            FROM billing_payment_attempts
            WHERE billing_scope_id = $1 AND subscriber_id = $2

            UNION

            SELECT gateway_account_id
            FROM billing_payment_methods
            WHERE billing_scope_id = $1 AND subscriber_id = $2
        ) AS affected_accounts
        ORDER BY gateway_account_id
        "#,
    )
    .bind(billing_scope_id)
    .bind(subscriber_id)
    .fetch_all(&mut *connection)
    .await?;
    for gateway_account_id in &gateway_account_ids {
        lock_payment_method_scrub_domain(
            connection,
            command.billing_scope_id(),
            command.subscriber_id(),
            gateway_account_id,
        )
        .await?;
    }
    // Approval writers and renewal reservation use the approval domain. Enter
    // it after the scrub domains and before any row lock, matching the global
    // order used by saved-card metadata repair.
    crate::enrollment_application::lock_payment_method_domains(
        connection,
        command.subscriber_id(),
        gateway_account_ids,
    )
    .await?;

    let payment_attempts = sqlx::query(
        r#"
        UPDATE billing_payment_attempts
        SET billing_first_name = NULL,
            billing_last_name = NULL,
            billing_email = NULL,
            billing_address_line1 = NULL,
            billing_address_line2 = NULL,
            billing_address_city = NULL,
            billing_address_region = NULL,
            billing_address_postal_code = NULL,
            billing_address_country = NULL,
            gateway_response = NULL,
            gateway_response_text = NULL,
            gateway_payment_method_reference = NULL,
            payment_type = NULL,
            card_brand = NULL,
            card_last4 = NULL,
            card_exp_month = NULL,
            card_exp_year = NULL,
            updated_at = now()
        WHERE billing_scope_id = $1 AND subscriber_id = $2
        "#,
    )
    .bind(billing_scope_id)
    .bind(subscriber_id)
    .execute(&mut *connection)
    .await?
    .rows_affected();

    let payment_methods = sqlx::query(
        r#"
        UPDATE billing_payment_methods
        SET status = 'disabled',
            gateway_payment_method_reference = 'erased:' || id::text,
            billing_name = NULL,
            billing_email = NULL,
            billing_address_line1 = NULL,
            billing_address_line2 = NULL,
            billing_address_city = NULL,
            billing_address_region = NULL,
            billing_address_postal_code = NULL,
            billing_address_country = NULL,
            payment_type = NULL,
            card_brand = NULL,
            card_last4 = NULL,
            card_exp_month = NULL,
            card_exp_year = NULL,
            updated_at = now()
        WHERE billing_scope_id = $1 AND subscriber_id = $2
        "#,
    )
    .bind(billing_scope_id)
    .bind(subscriber_id)
    .execute(&mut *connection)
    .await?
    .rows_affected();

    Ok(ScrubbedBillingRows::new(payment_attempts, payment_methods))
}

/// Enters the deployed scrub domain. Its key is intentionally preserved for
/// compatibility with v0.5.2 writers; callers coordinating both workflows also
/// enter the approval domain before acquiring row locks.
pub(crate) async fn lock_payment_method_scrub_domain(
    connection: &mut PgConnection,
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    gateway_account_id: &uuid::Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('syrup-rail:payment-method:' || $1::uuid::text || ':' || $2::uuid::text || ':' || $3::uuid::text, 0))")
        .bind(billing_scope_id.as_uuid())
        .bind(gateway_account_id)
        .bind(subscriber_id.as_uuid())
        .execute(connection).await?;
    Ok(())
}

#[cfg(test)]
mod tests;
