use std::{fmt, str::FromStr};

use chrono::{DateTime, Utc};
use thiserror::Error;

use crate::{
    ActorId, BillingPeriod, BillingScopeId, ChargeAmount, DiscountClaimId, PaymentMethodId,
    PlanKey, SubscriberId, SubscriptionGrantId, SubscriptionId, SubscriptionStatus,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Subscription {
    id: SubscriptionId,
    plan_key: PlanKey,
    status: SubscriptionStatus,
    payment_method_id: PaymentMethodId,
    recurring_charge: ChargeAmount,
    current_period: BillingPeriod,
    next_renewal_at: DateTime<Utc>,
}

impl Subscription {
    pub const fn new(
        id: SubscriptionId,
        plan_key: PlanKey,
        status: SubscriptionStatus,
        payment_method_id: PaymentMethodId,
        recurring_charge: ChargeAmount,
        current_period: BillingPeriod,
        next_renewal_at: DateTime<Utc>,
    ) -> Self {
        Self {
            id,
            plan_key,
            status,
            payment_method_id,
            recurring_charge,
            current_period,
            next_renewal_at,
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

    pub const fn payment_method_id(&self) -> PaymentMethodId {
        self.payment_method_id
    }

    pub const fn recurring_charge(&self) -> ChargeAmount {
        self.recurring_charge
    }

    pub const fn current_period(&self) -> &BillingPeriod {
        &self.current_period
    }

    pub const fn next_renewal_at(&self) -> &DateTime<Utc> {
        &self.next_renewal_at
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SubscriptionGrantKind {
    Testing,
    Promotion,
}

impl SubscriptionGrantKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Testing => "testing",
            Self::Promotion => "promotion",
        }
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("unknown subscription grant kind")]
pub struct SubscriptionGrantKindParseError;

impl FromStr for SubscriptionGrantKind {
    type Err = SubscriptionGrantKindParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "testing" => Ok(Self::Testing),
            "promotion" => Ok(Self::Promotion),
            _ => Err(SubscriptionGrantKindParseError),
        }
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum SubscriptionGrantError {
    #[error("subscription grant end must be after its start")]
    InvalidPeriod,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubscriptionGrant {
    id: SubscriptionGrantId,
    plan_key: PlanKey,
    kind: SubscriptionGrantKind,
    starts_at: DateTime<Utc>,
    ends_at: DateTime<Utc>,
    granted_by_actor_id: ActorId,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum SubscriptionGrantReasonError {
    #[error("subscription grant reason is empty")]
    Empty,
    #[error("subscription grant reason exceeds 500 characters")]
    TooLong,
    #[error("subscription grant reason contains raw payment card data")]
    ContainsRawCardData,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubscriptionGrantReason(String);

impl SubscriptionGrantReason {
    pub fn new(value: impl Into<String>) -> Result<Self, SubscriptionGrantReasonError> {
        let value = value.into();
        let value = value.trim();
        let length = value.chars().count();
        if length == 0 {
            return Err(SubscriptionGrantReasonError::Empty);
        }
        if length > 500 {
            return Err(SubscriptionGrantReasonError::TooLong);
        }
        if crate::string_contains_raw_card_data(value) {
            return Err(SubscriptionGrantReasonError::ContainsRawCardData);
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum SubscriptionGrantRecordError {
    #[error("subscription grant revocation fields must be all present or all absent")]
    InvalidRevocation,
    #[error("subscription grant revocation cannot precede the grant start")]
    RevocationBeforeStart,
    #[error("subscription grant update cannot precede creation")]
    UpdateBeforeCreation,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubscriptionGrantRecord {
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    grant: SubscriptionGrant,
    reason: SubscriptionGrantReason,
    revoked_at: Option<DateTime<Utc>>,
    revoked_by_actor_id: Option<ActorId>,
    revocation_reason: Option<SubscriptionGrantReason>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl SubscriptionGrantRecord {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        billing_scope_id: BillingScopeId,
        subscriber_id: SubscriberId,
        grant: SubscriptionGrant,
        reason: SubscriptionGrantReason,
        revoked_at: Option<DateTime<Utc>>,
        revoked_by_actor_id: Option<ActorId>,
        revocation_reason: Option<SubscriptionGrantReason>,
        created_at: DateTime<Utc>,
        updated_at: DateTime<Utc>,
    ) -> Result<Self, SubscriptionGrantRecordError> {
        if !matches!(
            (&revoked_at, &revoked_by_actor_id, &revocation_reason),
            (None, None, None) | (Some(_), Some(_), Some(_))
        ) {
            return Err(SubscriptionGrantRecordError::InvalidRevocation);
        }
        if revoked_at.is_some_and(|revoked_at| revoked_at < *grant.starts_at()) {
            return Err(SubscriptionGrantRecordError::RevocationBeforeStart);
        }
        if updated_at < created_at {
            return Err(SubscriptionGrantRecordError::UpdateBeforeCreation);
        }
        Ok(Self {
            billing_scope_id,
            subscriber_id,
            grant,
            reason,
            revoked_at,
            revoked_by_actor_id,
            revocation_reason,
            created_at,
            updated_at,
        })
    }

    pub const fn billing_scope_id(&self) -> BillingScopeId {
        self.billing_scope_id
    }

    pub const fn subscriber_id(&self) -> SubscriberId {
        self.subscriber_id
    }

    pub const fn grant(&self) -> &SubscriptionGrant {
        &self.grant
    }

    pub const fn reason(&self) -> &SubscriptionGrantReason {
        &self.reason
    }

    pub const fn revoked_at(&self) -> Option<&DateTime<Utc>> {
        self.revoked_at.as_ref()
    }

    pub const fn revoked_by_actor_id(&self) -> Option<ActorId> {
        self.revoked_by_actor_id
    }

    pub const fn revocation_reason(&self) -> Option<&SubscriptionGrantReason> {
        self.revocation_reason.as_ref()
    }

    pub const fn created_at(&self) -> &DateTime<Utc> {
        &self.created_at
    }

    pub const fn updated_at(&self) -> &DateTime<Utc> {
        &self.updated_at
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubscriptionGrantCreation {
    id: SubscriptionGrantId,
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    plan_key: PlanKey,
    kind: SubscriptionGrantKind,
    reason: SubscriptionGrantReason,
    ends_at: DateTime<Utc>,
    granted_by_actor_id: ActorId,
}

impl SubscriptionGrantCreation {
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        id: SubscriptionGrantId,
        billing_scope_id: BillingScopeId,
        subscriber_id: SubscriberId,
        plan_key: PlanKey,
        kind: SubscriptionGrantKind,
        reason: SubscriptionGrantReason,
        ends_at: DateTime<Utc>,
        granted_by_actor_id: ActorId,
    ) -> Self {
        Self {
            id,
            billing_scope_id,
            subscriber_id,
            plan_key,
            kind,
            reason,
            ends_at,
            granted_by_actor_id,
        }
    }

    pub const fn id(&self) -> SubscriptionGrantId {
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

    pub const fn kind(&self) -> SubscriptionGrantKind {
        self.kind
    }

    pub const fn reason(&self) -> &SubscriptionGrantReason {
        &self.reason
    }

    pub const fn ends_at(&self) -> &DateTime<Utc> {
        &self.ends_at
    }

    pub const fn granted_by_actor_id(&self) -> ActorId {
        self.granted_by_actor_id
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubscriptionGrantRevocation {
    id: SubscriptionGrantId,
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    plan_key: PlanKey,
    revoked_by_actor_id: ActorId,
    reason: SubscriptionGrantReason,
}

impl SubscriptionGrantRevocation {
    pub const fn new(
        id: SubscriptionGrantId,
        billing_scope_id: BillingScopeId,
        subscriber_id: SubscriberId,
        plan_key: PlanKey,
        revoked_by_actor_id: ActorId,
        reason: SubscriptionGrantReason,
    ) -> Self {
        Self {
            id,
            billing_scope_id,
            subscriber_id,
            plan_key,
            revoked_by_actor_id,
            reason,
        }
    }

    pub const fn id(&self) -> SubscriptionGrantId {
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

    pub const fn revoked_by_actor_id(&self) -> ActorId {
        self.revoked_by_actor_id
    }

    pub const fn reason(&self) -> &SubscriptionGrantReason {
        &self.reason
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SubscriptionGrantCreationOutcome {
    Created(Box<SubscriptionGrantRecord>),
    EndsAtNotFuture,
    CurrentPaidSubscription,
    ActiveGrant,
    BlockingInitialAttempt,
    PendingApprovedProcessorEvidence,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SubscriptionGrantRevocationOutcome {
    Revoked(Box<SubscriptionGrantRecord>),
    AlreadyRevoked(Box<SubscriptionGrantRecord>),
    NotFound,
    Expired,
}

impl SubscriptionGrant {
    pub fn new(
        id: SubscriptionGrantId,
        plan_key: PlanKey,
        kind: SubscriptionGrantKind,
        starts_at: DateTime<Utc>,
        ends_at: DateTime<Utc>,
        granted_by_actor_id: ActorId,
    ) -> Result<Self, SubscriptionGrantError> {
        if ends_at <= starts_at {
            return Err(SubscriptionGrantError::InvalidPeriod);
        }
        Ok(Self {
            id,
            plan_key,
            kind,
            starts_at,
            ends_at,
            granted_by_actor_id,
        })
    }

    pub const fn id(&self) -> SubscriptionGrantId {
        self.id
    }

    pub const fn plan_key(&self) -> &PlanKey {
        &self.plan_key
    }

    pub const fn kind(&self) -> SubscriptionGrantKind {
        self.kind
    }

    pub const fn starts_at(&self) -> &DateTime<Utc> {
        &self.starts_at
    }

    pub const fn ends_at(&self) -> &DateTime<Utc> {
        &self.ends_at
    }

    pub const fn granted_by_actor_id(&self) -> ActorId {
        self.granted_by_actor_id
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SubscriptionDiscountKind {
    AmountOffCents(PositiveDiscountCents),
    PercentOffBasisPoints(PercentOffBasisPoints),
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PositiveDiscountCents(i32);

impl PositiveDiscountCents {
    pub fn new(value: i32) -> Result<Self, SubscriptionDiscountError> {
        if value <= 0 {
            return Err(SubscriptionDiscountError::InvalidAmountOff);
        }
        Ok(Self(value))
    }

    pub const fn get(self) -> i32 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PercentOffBasisPoints(u16);

impl PercentOffBasisPoints {
    pub fn new(value: u16) -> Result<Self, SubscriptionDiscountError> {
        if !(1..=9_999).contains(&value) {
            return Err(SubscriptionDiscountError::InvalidPercentOff);
        }
        Ok(Self(value))
    }

    pub const fn get(self) -> u16 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SubscriptionDiscountDuration {
    Indefinite,
    LimitedMonths(LimitedDiscountMonths),
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct LimitedDiscountMonths(u8);

impl LimitedDiscountMonths {
    pub fn new(value: u8) -> Result<Self, SubscriptionDiscountError> {
        if !(1..=36).contains(&value) {
            return Err(SubscriptionDiscountError::InvalidDurationMonths);
        }
        Ok(Self(value))
    }

    pub const fn get(self) -> u8 {
        self.0
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum SubscriptionDiscountError {
    #[error("subscription discount code must contain 5 through 64 supported characters")]
    InvalidCode,
    #[error("subscription amount discount must be positive")]
    InvalidAmountOff,
    #[error("subscription percentage discount must be between 1 and 9999 basis points")]
    InvalidPercentOff,
    #[error("limited subscription discount must contain 1 through 36 months")]
    InvalidDurationMonths,
    #[error("discounted charge must use the base currency and cannot exceed the base charge")]
    InvalidChargeSnapshot,
}

#[derive(Clone, Eq, Hash, PartialEq)]
pub struct SubscriptionDiscountCode(String);

impl SubscriptionDiscountCode {
    pub fn new(value: &str) -> Result<Self, SubscriptionDiscountError> {
        let normalized = value.trim().to_ascii_uppercase();
        if !(5..=64).contains(&normalized.len())
            || !normalized
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(SubscriptionDiscountError::InvalidCode);
        }
        Ok(Self(normalized))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SubscriptionDiscountCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SubscriptionDiscountCode([redacted])")
    }
}

impl fmt::Display for SubscriptionDiscountCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[redacted]")
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubscriptionDiscountSnapshot {
    code: SubscriptionDiscountCode,
    label: Option<String>,
    kind: SubscriptionDiscountKind,
    duration: SubscriptionDiscountDuration,
    base_charge: ChargeAmount,
    discounted_charge: ChargeAmount,
}

impl SubscriptionDiscountSnapshot {
    pub fn new(
        code: SubscriptionDiscountCode,
        label: Option<String>,
        kind: SubscriptionDiscountKind,
        duration: SubscriptionDiscountDuration,
        base_charge: ChargeAmount,
        discounted_charge: ChargeAmount,
    ) -> Result<Self, SubscriptionDiscountError> {
        if base_charge.currency() != discounted_charge.currency()
            || discounted_charge.cents() > base_charge.cents()
        {
            return Err(SubscriptionDiscountError::InvalidChargeSnapshot);
        }
        Ok(Self {
            code,
            label,
            kind,
            duration,
            base_charge,
            discounted_charge,
        })
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

    pub const fn duration(&self) -> SubscriptionDiscountDuration {
        self.duration
    }

    pub const fn base_charge(&self) -> ChargeAmount {
        self.base_charge
    }

    pub const fn discounted_charge(&self) -> ChargeAmount {
        self.discounted_charge
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SavedSubscriptionDiscount {
    claim_id: DiscountClaimId,
    snapshot: SubscriptionDiscountSnapshot,
}

impl SavedSubscriptionDiscount {
    pub const fn new(claim_id: DiscountClaimId, snapshot: SubscriptionDiscountSnapshot) -> Self {
        Self { claim_id, snapshot }
    }

    pub const fn claim_id(&self) -> DiscountClaimId {
        self.claim_id
    }

    pub const fn snapshot(&self) -> &SubscriptionDiscountSnapshot {
        &self.snapshot
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppliedSubscriptionDiscount {
    claim_id: Option<DiscountClaimId>,
    snapshot: SubscriptionDiscountSnapshot,
    periods_remaining: Option<u8>,
}

impl AppliedSubscriptionDiscount {
    pub fn new(
        claim_id: Option<DiscountClaimId>,
        snapshot: SubscriptionDiscountSnapshot,
        periods_remaining: Option<u8>,
    ) -> Result<Self, SubscriptionDiscountError> {
        match (snapshot.duration(), periods_remaining) {
            (SubscriptionDiscountDuration::Indefinite, None) => {}
            (SubscriptionDiscountDuration::LimitedMonths(total), Some(remaining))
                if remaining < total.get() => {}
            _ => return Err(SubscriptionDiscountError::InvalidDurationMonths),
        }
        Ok(Self {
            claim_id,
            snapshot,
            periods_remaining,
        })
    }

    pub const fn claim_id(&self) -> Option<DiscountClaimId> {
        self.claim_id
    }

    pub const fn snapshot(&self) -> &SubscriptionDiscountSnapshot {
        &self.snapshot
    }

    pub const fn periods_remaining(&self) -> Option<u8> {
        self.periods_remaining
    }
}

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
    PastDue {
        subscription: Subscription,
        next_action: PastDueAction,
        applied_discount: Option<AppliedSubscriptionDiscount>,
    },
    Granted {
        grant: SubscriptionGrant,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EntitlementQuery {
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    plan_key: PlanKey,
}

impl EntitlementQuery {
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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BillingDeletionBlockers {
    active_subscription: bool,
    unresolved_payment: bool,
}

impl BillingDeletionBlockers {
    pub const fn new(active_subscription: bool, unresolved_payment: bool) -> Self {
        Self {
            active_subscription,
            unresolved_payment,
        }
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

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use uuid::Uuid;

    use super::*;
    use crate::{CurrencyCode, MoneyError};

    #[test]
    fn canonical_discount_code_has_one_probe_identity() {
        let code = SubscriptionDiscountCode::new(" summer-25 ").unwrap();
        assert_eq!(code.as_str(), "SUMMER-25");
        assert_eq!(
            SubscriptionDiscountCode::new("bad code"),
            Err(SubscriptionDiscountError::InvalidCode)
        );
        assert!(!format!("{code:?}").contains("SUMMER"));
    }

    #[test]
    fn discount_snapshot_rejects_cross_currency_or_increased_price() {
        let usd = CurrencyCode::new("USD").unwrap();
        let eur = CurrencyCode::new("EUR").unwrap();
        let code = SubscriptionDiscountCode::new("SAVE10").unwrap();
        let kind = SubscriptionDiscountKind::PercentOffBasisPoints(
            PercentOffBasisPoints::new(1_000).unwrap(),
        );
        assert_eq!(
            SubscriptionDiscountSnapshot::new(
                code.clone(),
                None,
                kind,
                SubscriptionDiscountDuration::Indefinite,
                ChargeAmount::new(1_000, usd).unwrap(),
                ChargeAmount::new(900, eur).unwrap(),
            ),
            Err(SubscriptionDiscountError::InvalidChargeSnapshot)
        );
        assert_eq!(
            SubscriptionDiscountSnapshot::new(
                code,
                None,
                kind,
                SubscriptionDiscountDuration::Indefinite,
                ChargeAmount::new(1_000, usd).unwrap(),
                ChargeAmount::new(1_001, usd).unwrap(),
            ),
            Err(SubscriptionDiscountError::InvalidChargeSnapshot)
        );
        assert_eq!(
            ChargeAmount::new(0, usd),
            Err(MoneyError::NonPositiveCharge)
        );
    }

    #[test]
    fn grant_period_is_valid_by_construction() {
        let starts = Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap();
        let id = SubscriptionGrantId::new(Uuid::from_u128(1));
        let actor = ActorId::new(Uuid::from_u128(2));
        assert_eq!(
            SubscriptionGrant::new(
                id,
                PlanKey::new("plan").unwrap(),
                SubscriptionGrantKind::Testing,
                starts,
                starts,
                actor,
            ),
            Err(SubscriptionGrantError::InvalidPeriod)
        );
    }

    #[test]
    fn grant_reason_is_trimmed_bounded_and_card_safe() {
        assert_eq!(
            SubscriptionGrantReason::new("  launch partner  ")
                .unwrap()
                .as_str(),
            "launch partner"
        );
        assert_eq!(
            SubscriptionGrantReason::new("   "),
            Err(SubscriptionGrantReasonError::Empty)
        );
        assert_eq!(
            SubscriptionGrantReason::new("x".repeat(501)),
            Err(SubscriptionGrantReasonError::TooLong)
        );
        assert_eq!(
            SubscriptionGrantReason::new("customer supplied 4242 4242 4242 4242"),
            Err(SubscriptionGrantReasonError::ContainsRawCardData)
        );
    }

    #[test]
    fn entitlement_variants_cannot_mix_grant_and_paid_owners() {
        let starts = Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap();
        let grant = SubscriptionGrant::new(
            SubscriptionGrantId::new(Uuid::from_u128(1)),
            PlanKey::new("plan").unwrap(),
            SubscriptionGrantKind::Promotion,
            starts,
            starts + chrono::Duration::days(30),
            ActorId::new(Uuid::from_u128(2)),
        )
        .unwrap();
        assert!(matches!(
            Entitlement::Granted { grant },
            Entitlement::Granted { .. }
        ));
    }
}
