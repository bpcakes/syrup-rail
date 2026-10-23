#![warn(missing_docs)]

use chrono::{DateTime, Utc};

use crate::{
    BillingEvent, BillingPeriod, BillingScopeId, PastDueAccessPolicy, PaymentAttemptId, PlanKey,
    SubscriberId, Subscription, SubscriptionId,
};

/// Whether subscription payment work may charge for, or apply, a billing period
/// that has already ended.
///
/// The host selects this policy; it is not persisted subscription terms.
/// Expiry is evaluated with the database clock at final admission and at
/// approval application, and a period whose end equals that time has expired.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum SubscriptionPeriodExpiryPolicy {
    /// Preserve the historical behavior: an approved charge applies its
    /// reserved period even when that period has already ended.
    #[default]
    Disabled,
    /// Refuse final admission of an expired renewal or recovery period and park
    /// any approval that arrives after its period ended for external reversal,
    /// without activating, renewing, or rescheduling the subscription.
    RejectExpiredPeriods,
}

impl SubscriptionPeriodExpiryPolicy {
    /// Returns whether this policy refuses expired billing periods.
    pub const fn rejects_expired_periods(self) -> bool {
        matches!(self, Self::RejectExpiredPeriods)
    }

    /// Returns whether a period ending at `period_end_at` has expired at the
    /// database time `now`. A period ending exactly at `now` has expired.
    pub fn period_has_expired(period_end_at: DateTime<Utc>, now: DateTime<Utc>) -> bool {
        period_end_at <= now
    }
}

/// Retires one exact obsolete due billing period as terminal `unpaid`.
///
/// The period is identified by its start, which must still be the
/// subscription's `next_renewal_at` when the command is applied.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetireExpiredSubscriptionPeriod {
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    plan_key: PlanKey,
    subscription_id: SubscriptionId,
    period_start_at: DateTime<Utc>,
}

impl RetireExpiredSubscriptionPeriod {
    /// Creates a retirement command for one exact subscription due period.
    pub const fn new(
        billing_scope_id: BillingScopeId,
        subscriber_id: SubscriberId,
        plan_key: PlanKey,
        subscription_id: SubscriptionId,
        period_start_at: DateTime<Utc>,
    ) -> Self {
        Self {
            billing_scope_id,
            subscriber_id,
            plan_key,
            subscription_id,
            period_start_at,
        }
    }

    /// Returns the billing scope.
    pub const fn billing_scope_id(&self) -> BillingScopeId {
        self.billing_scope_id
    }

    /// Returns the subscriber that owns the subscription.
    pub const fn subscriber_id(&self) -> SubscriberId {
        self.subscriber_id
    }

    /// Returns the exact plan owned by the subscription.
    pub const fn plan_key(&self) -> &PlanKey {
        &self.plan_key
    }

    /// Returns the subscription whose due period is retired.
    pub const fn subscription_id(&self) -> SubscriptionId {
        self.subscription_id
    }

    /// Returns the start of the due period being retired.
    pub const fn period_start_at(&self) -> &DateTime<Utc> {
        &self.period_start_at
    }
}

/// Result of retiring an obsolete due billing period.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SubscriptionPeriodRetirementOutcome {
    /// The expired period was retired and the lifecycle is now terminal
    /// `unpaid`. Any still-unsubmitted renewal or recovery authority for the
    /// subscription was rejected in the same transaction.
    Retired {
        /// The terminal subscription after retirement.
        subscription: Subscription,
        /// The event appended in the retirement transaction.
        event: BillingEvent,
        /// Prepared attempts rejected before they could be submitted.
        rejected_attempt_ids: Vec<PaymentAttemptId>,
    },
    /// The lifecycle was already terminal `unpaid` at the requested period.
    /// Replays and already-queued work observe this stable result.
    AlreadyUnpaid(Subscription),
    /// No subscription matches the exact scope, subscriber, plan, and ID.
    NotFound,
    /// A canceled lifecycle is never retired or revived.
    Canceled(Subscription),
    /// The subscription no longer awaits payment for the requested period.
    PeriodChanged(Subscription),
    /// The requested period has not ended at database time.
    NotExpired {
        /// The requested due period.
        period: BillingPeriod,
    },
    /// A submitted, unknown, review-required, or unreversed payment outcome
    /// remains for the subscription. The collection fence is retained until
    /// it is resolved.
    UnresolvedPayment,
}

/// Moves one existing subscription to a different persisted past-due access
/// policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangeSubscriptionPastDueAccess {
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    plan_key: PlanKey,
    subscription_id: SubscriptionId,
    past_due_access: PastDueAccessPolicy,
}

impl ChangeSubscriptionPastDueAccess {
    /// Creates a policy change for one exact subscription.
    pub const fn new(
        billing_scope_id: BillingScopeId,
        subscriber_id: SubscriberId,
        plan_key: PlanKey,
        subscription_id: SubscriptionId,
        past_due_access: PastDueAccessPolicy,
    ) -> Self {
        Self {
            billing_scope_id,
            subscriber_id,
            plan_key,
            subscription_id,
            past_due_access,
        }
    }

    /// Returns the billing scope.
    pub const fn billing_scope_id(&self) -> BillingScopeId {
        self.billing_scope_id
    }

    /// Returns the subscriber that owns the subscription.
    pub const fn subscriber_id(&self) -> SubscriberId {
        self.subscriber_id
    }

    /// Returns the exact plan owned by the subscription.
    pub const fn plan_key(&self) -> &PlanKey {
        &self.plan_key
    }

    /// Returns the subscription whose policy changes.
    pub const fn subscription_id(&self) -> SubscriptionId {
        self.subscription_id
    }

    /// Returns the requested past-due access policy.
    pub const fn past_due_access(&self) -> PastDueAccessPolicy {
        self.past_due_access
    }
}

/// Result of a past-due access policy change.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SubscriptionPastDueAccessChangeOutcome {
    /// The active or past-due subscription now persists the requested policy.
    /// Entitlement and later failure events derive access from it at once.
    Changed {
        /// The policy persisted before this change.
        previous: PastDueAccessPolicy,
        /// The subscription after the change.
        subscription: Subscription,
    },
    /// The subscription already persisted the requested policy.
    Unchanged(Subscription),
    /// Canceled and unpaid lifecycles keep their historical terms.
    Terminal(Subscription),
    /// No subscription matches the exact scope, subscriber, plan, and ID.
    NotFound,
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone};

    use super::*;

    #[test]
    fn expiry_policy_defaults_to_disabled() {
        assert_eq!(
            SubscriptionPeriodExpiryPolicy::default(),
            SubscriptionPeriodExpiryPolicy::Disabled
        );
        assert!(!SubscriptionPeriodExpiryPolicy::Disabled.rejects_expired_periods());
        assert!(SubscriptionPeriodExpiryPolicy::RejectExpiredPeriods.rejects_expired_periods());
    }

    #[test]
    fn a_period_ending_exactly_now_has_expired() {
        let now = Utc.with_ymd_and_hms(2026, 10, 2, 12, 0, 0).unwrap();
        assert!(SubscriptionPeriodExpiryPolicy::period_has_expired(now, now));
        assert!(SubscriptionPeriodExpiryPolicy::period_has_expired(
            now - Duration::microseconds(1),
            now
        ));
        assert!(!SubscriptionPeriodExpiryPolicy::period_has_expired(
            now + Duration::microseconds(1),
            now
        ));
    }
}
