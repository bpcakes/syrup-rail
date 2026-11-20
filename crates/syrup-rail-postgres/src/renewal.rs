use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, Row, Transaction, postgres::PgArguments, query::Query};
use syrup_rail::{
    BillingScopeId, GatewayAccountMode, PaymentAttemptId, PaymentAttemptKind,
    PaymentResolutionCode, RenewalAttemptState, RenewalDispatch, RenewalDispatchPage,
    RenewalDispatchPageCursor, SubscriptionId,
};
use thiserror::Error;

use crate::attempts::LocalAttemptPolicy;

// SQL composition contract: the head uses $4/$10/$12 and opens
// eligible_subscriptions; the body uses $1-$3/$5-$12 and closes it. The shared
// local expirable-status policy is $12. Filtered queries add an unconditional
// mode predicate at $13. Continuations then add their keyset predicate, and the
// final placeholder is always LIMIT.
#[macro_use]
mod page_sql;

macro_rules! all_first_page {
    ($query:ident $(, $arg:expr)*) => {
        due_renewals_statement!($query, "", "LIMIT $13\n" $(, $arg)*)
    };
}
macro_rules! all_continuation {
    ($query:ident $(, $arg:expr)*) => {
        due_renewals_statement!(
            $query,
            "        AND (subscriptions.next_payment_attempt_at, subscriptions.id) > ($13::timestamptz, $14::uuid)\n",
            "LIMIT $15\n" $(, $arg)*
        )
    };
}
macro_rules! mode_first_page {
    ($query:ident $(, $arg:expr)*) => {
        due_renewals_statement!(
            $query,
            "        AND subscriptions.required_gateway_account_mode = $13::text\n",
            "LIMIT $14\n" $(, $arg)*
        )
    };
}
macro_rules! mode_continuation {
    ($query:ident $(, $arg:expr)*) => {
        due_renewals_statement!(
            $query,
            "        AND subscriptions.required_gateway_account_mode = $13::text
                     AND (subscriptions.next_payment_attempt_at, subscriptions.id) > ($14::timestamptz, $15::uuid)\n",
            "LIMIT $16\n" $(, $arg)*
        )
    };
}

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
            None => checked_query_scalar!(DateTime<Utc>, "SELECT clock_timestamp()")
                .fetch_one(pool)
                .await
                .map(Self::First),
        }
    }

    #[cfg(test)]
    const fn sql(self, required_mode: Option<GatewayAccountMode>) -> &'static str {
        match (self, required_mode) {
            (Self::First(_), None) => all_first_page!(concat_sql),
            (Self::Continuation(_), None) => all_continuation!(concat_sql),
            (Self::First(_), Some(_)) => mode_first_page!(concat_sql),
            (Self::Continuation(_), Some(_)) => mode_continuation!(concat_sql),
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
        infrastructure_retry_codes: &'args [String],
        infrastructure_pacing_codes: &'args [String],
        rate_limit_pacing_codes: &'args [String],
        required_gateway_account_mode: Option<GatewayAccountMode>,
    ) -> Query<'args, Postgres, PgArguments> {
        let payment_method_update_policy =
            LocalAttemptPolicy::for_kind(PaymentAttemptKind::SubscriptionPaymentMethodUpdate);
        let subscription_charge_policy =
            LocalAttemptPolicy::for_kind(PaymentAttemptKind::SubscriptionRenewal);
        macro_rules! bind_page {
            ($statement:ident $(, $extra:expr)*) => {
                $statement!(
                    checked_query,
                    infrastructure_retry_codes,
                    infrastructure_pacing_codes,
                    rate_limit_pacing_codes,
                    payment_method_update_policy.stale_after_seconds(),
                    syrup_rail::MAX_RENEWAL_INFRASTRUCTURE_ATTEMPTS_PER_PERIOD_CONFIGURATION,
                    syrup_rail::RENEWAL_INFRASTRUCTURE_RETRY_AFTER_SECONDS,
                    syrup_rail::RENEWAL_RATE_LIMIT_FAST_RETRY_ATTEMPTS,
                    syrup_rail::RENEWAL_RATE_LIMIT_SLOW_RETRY_AFTER_SECONDS,
                    syrup_rail::GATEWAY_MUTATION_RATE_LIMIT_RETRY_AFTER_SECONDS,
                    self.observed_at(),
                    subscription_charge_policy.stale_after_seconds(),
                    LocalAttemptPolicy::expirable_status_values()
                    $(, $extra)*,
                    syrup_rail::RENEWAL_DISPATCH_LIMIT + 1
                )
            };
        }
        match (required_gateway_account_mode, self) {
            (None, Self::First(_)) => bind_page!(all_first_page),
            (None, Self::Continuation(cursor)) => bind_page!(
                all_continuation,
                cursor.next_payment_attempt_at(),
                cursor.subscription_id().into_uuid()
            ),
            (Some(mode), Self::First(_)) => bind_page!(mode_first_page, mode.as_str()),
            (Some(mode), Self::Continuation(cursor)) => bind_page!(
                mode_continuation,
                mode.as_str(),
                cursor.next_payment_attempt_at(),
                cursor.subscription_id().into_uuid()
            ),
        }
    }
}

#[derive(Debug, Error)]
pub enum RenewalStoreError {
    #[error("renewal storage operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("a gateway provider has no canonical cooldown row")]
    MissingProviderCooldown,
    #[error("renewal page cursor belongs to a different gateway account mode scan")]
    CursorModeMismatch,
}

/// Returns the first deterministic, provider-neutral renewal dispatch page.
///
/// This compatibility wrapper keeps the historical fixed one-hundred-item
/// first-page limit and ascending order. Hosts that need to drain one stable
/// observed scan should use [`due_renewals_page`] and retain its cursor.
pub async fn due_renewals(pool: &PgPool) -> Result<Vec<RenewalDispatch>, RenewalStoreError> {
    Ok(load_due_renewals_page(pool, None, None)
        .await?
        .into_dispatches())
}

/// Returns the first deterministic renewal page for one deployment mode.
///
/// Filtering happens in PostgreSQL before the fixed page limit.
pub async fn due_renewals_for_mode(
    pool: &PgPool,
    required_gateway_account_mode: GatewayAccountMode,
) -> Result<Vec<RenewalDispatch>, RenewalStoreError> {
    Ok(
        load_due_renewals_page(pool, None, Some(required_gateway_account_mode))
            .await?
            .into_dispatches(),
    )
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
pub async fn due_renewals_page(
    pool: &PgPool,
    cursor: Option<&RenewalDispatchPageCursor>,
) -> Result<RenewalDispatchPage, RenewalStoreError> {
    load_due_renewals_page(pool, cursor, None).await
}

/// Returns one deterministic page of renewal work for one deployment mode.
///
/// Use this from a mode-specific scheduler so other-mode subscriptions do not
/// consume the fixed page limit. The returned cursor records
/// `required_gateway_account_mode`; passing it to another mode or an all-mode
/// scan returns [`RenewalStoreError::CursorModeMismatch`]. A central router
/// serving both modes can use [`due_renewals_page`] and route each dispatch by
/// its durable mode.
pub async fn due_renewals_page_for_mode(
    pool: &PgPool,
    required_gateway_account_mode: GatewayAccountMode,
    cursor: Option<&RenewalDispatchPageCursor>,
) -> Result<RenewalDispatchPage, RenewalStoreError> {
    load_due_renewals_page(pool, cursor, Some(required_gateway_account_mode)).await
}

async fn load_due_renewals_page(
    pool: &PgPool,
    cursor: Option<&RenewalDispatchPageCursor>,
    required_gateway_account_mode: Option<GatewayAccountMode>,
) -> Result<RenewalDispatchPage, RenewalStoreError> {
    if cursor.is_some_and(|cursor| {
        cursor.required_gateway_account_mode() != required_gateway_account_mode
    }) {
        return Err(RenewalStoreError::CursorModeMismatch);
    }
    let page_query = DueRenewalPageQuery::load(pool, cursor).await?;
    let observed_at = page_query.observed_at();
    let missing_provider_cooldown = checked_query_scalar!(
        bool,
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM billing_gateway_accounts AS accounts
            LEFT JOIN billing_gateway_provider_rate_limits AS provider_limits
                ON provider_limits.provider_key = accounts.provider_key
            WHERE provider_limits.provider_key IS NULL
        )
        "#,
    )
    .fetch_one(pool)
    .await?;
    if missing_provider_cooldown {
        return Err(RenewalStoreError::MissingProviderCooldown);
    }

    let infrastructure_retry_codes =
        resolution_strings(PaymentResolutionCode::RENEWAL_INFRASTRUCTURE_RETRY_CODES);
    let infrastructure_pacing_codes =
        resolution_strings(PaymentResolutionCode::RENEWAL_INFRASTRUCTURE_PACING_CODES);
    let rate_limit_pacing_codes =
        resolution_strings(PaymentResolutionCode::RENEWAL_RATE_LIMIT_PACING_CODES);
    let rows = page_query
        .bind(
            &infrastructure_retry_codes,
            &infrastructure_pacing_codes,
            &rate_limit_pacing_codes,
            required_gateway_account_mode,
        )
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
                    row.try_get::<String, _>("required_gateway_account_mode")?
                        .parse::<GatewayAccountMode>()
                        .map_err(|_| sqlx::Error::ColumnDecode {
                            index: "required_gateway_account_mode".to_owned(),
                            source: Box::new(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                "invalid gateway account mode",
                            )),
                        })?,
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
            required_gateway_account_mode,
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
    let rate_limit_pacing_codes =
        resolution_strings(PaymentResolutionCode::RENEWAL_RATE_LIMIT_PACING_CODES);
    let row = checked_query!(
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
                    AND resolution_code = ANY($6::text[])
            ) AS rate_limited_attempt_count,
            MAX(resolved_at) FILTER (
                WHERE attempt_kind = 'subscription_renewal'
                    AND status = 'failed'
                    AND resolution_code = ANY($6::text[])
            ) AS last_rate_limited_at,
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
        subscription_id.as_uuid(),
        period_start_at,
        excluded_attempt_id.map(PaymentAttemptId::into_uuid),
        &infrastructure_retry_codes,
        &infrastructure_pacing_codes,
        &rate_limit_pacing_codes,
        LocalAttemptPolicy::expirable_status_values(),
        policy.stale_after_seconds(),
    )
    .fetch_one(&mut **transaction)
    .await?;
    Ok(RenewalAttemptState {
        attempt_sequence_count: row.try_get("attempt_sequence_count")?,
        automatic_infrastructure_attempt_count: row
            .try_get("automatic_infrastructure_attempt_count")?,
        last_automatic_infrastructure_failure_at: row
            .try_get("last_automatic_infrastructure_failure_at")?,
        rate_limited_attempt_count: row.try_get("rate_limited_attempt_count")?,
        last_rate_limited_at: row.try_get("last_rate_limited_at")?,
        has_blocking_attempt: row.try_get("has_blocking_attempt")?,
    })
}

fn resolution_strings(codes: &[PaymentResolutionCode]) -> Vec<String> {
    codes.iter().map(|code| code.as_str().to_owned()).collect()
}

#[cfg(test)]
mod tests;
