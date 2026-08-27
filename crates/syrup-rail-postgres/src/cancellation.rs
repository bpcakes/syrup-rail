use chrono::{DateTime, Utc};
use sqlx::{PgConnection, Postgres, Row, Transaction};
use syrup_rail::{
    BillingEvent, CancelSubscription, CancelSubscriptionOutcome, PastDueAccessPolicy,
    PaymentAttemptKind, PlanKey, Subscription, SubscriptionId, SubscriptionStatus,
};
use thiserror::Error;
use uuid::Uuid;

use crate::{
    attempts::{
        LocalAttemptPolicy, blocking_payment_method_update_exists,
        fail_stale_unsubmitted_subscription_charges,
    },
    renewal_failure::{RenewalFailureStoreError, past_due_causal_history},
    subscription_persistence::{
        SubscriptionPersistenceCodecError, subscription_from_row as decode_subscription_row,
    },
};

const BILLING_ROW_LOCK_TIMEOUT: &str = "250ms";
const CURRENT_SUBSCRIPTION_LOCK_MAX_ATTEMPTS: usize = 2;
const UNSUBMITTED_PAYMENT_METHOD_UPDATE_FAILED_RESPONSE_TEXT: &str =
    "Payment method update was abandoned before gateway submission.";
const INVALID_SUBSCRIPTION_STATE: &str = "canonical subscription state is invalid";
const UNSTABLE_CURRENT_SUBSCRIPTION: &str =
    "current subscription ranking did not stabilize while acquiring the row lock";

#[derive(Debug, Error)]
pub enum SubscriptionCancellationError {
    #[error("subscription cancellation failed")]
    Sql(#[from] sqlx::Error),
    #[error("{0}")]
    InvalidState(&'static str),
}

impl From<RenewalFailureStoreError> for SubscriptionCancellationError {
    fn from(error: RenewalFailureStoreError) -> Self {
        match error {
            RenewalFailureStoreError::Sql(error) => Self::Sql(error),
            RenewalFailureStoreError::Attempt(_) | RenewalFailureStoreError::InvalidState(_) => {
                Self::InvalidState(INVALID_SUBSCRIPTION_STATE)
            }
        }
    }
}

fn map_subscription_persistence_error(
    error: SubscriptionPersistenceCodecError,
) -> SubscriptionCancellationError {
    match error {
        SubscriptionPersistenceCodecError::RowRead(error) => {
            SubscriptionCancellationError::Sql(error)
        }
        SubscriptionPersistenceCodecError::InvalidState => {
            SubscriptionCancellationError::InvalidState(INVALID_SUBSCRIPTION_STATE)
        }
    }
}

/// Cancels one exact scope/subscriber/plan subscription inside the caller's transaction.
///
/// The caller must acquire any host recipient lock before invoking this operation and append the
/// returned event before committing. Cancellation never changes the stored payment method.
/// Active and past-due subscriptions return `Canceled` after their in-flight fences pass. The
/// newest exact canceled lifecycle returns `AlreadyCanceled`, including after paid-through access
/// expires. A newest terminal `Unpaid` lifecycle and an owner/plan with no subscription both return
/// `NotFound`; `NotFound` therefore means that no cancelable lifecycle exists, not necessarily that
/// no financial history exists. A past-due row must have either qualifying automatic-renewal
/// failure history or the operator-reviewed, manually failed active-snapshot recovery that version
/// 1 could use to enter `past_due`; cancellation never fabricates a financial timestamp to repair
/// corrupt causal history.
pub async fn cancel_subscription_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    command: &CancelSubscription,
) -> Result<CancelSubscriptionOutcome, SubscriptionCancellationError> {
    cancel_subscription_on_connection(transaction, command).await
}

/// Executes cancellation on a connection that is already inside the caller's
/// transaction.
///
/// This is crate-visible for the host-prepared billing transaction facade.
/// The caller owns transaction completion and must append a returned event
/// before committing.
pub(crate) async fn cancel_subscription_on_connection(
    connection: &mut PgConnection,
    command: &CancelSubscription,
) -> Result<CancelSubscriptionOutcome, SubscriptionCancellationError> {
    set_lock_timeout(connection).await?;
    lock_subscription_aggregate(
        connection,
        command.subscriber_id().as_uuid(),
        command.plan_key(),
    )
    .await?;

    let Some(subscription) = current_subscription(connection, command).await? else {
        return Ok(CancelSubscriptionOutcome::NotFound);
    };
    match subscription.status() {
        SubscriptionStatus::Canceled => {
            Ok(CancelSubscriptionOutcome::AlreadyCanceled(subscription))
        }
        SubscriptionStatus::Active | SubscriptionStatus::PastDue => {
            fail_stale_unsubmitted_subscription_charges(connection, subscription.id()).await?;
            if has_blocking_renewal(connection, &subscription).await? {
                return Ok(CancelSubscriptionOutcome::BlockedByRenewal);
            }
            expire_stale_payment_method_updates(connection, subscription.id()).await?;
            if blocking_payment_method_update_exists(connection, subscription.id()).await? {
                return Ok(CancelSubscriptionOutcome::BlockedByPaymentMethodUpdate);
            }

            let prior_status = subscription.status();
            let access_ends_at_before_cancel = match prior_status {
                SubscriptionStatus::Active => Some(*subscription.current_period().end_at()),
                SubscriptionStatus::PastDue
                    if subscription.renewal_failure().past_due_access()
                        == PastDueAccessPolicy::ContinueUntilDunningExhausted
                        && subscription.next_payment_attempt_at().is_some() =>
                {
                    None
                }
                SubscriptionStatus::PastDue => {
                    let history = past_due_causal_history(
                        connection,
                        subscription.id(),
                        *subscription.next_renewal_at(),
                    )
                    .await?;
                    Some(
                        history
                            .access_ended_at(subscription.renewal_failure().past_due_access())
                            .ok_or(SubscriptionCancellationError::InvalidState(
                                INVALID_SUBSCRIPTION_STATE,
                            ))?,
                    )
                }
                SubscriptionStatus::Canceled | SubscriptionStatus::Unpaid => {
                    return Err(SubscriptionCancellationError::InvalidState(
                        INVALID_SUBSCRIPTION_STATE,
                    ));
                }
            };
            let (subscription, canceled_at) =
                cancel_current_subscription(connection, command, subscription.id(), prior_status)
                    .await?
                    .ok_or(SubscriptionCancellationError::InvalidState(
                        INVALID_SUBSCRIPTION_STATE,
                    ))?;
            let access_ends_at = access_ends_at_before_cancel.unwrap_or(canceled_at);
            let event = BillingEvent::SubscriptionCanceled {
                subscription_id: subscription.id(),
                plan_key: subscription.plan_key().clone(),
                access_ends_at,
            };
            Ok(CancelSubscriptionOutcome::Canceled {
                subscription,
                event,
            })
        }
        SubscriptionStatus::Unpaid => Ok(CancelSubscriptionOutcome::NotFound),
    }
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
    subscriber_id: &Uuid,
    plan_key: &PlanKey,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::uuid::text || ':' || $2, 0))")
        .bind(subscriber_id)
        .bind(plan_key.as_str())
        .execute(connection)
        .await?;
    Ok(())
}

async fn current_subscription(
    connection: &mut PgConnection,
    command: &CancelSubscription,
) -> Result<Option<Subscription>, SubscriptionCancellationError> {
    for attempt in 0..CURRENT_SUBSCRIPTION_LOCK_MAX_ATTEMPTS {
        let Some(candidate_id) = selected_subscription_id(connection, command).await? else {
            return Ok(None);
        };
        let row = sqlx::query(
            r#"
            SELECT id, plan_key, status, payment_method_id, amount_cents, currency,
                current_period_start_at, current_period_end_at, next_renewal_at,
                phase, recurring_period_kind, recurring_period_count,
                dunning_retry_delays_seconds, dunning_exhaustion, past_due_access,
                next_payment_attempt_at, required_gateway_account_mode
            FROM billing_subscriptions
            WHERE id = $1
                AND billing_scope_id = $2
                AND subscriber_id = $3
                AND plan_key = $4
            FOR NO KEY UPDATE
            "#,
        )
        .bind(candidate_id)
        .bind(command.billing_scope_id().as_uuid())
        .bind(command.subscriber_id().as_uuid())
        .bind(command.plan_key().as_str())
        .fetch_optional(&mut *connection)
        .await?;
        let Some(row) = row else {
            require_stabilization_retry(attempt)?;
            continue;
        };
        match selected_subscription_id(connection, command).await? {
            Some(current_id) if current_id == candidate_id => {
                return decode_subscription_row(&row)
                    .map_err(map_subscription_persistence_error)
                    .map(Some);
            }
            Some(_) => require_stabilization_retry(attempt)?,
            None => return Ok(None),
        }
    }
    Err(SubscriptionCancellationError::InvalidState(
        UNSTABLE_CURRENT_SUBSCRIPTION,
    ))
}

fn require_stabilization_retry(attempt: usize) -> Result<(), SubscriptionCancellationError> {
    if attempt + 1 < CURRENT_SUBSCRIPTION_LOCK_MAX_ATTEMPTS {
        Ok(())
    } else {
        Err(SubscriptionCancellationError::InvalidState(
            UNSTABLE_CURRENT_SUBSCRIPTION,
        ))
    }
}

async fn current_subscription_id(
    connection: &mut PgConnection,
    command: &CancelSubscription,
) -> Result<Option<Uuid>, sqlx::Error> {
    sqlx::query_scalar(
        r#"
        SELECT id
        FROM billing_current_subscriptions
        WHERE billing_scope_id = $1
            AND subscriber_id = $2
            AND plan_key = $3
        ORDER BY current_subscription_rank, updated_at DESC, id DESC
        LIMIT 1
        "#,
    )
    .bind(command.billing_scope_id().as_uuid())
    .bind(command.subscriber_id().as_uuid())
    .bind(command.plan_key().as_str())
    .fetch_optional(&mut *connection)
    .await
}

async fn selected_subscription_id(
    connection: &mut PgConnection,
    command: &CancelSubscription,
) -> Result<Option<Uuid>, sqlx::Error> {
    if let Some(current) = current_subscription_id(connection, command).await? {
        return Ok(Some(current));
    }
    sqlx::query_scalar(
        r#"
        SELECT id
        FROM billing_subscriptions
        WHERE billing_scope_id = $1
            AND subscriber_id = $2
            AND plan_key = $3
        ORDER BY created_at DESC, id DESC
        LIMIT 1
        "#,
    )
    .bind(command.billing_scope_id().as_uuid())
    .bind(command.subscriber_id().as_uuid())
    .bind(command.plan_key().as_str())
    .fetch_optional(&mut *connection)
    .await
}

async fn has_blocking_renewal(
    connection: &mut PgConnection,
    subscription: &Subscription,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM billing_payment_attempts
            WHERE subscription_id = $1
                AND attempt_kind IN ('subscription_renewal', 'subscription_recovery')
                AND billing_period_start_at = $2
                AND status IN ('pending', 'unknown', 'review_required', 'approved')
        )
        "#,
    )
    .bind(subscription.id().as_uuid())
    .bind(subscription.next_renewal_at())
    .fetch_one(&mut *connection)
    .await
}

async fn expire_stale_payment_method_updates(
    connection: &mut PgConnection,
    subscription_id: SubscriptionId,
) -> Result<(), sqlx::Error> {
    let policy = LocalAttemptPolicy::for_kind(PaymentAttemptKind::SubscriptionPaymentMethodUpdate);
    sqlx::query(
        r#"
        WITH stale_attempts AS (
            SELECT id
            FROM billing_payment_attempts
            WHERE subscription_id = $1
                AND attempt_kind = 'subscription_payment_method_update'
                AND status = ANY($2::text[])
                AND submitted_at IS NULL
                AND created_at <= now() - ($3::bigint * interval '1 second')
            FOR UPDATE SKIP LOCKED
        )
        UPDATE billing_payment_attempts attempts
        SET status = 'failed',
            gateway_response_text = COALESCE(gateway_response_text, $4),
            gateway_condition = COALESCE(gateway_condition, 'failed'),
            resolved_at = now(),
            updated_at = now()
        FROM stale_attempts
        WHERE attempts.id = stale_attempts.id
        "#,
    )
    .bind(subscription_id.as_uuid())
    .bind(LocalAttemptPolicy::expirable_status_values())
    .bind(policy.stale_after_seconds())
    .bind(UNSUBMITTED_PAYMENT_METHOD_UPDATE_FAILED_RESPONSE_TEXT)
    .execute(connection)
    .await?;
    Ok(())
}

async fn cancel_current_subscription(
    connection: &mut PgConnection,
    command: &CancelSubscription,
    subscription_id: SubscriptionId,
    expected_status: SubscriptionStatus,
) -> Result<Option<(Subscription, DateTime<Utc>)>, SubscriptionCancellationError> {
    let row = sqlx::query(
        r#"
        UPDATE billing_subscriptions
        SET status = 'canceled',
            canceled_at = now(),
            next_payment_attempt_at = NULL,
            updated_at = now()
        WHERE id = $1
            AND billing_scope_id = $2
            AND subscriber_id = $3
            AND plan_key = $4
            AND status = $5
        RETURNING id, plan_key, status, payment_method_id, amount_cents, currency,
            current_period_start_at, current_period_end_at, next_renewal_at,
            phase, recurring_period_kind, recurring_period_count,
            dunning_retry_delays_seconds, dunning_exhaustion, past_due_access,
            next_payment_attempt_at, required_gateway_account_mode, canceled_at
        "#,
    )
    .bind(subscription_id.as_uuid())
    .bind(command.billing_scope_id().as_uuid())
    .bind(command.subscriber_id().as_uuid())
    .bind(command.plan_key().as_str())
    .bind(expected_status.as_str())
    .fetch_optional(&mut *connection)
    .await?;
    row.as_ref()
        .map(|row| {
            Ok((
                decode_subscription_row(row).map_err(map_subscription_persistence_error)?,
                row.try_get::<DateTime<Utc>, _>("canceled_at")?,
            ))
        })
        .transpose()
}

#[cfg(test)]
mod tests;
