use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgConnection, PgPool};
use syrup_rail::{GatewayDiagnostic, GatewayPaymentDescriptor, PaymentCardBrand, PlanKey};
use uuid::Uuid;

use super::{
    PaymentMethodMetadataRefreshError, PaymentMethodMetadataRefreshOutcome as Outcome,
    RefreshPaymentMethodMetadata,
};

#[derive(FromRow, PartialEq)]
pub(super) struct Candidate {
    pub account_id: Uuid,
    pub configuration_id: Uuid,
    pub provider_key: String,
    pub transaction_id: String,
    pub reference: String,
    method_id: Uuid,
    subscription_id: Uuid,
    plan_key: String,
    method_updated_at: DateTime<Utc>,
    subscription_updated_at: DateTime<Utc>,
    card_brand: Option<String>,
    card_last4: Option<String>,
    card_exp_month: Option<i16>,
    card_exp_year: Option<i16>,
}

impl Candidate {
    pub fn complete(&self) -> bool {
        self.card_brand.is_some()
            && self.card_last4.is_some()
            && self.card_exp_month.is_some()
            && self.card_exp_year.is_some()
    }
}

// Keep the plan tests and the checked production statements on the same SQL.
macro_rules! candidate_statement {
    ($query:ident, $lock:literal $(, $arg:expr)*) => {
        $query!(r#"SELECT g.id AS account_id, g.gateway_configuration_id AS configuration_id,
       g.provider_key, a.gateway_transaction_id AS transaction_id,
       m.gateway_payment_method_reference AS reference, m.id AS method_id,
       s.id AS subscription_id, s.plan_key,
       m.updated_at AS method_updated_at, s.updated_at AS subscription_updated_at,
       m.card_brand, m.card_last4, m.card_exp_month, m.card_exp_year
FROM billing_payment_attempts AS a
JOIN billing_gateway_accounts AS g
  ON g.id = a.gateway_account_id AND g.billing_scope_id = a.billing_scope_id
JOIN billing_payment_methods AS m
  ON m.id = a.payment_method_id AND m.billing_scope_id = a.billing_scope_id
 AND m.subscriber_id = a.subscriber_id AND m.gateway_account_id = a.gateway_account_id
 AND (m.gateway_payment_method_reference = a.gateway_payment_method_reference
      OR (a.attempt_kind = 'subscription_renewal'
          AND a.gateway_payment_method_reference IS NULL))
JOIN billing_subscriptions AS s
  ON s.id = a.subscription_id AND s.billing_scope_id = a.billing_scope_id
 AND s.subscriber_id = a.subscriber_id AND s.gateway_account_id = a.gateway_account_id
 AND s.plan_key = a.plan_key
WHERE a.billing_scope_id = $1 AND a.subscriber_id = $2 AND a.id = $3
  AND a.status = 'approved' AND a.gateway_transaction_id IS NOT NULL
  AND m.status = 'active'
  AND m.gateway_payment_method_reference <> 'erased:' || m.id::text
  AND EXISTS (
      SELECT 1 FROM billing_payment_attempts AS current_approval
      JOIN billing_subscriptions AS current_subscription
        ON current_subscription.id = current_approval.subscription_id
       AND current_subscription.plan_key = current_approval.plan_key
      WHERE current_approval.payment_method_id = m.id
        AND current_approval.status = 'approved'
        AND current_subscription.billing_scope_id = a.billing_scope_id
        AND current_subscription.subscriber_id = a.subscriber_id
        AND current_subscription.gateway_account_id = a.gateway_account_id
        AND current_subscription.payment_method_id = m.id
  )
  AND NOT EXISTS (
      SELECT 1 FROM billing_payment_attempts AS newer
      WHERE newer.payment_method_id = m.id AND newer.status = 'approved'
        AND (newer.resolved_at, newer.id) > (a.resolved_at, a.id)
  )
"# + $lock $(, $arg)*)
    };
}

#[cfg(test)]
const CANDIDATE_SQL: &str = candidate_statement!(concat_sql, "");
#[cfg(test)]
const LOCKED_CANDIDATE_SQL: &str =
    candidate_statement!(concat_sql, " FOR UPDATE OF a, m, s FOR SHARE OF g");

macro_rules! candidate_query {
    ($($sql:tt)*) => {
        checked_query_as!(Candidate, $($sql)*)
    };
}

#[cfg(test)]
#[path = "query_plan_tests.rs"]
mod query_plan_tests;

pub(super) async fn load_candidate(
    connection: &mut PgConnection,
    command: RefreshPaymentMethodMetadata,
) -> Result<Option<Candidate>, sqlx::Error> {
    candidate_statement!(
        candidate_query,
        "",
        command.billing_scope_id.into_uuid(),
        command.subscriber_id.into_uuid(),
        command.attempt_id.into_uuid()
    )
    .fetch_optional(connection)
    .await
}

pub(super) async fn lock_candidate(
    connection: &mut PgConnection,
    command: RefreshPaymentMethodMetadata,
    candidate: &Candidate,
) -> Result<Option<Candidate>, sqlx::Error> {
    crate::enrollment_application::set_application_timeouts(connection).await?;
    // Give the scoped cancellation test time to abort, with a server-side bound.
    #[cfg(test)]
    if super::EXTEND_WRITE_WAIT.try_with(|()| ()).is_ok() {
        checked_query!("SELECT set_config('lock_timeout', '60s', true) AS lock_timeout, set_config('statement_timeout', '60s', true)")
            .execute(&mut *connection).await?;
    }
    // v0.5.2 scrub and approval use different domains. Enter both before any row
    // locks, then the approval plan aggregate. Neither lock is held during I/O.
    // The approval domain spans this subscriber's plans: it also stabilizes the
    // current-reference EXISTS check when another plan still uses the method.
    // Approved attempt identity is rechecked by the query. Its lifecycle-only
    // updates do not invalidate display; method/subscription timestamps still
    // fence intervening projection changes, including away-and-back replacement.
    crate::deletion::lock_payment_method_scrub_domain(
        connection,
        command.billing_scope_id,
        command.subscriber_id,
        &candidate.account_id,
    )
    .await?;
    crate::enrollment_application::lock_payment_method_domain(
        connection,
        command.subscriber_id,
        &candidate.account_id,
    )
    .await?;
    let plan_key =
        PlanKey::new(&candidate.plan_key).map_err(|error| sqlx::Error::Decode(Box::new(error)))?;
    crate::attempts::lock_subscription_aggregate(connection, command.subscriber_id, &plan_key)
        .await?;
    // Keep configuration/provider identity stable from revalidation through
    // commit. Snapshot comparison alone cannot close that final write window.
    candidate_statement!(
        candidate_query,
        " FOR UPDATE OF a, m, s FOR SHARE OF g",
        command.billing_scope_id.into_uuid(),
        command.subscriber_id.into_uuid(),
        command.attempt_id.into_uuid()
    )
    .fetch_optional(connection)
    .await
}

pub(super) async fn fill_missing_fields(
    connection: &mut PgConnection,
    current: &Candidate,
    descriptor: &GatewayPaymentDescriptor,
) -> Result<Outcome, PaymentMethodMetadataRefreshError> {
    // Preserve the provider evidence format used by approval. Presentation wire
    // labels are a separate contract and must not become a second storage codec.
    let brand_text = descriptor
        .card_brand()
        .map(GatewayDiagnostic::expose)
        .filter(|value| !value.is_empty());
    let brand = known_brand(brand_text);
    let last4 = descriptor.card_last_four().map(|value| value.expose());
    let month = descriptor.card_exp_month();
    let year = descriptor.card_exp_year();
    if conflicts(known_brand(current.card_brand.as_deref()), brand)
        || conflicts(current.card_last4.as_deref(), last4)
        || conflicts(current.card_exp_month, month)
        || conflicts(current.card_exp_year, year)
    {
        return Ok(Outcome::EvidenceRejected);
    }
    if !(current.card_brand.is_none() && brand_text.is_some()
        || current.card_last4.is_none() && last4.is_some()
        || current.card_exp_month.is_none() && month.is_some()
        || current.card_exp_year.is_none() && year.is_some())
    {
        return Ok(Outcome::Unchanged);
    }
    let written = sqlx::query!(
        r#"
        UPDATE billing_payment_methods
        SET card_brand = COALESCE(card_brand, $2),
            card_last4 = COALESCE(card_last4, $3),
            card_exp_month = COALESCE(card_exp_month, $4),
            card_exp_year = COALESCE(card_exp_year, $5),
            updated_at = clock_timestamp()
        WHERE id = $1
        "#,
        current.method_id,
        brand_text,
        last4,
        month,
        year,
    )
    .execute(connection)
    .await?;
    if written.rows_affected() != 1 {
        return Err(sqlx::Error::RowNotFound.into());
    }
    Ok(Outcome::Updated)
}

/// Extends the shared provider cooldown after a read-only query was
/// rate-limited, using the ordinary billing database timeouts.
pub(super) async fn record_provider_cooldown(
    pool: &PgPool,
    billing_scope_id: syrup_rail::BillingScopeId,
    account_id: syrup_rail::GatewayAccountId,
    provider: &syrup_rail::GatewayProviderKey,
) -> Result<(), sqlx::Error> {
    let mut transaction = pool.begin().await?;
    crate::enrollment_application::set_application_timeouts(&mut transaction).await?;
    persist_provider_cooldown(transaction, billing_scope_id, account_id, provider).await
}

/// Extends the shared provider cooldown in a transaction whose timeouts the
/// caller has already set. Used by saved-card metadata repair and payment
/// diagnostics; it is the only write either read-only query may perform.
pub(crate) async fn persist_provider_cooldown(
    mut transaction: sqlx::Transaction<'_, sqlx::Postgres>,
    billing_scope_id: syrup_rail::BillingScopeId,
    account_id: syrup_rail::GatewayAccountId,
    provider: &syrup_rail::GatewayProviderKey,
) -> Result<(), sqlx::Error> {
    use crate::enrollment_application::{
        RateLimitCooldownPersistence, persist_bound_provider_rate_limit_cooldown,
    };
    match persist_bound_provider_rate_limit_cooldown(
        &mut transaction,
        billing_scope_id,
        account_id,
        provider,
    )
    .await?
    {
        RateLimitCooldownPersistence::Applied => transaction.commit().await?,
        // The error belongs to the originally resolved provider. Never extend
        // cooldown for a different provider after a concurrent reconfiguration.
        RateLimitCooldownPersistence::IdentityChanged => {}
        RateLimitCooldownPersistence::MissingProviderCooldown => {
            return Err(sqlx::Error::RowNotFound);
        }
    }
    Ok(())
}

fn conflicts<T: PartialEq>(existing: Option<T>, observed: Option<T>) -> bool {
    matches!((existing, observed), (Some(existing), Some(observed)) if existing != observed)
}

fn known_brand(value: Option<&str>) -> Option<PaymentCardBrand> {
    // Unknown text is retained, but cannot establish a brand disagreement.
    value
        .and_then(PaymentCardBrand::from_provider)
        .filter(|brand| *brand != PaymentCardBrand::Other)
}
