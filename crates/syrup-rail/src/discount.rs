use std::fmt;

use chrono::{DateTime, Utc};

use crate::{
    BillingScopeId, ChargeAmount, CurrencyCode, DiscountClaimId, DiscountCodeId, PaymentAttemptId,
    PlanKey, SubscriberId, SubscriptionDiscountCode, SubscriptionDiscountDuration,
    SubscriptionDiscountError, SubscriptionDiscountKind, SubscriptionDiscountSnapshot,
    SubscriptionId, SubscriptionOffer,
};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SubscriptionDiscountCodeStatus {
    Active,
    Disabled,
}

impl SubscriptionDiscountCodeStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Disabled => "disabled",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SubscriptionDiscountClaimStatus {
    Saved,
    Applied,
    Superseded,
    Expired,
}

impl SubscriptionDiscountClaimStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Saved => "saved",
            Self::Applied => "applied",
            Self::Superseded => "superseded",
            Self::Expired => "expired",
        }
    }
}

/// The complete lifecycle state of a persisted subscription discount claim.
///
/// The database persists this as a status and nullable detail columns for
/// compatibility. This type closes those fields into the only valid shapes for
/// domain consumers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SubscriptionDiscountClaimState {
    /// The claim is available for a future initial subscription payment.
    Saved,
    /// The claim was consumed by an initial subscription payment.
    Applied {
        /// When the claim was applied.
        applied_at: DateTime<Utc>,
        /// The subscription created with the claim.
        subscription_id: SubscriptionId,
        /// The initial payment attempt that consumed the claim.
        payment_attempt_id: PaymentAttemptId,
    },
    /// A later saved claim replaced this one.
    Superseded {
        /// When the claim was superseded.
        superseded_at: DateTime<Utc>,
    },
    /// The saved claim was cleared or its discount code was disabled.
    Expired,
}

impl SubscriptionDiscountClaimState {
    /// Returns the compatibility status projection stored in the flat schema.
    pub const fn status(&self) -> SubscriptionDiscountClaimStatus {
        match self {
            Self::Saved => SubscriptionDiscountClaimStatus::Saved,
            Self::Applied { .. } => SubscriptionDiscountClaimStatus::Applied,
            Self::Superseded { .. } => SubscriptionDiscountClaimStatus::Superseded,
            Self::Expired => SubscriptionDiscountClaimStatus::Expired,
        }
    }

    /// Validates historical status/detail fields and closes them into one state.
    pub fn from_legacy_parts(
        status: SubscriptionDiscountClaimStatus,
        applied_at: Option<DateTime<Utc>>,
        applied_subscription_id: Option<SubscriptionId>,
        applied_payment_attempt_id: Option<PaymentAttemptId>,
        superseded_at: Option<DateTime<Utc>>,
    ) -> Result<Self, SubscriptionDiscountError> {
        match (
            status,
            applied_at,
            applied_subscription_id,
            applied_payment_attempt_id,
            superseded_at,
        ) {
            (SubscriptionDiscountClaimStatus::Saved, None, None, None, None) => Ok(Self::Saved),
            (
                SubscriptionDiscountClaimStatus::Applied,
                Some(applied_at),
                Some(subscription_id),
                Some(payment_attempt_id),
                None,
            ) => Ok(Self::Applied {
                applied_at,
                subscription_id,
                payment_attempt_id,
            }),
            (
                SubscriptionDiscountClaimStatus::Superseded,
                None,
                None,
                None,
                Some(superseded_at),
            ) => Ok(Self::Superseded { superseded_at }),
            (SubscriptionDiscountClaimStatus::Expired, None, None, None, None) => Ok(Self::Expired),
            _ => Err(SubscriptionDiscountError::InvalidState),
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct SubscriptionDiscountCodeRecord {
    id: DiscountCodeId,
    billing_scope_id: BillingScopeId,
    plan_key: PlanKey,
    code: SubscriptionDiscountCode,
    display_code: String,
    label: Option<String>,
    status: SubscriptionDiscountCodeStatus,
    kind: SubscriptionDiscountKind,
    currency: CurrencyCode,
    duration: SubscriptionDiscountDuration,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl fmt::Debug for SubscriptionDiscountCodeRecord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SubscriptionDiscountCodeRecord")
            .field("id", &self.id)
            .field("billing_scope_id", &self.billing_scope_id)
            .field("plan_key", &self.plan_key)
            .field("code", &self.code)
            .field("display_code", &"[redacted]")
            .field("label", &self.label)
            .field("status", &self.status)
            .field("kind", &self.kind)
            .field("currency", &self.currency)
            .field("duration", &self.duration)
            .field("created_at", &self.created_at)
            .field("updated_at", &self.updated_at)
            .finish()
    }
}

impl SubscriptionDiscountCodeRecord {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: DiscountCodeId,
        billing_scope_id: BillingScopeId,
        plan_key: PlanKey,
        code: SubscriptionDiscountCode,
        display_code: String,
        label: Option<String>,
        status: SubscriptionDiscountCodeStatus,
        kind: SubscriptionDiscountKind,
        currency: CurrencyCode,
        duration: SubscriptionDiscountDuration,
        created_at: DateTime<Utc>,
        updated_at: DateTime<Utc>,
    ) -> Result<Self, SubscriptionDiscountError> {
        if display_code.trim().is_empty()
            || updated_at < created_at
            || label.as_ref().is_some_and(|value| {
                value.trim() != value || value.is_empty() || value.chars().count() > 120
            })
        {
            return Err(SubscriptionDiscountError::InvalidState);
        }
        Ok(Self {
            id,
            billing_scope_id,
            plan_key,
            code,
            display_code,
            label,
            status,
            kind,
            currency,
            duration,
            created_at,
            updated_at,
        })
    }

    pub const fn id(&self) -> DiscountCodeId {
        self.id
    }
    pub const fn billing_scope_id(&self) -> BillingScopeId {
        self.billing_scope_id
    }
    pub const fn plan_key(&self) -> &PlanKey {
        &self.plan_key
    }
    pub const fn code(&self) -> &SubscriptionDiscountCode {
        &self.code
    }
    pub fn display_code(&self) -> &str {
        &self.display_code
    }
    pub fn label(&self) -> Option<&str> {
        self.label.as_deref()
    }
    pub const fn status(&self) -> SubscriptionDiscountCodeStatus {
        self.status
    }
    pub const fn kind(&self) -> SubscriptionDiscountKind {
        self.kind
    }
    pub const fn currency(&self) -> CurrencyCode {
        self.currency
    }
    pub const fn duration(&self) -> SubscriptionDiscountDuration {
        self.duration
    }
    pub const fn created_at(&self) -> &DateTime<Utc> {
        &self.created_at
    }
    pub const fn updated_at(&self) -> &DateTime<Utc> {
        &self.updated_at
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubscriptionDiscountCodeQuote {
    code: SubscriptionDiscountCodeRecord,
    base_charge: ChargeAmount,
    discounted_charge: ChargeAmount,
}

impl SubscriptionDiscountCodeQuote {
    pub fn new(
        code: SubscriptionDiscountCodeRecord,
        offer: &SubscriptionOffer,
    ) -> Result<Self, SubscriptionDiscountError> {
        if code.plan_key() != offer.plan_key() {
            return Err(SubscriptionDiscountError::InvalidState);
        }
        if matches!(
            code.duration(),
            SubscriptionDiscountDuration::LimitedMonths(_)
        ) && !offer.recurring().period().is_one_calendar_month()
        {
            return Err(SubscriptionDiscountError::LimitedDiscountCadence);
        }
        let base_charge = offer.recurring().charge();
        let discounted_charge = discounted_charge(base_charge, code.currency(), code.kind())?;
        Ok(Self {
            code,
            base_charge,
            discounted_charge,
        })
    }

    pub const fn code(&self) -> &SubscriptionDiscountCodeRecord {
        &self.code
    }
    pub const fn base_charge(&self) -> ChargeAmount {
        self.base_charge
    }
    pub const fn discounted_charge(&self) -> ChargeAmount {
        self.discounted_charge
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubscriptionDiscountCodeCreation {
    id: DiscountCodeId,
    billing_scope_id: BillingScopeId,
    plan_key: PlanKey,
    code: SubscriptionDiscountCode,
    label: Option<String>,
    kind: SubscriptionDiscountKind,
    currency: CurrencyCode,
    duration: SubscriptionDiscountDuration,
}

impl SubscriptionDiscountCodeCreation {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: DiscountCodeId,
        billing_scope_id: BillingScopeId,
        plan_key: PlanKey,
        code: SubscriptionDiscountCode,
        label: Option<String>,
        kind: SubscriptionDiscountKind,
        currency: CurrencyCode,
        duration: SubscriptionDiscountDuration,
    ) -> Result<Self, SubscriptionDiscountError> {
        Ok(Self {
            id,
            billing_scope_id,
            plan_key,
            code,
            label: normalize_label(label)?,
            kind,
            currency,
            duration,
        })
    }

    pub const fn id(&self) -> DiscountCodeId {
        self.id
    }
    pub const fn billing_scope_id(&self) -> BillingScopeId {
        self.billing_scope_id
    }
    pub const fn plan_key(&self) -> &PlanKey {
        &self.plan_key
    }
    pub const fn code(&self) -> &SubscriptionDiscountCode {
        &self.code
    }
    pub fn label(&self) -> Option<&str> {
        self.label.as_deref()
    }
    pub const fn kind(&self) -> SubscriptionDiscountKind {
        self.kind
    }
    pub const fn currency(&self) -> CurrencyCode {
        self.currency
    }
    pub const fn duration(&self) -> SubscriptionDiscountDuration {
        self.duration
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubscriptionDiscountCodeUpdate {
    id: DiscountCodeId,
    billing_scope_id: BillingScopeId,
    plan_key: PlanKey,
    label: Option<String>,
    status: SubscriptionDiscountCodeStatus,
    kind: SubscriptionDiscountKind,
    currency: CurrencyCode,
    duration: SubscriptionDiscountDuration,
}

impl SubscriptionDiscountCodeUpdate {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: DiscountCodeId,
        billing_scope_id: BillingScopeId,
        plan_key: PlanKey,
        label: Option<String>,
        status: SubscriptionDiscountCodeStatus,
        kind: SubscriptionDiscountKind,
        currency: CurrencyCode,
        duration: SubscriptionDiscountDuration,
    ) -> Result<Self, SubscriptionDiscountError> {
        Ok(Self {
            id,
            billing_scope_id,
            plan_key,
            label: normalize_label(label)?,
            status,
            kind,
            currency,
            duration,
        })
    }

    pub const fn id(&self) -> DiscountCodeId {
        self.id
    }
    pub const fn billing_scope_id(&self) -> BillingScopeId {
        self.billing_scope_id
    }
    pub const fn plan_key(&self) -> &PlanKey {
        &self.plan_key
    }
    pub fn label(&self) -> Option<&str> {
        self.label.as_deref()
    }
    pub const fn status(&self) -> SubscriptionDiscountCodeStatus {
        self.status
    }
    pub const fn kind(&self) -> SubscriptionDiscountKind {
        self.kind
    }
    pub const fn currency(&self) -> CurrencyCode {
        self.currency
    }
    pub const fn duration(&self) -> SubscriptionDiscountDuration {
        self.duration
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubscriptionDiscountClaim {
    id: DiscountClaimId,
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    plan_key: PlanKey,
    code: SubscriptionDiscountCode,
}

impl SubscriptionDiscountClaim {
    pub const fn new(
        id: DiscountClaimId,
        billing_scope_id: BillingScopeId,
        subscriber_id: SubscriberId,
        plan_key: PlanKey,
        code: SubscriptionDiscountCode,
    ) -> Self {
        Self {
            id,
            billing_scope_id,
            subscriber_id,
            plan_key,
            code,
        }
    }

    pub const fn id(&self) -> DiscountClaimId {
        self.id
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
    pub const fn code(&self) -> &SubscriptionDiscountCode {
        &self.code
    }
}

/// An exact subscriber-owned saved-discount removal request.
///
/// Hosts authenticate and authorize the scope, subscriber, and plan before
/// submitting this command. The command deliberately carries no discount-code
/// identity: it clears only the current saved claim for this exact aggregate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClearSubscriptionDiscount {
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    plan_key: PlanKey,
}

impl ClearSubscriptionDiscount {
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
pub struct SubscriptionDiscountClaimRecord {
    id: DiscountClaimId,
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    plan_key: PlanKey,
    discount_code_id: DiscountCodeId,
    snapshot: SubscriptionDiscountSnapshot,
    state: SubscriptionDiscountClaimState,
    claimed_at: DateTime<Utc>,
}

impl SubscriptionDiscountClaimRecord {
    /// Compatibility constructor for the historical flat lifecycle fields.
    ///
    /// New code should use [`Self::from_state`]. For a named fallible
    /// conversion from a flat representation, use [`Self::from_legacy_parts`].
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: DiscountClaimId,
        billing_scope_id: BillingScopeId,
        subscriber_id: SubscriberId,
        plan_key: PlanKey,
        discount_code_id: DiscountCodeId,
        snapshot: SubscriptionDiscountSnapshot,
        status: SubscriptionDiscountClaimStatus,
        claimed_at: DateTime<Utc>,
        applied_at: Option<DateTime<Utc>>,
        applied_subscription_id: Option<SubscriptionId>,
        applied_payment_attempt_id: Option<PaymentAttemptId>,
        superseded_at: Option<DateTime<Utc>>,
    ) -> Result<Self, SubscriptionDiscountError> {
        Self::from_legacy_parts(
            id,
            billing_scope_id,
            subscriber_id,
            plan_key,
            discount_code_id,
            snapshot,
            status,
            claimed_at,
            applied_at,
            applied_subscription_id,
            applied_payment_attempt_id,
            superseded_at,
        )
    }

    /// Builds a record from a closed lifecycle state.
    #[allow(clippy::too_many_arguments)]
    pub fn from_state(
        id: DiscountClaimId,
        billing_scope_id: BillingScopeId,
        subscriber_id: SubscriberId,
        plan_key: PlanKey,
        discount_code_id: DiscountCodeId,
        snapshot: SubscriptionDiscountSnapshot,
        state: SubscriptionDiscountClaimState,
        claimed_at: DateTime<Utc>,
    ) -> Self {
        Self {
            id,
            billing_scope_id,
            subscriber_id,
            plan_key,
            discount_code_id,
            snapshot,
            state,
            claimed_at,
        }
    }

    /// Validates historical flat lifecycle fields before constructing the typed record.
    #[allow(clippy::too_many_arguments)]
    pub fn from_legacy_parts(
        id: DiscountClaimId,
        billing_scope_id: BillingScopeId,
        subscriber_id: SubscriberId,
        plan_key: PlanKey,
        discount_code_id: DiscountCodeId,
        snapshot: SubscriptionDiscountSnapshot,
        status: SubscriptionDiscountClaimStatus,
        claimed_at: DateTime<Utc>,
        applied_at: Option<DateTime<Utc>>,
        applied_subscription_id: Option<SubscriptionId>,
        applied_payment_attempt_id: Option<PaymentAttemptId>,
        superseded_at: Option<DateTime<Utc>>,
    ) -> Result<Self, SubscriptionDiscountError> {
        let state = SubscriptionDiscountClaimState::from_legacy_parts(
            status,
            applied_at,
            applied_subscription_id,
            applied_payment_attempt_id,
            superseded_at,
        )?;
        Ok(Self::from_state(
            id,
            billing_scope_id,
            subscriber_id,
            plan_key,
            discount_code_id,
            snapshot,
            state,
            claimed_at,
        ))
    }

    pub const fn id(&self) -> DiscountClaimId {
        self.id
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
    pub const fn discount_code_id(&self) -> DiscountCodeId {
        self.discount_code_id
    }
    pub const fn snapshot(&self) -> &SubscriptionDiscountSnapshot {
        &self.snapshot
    }
    pub const fn state(&self) -> &SubscriptionDiscountClaimState {
        &self.state
    }
    pub const fn status(&self) -> SubscriptionDiscountClaimStatus {
        self.state.status()
    }
    pub const fn claimed_at(&self) -> &DateTime<Utc> {
        &self.claimed_at
    }
    pub const fn applied_at(&self) -> Option<&DateTime<Utc>> {
        match &self.state {
            SubscriptionDiscountClaimState::Applied { applied_at, .. } => Some(applied_at),
            _ => None,
        }
    }
    pub const fn applied_subscription_id(&self) -> Option<SubscriptionId> {
        match &self.state {
            SubscriptionDiscountClaimState::Applied {
                subscription_id, ..
            } => Some(*subscription_id),
            _ => None,
        }
    }
    pub const fn applied_payment_attempt_id(&self) -> Option<PaymentAttemptId> {
        match &self.state {
            SubscriptionDiscountClaimState::Applied {
                payment_attempt_id, ..
            } => Some(*payment_attempt_id),
            _ => None,
        }
    }
    pub const fn superseded_at(&self) -> Option<&DateTime<Utc>> {
        match &self.state {
            SubscriptionDiscountClaimState::Superseded { superseded_at } => Some(superseded_at),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SubscriptionDiscountClaimOutcome {
    Saved(Box<SubscriptionDiscountClaimRecord>),
    Existing(Box<SubscriptionDiscountClaimRecord>),
    BlockedBySubscription,
    BlockedByInitialAttempt,
    NotFound,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SubscriptionDiscountClearOutcome {
    Cleared(Box<SubscriptionDiscountClaimRecord>),
    NotFound,
    BlockedByInitialAttempt,
}

pub fn discounted_charge(
    base_charge: ChargeAmount,
    currency: CurrencyCode,
    kind: SubscriptionDiscountKind,
) -> Result<ChargeAmount, SubscriptionDiscountError> {
    if currency != base_charge.currency() {
        return Err(SubscriptionDiscountError::InvalidChargeSnapshot);
    }
    let discounted_cents = match kind {
        SubscriptionDiscountKind::AmountOffCents(amount) => {
            base_charge.cents().saturating_sub(amount.get())
        }
        SubscriptionDiscountKind::PercentOffBasisPoints(percent) => {
            let amount = (i64::from(base_charge.cents()) * i64::from(percent.get())) / 10_000;
            base_charge.cents().saturating_sub(
                i32::try_from(amount)
                    .map_err(|_| SubscriptionDiscountError::InvalidChargeSnapshot)?,
            )
        }
    };
    ChargeAmount::new(discounted_cents, currency)
        .map_err(|_| SubscriptionDiscountError::InvalidChargeSnapshot)
}

pub(crate) fn normalize_label(
    label: Option<String>,
) -> Result<Option<String>, SubscriptionDiscountError> {
    Ok(label.and_then(|value| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| trimmed.chars().take(120).collect())
    }))
}

#[cfg(test)]
#[path = "discount/tests.rs"]
mod tests;
