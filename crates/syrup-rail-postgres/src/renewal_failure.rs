use chrono::{DateTime, Utc};
use sqlx::{PgConnection, Row, postgres::PgRow};
use syrup_rail::{
    BillingEvent, PastDueAccessPolicy, PaymentAttempt, PaymentAttemptId, PaymentAttemptIdentity,
    PaymentAttemptKind, PaymentAttemptStatus, PaymentAttemptTarget, PlanKey,
    RenewalFailureDisposition, RenewalFailurePolicy, SubscriptionEndReason, SubscriptionId,
    SubscriptionPaymentFailureDisposition, SubscriptionStatus, renewal_failure_disposition,
};
use thiserror::Error;
use uuid::Uuid;

use crate::{
    PaymentAttemptStoreError,
    attempts::find_payment_attempt_by_id_on_connection,
    subscription_persistence::{
        RenewalFailurePolicyScalars, SubscriptionPersistenceCodecError,
        renewal_failure_policy_from_scalars,
    },
};

const INVALID_RENEWAL_FAILURE_STATE: &str =
    "automatic renewal failure state cannot be projected deterministically";

#[derive(Debug, Error)]
pub(crate) enum RenewalFailureStoreError {
    #[error("automatic renewal failure storage operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("automatic renewal failure attempt could not be reconstructed")]
    Attempt(#[from] PaymentAttemptStoreError),
    #[error("{0}")]
    InvalidState(&'static str),
}

fn map_subscription_persistence_error(
    error: SubscriptionPersistenceCodecError,
) -> RenewalFailureStoreError {
    match error {
        SubscriptionPersistenceCodecError::RowRead(error) => RenewalFailureStoreError::Sql(error),
        SubscriptionPersistenceCodecError::InvalidState => {
            RenewalFailureStoreError::InvalidState(INVALID_RENEWAL_FAILURE_STATE)
        }
    }
}

const fn invalid_state() -> RenewalFailureStoreError {
    RenewalFailureStoreError::InvalidState(INVALID_RENEWAL_FAILURE_STATE)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ResolvedAutomaticRenewalFailure {
    attempt_id: PaymentAttemptId,
    resolved_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AutomaticRenewalFailureHistory {
    Empty,
    First {
        failure: ResolvedAutomaticRenewalFailure,
    },
    Repeated {
        count: u16,
        first: ResolvedAutomaticRenewalFailure,
        previous: ResolvedAutomaticRenewalFailure,
        latest: ResolvedAutomaticRenewalFailure,
    },
}

impl AutomaticRenewalFailureHistory {
    pub(crate) const fn count(self) -> u16 {
        match self {
            Self::Empty => 0,
            Self::First { .. } => 1,
            Self::Repeated { count, .. } => count,
        }
    }

    pub(crate) const fn first_resolved_at(self) -> Option<DateTime<Utc>> {
        match self {
            Self::Empty => None,
            Self::First { failure } => Some(failure.resolved_at),
            Self::Repeated { first, .. } => Some(first.resolved_at),
        }
    }

    pub(crate) const fn latest_resolved_at(self) -> Option<DateTime<Utc>> {
        match self {
            Self::Empty => None,
            Self::First { failure } => Some(failure.resolved_at),
            Self::Repeated { latest, .. } => Some(latest.resolved_at),
        }
    }
}

/// Canonical payment history for admitting a past-due transition and deriving
/// its access boundary. Automatic renewals own dunning count and pacing;
/// legacy recovery evidence can only prove pre-v2 status and suspension.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PastDueCausalHistory {
    automatic: AutomaticRenewalFailureHistory,
    legacy_recovery_access_ended_at: Option<DateTime<Utc>>,
}

impl PastDueCausalHistory {
    const fn automatic(self) -> AutomaticRenewalFailureHistory {
        self.automatic
    }

    const fn has_legacy_recovery(self) -> bool {
        self.legacy_recovery_access_ended_at.is_some()
    }

    pub(crate) fn access_ended_at(self, policy: PastDueAccessPolicy) -> Option<DateTime<Utc>> {
        match policy {
            PastDueAccessPolicy::SuspendImmediately => match (
                self.automatic.first_resolved_at(),
                self.legacy_recovery_access_ended_at,
            ) {
                (Some(automatic), Some(legacy)) => Some(automatic.min(legacy)),
                (automatic, legacy) => automatic.or(legacy),
            },
            PastDueAccessPolicy::ContinueUntilDunningExhausted => {
                self.automatic.latest_resolved_at()
            }
        }
    }
}

fn automatic_renewal_failure_history_from_resolved(
    resolved: &[ResolvedAutomaticRenewalFailure],
) -> Result<AutomaticRenewalFailureHistory, RenewalFailureStoreError> {
    let count = u16::try_from(resolved.len())
        .map_err(|_| RenewalFailureStoreError::InvalidState(INVALID_RENEWAL_FAILURE_STATE))?;
    let Some(first) = resolved.first().copied() else {
        return Ok(AutomaticRenewalFailureHistory::Empty);
    };
    let Some((latest, earlier)) = resolved.split_last() else {
        return Ok(AutomaticRenewalFailureHistory::Empty);
    };
    let Some((previous, _)) = earlier.split_last() else {
        return Ok(AutomaticRenewalFailureHistory::First { failure: first });
    };
    Ok(AutomaticRenewalFailureHistory::Repeated {
        count,
        first,
        previous: *previous,
        latest: *latest,
    })
}

async fn automatic_renewal_failure_history(
    connection: &mut PgConnection,
    subscription_id: SubscriptionId,
    period_start_at: DateTime<Utc>,
) -> Result<AutomaticRenewalFailureHistory, RenewalFailureStoreError> {
    let rows = sqlx::query(
        r#"
        SELECT id, resolved_at
        FROM billing_payment_attempts
        WHERE subscription_id = $1
            AND billing_period_start_at = $2
            AND attempt_kind = 'subscription_renewal'
            AND submitted_at IS NOT NULL
            AND status IN ('declined', 'failed')
            AND resolution_code IS NULL
        ORDER BY resolved_at ASC, id ASC
        "#,
    )
    .bind(subscription_id.as_uuid())
    .bind(period_start_at)
    .fetch_all(&mut *connection)
    .await?;

    let resolved = rows
        .iter()
        .map(|row| {
            let attempt_id = PaymentAttemptId::new(row.try_get::<Uuid, _>("id")?);
            let resolved_at = row
                .try_get::<Option<DateTime<Utc>>, _>("resolved_at")?
                .ok_or(RenewalFailureStoreError::InvalidState(
                    INVALID_RENEWAL_FAILURE_STATE,
                ))?;
            Ok(ResolvedAutomaticRenewalFailure {
                attempt_id,
                resolved_at,
            })
        })
        .collect::<Result<Vec<_>, RenewalFailureStoreError>>()?;
    automatic_renewal_failure_history_from_resolved(&resolved)
}

/// Returns the access-suspension timestamp for the version-1 recovery path
/// that could move an active subscription to `past_due`.
///
/// Version 2 never counts this recovery as automatic dunning. Its retained
/// review timestamp and terminal `failed` state identify the version-1 manual
/// failure path, while the optimistic `active` snapshot distinguishes that
/// authority from recoveries freshly reserved under version 2.
async fn legacy_recovery_failure_access_ended_at(
    connection: &mut PgConnection,
    subscription_id: SubscriptionId,
    period_start_at: DateTime<Utc>,
) -> Result<Option<DateTime<Utc>>, RenewalFailureStoreError> {
    let row = sqlx::query(
        r#"
        SELECT resolved_at
        FROM billing_payment_attempts
        WHERE subscription_id = $1
            AND billing_period_start_at = $2
            AND attempt_kind = 'subscription_recovery'
            AND subscription_expected_status = 'active'
            AND submitted_at IS NOT NULL
            AND review_required_at IS NOT NULL
            AND status = 'failed'
            AND resolution_code IS NULL
        ORDER BY resolved_at ASC NULLS FIRST, id ASC
        LIMIT 1
        "#,
    )
    .bind(subscription_id.as_uuid())
    .bind(period_start_at)
    .fetch_optional(&mut *connection)
    .await?;
    row.map(|row| {
        row.try_get::<Option<DateTime<Utc>>, _>("resolved_at")?
            .ok_or_else(invalid_state)
    })
    .transpose()
}

pub(crate) async fn past_due_causal_history(
    connection: &mut PgConnection,
    subscription_id: SubscriptionId,
    period_start_at: DateTime<Utc>,
) -> Result<PastDueCausalHistory, RenewalFailureStoreError> {
    Ok(PastDueCausalHistory {
        automatic: automatic_renewal_failure_history(connection, subscription_id, period_start_at)
            .await?,
        legacy_recovery_access_ended_at: legacy_recovery_failure_access_ended_at(
            connection,
            subscription_id,
            period_start_at,
        )
        .await?,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RenewalFailureApplication {
    Applied {
        disposition: RenewalFailureDisposition,
        events: Vec<BillingEvent>,
    },
    Noop,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ValidatedRenewalFailureAttempt {
    identity: PaymentAttemptIdentity,
    subscription_id: SubscriptionId,
    plan_key: PlanKey,
    period_start_at: DateTime<Utc>,
    submitted_at: DateTime<Utc>,
    resolved_at: DateTime<Utc>,
}

fn validate_resolved_renewal_attempt(
    attempt: &PaymentAttempt,
) -> Result<ValidatedRenewalFailureAttempt, RenewalFailureStoreError> {
    let PaymentAttemptTarget::SubscriptionRenewal {
        plan_key,
        period,
        expected_state,
        ..
    } = attempt.request().target()
    else {
        return Err(invalid_state());
    };
    if attempt.kind() != PaymentAttemptKind::SubscriptionRenewal
        || !matches!(
            attempt.status(),
            PaymentAttemptStatus::Declined | PaymentAttemptStatus::Failed
        )
        || attempt.state().resolution_code().is_some()
    {
        return Err(invalid_state());
    }
    let submitted_at = attempt
        .state()
        .timestamps()
        .submitted_at()
        .ok_or_else(invalid_state)?;
    let resolved_at = attempt
        .state()
        .timestamps()
        .resolved_at()
        .ok_or_else(invalid_state)?;
    Ok(ValidatedRenewalFailureAttempt {
        identity: attempt.identity(),
        subscription_id: expected_state.subscription_id(),
        plan_key: plan_key.clone(),
        period_start_at: *period.start_at(),
        submitted_at,
        resolved_at,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LockedRenewalFailureState {
    current: FailureProjection,
    next_renewal_at: DateTime<Utc>,
    policy: RenewalFailurePolicy,
}

impl LockedRenewalFailureState {
    fn from_row(row: &PgRow) -> Result<Self, RenewalFailureStoreError> {
        let status = row
            .try_get::<String, _>("status")?
            .parse::<SubscriptionStatus>()
            .map_err(|_| invalid_state())?;
        let next_renewal_at = row.try_get("next_renewal_at")?;
        let next_payment_attempt_at = row.try_get("next_payment_attempt_at")?;
        let unpaid_at = row.try_get("unpaid_at")?;
        let retry_delays_seconds = row.try_get("dunning_retry_delays_seconds")?;
        let exhaustion = row.try_get::<String, _>("dunning_exhaustion")?;
        let past_due_access = row.try_get::<String, _>("past_due_access")?;
        let policy = renewal_failure_policy_from_scalars(RenewalFailurePolicyScalars::new(
            retry_delays_seconds,
            &exhaustion,
            &past_due_access,
        ))
        .map_err(map_subscription_persistence_error)?;
        Ok(Self {
            current: FailureProjection {
                status,
                next_payment_attempt_at,
                unpaid_at,
            },
            next_renewal_at,
            policy,
        })
    }
}

async fn load_locked_renewal_failure_state(
    connection: &mut PgConnection,
    attempt: &ValidatedRenewalFailureAttempt,
) -> Result<LockedRenewalFailureState, RenewalFailureStoreError> {
    let row = sqlx::query(
        r#"
        SELECT status, next_renewal_at, next_payment_attempt_at, unpaid_at,
            dunning_retry_delays_seconds, dunning_exhaustion, past_due_access
        FROM billing_subscriptions
        WHERE id = $1
            AND billing_scope_id = $2
            AND subscriber_id = $3
            AND plan_key = $4
        FOR NO KEY UPDATE
        "#,
    )
    .bind(attempt.subscription_id.as_uuid())
    .bind(attempt.identity.billing_scope_id().as_uuid())
    .bind(attempt.identity.subscriber_id().as_uuid())
    .bind(attempt.plan_key.as_str())
    .fetch_optional(&mut *connection)
    .await?
    .ok_or_else(invalid_state)?;
    LockedRenewalFailureState::from_row(&row)
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum RenewalFailureDecision {
    Noop,
    Apply(RenewalFailureTransition),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RenewalFailureTransition {
    before: FailureProjection,
    after: FailureProjection,
    disposition: RenewalFailureDisposition,
    events: Vec<BillingEvent>,
}

fn decide_renewal_failure(
    attempt: &ValidatedRenewalFailureAttempt,
    subscription: &LockedRenewalFailureState,
    causal_history: PastDueCausalHistory,
) -> Result<RenewalFailureDecision, RenewalFailureStoreError> {
    if subscription.next_renewal_at != attempt.period_start_at {
        return Err(invalid_state());
    }
    let history = causal_history.automatic();
    let latest_failure = match history {
        AutomaticRenewalFailureHistory::Empty => return Err(invalid_state()),
        AutomaticRenewalFailureHistory::First { failure } => failure,
        AutomaticRenewalFailureHistory::Repeated { latest, .. } => latest,
    };
    if latest_failure.attempt_id != attempt.identity.attempt_id() {
        return Ok(RenewalFailureDecision::Noop);
    }
    if latest_failure.resolved_at != attempt.resolved_at {
        return Err(invalid_state());
    }
    let disposition = renewal_failure_disposition(
        &subscription.policy,
        history.count(),
        latest_failure.resolved_at,
    )
    .map_err(|_| invalid_state())?;

    if let AutomaticRenewalFailureHistory::Repeated {
        count, previous, ..
    } = history
    {
        let previous =
            renewal_failure_disposition(&subscription.policy, count - 1, previous.resolved_at)
                .map_err(|_| invalid_state())?;
        if !matches!(previous, RenewalFailureDisposition::RetryScheduled { .. }) {
            return Err(invalid_state());
        }
    }

    let after = projection(disposition);
    if subscription.current == after {
        return Ok(RenewalFailureDecision::Noop);
    }
    let before_is_valid = match history {
        AutomaticRenewalFailureHistory::Empty => false,
        AutomaticRenewalFailureHistory::First { .. } => {
            (subscription.current.status == SubscriptionStatus::Active
                || (subscription.current.status == SubscriptionStatus::PastDue
                    && causal_history.has_legacy_recovery()))
                && subscription.current.next_payment_attempt_at
                    == Some(subscription.next_renewal_at)
                && subscription
                    .current
                    .next_payment_attempt_at
                    .is_some_and(|due_at| due_at <= attempt.submitted_at)
                && subscription.current.unpaid_at.is_none()
        }
        AutomaticRenewalFailureHistory::Repeated { .. } => {
            subscription.current.status == SubscriptionStatus::PastDue
                && subscription
                    .current
                    .next_payment_attempt_at
                    .is_some_and(|due_at| due_at <= attempt.submitted_at)
                && subscription.current.unpaid_at.is_none()
        }
    };
    if !before_is_valid {
        return Err(invalid_state());
    }

    Ok(RenewalFailureDecision::Apply(RenewalFailureTransition {
        before: subscription.current,
        after,
        disposition,
        events: failure_events(attempt, &subscription.policy, causal_history, disposition)?,
    }))
}

fn failure_events(
    attempt: &ValidatedRenewalFailureAttempt,
    policy: &RenewalFailurePolicy,
    causal_history: PastDueCausalHistory,
    disposition: RenewalFailureDisposition,
) -> Result<Vec<BillingEvent>, RenewalFailureStoreError> {
    let event_disposition = match disposition {
        RenewalFailureDisposition::RetryScheduled { retry_at } => {
            SubscriptionPaymentFailureDisposition::RetryScheduled { retry_at }
        }
        RenewalFailureDisposition::RemainPastDue { exhausted_at } => {
            SubscriptionPaymentFailureDisposition::DunningExhausted { exhausted_at }
        }
        RenewalFailureDisposition::MarkUnpaid { ended_at } => {
            SubscriptionPaymentFailureDisposition::SubscriptionEnded { ended_at }
        }
    };
    let mut events = vec![BillingEvent::SubscriptionPaymentFailed {
        attempt_id: attempt.identity.attempt_id(),
        subscription_id: attempt.subscription_id,
        plan_key: attempt.plan_key.clone(),
        disposition: event_disposition,
    }];
    if let RenewalFailureDisposition::MarkUnpaid { ended_at } = disposition {
        let access_ends_at = causal_history
            .access_ended_at(policy.past_due_access())
            .ok_or_else(invalid_state)?;
        events.push(BillingEvent::SubscriptionEnded {
            attempt_id: attempt.identity.attempt_id(),
            subscription_id: attempt.subscription_id,
            plan_key: attempt.plan_key.clone(),
            reason: SubscriptionEndReason::NonPayment,
            ended_at,
            access_ends_at,
        });
    }
    Ok(events)
}

async fn persist_renewal_failure_transition(
    connection: &mut PgConnection,
    subscription_id: SubscriptionId,
    transition: &RenewalFailureTransition,
) -> Result<(), RenewalFailureStoreError> {
    let updated = sqlx::query(
        r#"
        UPDATE billing_subscriptions
        SET status = $2,
            next_payment_attempt_at = $3,
            unpaid_at = $4,
            updated_at = clock_timestamp()
        WHERE id = $1
            AND status = $5
            AND next_payment_attempt_at IS NOT DISTINCT FROM $6
            AND unpaid_at IS NOT DISTINCT FROM $7
        "#,
    )
    .bind(subscription_id.as_uuid())
    .bind(transition.after.status.as_str())
    .bind(transition.after.next_payment_attempt_at)
    .bind(transition.after.unpaid_at)
    .bind(transition.before.status.as_str())
    .bind(transition.before.next_payment_attempt_at)
    .bind(transition.before.unpaid_at)
    .execute(&mut *connection)
    .await?;
    if updated.rows_affected() != 1 {
        return Err(invalid_state());
    }
    Ok(())
}

pub(crate) async fn apply_resolved_automatic_renewal_failure(
    connection: &mut PgConnection,
    attempt: &PaymentAttempt,
) -> Result<RenewalFailureApplication, RenewalFailureStoreError> {
    let validated = validate_resolved_renewal_attempt(attempt)?;

    let durable = find_payment_attempt_by_id_on_connection(
        connection,
        validated.identity.billing_scope_id(),
        validated.identity.attempt_id(),
    )
    .await?
    .ok_or_else(invalid_state)?;
    if &durable != attempt {
        return Err(invalid_state());
    }

    let locked = load_locked_renewal_failure_state(connection, &validated).await?;
    let history = past_due_causal_history(
        connection,
        validated.subscription_id,
        locked.next_renewal_at,
    )
    .await?;
    match decide_renewal_failure(&validated, &locked, history)? {
        RenewalFailureDecision::Noop => Ok(RenewalFailureApplication::Noop),
        RenewalFailureDecision::Apply(transition) => {
            persist_renewal_failure_transition(connection, validated.subscription_id, &transition)
                .await?;
            Ok(RenewalFailureApplication::Applied {
                disposition: transition.disposition,
                events: transition.events,
            })
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FailureProjection {
    status: SubscriptionStatus,
    next_payment_attempt_at: Option<DateTime<Utc>>,
    unpaid_at: Option<DateTime<Utc>>,
}

const fn projection(disposition: RenewalFailureDisposition) -> FailureProjection {
    match disposition {
        RenewalFailureDisposition::RetryScheduled { retry_at } => FailureProjection {
            status: SubscriptionStatus::PastDue,
            next_payment_attempt_at: Some(retry_at),
            unpaid_at: None,
        },
        RenewalFailureDisposition::RemainPastDue { .. } => FailureProjection {
            status: SubscriptionStatus::PastDue,
            next_payment_attempt_at: None,
            unpaid_at: None,
        },
        RenewalFailureDisposition::MarkUnpaid { ended_at } => FailureProjection {
            status: SubscriptionStatus::Unpaid,
            next_payment_attempt_at: None,
            unpaid_at: Some(ended_at),
        },
    }
}

#[cfg(test)]
mod tests;
