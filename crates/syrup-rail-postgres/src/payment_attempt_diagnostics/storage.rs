use std::{io, time::Duration};

use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgPool, Postgres, Transaction};
use syrup_rail::{BillingScopeId, PlanKey, SubscriberId};
use tokio::time::Instant;
use uuid::Uuid;

use super::{
    DiagnosticCandidate, PaymentAttemptDiagnosticEligibility, PaymentAttemptDiagnosticTarget,
    PaymentAttemptDiagnosticsError, PaymentAttemptDiagnosticsOutcome,
};
use crate::{GatewayMutationCooldownScope, enrollment_application::set_application_timeouts};

// Ownership is scope and subscriber plus the exact plan. Host charges have no
// plan; they are reported as an unsupported kind rather than hidden.
const ELIGIBILITY_SQL: &str = r#"
    SELECT
        attempts.id AS attempt_id,
        attempts.attempt_kind,
        attempts.submitted_at IS NOT NULL AS submitted,
        attempts.status,
        public.billing_canonical_gateway_transaction_id(attempts.gateway_transaction_id)
            IS NOT NULL AS has_transaction_id
    FROM unnest($4::uuid[]) WITH ORDINALITY AS requested(attempt_id, position)
    INNER JOIN billing_payment_attempts AS attempts
        ON attempts.id = requested.attempt_id
    WHERE attempts.billing_scope_id = $1
        AND attempts.subscriber_id = $2
        AND (attempts.attempt_kind = 'host_charge' OR attempts.plan_key = $3)
    ORDER BY requested.position
"#;

const TARGET_SQL: &str = r#"
    SELECT
        attempts.attempt_kind,
        attempts.status,
        attempts.submitted_at IS NOT NULL AS submitted,
        attempts.gateway_transaction_id,
        public.billing_canonical_gateway_transaction_id(attempts.gateway_transaction_id)
            IS NOT NULL AS has_transaction_id,
        attempts.amount_cents,
        attempts.currency,
        attempts.gateway_order_id,
        attempts.gateway_account_id,
        accounts.gateway_configuration_id,
        accounts.provider_key
    FROM billing_payment_attempts AS attempts
    INNER JOIN billing_gateway_accounts AS accounts
        ON accounts.id = attempts.gateway_account_id
        AND accounts.billing_scope_id = attempts.billing_scope_id
    WHERE attempts.billing_scope_id = $1
        AND attempts.subscriber_id = $2
        AND attempts.id = $4
        AND (attempts.attempt_kind = 'host_charge' OR attempts.plan_key = $3)
"#;

const REVALIDATE_SQL: &str = r#"
    SELECT
        attempts.status,
        attempts.gateway_transaction_id,
        attempts.gateway_account_id,
        accounts.gateway_configuration_id,
        accounts.provider_key,
        clock_timestamp() AS observed_at
    FROM billing_payment_attempts AS attempts
    INNER JOIN billing_gateway_accounts AS accounts
        ON accounts.id = attempts.gateway_account_id
        AND accounts.billing_scope_id = attempts.billing_scope_id
    WHERE attempts.billing_scope_id = $1
        AND attempts.subscriber_id = $2
        AND attempts.id = $4
        AND attempts.plan_key = $3
"#;

#[derive(FromRow)]
pub(super) struct EligibilityRow {
    pub attempt_id: Uuid,
    pub attempt_kind: String,
    pub submitted: bool,
    pub status: String,
    pub has_transaction_id: bool,
}

#[derive(FromRow)]
pub(super) struct TargetRow {
    pub attempt_kind: String,
    pub status: String,
    pub submitted: bool,
    pub gateway_transaction_id: Option<String>,
    pub has_transaction_id: bool,
    pub amount_cents: i32,
    pub currency: String,
    pub gateway_order_id: String,
    pub gateway_account_id: Uuid,
    pub gateway_configuration_id: Uuid,
    pub provider_key: String,
}

#[derive(FromRow)]
struct RevalidationRow {
    status: String,
    gateway_transaction_id: Option<String>,
    gateway_account_id: Uuid,
    gateway_configuration_id: Uuid,
    provider_key: String,
    observed_at: DateTime<Utc>,
}

pub(super) enum Prepared {
    Done(PaymentAttemptDiagnosticsOutcome),
    Query(DiagnosticCandidate),
}

pub(super) enum Revalidation {
    Unchanged {
        observed_at: DateTime<Utc>,
    },
    ConfigurationChanged,
    TargetChanged,
    /// The caller's deadline left no time to finish revalidation.
    DeadlineExceeded,
}

/// Server statement timeouts end this long before the caller's deadline, so
/// an overrunning statement normally fails cleanly before the client-side
/// bound abandons a stalled connection at the deadline itself.
const SERVER_TIMEOUT_MARGIN: Duration = Duration::from_millis(100);

/// Begins a post-query transaction whose server-side work ends before
/// `deadline`: the connection wait is bounded by the remaining time, and each
/// of the `statements` still to run, including `COMMIT`, receives an equal
/// share of it as its statement timeout. Callers also bound the client-side
/// wait with the same deadline for a connection that stops responding.
pub(super) async fn begin_before(
    pool: &PgPool,
    deadline: Instant,
    statements: u32,
) -> Result<Transaction<'static, Postgres>, sqlx::Error> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(sqlx::Error::PoolTimedOut);
    }
    let mut transaction = tokio::time::timeout(remaining, pool.begin())
        .await
        .map_err(|_| sqlx::Error::PoolTimedOut)??;
    let remaining = deadline
        .saturating_duration_since(Instant::now())
        .saturating_sub(SERVER_TIMEOUT_MARGIN);
    let statement_ms = (remaining.as_millis() / u128::from(statements.max(1))).clamp(1, 5_000);
    sqlx::query(
        "SELECT set_config('statement_timeout', $1, true), set_config('lock_timeout', $2, true)",
    )
    .bind(format!("{statement_ms}ms"))
    .bind(format!("{}ms", statement_ms.min(250)))
    .execute(&mut *transaction)
    .await?;
    Ok(transaction)
}

/// Whether an error came from the deadline bound rather than from storage.
fn exceeded_deadline(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::PoolTimedOut)
        || error
            .as_database_error()
            .and_then(|error| error.code())
            .is_some_and(|code| matches!(code.as_ref(), "57014" | "55P03"))
}

pub(super) async fn load_eligibility(
    pool: &PgPool,
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    plan_key: &PlanKey,
    attempt_ids: &[Uuid],
) -> Result<Vec<EligibilityRow>, sqlx::Error> {
    let mut transaction = pool.begin().await?;
    set_application_timeouts(&mut transaction).await?;
    let rows = sqlx::query_as(ELIGIBILITY_SQL)
        .bind(billing_scope_id.as_uuid())
        .bind(subscriber_id.as_uuid())
        .bind(plan_key.as_str())
        .bind(attempt_ids)
        .fetch_all(&mut *transaction)
        .await?;
    transaction.commit().await?;
    Ok(rows)
}

/// Records the shared provider cooldown after a rate-limited query within the
/// caller's deadline.
pub(super) async fn record_provider_cooldown_before(
    pool: &PgPool,
    deadline: Instant,
    target: &PaymentAttemptDiagnosticTarget,
    candidate: &DiagnosticCandidate,
) -> Result<(), sqlx::Error> {
    // Lock the account row, update the provider row, and commit. Only a
    // connection that stops responding is abandoned at the deadline; the
    // caller then reports a cooldown persistence failure so the host still
    // backs off.
    tokio::time::timeout_at(deadline, async {
        let transaction = begin_before(pool, deadline, 3).await?;
        crate::payment_method_metadata::persist_provider_cooldown(
            transaction,
            target.billing_scope_id,
            candidate.account_id,
            &candidate.provider_key,
        )
        .await
    })
    .await
    .unwrap_or_else(|_| {
        Err(sqlx::Error::Io(io::Error::new(
            io::ErrorKind::TimedOut,
            "diagnostic cooldown write did not finish before the deadline",
        )))
    })
}

/// Reads the target and its cooldowns in one short transaction that ends
/// before gateway resolution or provider I/O.
pub(super) async fn prepare(
    pool: &PgPool,
    target: &PaymentAttemptDiagnosticTarget,
) -> Result<Prepared, PaymentAttemptDiagnosticsError> {
    use PaymentAttemptDiagnosticsOutcome as Outcome;

    let mut transaction = pool.begin().await?;
    set_application_timeouts(&mut transaction).await?;
    let row: Option<TargetRow> = sqlx::query_as(TARGET_SQL)
        .bind(target.billing_scope_id.as_uuid())
        .bind(target.subscriber_id.as_uuid())
        .bind(target.plan_key.as_str())
        .bind(target.attempt_id.as_uuid())
        .fetch_optional(&mut *transaction)
        .await?;
    let Some(row) = row else {
        transaction.commit().await?;
        return Ok(Prepared::Done(Outcome::NotFound));
    };
    if let PaymentAttemptDiagnosticEligibility::Ineligible(reason) =
        PaymentAttemptDiagnosticEligibility::classify(
            &row.attempt_kind,
            row.submitted,
            &row.status,
            row.has_transaction_id,
        )
    {
        transaction.commit().await?;
        return Ok(Prepared::Done(Outcome::Ineligible(reason)));
    }
    let candidate = DiagnosticCandidate::new(target.attempt_id, row)?;
    let (account, provider) = crate::gateway_accounts::load_gateway_cooldown(
        &mut *transaction,
        candidate.account_id,
        &candidate.provider_key,
    )
    .await?
    .ok_or(sqlx::Error::RowNotFound)?;
    transaction.commit().await?;
    if let Some(scope) = GatewayMutationCooldownScope::from_active_flags(account, provider) {
        return Ok(Prepared::Done(Outcome::CooldownActive(scope)));
    }
    Ok(Prepared::Query(candidate))
}

/// Confirms after provider I/O, within the caller's deadline, that the queried
/// identity still describes the target, and reads the observation time from
/// the database clock.
pub(super) async fn revalidate(
    pool: &PgPool,
    target: &PaymentAttemptDiagnosticTarget,
    candidate: &DiagnosticCandidate,
    deadline: Instant,
) -> Result<Revalidation, sqlx::Error> {
    let row = tokio::time::timeout_at(deadline, async {
        let mut transaction = begin_before(pool, deadline, 2).await?;
        let row: Option<RevalidationRow> = sqlx::query_as(REVALIDATE_SQL)
            .bind(target.billing_scope_id.as_uuid())
            .bind(target.subscriber_id.as_uuid())
            .bind(target.plan_key.as_str())
            .bind(target.attempt_id.as_uuid())
            .fetch_optional(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok::<_, sqlx::Error>(row)
    })
    .await;
    let row = match row {
        Ok(Ok(row)) => row,
        Err(_) => return Ok(Revalidation::DeadlineExceeded),
        Ok(Err(error)) if exceeded_deadline(&error) => {
            return Ok(Revalidation::DeadlineExceeded);
        }
        Ok(Err(error)) => return Err(error),
    };
    let Some(row) = row else {
        return Ok(Revalidation::TargetChanged);
    };
    if row.gateway_account_id != candidate.account_id.into_uuid()
        || row.gateway_configuration_id != candidate.configuration_id.into_uuid()
        || row.provider_key != candidate.provider_key.as_str()
    {
        return Ok(Revalidation::ConfigurationChanged);
    }
    if row.status != candidate.status
        || row.gateway_transaction_id.as_deref() != Some(candidate.stored_transaction_id.as_str())
    {
        return Ok(Revalidation::TargetChanged);
    }
    Ok(Revalidation::Unchanged {
        observed_at: row.observed_at,
    })
}
