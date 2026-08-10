use std::{fmt, str::FromStr};

use chrono::{DateTime, Utc};
use thiserror::Error;

use crate::{
    ActorId, BillingPeriod, BillingScopeId, ChargeAmount, CurrencyCode, DiscountClaimId,
    PastDueAccessPolicy, PaymentMethodId, PlanKey, RenewalFailurePolicy, SubscriberId,
    SubscriptionGrantId, SubscriptionId, SubscriptionPeriodRule, SubscriptionPhase,
    SubscriptionStatus,
};

pub use access::{
    BillingDeletionBlockers, DeletionBlockerQuery, Entitlement, EntitlementGuard, EntitlementQuery,
    MissingSubscriptionAction, PastDueAccess, PastDueAction, ScrubSubscriberBillingData,
    ScrubbedBillingRows, classify_past_due_access,
};
pub use discount::{
    AppliedSubscriptionDiscount, LimitedDiscountMonths, PercentOffBasisPoints,
    PositiveDiscountCents, SavedSubscriptionDiscount, SubscriptionDiscountCode,
    SubscriptionDiscountDuration, SubscriptionDiscountError, SubscriptionDiscountKind,
    SubscriptionDiscountSnapshot,
};
pub use grant::{
    SubscriptionGrant, SubscriptionGrantCreation, SubscriptionGrantCreationOutcome,
    SubscriptionGrantError, SubscriptionGrantKind, SubscriptionGrantKindParseError,
    SubscriptionGrantReason, SubscriptionGrantReasonError, SubscriptionGrantRecord,
    SubscriptionGrantRecordError, SubscriptionGrantRevocation, SubscriptionGrantRevocationOutcome,
};

mod access;
mod discount;
mod grant;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Subscription {
    id: SubscriptionId,
    plan_key: PlanKey,
    status: SubscriptionStatus,
    phase: SubscriptionPhase,
    payment_method_id: PaymentMethodId,
    recurring_charge: ChargeAmount,
    recurring_period: SubscriptionPeriodRule,
    renewal_failure: RenewalFailurePolicy,
    current_period: BillingPeriod,
    next_renewal_at: DateTime<Utc>,
    next_payment_attempt_at: Option<DateTime<Utc>>,
}

impl Subscription {
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        id: SubscriptionId,
        plan_key: PlanKey,
        status: SubscriptionStatus,
        phase: SubscriptionPhase,
        payment_method_id: PaymentMethodId,
        recurring_charge: ChargeAmount,
        recurring_period: SubscriptionPeriodRule,
        renewal_failure: RenewalFailurePolicy,
        current_period: BillingPeriod,
        next_renewal_at: DateTime<Utc>,
        next_payment_attempt_at: Option<DateTime<Utc>>,
    ) -> Self {
        Self {
            id,
            plan_key,
            status,
            phase,
            payment_method_id,
            recurring_charge,
            recurring_period,
            renewal_failure,
            current_period,
            next_renewal_at,
            next_payment_attempt_at,
        }
    }

    pub const fn id(&self) -> SubscriptionId {
        self.id
    }

    pub const fn plan_key(&self) -> &PlanKey {
        &self.plan_key
    }

    pub const fn status(&self) -> SubscriptionStatus {
        self.status
    }

    pub const fn phase(&self) -> SubscriptionPhase {
        self.phase
    }

    pub const fn payment_method_id(&self) -> PaymentMethodId {
        self.payment_method_id
    }

    pub const fn recurring_charge(&self) -> ChargeAmount {
        self.recurring_charge
    }

    pub const fn recurring_period(&self) -> SubscriptionPeriodRule {
        self.recurring_period
    }

    pub const fn renewal_failure(&self) -> &RenewalFailurePolicy {
        &self.renewal_failure
    }

    pub const fn current_period(&self) -> &BillingPeriod {
        &self.current_period
    }

    pub const fn next_renewal_at(&self) -> &DateTime<Utc> {
        &self.next_renewal_at
    }

    pub const fn next_payment_attempt_at(&self) -> Option<&DateTime<Utc>> {
        self.next_payment_attempt_at.as_ref()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CancelSubscription {
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    plan_key: PlanKey,
}

impl CancelSubscription {
    pub const fn new(
        billing_scope_id: BillingScopeId,
        subscriber_id: SubscriberId,
        plan_key: PlanKey,
    ) -> Self {
        Self {
            billing_scope_id,
            subscriber_id,
            plan_key,
        }
    }

    pub const fn billing_scope_id(&self) -> BillingScopeId {
        self.billing_scope_id
    }

    pub const fn subscriber_id(&self) -> SubscriberId {
        self.subscriber_id
    }

    pub const fn plan_key(&self) -> &PlanKey {
        &self.plan_key
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CancelSubscriptionOutcome {
    /// The current active or past-due lifecycle was canceled.
    Canceled {
        subscription: Subscription,
        event: crate::BillingEvent,
    },
    /// The newest exact lifecycle is already canceled, including when its
    /// paid-through access period has expired.
    AlreadyCanceled(Subscription),
    BlockedByRenewal,
    BlockedByPaymentMethodUpdate,
    /// No cancelable lifecycle exists. This also represents a newest terminal
    /// unpaid lifecycle; retained financial history may still exist.
    NotFound,
}

#[cfg(test)]
mod tests;
