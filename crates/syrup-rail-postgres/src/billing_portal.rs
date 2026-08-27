use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, Row, postgres::PgArguments, query::Query};
use syrup_rail::{
    BillingPeriod, Entitlement, EntitlementQuery, Money, PaymentAttemptId, PaymentAttemptKind,
    PaymentAttemptStatus, SubscriptionBillingPortalQuery, SubscriptionBillingPortalSnapshot,
    SubscriptionPaymentHistoryCursor, SubscriptionPaymentHistoryItem,
    SubscriptionPaymentHistoryPage, SubscriptionPaymentHistoryPageLimit,
    SubscriptionPaymentMethodDisplay,
};
use thiserror::Error;

use crate::entitlement::{EntitlementQueryError, entitlement_on_connection};

const INVALID_BILLING_PORTAL_STATE: &str = "canonical subscription billing portal state is invalid";
const SUBSCRIPTION_PAYMENT_HISTORY_FIRST_PAGE_SQL: &str = concat!(
    include_str!("billing_portal/subscription_payment_history_page_head.sql"),
    include_str!("billing_portal/subscription_payment_history_page_body.sql"),
    "LIMIT $4\n"
);
const SUBSCRIPTION_PAYMENT_HISTORY_CONTINUATION_SQL: &str = concat!(
    include_str!("billing_portal/subscription_payment_history_page_head.sql"),
    "    AND (created_at, id) < ($4::timestamptz, $5::uuid)\n",
    include_str!("billing_portal/subscription_payment_history_page_body.sql"),
    "LIMIT $6\n"
);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SubscriptionPaymentHistoryPageQuery {
    First,
    Continuation(SubscriptionPaymentHistoryCursor),
}

impl SubscriptionPaymentHistoryPageQuery {
    fn from_cursor(cursor: Option<&SubscriptionPaymentHistoryCursor>) -> Self {
        match cursor {
            Some(cursor) => Self::Continuation(*cursor),
            None => Self::First,
        }
    }

    const fn sql(self) -> &'static str {
        match self {
            Self::First => SUBSCRIPTION_PAYMENT_HISTORY_FIRST_PAGE_SQL,
            Self::Continuation(_) => SUBSCRIPTION_PAYMENT_HISTORY_CONTINUATION_SQL,
        }
    }

    fn bind<'args>(
        self,
        identity: &'args SubscriptionBillingPortalQuery,
        limit: SubscriptionPaymentHistoryPageLimit,
    ) -> Query<'args, Postgres, PgArguments> {
        let query = sqlx::query(self.sql())
            .bind(identity.billing_scope_id().into_uuid())
            .bind(identity.subscriber_id().into_uuid())
            .bind(identity.plan_key().as_str());
        match self {
            Self::First => query.bind(limit.get() + 1),
            Self::Continuation(cursor) => query
                .bind(cursor.created_at())
                .bind(cursor.payment_attempt_id().into_uuid())
                .bind(limit.get() + 1),
        }
    }
}

/// Error returned while loading a customer-facing billing portal projection.
#[derive(Debug, Error)]
pub enum SubscriptionBillingPortalQueryError {
    #[error("subscription billing portal query failed")]
    Sql(#[from] sqlx::Error),
    #[error("{0}")]
    InvalidState(&'static str),
}

/// Loads a provider-neutral customer billing portal from one PostgreSQL snapshot.
///
/// The returned entitlement is the canonical [`syrup_rail::Entitlement`]
/// projection. Its stored method display contains only safe masked-card fields
/// for the selected subscription; missing and grant-only entitlements have no
/// payment-method display, as do disabled or fully scrubbed methods. Hosts
/// must authenticate and authorize `query` before calling this read.
pub async fn subscription_billing_portal(
    pool: &PgPool,
    query: &SubscriptionBillingPortalQuery,
) -> Result<SubscriptionBillingPortalSnapshot, SubscriptionBillingPortalQueryError> {
    let mut transaction = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *transaction)
        .await?;

    let entitlement_query = EntitlementQuery::new(
        query.billing_scope_id(),
        query.subscriber_id(),
        query.plan_key().clone(),
    )
    .across_gateway_account_modes();
    let entitlement = entitlement_on_connection(&mut transaction, &entitlement_query)
        .await
        .map_err(map_entitlement_error)?;
    let payment_method_display = match payment_method_id(&entitlement) {
        Some(payment_method_id) => {
            payment_method_display(&mut transaction, query, payment_method_id).await?
        }
        None => None,
    };

    transaction.commit().await?;
    Ok(SubscriptionBillingPortalSnapshot::new(
        entitlement,
        payment_method_display,
    ))
}

/// Returns one strict descending page of exact-plan subscription payment history.
///
/// The page deliberately selects only provider-neutral attempt lifecycle,
/// money, billing-period, and timestamp facts. It never reads provider
/// payment-method references, gateway transaction identifiers, contacts,
/// response text, or raw diagnostics.
pub async fn subscription_payment_history_page(
    pool: &PgPool,
    query: &SubscriptionBillingPortalQuery,
    cursor: Option<&SubscriptionPaymentHistoryCursor>,
    limit: SubscriptionPaymentHistoryPageLimit,
) -> Result<SubscriptionPaymentHistoryPage, SubscriptionBillingPortalQueryError> {
    let rows = SubscriptionPaymentHistoryPageQuery::from_cursor(cursor)
        .bind(query, limit)
        .fetch_all(pool)
        .await?;

    let mut items = rows
        .iter()
        .map(subscription_payment_history_item_from_row)
        .collect::<Result<Vec<_>, _>>()?;
    let has_more = items.len() > limit.get() as usize;
    if has_more {
        items.pop();
    }
    let next_cursor = has_more.then(|| {
        let item = items
            .last()
            .expect("a page with an extra row always retains one item");
        SubscriptionPaymentHistoryCursor::new(item.created_at(), item.payment_attempt_id())
    });

    Ok(SubscriptionPaymentHistoryPage::new(items, next_cursor))
}

fn map_entitlement_error(error: EntitlementQueryError) -> SubscriptionBillingPortalQueryError {
    match error {
        EntitlementQueryError::Sql(error) => SubscriptionBillingPortalQueryError::Sql(error),
        EntitlementQueryError::InvalidState(_) => {
            SubscriptionBillingPortalQueryError::InvalidState(INVALID_BILLING_PORTAL_STATE)
        }
    }
}

fn payment_method_id(entitlement: &Entitlement) -> Option<syrup_rail::PaymentMethodId> {
    match entitlement {
        Entitlement::PaidActive { subscription, .. }
        | Entitlement::PaidThroughCancellation { subscription, .. }
        | Entitlement::PastDue { subscription, .. } => Some(subscription.payment_method_id()),
        Entitlement::Missing { .. } | Entitlement::Granted { .. } => None,
    }
}

async fn payment_method_display(
    connection: &mut sqlx::PgConnection,
    query: &SubscriptionBillingPortalQuery,
    payment_method_id: syrup_rail::PaymentMethodId,
) -> Result<Option<SubscriptionPaymentMethodDisplay>, SubscriptionBillingPortalQueryError> {
    let row = sqlx::query(
        r#"
        SELECT card_brand, card_last4, card_exp_month, card_exp_year
        FROM billing_payment_methods
        WHERE id = $1
            AND billing_scope_id = $2
            AND subscriber_id = $3
            AND status = 'active'
        "#,
    )
    .bind(payment_method_id.as_uuid())
    .bind(query.billing_scope_id().as_uuid())
    .bind(query.subscriber_id().as_uuid())
    .fetch_optional(connection)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };

    let card_brand: Option<String> = row.try_get("card_brand")?;
    let card_last_four: Option<String> = row.try_get("card_last4")?;
    let card_expiration_month = row
        .try_get::<Option<i16>, _>("card_exp_month")?
        .map(|value| u8::try_from(value).map_err(|_| invalid_state()))
        .transpose()?;
    let card_expiration_year = row
        .try_get::<Option<i16>, _>("card_exp_year")?
        .map(|value| u16::try_from(value).map_err(|_| invalid_state()))
        .transpose()?;
    SubscriptionPaymentMethodDisplay::from_provider_parts(
        card_brand.as_deref(),
        card_last_four,
        card_expiration_month,
        card_expiration_year,
    )
    .map_err(|_| invalid_state())
}

fn subscription_payment_history_item_from_row(
    row: &sqlx::postgres::PgRow,
) -> Result<SubscriptionPaymentHistoryItem, SubscriptionBillingPortalQueryError> {
    let kind = row
        .try_get::<String, _>("attempt_kind")?
        .parse::<PaymentAttemptKind>()
        .map_err(|_| invalid_state())?;
    let status = row
        .try_get::<String, _>("status")?
        .parse::<PaymentAttemptStatus>()
        .map_err(|_| invalid_state())?;
    let currency = syrup_rail::CurrencyCode::new(&row.try_get::<String, _>("currency")?)
        .map_err(|_| invalid_state())?;
    let amount = Money::new(row.try_get("amount_cents")?, currency).map_err(|_| invalid_state())?;
    let billing_period = billing_period_from_row(row)?;

    SubscriptionPaymentHistoryItem::new(
        PaymentAttemptId::new(row.try_get("id")?),
        kind,
        status,
        amount,
        billing_period,
        row.try_get::<Option<DateTime<Utc>>, _>("submitted_at")?,
        row.try_get::<Option<DateTime<Utc>>, _>("resolved_at")?,
        row.try_get("created_at")?,
    )
    .map_err(|_| invalid_state())
}

fn billing_period_from_row(
    row: &sqlx::postgres::PgRow,
) -> Result<Option<BillingPeriod>, SubscriptionBillingPortalQueryError> {
    let start_at = row.try_get::<Option<DateTime<Utc>>, _>("billing_period_start_at")?;
    let end_at = row.try_get::<Option<DateTime<Utc>>, _>("billing_period_end_at")?;
    match (start_at, end_at) {
        (None, None) => Ok(None),
        (Some(start_at), Some(end_at)) => BillingPeriod::new(start_at, end_at)
            .map(Some)
            .map_err(|_| invalid_state()),
        _ => Err(invalid_state()),
    }
}

fn invalid_state() -> SubscriptionBillingPortalQueryError {
    SubscriptionBillingPortalQueryError::InvalidState(INVALID_BILLING_PORTAL_STATE)
}

#[cfg(test)]
mod tests;
