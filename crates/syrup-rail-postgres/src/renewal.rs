use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, Row, Transaction, postgres::PgArguments, query::Query};
use syrup_rail::{
    BillingScopeId, PaymentAttemptId, PaymentAttemptKind, PaymentResolutionCode,
    RenewalAttemptState, RenewalDispatch, RenewalDispatchPage, RenewalDispatchPageCursor,
    SubscriptionId,
};
use thiserror::Error;

use crate::attempts::LocalAttemptPolicy;

// SQL composition contract: the head uses $4/$10 and opens
// eligible_subscriptions; the body uses $1-$3/$5-$11 and closes it. The
// shared local-status policy is $12. The continuation inserts only its
// $13/$14 keyset predicate. The final placeholder is LIMIT: $13 on the first
// page and $15 on a continuation.
const DUE_RENEWALS_FIRST_PAGE_SQL: &str = concat!(
    include_str!("renewal/due_renewals_page_head.sql"),
    include_str!("renewal/due_renewals_page_body.sql"),
    "LIMIT $13\n"
);
const DUE_RENEWALS_CONTINUATION_SQL: &str = concat!(
    include_str!("renewal/due_renewals_page_head.sql"),
    "        AND (subscriptions.next_payment_attempt_at, subscriptions.id)\n",
    "            > ($13::timestamptz, $14::uuid)\n",
    include_str!("renewal/due_renewals_page_body.sql"),
    "LIMIT $15\n"
);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DueRenewalPageQuery {
    First(DateTime<Utc>),
    Continuation(RenewalDispatchPageCursor),
}

impl DueRenewalPageQuery {
    async fn load(
        pool: &PgPool,
        cursor: Option<&RenewalDispatchPageCursor>,
    ) -> Result<Self, sqlx::Error> {
        match cursor {
            Some(cursor) => Ok(Self::Continuation(*cursor)),
            None => sqlx::query_scalar::<_, DateTime<Utc>>("SELECT clock_timestamp()")
                .fetch_one(pool)
                .await
                .map(Self::First),
        }
    }

    const fn sql(self) -> &'static str {
        match self {
            Self::First(_) => DUE_RENEWALS_FIRST_PAGE_SQL,
            Self::Continuation(_) => DUE_RENEWALS_CONTINUATION_SQL,
        }
    }

    const fn observed_at(self) -> DateTime<Utc> {
        match self {
            Self::First(observed_at) => observed_at,
            Self::Continuation(cursor) => cursor.observed_at(),
        }
    }

    fn bind<'args>(
        self,
        infrastructure_retry_codes: &'args [&'static str],
        infrastructure_pacing_codes: &'args [&'static str],
    ) -> Query<'args, Postgres, PgArguments> {
        let payment_method_update_policy =
            LocalAttemptPolicy::for_kind(PaymentAttemptKind::SubscriptionPaymentMethodUpdate);
        let subscription_charge_policy =
            LocalAttemptPolicy::for_kind(PaymentAttemptKind::SubscriptionRenewal);
        let query = sqlx::query(self.sql())
            .bind(infrastructure_retry_codes)
            .bind(infrastructure_pacing_codes)
            .bind(PaymentResolutionCode::GatewayProviderRateLimitedBeforeSubmission.as_str())
            .bind(payment_method_update_policy.stale_after_seconds())
            .bind(syrup_rail::MAX_RENEWAL_INFRASTRUCTURE_ATTEMPTS_PER_PERIOD_CONFIGURATION)
            .bind(syrup_rail::RENEWAL_INFRASTRUCTURE_RETRY_AFTER_SECONDS)
            .bind(syrup_rail::RENEWAL_PROVIDER_RATE_LIMIT_FAST_RETRY_ATTEMPTS)
            .bind(syrup_rail::RENEWAL_PROVIDER_RATE_LIMIT_SLOW_RETRY_AFTER_SECONDS)
            .bind(syrup_rail::RENEWAL_PROVIDER_RATE_LIMIT_RETRY_AFTER_SECONDS)
            .bind(self.observed_at())
            .bind(subscription_charge_policy.stale_after_seconds())
            .bind(LocalAttemptPolicy::expirable_status_values());
        match self {
            Self::First(_) => query.bind(syrup_rail::RENEWAL_DISPATCH_LIMIT + 1),
            Self::Continuation(cursor) => query
                .bind(cursor.next_payment_attempt_at())
                .bind(cursor.subscription_id().into_uuid())
                .bind(syrup_rail::RENEWAL_DISPATCH_LIMIT + 1),
        }
    }
}

#[derive(Debug, Error)]
pub enum RenewalStoreError {
    #[error("renewal storage operation failed")]
    Sql(#[from] sqlx::Error),
    #[deprecated(
        note = "provider cooldown presence is enforced by the v3 schema and mandatory startup conformance; retained until a breaking release"
    )]
    #[error("a gateway provider has no canonical cooldown row")]
    MissingProviderCooldown,
}

/// Returns the first deterministic, provider-neutral renewal dispatch page.
///
/// This compatibility wrapper keeps the historical fixed one-hundred-item
/// first-page limit and ascending order. Hosts that need to drain one stable
/// observed scan should use [`due_renewals_page`] and retain its cursor.
pub async fn due_renewals(pool: &PgPool) -> Result<Vec<RenewalDispatch>, RenewalStoreError> {
    Ok(due_renewals_page(pool, None).await?.into_dispatches())
}

/// Returns one deterministic page of renewal work due in a stable scan.
///
/// On the first page PostgreSQL supplies one observed timestamp. Every
/// continuation preserves it for every time-dependent eligibility gate, while
/// a strict ascending `(next_payment_attempt_at, subscription_id)` key avoids
/// offset and timestamp-tie gaps or repeats for unchanged candidates. This is
/// not a cross-page MVCC snapshot: concurrently inserted, retimed, or
/// unblocked candidates behind the continuation key wait for a fresh scan.
/// Nor is it a dispatch lease: hosts own queue/outbox persistence and the
/// eventual renewal operation revalidates mutable state.
///
/// Hosts must successfully run [`crate::assert_runtime_schema_v3_compatible`]
/// before accepting traffic. Pagination relies on the validated immediate
/// foreign keys from subscriptions to accounts and accounts to provider
/// cooldown rows rather than repeating a global orphan scan on every page.
pub async fn due_renewals_page(
    pool: &PgPool,
    cursor: Option<&RenewalDispatchPageCursor>,
) -> Result<RenewalDispatchPage, RenewalStoreError> {
    let page_query = DueRenewalPageQuery::load(pool, cursor).await?;
    let observed_at = page_query.observed_at();
    let infrastructure_retry_codes =
        resolution_strings(PaymentResolutionCode::RENEWAL_INFRASTRUCTURE_RETRY_CODES);
    let infrastructure_pacing_codes =
        resolution_strings(PaymentResolutionCode::RENEWAL_INFRASTRUCTURE_PACING_CODES);
    let rows = page_query
        .bind(&infrastructure_retry_codes, &infrastructure_pacing_codes)
        .fetch_all(pool)
        .await?;
    let mut candidates = rows
        .iter()
        .map(|row| {
            let subscription_id = SubscriptionId::new(row.try_get("id")?);
            Ok(DueRenewalCandidate {
                dispatch: RenewalDispatch::new(
                    BillingScopeId::new(row.try_get("billing_scope_id")?),
                    subscription_id,
                    row.try_get("next_renewal_at")?,
                    row.try_get("attempt_sequence_count")?,
                ),
                next_payment_attempt_at: row.try_get("next_payment_attempt_at")?,
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()?;
    let has_more = candidates.len() > syrup_rail::RENEWAL_DISPATCH_LIMIT as usize;
    if has_more {
        candidates.pop();
    }
    let next_cursor = has_more.then(|| {
        let last = candidates
            .last()
            .expect("a renewal page with an extra row always retains one item");
        RenewalDispatchPageCursor::new(
            observed_at,
            last.next_payment_attempt_at,
            last.dispatch.subscription_id(),
        )
    });
    Ok(RenewalDispatchPage::new(
        candidates
            .into_iter()
            .map(|candidate| candidate.dispatch)
            .collect(),
        next_cursor,
    ))
}

struct DueRenewalCandidate {
    dispatch: RenewalDispatch,
    next_payment_attempt_at: DateTime<Utc>,
}

/// Computes the one shared renewal/recovery period ledger state.
pub async fn renewal_attempt_state(
    transaction: &mut Transaction<'_, Postgres>,
    subscription_id: SubscriptionId,
    period_start_at: DateTime<Utc>,
    excluded_attempt_id: Option<PaymentAttemptId>,
) -> Result<RenewalAttemptState, RenewalStoreError> {
    let policy = LocalAttemptPolicy::for_kind(PaymentAttemptKind::SubscriptionRenewal);
    let infrastructure_retry_codes =
        resolution_strings(PaymentResolutionCode::RENEWAL_INFRASTRUCTURE_RETRY_CODES);
    let infrastructure_pacing_codes =
        resolution_strings(PaymentResolutionCode::RENEWAL_INFRASTRUCTURE_PACING_CODES);
    let row = sqlx::query(
        r#"
        SELECT
            COUNT(*) AS attempt_sequence_count,
            COUNT(*) FILTER (
                WHERE attempt_kind = 'subscription_renewal'
                    AND status = 'failed'
                    AND resolution_code = ANY($4::text[])
                    AND gateway_configuration_id = (
                        SELECT accounts.gateway_configuration_id
                        FROM billing_subscriptions AS subscriptions
                        JOIN billing_gateway_accounts AS accounts
                            ON accounts.id = subscriptions.gateway_account_id
                            AND accounts.billing_scope_id = subscriptions.billing_scope_id
                        WHERE subscriptions.id = $1
                    )
            ) AS automatic_infrastructure_attempt_count,
            MAX(resolved_at) FILTER (
                WHERE attempt_kind = 'subscription_renewal'
                    AND status = 'failed'
                    AND resolution_code = ANY($5::text[])
                    AND gateway_configuration_id = (
                        SELECT accounts.gateway_configuration_id
                        FROM billing_subscriptions AS subscriptions
                        JOIN billing_gateway_accounts AS accounts
                            ON accounts.id = subscriptions.gateway_account_id
                            AND accounts.billing_scope_id = subscriptions.billing_scope_id
                        WHERE subscriptions.id = $1
                    )
            ) AS last_automatic_infrastructure_failure_at,
            COUNT(*) FILTER (
                WHERE attempt_kind = 'subscription_renewal'
                    AND status = 'failed'
                    AND resolution_code = $6
            ) AS provider_rate_limited_attempt_count,
            MAX(resolved_at) FILTER (
                WHERE attempt_kind = 'subscription_renewal'
                    AND status = 'failed'
                    AND resolution_code = $6
            ) AS last_provider_rate_limited_at,
            COALESCE(
                BOOL_OR(
                    status IN ('pending', 'unknown', 'review_required', 'approved')
                    AND NOT (
                        status = ANY($7::text[])
                        AND submitted_at IS NULL
                        AND created_at <= clock_timestamp()
                            - ($8::bigint * interval '1 second')
                    )
                ),
                false
            ) AS has_blocking_attempt
        FROM billing_payment_attempts
        WHERE attempt_kind IN ('subscription_renewal', 'subscription_recovery')
            AND subscription_id = $1
            AND billing_period_start_at = $2
            AND ($3::uuid IS NULL OR id <> $3)
        "#,
    )
    .bind(subscription_id.as_uuid())
    .bind(period_start_at)
    .bind(excluded_attempt_id.map(PaymentAttemptId::into_uuid))
    .bind(&infrastructure_retry_codes)
    .bind(&infrastructure_pacing_codes)
    .bind(PaymentResolutionCode::GatewayProviderRateLimitedBeforeSubmission.as_str())
    .bind(LocalAttemptPolicy::expirable_status_values())
    .bind(policy.stale_after_seconds())
    .fetch_one(&mut **transaction)
    .await?;
    Ok(RenewalAttemptState {
        attempt_sequence_count: row.try_get("attempt_sequence_count")?,
        automatic_infrastructure_attempt_count: row
            .try_get("automatic_infrastructure_attempt_count")?,
        last_automatic_infrastructure_failure_at: row
            .try_get("last_automatic_infrastructure_failure_at")?,
        provider_rate_limited_attempt_count: row.try_get("provider_rate_limited_attempt_count")?,
        last_provider_rate_limited_at: row.try_get("last_provider_rate_limited_at")?,
        has_blocking_attempt: row.try_get("has_blocking_attempt")?,
    })
}

fn resolution_strings(codes: &[PaymentResolutionCode]) -> Vec<&'static str> {
    codes.iter().map(|code| code.as_str()).collect()
}

#[cfg(test)]
mod tests;
