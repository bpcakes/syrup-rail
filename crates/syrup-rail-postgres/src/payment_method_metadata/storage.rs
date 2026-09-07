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

const CANDIDATE_SQL: &str = include_str!("candidate.sql");
// Keep both production statements available verbatim to the generic-plan test.
const LOCKED_CANDIDATE_SQL: &str = concat!(
    include_str!("candidate.sql"),
    " FOR UPDATE OF a, m, s FOR SHARE OF g"
);

#[cfg(test)]
#[path = "query_plan_tests.rs"]
mod query_plan_tests;

pub(super) async fn load_candidate(
    pool: &PgPool,
    command: RefreshPaymentMethodMetadata,
) -> Result<Option<Candidate>, sqlx::Error> {
    sqlx::query_as(CANDIDATE_SQL)
        .bind(command.billing_scope_id.as_uuid())
        .bind(command.subscriber_id.as_uuid())
        .bind(command.attempt_id.as_uuid())
        .fetch_optional(pool)
        .await
}

pub(super) async fn lock_candidate(
    connection: &mut PgConnection,
    command: RefreshPaymentMethodMetadata,
    candidate: &Candidate,
) -> Result<Option<Candidate>, sqlx::Error> {
    crate::enrollment_application::set_application_timeouts(connection).await?;
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
    sqlx::query_as(LOCKED_CANDIDATE_SQL)
        .bind(command.billing_scope_id.as_uuid())
        .bind(command.subscriber_id.as_uuid())
        .bind(command.attempt_id.as_uuid())
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
    let written = sqlx::query(
        r#"
        UPDATE billing_payment_methods
        SET card_brand = COALESCE(card_brand, $2),
            card_last4 = COALESCE(card_last4, $3),
            card_exp_month = COALESCE(card_exp_month, $4),
            card_exp_year = COALESCE(card_exp_year, $5),
            updated_at = clock_timestamp()
        WHERE id = $1
        "#,
    )
    .bind(current.method_id)
    .bind(brand_text)
    .bind(last4)
    .bind(month)
    .bind(year)
    .execute(connection)
    .await?;
    if written.rows_affected() != 1 {
        return Err(sqlx::Error::RowNotFound.into());
    }
    Ok(Outcome::Updated)
}

pub(super) async fn record_provider_cooldown(
    pool: &PgPool,
    command: RefreshPaymentMethodMetadata,
    account_id: syrup_rail::GatewayAccountId,
    provider: &syrup_rail::GatewayProviderKey,
) -> Result<(), sqlx::Error> {
    use crate::enrollment_application::{
        RateLimitCooldownPersistence, persist_bound_provider_rate_limit_cooldown,
        set_application_timeouts,
    };
    let mut transaction = pool.begin().await?;
    set_application_timeouts(&mut transaction).await?;
    match persist_bound_provider_rate_limit_cooldown(
        &mut transaction,
        command.billing_scope_id,
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
