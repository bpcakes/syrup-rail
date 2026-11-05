use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MissingSubscriptionAction {
    StartSubscription,
    ConfirmInitialPayment,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PastDueAction {
    RecoverPayment,
    ConfirmRecoveryPayment,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Host-visible access classification for a subscription in payment dunning.
///
/// `Entitlement::PastDue` is not itself a denial. Hosts must use this value for
/// reads and Syrup Rail uses the same classification for protected writes.
pub enum PastDueAccess {
    /// The accepted policy keeps product access open while another automatic
    /// payment attempt remains scheduled.
    AllowedDuringDunning,
    /// Product access has ended, either immediately on failure or because the
    /// configured dunning schedule is exhausted.
    Suspended,
}

/// Classifies access from the snapshotted policy and the next scheduled payment.
///
/// A present timestamp means dunning remains scheduled; its distance from the
/// current time does not change this classification.
pub const fn classify_past_due_access(
    policy: PastDueAccessPolicy,
    next_payment_attempt_at: Option<DateTime<Utc>>,
) -> PastDueAccess {
    match (policy, next_payment_attempt_at) {
        (PastDueAccessPolicy::ContinueUntilDunningExhausted, Some(_)) => {
            PastDueAccess::AllowedDuringDunning
        }
        _ => PastDueAccess::Suspended,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Entitlement {
    Missing {
        next_action: MissingSubscriptionAction,
        saved_discount: Option<SavedSubscriptionDiscount>,
    },
    PaidActive {
        subscription: Subscription,
        applied_discount: Option<AppliedSubscriptionDiscount>,
    },
    PaidThroughCancellation {
        subscription: Subscription,
        applied_discount: Option<AppliedSubscriptionDiscount>,
    },
    /// Payment is past due. Product access is determined by `access`, not by
    /// this variant alone.
    PastDue {
        subscription: Subscription,
        access: PastDueAccess,
        next_action: PastDueAction,
        applied_discount: Option<AppliedSubscriptionDiscount>,
    },
    Granted {
        grant: SubscriptionGrant,
    },
}

impl Entitlement {
    /// Returns whether this subscription entitlement permits product access.
    ///
    /// This applies Syrup Rail's subscription-entitlement policy only. Hosts
    /// must authenticate and authorize the subject before using the result to
    /// grant access to their product.
    pub const fn permits_product_access(&self) -> bool {
        match self {
            Self::PaidActive { .. }
            | Self::PaidThroughCancellation { .. }
            | Self::Granted { .. }
            | Self::PastDue {
                access: PastDueAccess::AllowedDuringDunning,
                ..
            } => true,
            Self::Missing { .. }
            | Self::PastDue {
                access: PastDueAccess::Suspended,
                ..
            } => false,
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
struct EntitlementSelector {
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    plan_key: PlanKey,
    required_gateway_account_mode: Option<GatewayAccountMode>,
}

impl EntitlementSelector {
    const fn new(
        billing_scope_id: BillingScopeId,
        subscriber_id: SubscriberId,
        plan_key: PlanKey,
    ) -> Self {
        Self {
            billing_scope_id,
            subscriber_id,
            plan_key,
            required_gateway_account_mode: Some(GatewayAccountMode::Live),
        }
    }

    const fn require_gateway_account_mode(&mut self, mode: GatewayAccountMode) {
        self.required_gateway_account_mode = Some(mode);
    }

    const fn allow_all_gateway_account_modes(&mut self) {
        self.required_gateway_account_mode = None;
    }

    fn fmt_as(&self, name: &str, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct(name)
            .field("billing_scope_id", &self.billing_scope_id)
            .field("subscriber_id", &self.subscriber_id)
            .field("plan_key", &self.plan_key)
            .field(
                "required_gateway_account_mode",
                &self.required_gateway_account_mode,
            )
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct EntitlementQuery {
    selector: EntitlementSelector,
}

impl fmt::Debug for EntitlementQuery {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.selector.fmt_as("EntitlementQuery", formatter)
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct EntitlementGuard {
    selector: EntitlementSelector,
}

impl fmt::Debug for EntitlementGuard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.selector.fmt_as("EntitlementGuard", formatter)
    }
}

impl EntitlementGuard {
    /// Creates a production-safe guard that admits live paid subscriptions.
    /// Host-issued grants remain mode-neutral.
    pub const fn new(
        billing_scope_id: BillingScopeId,
        subscriber_id: SubscriberId,
        plan_key: PlanKey,
    ) -> Self {
        Self {
            selector: EntitlementSelector::new(billing_scope_id, subscriber_id, plan_key),
        }
    }

    /// Restricts paid-subscription access to one durable gateway mode.
    /// Host-issued grants remain mode-neutral.
    pub const fn with_required_gateway_account_mode(mut self, mode: GatewayAccountMode) -> Self {
        self.selector.require_gateway_account_mode(mode);
        self
    }

    /// Explicitly admits paid subscriptions from either gateway mode.
    pub const fn across_gateway_account_modes(mut self) -> Self {
        self.selector.allow_all_gateway_account_modes();
        self
    }

    pub const fn billing_scope_id(&self) -> BillingScopeId {
        self.selector.billing_scope_id
    }

    pub const fn subscriber_id(&self) -> SubscriberId {
        self.selector.subscriber_id
    }

    pub const fn plan_key(&self) -> &PlanKey {
        &self.selector.plan_key
    }

    pub const fn required_gateway_account_mode(&self) -> Option<GatewayAccountMode> {
        self.selector.required_gateway_account_mode
    }
}

impl EntitlementQuery {
    /// Creates a production-safe query for live paid subscriptions.
    /// Host-issued grants remain mode-neutral.
    pub const fn new(
        billing_scope_id: BillingScopeId,
        subscriber_id: SubscriberId,
        plan_key: PlanKey,
    ) -> Self {
        Self {
            selector: EntitlementSelector::new(billing_scope_id, subscriber_id, plan_key),
        }
    }

    /// Restricts paid-subscription access to one durable gateway mode.
    /// Host-issued grants remain mode-neutral.
    pub const fn with_required_gateway_account_mode(mut self, mode: GatewayAccountMode) -> Self {
        self.selector.require_gateway_account_mode(mode);
        self
    }

    /// Explicitly reads paid subscriptions from either gateway mode.
    pub const fn across_gateway_account_modes(mut self) -> Self {
        self.selector.allow_all_gateway_account_modes();
        self
    }

    pub const fn billing_scope_id(&self) -> BillingScopeId {
        self.selector.billing_scope_id
    }

    pub const fn subscriber_id(&self) -> SubscriberId {
        self.selector.subscriber_id
    }

    pub const fn plan_key(&self) -> &PlanKey {
        &self.selector.plan_key
    }

    pub const fn required_gateway_account_mode(&self) -> Option<GatewayAccountMode> {
        self.selector.required_gateway_account_mode
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeletionBlockerQuery {
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
}

impl DeletionBlockerQuery {
    pub const fn new(billing_scope_id: BillingScopeId, subscriber_id: SubscriberId) -> Self {
        Self {
            billing_scope_id,
            subscriber_id,
        }
    }

    pub const fn billing_scope_id(self) -> BillingScopeId {
        self.billing_scope_id
    }

    pub const fn subscriber_id(self) -> SubscriberId {
        self.subscriber_id
    }
}

/// A billing condition that prevents subscriber deletion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BillingDeletionBlocker {
    /// A live subscription still requires the retained billing subject.
    ActiveSubscription,
    /// A payment attempt or charge still needs resolution.
    UnresolvedPayment,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BillingDeletionBlockers {
    active_subscription: bool,
    unresolved_payment: bool,
}

impl BillingDeletionBlockers {
    /// Collects the reasons preventing deletion. An empty iterator yields no
    /// billing blockers; repeated reasons are idempotent.
    pub fn new(blockers: impl IntoIterator<Item = BillingDeletionBlocker>) -> Self {
        let mut result = Self::default();
        for blocker in blockers {
            match blocker {
                BillingDeletionBlocker::ActiveSubscription => result.active_subscription = true,
                BillingDeletionBlocker::UnresolvedPayment => result.unresolved_payment = true,
            }
        }
        result
    }

    pub const fn active_subscription(self) -> bool {
        self.active_subscription
    }

    pub const fn unresolved_payment(self) -> bool {
        self.unresolved_payment
    }

    pub const fn is_empty(self) -> bool {
        !self.active_subscription && !self.unresolved_payment
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScrubSubscriberBillingData {
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
}

impl ScrubSubscriberBillingData {
    pub const fn new(billing_scope_id: BillingScopeId, subscriber_id: SubscriberId) -> Self {
        Self {
            billing_scope_id,
            subscriber_id,
        }
    }

    pub const fn billing_scope_id(self) -> BillingScopeId {
        self.billing_scope_id
    }

    pub const fn subscriber_id(self) -> SubscriberId {
        self.subscriber_id
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ScrubbedBillingRows {
    payment_attempts: u64,
    payment_methods: u64,
}

impl ScrubbedBillingRows {
    pub const fn new(payment_attempts: u64, payment_methods: u64) -> Self {
        Self {
            payment_attempts,
            payment_methods,
        }
    }

    pub const fn payment_attempts(self) -> u64 {
        self.payment_attempts
    }

    pub const fn payment_methods(self) -> u64 {
        self.payment_methods
    }

    pub const fn is_empty(self) -> bool {
        self.payment_attempts == 0 && self.payment_methods == 0
    }
}
