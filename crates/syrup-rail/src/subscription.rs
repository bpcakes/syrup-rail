use std::{fmt, str::FromStr};

use chrono::{DateTime, Utc};
use thiserror::Error;

use crate::{
    ActorId, BillingPeriod, BillingScopeId, ChargeAmount, CurrencyCode, DiscountClaimId,
    GatewayAccountMode, PastDueAccessPolicy, PaymentMethodId, PlanKey, RenewalFailurePolicy,
    SubscriberId, SubscriptionGrantId, SubscriptionId, SubscriptionPeriodRule, SubscriptionPhase,
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
    SubscriptionGrantRecordError, SubscriptionGrantRevocation, SubscriptionGrantRevocationAudit,
    SubscriptionGrantRevocationOutcome, SubscriptionGrantRevocationState,
};

mod access;
mod discount;
mod grant;

/// Failure to combine flat subscription lifecycle fields into one valid state.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum SubscriptionLifecycleError {
    /// The separately supplied renewal time differs from billing-period end.
    #[error("next renewal must equal the current billing period end")]
    NextRenewalDoesNotMatchPeriod,
    /// The status and optional payment time violate the lifecycle matrix.
    #[error("subscription status and next payment attempt form an invalid schedule")]
    InvalidPaymentSchedule,
}

/// One validated subscription status, billing period, and payment schedule.
///
/// The current billing period owns the economic renewal boundary. Active
/// subscriptions schedule a payment at that boundary, past-due subscriptions
/// may schedule one at or after it, and terminal subscriptions schedule none.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubscriptionLifecycle {
    state: SubscriptionLifecycleState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum SubscriptionLifecycleState {
    Active(BillingPeriod),
    PastDue {
        current_period: BillingPeriod,
        next_payment_attempt_at: Option<DateTime<Utc>>,
    },
    Canceled(BillingPeriod),
    Unpaid(BillingPeriod),
}

impl SubscriptionLifecycle {
    /// Constructs an active lifecycle whose next payment is due at period end.
    pub const fn active(current_period: BillingPeriod) -> Self {
        Self {
            state: SubscriptionLifecycleState::Active(current_period),
        }
    }

    /// Constructs a past-due lifecycle with no retry or with one at or after
    /// the economic renewal boundary.
    pub fn past_due(
        current_period: BillingPeriod,
        next_payment_attempt_at: Option<DateTime<Utc>>,
    ) -> Result<Self, SubscriptionLifecycleError> {
        if next_payment_attempt_at
            .as_ref()
            .is_some_and(|next_attempt| next_attempt < current_period.end_at())
        {
            return Err(SubscriptionLifecycleError::InvalidPaymentSchedule);
        }
        Ok(Self {
            state: SubscriptionLifecycleState::PastDue {
                current_period,
                next_payment_attempt_at,
            },
        })
    }

    /// Constructs a canceled lifecycle with no scheduled payment.
    pub const fn canceled(current_period: BillingPeriod) -> Self {
        Self {
            state: SubscriptionLifecycleState::Canceled(current_period),
        }
    }

    /// Constructs a terminal unpaid lifecycle with no scheduled payment.
    pub const fn unpaid(current_period: BillingPeriod) -> Self {
        Self {
            state: SubscriptionLifecycleState::Unpaid(current_period),
        }
    }

    /// Validates flat status and timestamp projections as one lifecycle value.
    pub fn from_parts(
        status: SubscriptionStatus,
        current_period: BillingPeriod,
        next_renewal_at: DateTime<Utc>,
        next_payment_attempt_at: Option<DateTime<Utc>>,
    ) -> Result<Self, SubscriptionLifecycleError> {
        if &next_renewal_at != current_period.end_at() {
            return Err(SubscriptionLifecycleError::NextRenewalDoesNotMatchPeriod);
        }

        match status {
            SubscriptionStatus::Active
                if next_payment_attempt_at.as_ref() == Some(current_period.end_at()) =>
            {
                Ok(Self::active(current_period))
            }
            SubscriptionStatus::PastDue => Self::past_due(current_period, next_payment_attempt_at),
            SubscriptionStatus::Canceled if next_payment_attempt_at.is_none() => {
                Ok(Self::canceled(current_period))
            }
            SubscriptionStatus::Unpaid if next_payment_attempt_at.is_none() => {
                Ok(Self::unpaid(current_period))
            }
            SubscriptionStatus::Active
            | SubscriptionStatus::Canceled
            | SubscriptionStatus::Unpaid => Err(SubscriptionLifecycleError::InvalidPaymentSchedule),
        }
    }

    /// Returns the status projection for this lifecycle.
    pub const fn status(&self) -> SubscriptionStatus {
        match self.state {
            SubscriptionLifecycleState::Active(_) => SubscriptionStatus::Active,
            SubscriptionLifecycleState::PastDue { .. } => SubscriptionStatus::PastDue,
            SubscriptionLifecycleState::Canceled(_) => SubscriptionStatus::Canceled,
            SubscriptionLifecycleState::Unpaid(_) => SubscriptionStatus::Unpaid,
        }
    }

    /// Returns the lifecycle's authoritative current billing period.
    pub const fn current_period(&self) -> &BillingPeriod {
        match &self.state {
            SubscriptionLifecycleState::Active(current_period)
            | SubscriptionLifecycleState::Canceled(current_period)
            | SubscriptionLifecycleState::Unpaid(current_period)
            | SubscriptionLifecycleState::PastDue { current_period, .. } => current_period,
        }
    }

    /// Returns the economic renewal boundary derived from the current period.
    pub const fn next_renewal_at(&self) -> &DateTime<Utc> {
        self.current_period().end_at()
    }

    /// Returns the next automatic payment time when one remains scheduled.
    pub const fn next_payment_attempt_at(&self) -> Option<&DateTime<Utc>> {
        match &self.state {
            SubscriptionLifecycleState::Active(current_period) => Some(current_period.end_at()),
            SubscriptionLifecycleState::PastDue {
                next_payment_attempt_at,
                ..
            } => next_payment_attempt_at.as_ref(),
            SubscriptionLifecycleState::Canceled(_) | SubscriptionLifecycleState::Unpaid(_) => None,
        }
    }

    fn into_parts(self) -> (SubscriptionStatus, BillingPeriod, Option<DateTime<Utc>>) {
        match self.state {
            SubscriptionLifecycleState::Active(current_period) => {
                let next_payment_attempt_at = Some(*current_period.end_at());
                (
                    SubscriptionStatus::Active,
                    current_period,
                    next_payment_attempt_at,
                )
            }
            SubscriptionLifecycleState::PastDue {
                current_period,
                next_payment_attempt_at,
            } => (
                SubscriptionStatus::PastDue,
                current_period,
                next_payment_attempt_at,
            ),
            SubscriptionLifecycleState::Canceled(current_period) => {
                (SubscriptionStatus::Canceled, current_period, None)
            }
            SubscriptionLifecycleState::Unpaid(current_period) => {
                (SubscriptionStatus::Unpaid, current_period, None)
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Subscription {
    id: SubscriptionId,
    plan_key: PlanKey,
    status: SubscriptionStatus,
    phase: SubscriptionPhase,
    required_gateway_account_mode: GatewayAccountMode,
    payment_method_id: PaymentMethodId,
    recurring_charge: ChargeAmount,
    recurring_period: SubscriptionPeriodRule,
    renewal_failure: RenewalFailurePolicy,
    current_period: BillingPeriod,
    next_renewal_at: DateTime<Utc>,
    next_payment_attempt_at: Option<DateTime<Utc>>,
}

impl Subscription {
    /// Compatibility constructor for the original flat subscription fields.
    ///
    /// This remains infallible so existing consumers retain their source and
    /// behavior contract. New validated paths should construct a
    /// [`SubscriptionLifecycle`] and call [`Self::from_lifecycle`].
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        id: SubscriptionId,
        plan_key: PlanKey,
        status: SubscriptionStatus,
        phase: SubscriptionPhase,
        required_gateway_account_mode: GatewayAccountMode,
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
            required_gateway_account_mode,
            payment_method_id,
            recurring_charge,
            recurring_period,
            renewal_failure,
            current_period,
            next_renewal_at,
            next_payment_attempt_at,
        }
    }

    /// Constructs a subscription from one validated lifecycle value.
    #[allow(clippy::too_many_arguments)]
    pub fn from_lifecycle(
        id: SubscriptionId,
        plan_key: PlanKey,
        phase: SubscriptionPhase,
        required_gateway_account_mode: GatewayAccountMode,
        payment_method_id: PaymentMethodId,
        recurring_charge: ChargeAmount,
        recurring_period: SubscriptionPeriodRule,
        renewal_failure: RenewalFailurePolicy,
        lifecycle: SubscriptionLifecycle,
    ) -> Self {
        let (status, current_period, next_payment_attempt_at) = lifecycle.into_parts();
        let next_renewal_at = *current_period.end_at();
        Self {
            id,
            plan_key,
            status,
            phase,
            required_gateway_account_mode,
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

    /// Deployment mode durably authorized when this subscription was created.
    pub const fn required_gateway_account_mode(&self) -> GatewayAccountMode {
        self.required_gateway_account_mode
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
