//! Opt-in billing-period expiry: database-clock checks, canonical retirement
//! of an obsolete due period, and past-due access policy changes.

use chrono::{DateTime, Utc};
use sqlx::{PgConnection, Postgres, Row, Transaction, postgres::PgRow};
use syrup_rail::{
    BillingEvent, ChangeSubscriptionPastDueAccess, PastDueAccessPolicy, PaymentAttemptId,
    PaymentResolutionCode, PlanKey, RetireExpiredSubscriptionPeriod, SubscriberId,
    SubscriptionPastDueAccessChangeOutcome, SubscriptionPeriodRetirementOutcome,
    SubscriptionStatus, next_billing_period,
};
use thiserror::Error;
use uuid::Uuid;

use crate::{
    attempts::fail_stale_unsubmitted_subscription_charges,
    renewal_failure::{RenewalFailureStoreError, past_due_causal_history},
    subscription_persistence::{
        SubscriptionPersistenceCodecError, subscription_from_row as decode_subscription_row,
    },
};

const BILLING_ROW_LOCK_TIMEOUT: &str = "250ms";
const INVALID_SUBSCRIPTION_STATE: &str = "canonical subscription state is invalid";
const RETIRED_PERIOD_UNSUBMITTED_TEXT: &str =
    "Subscription payment was canceled before submission because its billing period ended.";
const SUBSCRIPTION_COLUMNS: &str = r#"
    id, plan_key, status, payment_method_id, amount_cents, currency,
    current_period_start_at, current_period_end_at, next_renewal_at,
    phase, recurring_period_kind, recurring_period_count,
    dunning_retry_delays_seconds, dunning_exhaustion, past_due_access,
    next_payment_attempt_at, required_gateway_account_mode
"#;

/// Failure of a billing-period expiry or past-due access policy operation.
#[derive(Debug, Error)]
pub enum SubscriptionPeriodExpiryError {
    /// Generic SQL failure.
    #[error("subscription period expiry storage failed")]
    Sql(#[from] sqlx::Error),
    /// Canonical subscription or payment history is internally inconsistent.
    #[error("{0}")]
    InvalidState(&'static str),
}

impl From<SubscriptionPersistenceCodecError> for SubscriptionPeriodExpiryError {
    fn from(error: SubscriptionPersistenceCodecError) -> Self {
        match error {
            SubscriptionPersistenceCodecError::RowRead(error) => Self::Sql(error),
            SubscriptionPersistenceCodecError::InvalidState => {
                Self::InvalidState(INVALID_SUBSCRIPTION_STATE)
            }
        }
    }
}

impl From<RenewalFailureStoreError> for SubscriptionPeriodExpiryError {
    fn from(error: RenewalFailureStoreError) -> Self {
        match error {
            RenewalFailureStoreError::Sql(error) => Self::Sql(error),
            RenewalFailureStoreError::Attempt(_) | RenewalFailureStoreError::InvalidState(_) => {
                Self::InvalidState(INVALID_SUBSCRIPTION_STATE)
            }
        }
    }
}

/// Returns whether a billing period ending at `period_end_at` has expired at
/// the database clock. A period ending exactly now has expired.
pub(crate) async fn period_has_expired_at_database_time(
    connection: &mut PgConnection,
    period_end_at: DateTime<Utc>,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT $1::timestamptz <= clock_timestamp()")
        .bind(period_end_at)
        .fetch_one(connection)
        .await
}

/// Pool form of [`period_has_expired_at_database_time`] for paths that cannot
/// lock the attempt.
pub(crate) async fn period_has_expired_on_pool(
    pool: &sqlx::PgPool,
    period_end_at: DateTime<Utc>,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT $1::timestamptz <= clock_timestamp()")
        .bind(period_end_at)
        .fetch_one(pool)
        .await
}

/// Retires one exact obsolete due period inside the caller's transaction.
///
/// The caller must acquire its host recipient lock first and append a
/// returned event before committing. Under the subscriber/plan aggregate lock
/// and a row lock, the operation requires the exact scope, subscriber, plan,
/// and subscription; an `active` or `past_due` status whose `next_renewal_at`
/// still equals the requested period start; and a period that has ended at the
/// database clock. It refuses while any submitted, unknown, review-required,
/// or same-period approved renewal or recovery attempt remains, or while any
/// processor charge for the subscription is still pending, awaiting
/// reconciliation, or awaiting external reversal. Definitively declined,
/// failed, or fully reversed history does not block retirement.
///
/// In one transaction it then terminally rejects every still-unsubmitted
/// renewal or recovery attempt for that period with
/// [`PaymentResolutionCode::SubscriptionPeriodExpiredBeforeCharge`] and marks
/// the lifecycle `unpaid`, preserving every date, amount, identifier, trial
/// term, and payment record. It never synthesizes a decline, moves the period
/// anchor, or touches a canceled lifecycle. A replay against a lifecycle that
/// is already `unpaid` at the same period returns `AlreadyUnpaid`.
pub async fn retire_expired_subscription_period_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    command: &RetireExpiredSubscriptionPeriod,
) -> Result<SubscriptionPeriodRetirementOutcome, SubscriptionPeriodExpiryError> {
    retire_expired_subscription_period_on_connection(transaction, command).await
}

pub(crate) async fn retire_expired_subscription_period_on_connection(
    connection: &mut PgConnection,
    command: &RetireExpiredSubscriptionPeriod,
) -> Result<SubscriptionPeriodRetirementOutcome, SubscriptionPeriodExpiryError> {
    set_lock_timeout(connection).await?;
    lock_subscription_aggregate(connection, command.subscriber_id(), command.plan_key()).await?;
    let Some(row) = lock_exact_subscription(
        connection,
        command.billing_scope_id().as_uuid(),
        command.subscriber_id(),
        command.plan_key(),
        command.subscription_id().as_uuid(),
    )
    .await?
    else {
        return Ok(SubscriptionPeriodRetirementOutcome::NotFound);
    };
    let subscription = decode_subscription_row(&row)?;
    let period_start_at = *command.period_start_at();
    match subscription.status() {
        SubscriptionStatus::Canceled => {
            return Ok(SubscriptionPeriodRetirementOutcome::Canceled(subscription));
        }
        SubscriptionStatus::Unpaid if *subscription.next_renewal_at() == period_start_at => {
            return Ok(SubscriptionPeriodRetirementOutcome::AlreadyUnpaid(
                subscription,
            ));
        }
        SubscriptionStatus::Unpaid => {
            return Ok(SubscriptionPeriodRetirementOutcome::PeriodChanged(
                subscription,
            ));
        }
        SubscriptionStatus::Active | SubscriptionStatus::PastDue => {}
    }
    if *subscription.next_renewal_at() != period_start_at {
        return Ok(SubscriptionPeriodRetirementOutcome::PeriodChanged(
            subscription,
        ));
    }
    let period = next_billing_period(period_start_at, subscription.recurring_period())
        .map_err(|_| SubscriptionPeriodExpiryError::InvalidState(INVALID_SUBSCRIPTION_STATE))?;
    if !period_has_expired_at_database_time(connection, *period.end_at()).await? {
        return Ok(SubscriptionPeriodRetirementOutcome::NotExpired { period });
    }

    fail_stale_unsubmitted_subscription_charges(connection, subscription.id()).await?;
    if unresolved_payment_exists(
        connection,
        command.subscription_id().as_uuid(),
        period_start_at,
    )
    .await?
    {
        return Ok(SubscriptionPeriodRetirementOutcome::UnresolvedPayment);
    }

    // Product access is still open for an active lifecycle and for a past-due
    // lifecycle that continues during scheduled dunning; retirement ends it.
    // Otherwise access already ended at the causal failure boundary.
    let prior_status = subscription.status();
    let policy = subscription.renewal_failure().past_due_access();
    let access_open = match prior_status {
        SubscriptionStatus::Active => true,
        SubscriptionStatus::PastDue => {
            policy == PastDueAccessPolicy::ContinueUntilDunningExhausted
                && subscription.next_payment_attempt_at().is_some()
        }
        SubscriptionStatus::Canceled | SubscriptionStatus::Unpaid => {
            return Err(SubscriptionPeriodExpiryError::InvalidState(
                INVALID_SUBSCRIPTION_STATE,
            ));
        }
    };
    let causal_access_ended_at = if access_open {
        None
    } else {
        Some(
            past_due_causal_history(connection, subscription.id(), period_start_at)
                .await?
                .access_ended_at(policy)
                .ok_or(SubscriptionPeriodExpiryError::InvalidState(
                    INVALID_SUBSCRIPTION_STATE,
                ))?,
        )
    };

    let rejected_attempt_ids = reject_unsubmitted_period_attempts(
        connection,
        command.subscription_id().as_uuid(),
        period_start_at,
    )
    .await?;
    let query = format!(
        r#"
        UPDATE billing_subscriptions
        SET status = 'unpaid',
            unpaid_at = clock_timestamp(),
            next_payment_attempt_at = NULL,
            updated_at = clock_timestamp()
        WHERE id = $1 AND billing_scope_id = $2 AND subscriber_id = $3
            AND plan_key = $4 AND status = $5 AND next_renewal_at = $6
            AND unpaid_at IS NULL AND canceled_at IS NULL
        RETURNING {SUBSCRIPTION_COLUMNS}, unpaid_at
        "#
    );
    let row = sqlx::query(&query)
        .bind(command.subscription_id().as_uuid())
        .bind(command.billing_scope_id().as_uuid())
        .bind(command.subscriber_id().as_uuid())
        .bind(command.plan_key().as_str())
        .bind(prior_status.as_str())
        .bind(period_start_at)
        .fetch_optional(&mut *connection)
        .await?
        .ok_or(SubscriptionPeriodExpiryError::InvalidState(
            INVALID_SUBSCRIPTION_STATE,
        ))?;
    let subscription = decode_subscription_row(&row)?;
    let ended_at: DateTime<Utc> = row.try_get("unpaid_at")?;
    let event = BillingEvent::SubscriptionPeriodExpired {
        subscription_id: subscription.id(),
        plan_key: subscription.plan_key().clone(),
        period,
        ended_at,
        access_ends_at: causal_access_ended_at.unwrap_or(ended_at),
    };
    Ok(SubscriptionPeriodRetirementOutcome::Retired {
        subscription,
        event,
        rejected_attempt_ids,
    })
}

/// Changes one existing subscription's persisted past-due access policy
/// inside the caller's transaction.
///
/// The operation takes the subscriber/plan aggregate lock, then the exact row
/// lock. Only `active` and `past_due` lifecycles change; canceled and unpaid
/// lifecycles keep their historical terms and return `Terminal`. Entitlement
/// and every later failure, cancellation, or terminal event derive product
/// access from the persisted policy and the cycle's durable failure history,
/// so a past-due row moved to
/// [`PastDueAccessPolicy::SuspendImmediately`] loses access at once and its
/// canonical access boundary is the cycle's first qualifying failure. No
/// dunning schedule, retry time, date, amount, or payment record changes. A
/// repeated change to the persisted policy returns `Unchanged`.
pub async fn change_subscription_past_due_access_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    command: &ChangeSubscriptionPastDueAccess,
) -> Result<SubscriptionPastDueAccessChangeOutcome, SubscriptionPeriodExpiryError> {
    let connection: &mut PgConnection = transaction;
    set_lock_timeout(connection).await?;
    lock_subscription_aggregate(connection, command.subscriber_id(), command.plan_key()).await?;
    let Some(row) = lock_exact_subscription(
        connection,
        command.billing_scope_id().as_uuid(),
        command.subscriber_id(),
        command.plan_key(),
        command.subscription_id().as_uuid(),
    )
    .await?
    else {
        return Ok(SubscriptionPastDueAccessChangeOutcome::NotFound);
    };
    let subscription = decode_subscription_row(&row)?;
    if matches!(
        subscription.status(),
        SubscriptionStatus::Canceled | SubscriptionStatus::Unpaid
    ) {
        return Ok(SubscriptionPastDueAccessChangeOutcome::Terminal(
            subscription,
        ));
    }
    let previous = subscription.renewal_failure().past_due_access();
    if previous == command.past_due_access() {
        return Ok(SubscriptionPastDueAccessChangeOutcome::Unchanged(
            subscription,
        ));
    }
    let query = format!(
        r#"
        UPDATE billing_subscriptions
        SET past_due_access = $5, updated_at = clock_timestamp()
        WHERE id = $1 AND billing_scope_id = $2 AND subscriber_id = $3
            AND plan_key = $4 AND past_due_access = $6
            AND status IN ('active', 'past_due')
        RETURNING {SUBSCRIPTION_COLUMNS}
        "#
    );
    let row = sqlx::query(&query)
        .bind(command.subscription_id().as_uuid())
        .bind(command.billing_scope_id().as_uuid())
        .bind(command.subscriber_id().as_uuid())
        .bind(command.plan_key().as_str())
        .bind(command.past_due_access().as_str())
        .bind(previous.as_str())
        .fetch_optional(&mut *connection)
        .await?
        .ok_or(SubscriptionPeriodExpiryError::InvalidState(
            INVALID_SUBSCRIPTION_STATE,
        ))?;
    Ok(SubscriptionPastDueAccessChangeOutcome::Changed {
        previous,
        subscription: decode_subscription_row(&row)?,
    })
}

async fn set_lock_timeout(connection: &mut PgConnection) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT set_config('lock_timeout', $1, true)")
        .bind(BILLING_ROW_LOCK_TIMEOUT)
        .execute(connection)
        .await?;
    Ok(())
}

async fn lock_subscription_aggregate(
    connection: &mut PgConnection,
    subscriber_id: SubscriberId,
    plan_key: &PlanKey,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::uuid::text || ':' || $2, 0))")
        .bind(subscriber_id.as_uuid())
        .bind(plan_key.as_str())
        .execute(connection)
        .await?;
    Ok(())
}

async fn lock_exact_subscription(
    connection: &mut PgConnection,
    billing_scope_id: &Uuid,
    subscriber_id: SubscriberId,
    plan_key: &PlanKey,
    subscription_id: &Uuid,
) -> Result<Option<PgRow>, sqlx::Error> {
    let query = format!(
        r#"
        SELECT {SUBSCRIPTION_COLUMNS}
        FROM billing_subscriptions
        WHERE id = $1 AND billing_scope_id = $2 AND subscriber_id = $3 AND plan_key = $4
        FOR NO KEY UPDATE
        "#
    );
    sqlx::query(&query)
        .bind(subscription_id)
        .bind(billing_scope_id)
        .bind(subscriber_id.as_uuid())
        .bind(plan_key.as_str())
        .fetch_optional(connection)
        .await
}

async fn unresolved_payment_exists(
    connection: &mut PgConnection,
    subscription_id: &Uuid,
    period_start_at: DateTime<Utc>,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM billing_payment_attempts
            WHERE subscription_id = $1
                AND attempt_kind IN ('subscription_renewal', 'subscription_recovery')
                AND (
                    status IN ('unknown', 'review_required')
                    OR (status = 'pending' AND submitted_at IS NOT NULL)
                    OR (status = 'approved' AND billing_period_start_at = $2)
                )
        )
        OR EXISTS (
            SELECT 1
            FROM billing_processor_charges AS charges
            INNER JOIN billing_payment_attempts AS attempts
                ON attempts.id = charges.attempt_id
            WHERE attempts.subscription_id = $1
                AND charges.attempt_kind IN (
                    'subscription_initial',
                    'subscription_renewal',
                    'subscription_recovery'
                )
                AND charges.progression_state IN (
                    'pending',
                    'reconciliation_required',
                    'external_reversal_required'
                )
        )
        "#,
    )
    .bind(subscription_id)
    .bind(period_start_at)
    .fetch_one(connection)
    .await
}

async fn reject_unsubmitted_period_attempts(
    connection: &mut PgConnection,
    subscription_id: &Uuid,
    period_start_at: DateTime<Utc>,
) -> Result<Vec<PaymentAttemptId>, sqlx::Error> {
    let rows = sqlx::query_scalar::<_, Uuid>(
        r#"
        UPDATE billing_payment_attempts
        SET status = 'failed', resolution_code = $3,
            gateway_response_text = $4,
            resolved_at = COALESCE(resolved_at, clock_timestamp()),
            updated_at = clock_timestamp()
        WHERE subscription_id = $1
            AND attempt_kind IN ('subscription_renewal', 'subscription_recovery')
            AND billing_period_start_at = $2
            AND status = 'pending' AND submitted_at IS NULL
        RETURNING id
        "#,
    )
    .bind(subscription_id)
    .bind(period_start_at)
    .bind(PaymentResolutionCode::SubscriptionPeriodExpiredBeforeCharge.as_str())
    .bind(RETIRED_PERIOD_UNSUBMITTED_TEXT)
    .fetch_all(connection)
    .await?;
    let mut ids = rows
        .into_iter()
        .map(PaymentAttemptId::new)
        .collect::<Vec<_>>();
    ids.sort_by_key(|id| *id.as_uuid());
    Ok(ids)
}
