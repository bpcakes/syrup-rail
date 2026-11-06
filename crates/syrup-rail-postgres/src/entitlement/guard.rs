//! Access projection from validated collection state, independent paid-through
//! dates, and the snapshotted access policy. Grant overlap still fails closed.
use super::*;
use syrup_rail::SubscriptionCollectionState;

pub(super) type GuardGrantTimeState = (DateTime<Utc>, DateTime<Utc>, Option<DateTime<Utc>>);
pub(super) type GuardSubscriptionState = (
    String,
    DateTime<Utc>,
    String,
    Option<DateTime<Utc>>,
    DateTime<Utc>,
);

pub(super) fn classify_guard_access(
    subscriptions: &[GuardSubscriptionState],
    grants: &[GuardGrantTimeState],
    access_at: DateTime<Utc>,
) -> GuardAccess {
    let states = subscriptions
        .iter()
        .map(|(status, period_end, policy, next_attempt, next_renewal)| {
            let state = SubscriptionCollectionState::from_persisted(
                status.parse().ok()?,
                *next_renewal,
                *next_attempt,
            )
            .ok()?;
            Some((state, *period_end, policy))
        })
        .collect::<Option<Vec<_>>>();
    let Some(states) = states else {
        return GuardAccess::Invalid;
    };
    let current_paid = states
        .iter()
        .filter(|(state, end, _)| {
            state.accepts_payment()
                || (*state == SubscriptionCollectionState::Canceled && *end > access_at)
        })
        .collect::<Vec<_>>();
    let active_grant_count = grants
        .iter()
        .filter(|(starts_at, ends_at, revoked_at)| {
            *starts_at <= access_at && *ends_at > access_at && revoked_at.is_none()
        })
        .count();
    if current_paid.len() > 1
        || active_grant_count > 1
        || (!current_paid.is_empty() && active_grant_count > 0)
    {
        return GuardAccess::Invalid;
    }
    if active_grant_count == 1 {
        return GuardAccess::Granted;
    }
    let Some((collection, _, policy)) = current_paid.first() else {
        return GuardAccess::Missing;
    };
    match collection {
        SubscriptionCollectionState::Scheduled { .. } => GuardAccess::Paid,
        SubscriptionCollectionState::Canceled => GuardAccess::PaidThroughCancellation,
        SubscriptionCollectionState::RetryScheduled { .. }
        | SubscriptionCollectionState::Exhausted => match policy.parse::<PastDueAccessPolicy>() {
            Ok(policy)
                if collection.past_due_access(policy)
                    == Some(PastDueAccess::AllowedDuringDunning) =>
            {
                GuardAccess::Paid
            }
            Ok(_) => GuardAccess::PastDue,
            Err(_) => GuardAccess::Invalid,
        },
        SubscriptionCollectionState::Unpaid => GuardAccess::Invalid,
    }
}
