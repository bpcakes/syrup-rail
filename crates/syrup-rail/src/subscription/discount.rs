use super::*;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SubscriptionDiscountKind {
    AmountOffCents(PositiveDiscountCents),
    PercentOffBasisPoints(PercentOffBasisPoints),
}

impl SubscriptionDiscountKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AmountOffCents(_) => "amount_off",
            Self::PercentOffBasisPoints(_) => "percent_off",
        }
    }
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

impl SubscriptionDiscountDuration {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Indefinite => "indefinite",
            Self::LimitedMonths(_) => "limited_months",
        }
    }
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
    #[error("limited-month discounts require a one-calendar-month recurring period")]
    LimitedDiscountCadence,
    #[error("discounted charge must use the base currency and cannot exceed the base charge")]
    InvalidChargeSnapshot,
    #[error("subscription discount persisted state is invalid")]
    InvalidState,
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

    pub const fn currency(&self) -> &CurrencyCode {
        self.base_charge.currency_code()
    }

    /// Compares the customer-accepted economic terms of two snapshots.
    ///
    /// The optional label is presentation metadata captured durably for later
    /// display, not part of the amount, duration, or discount identity accepted
    /// at checkout.
    pub fn has_same_charge_terms(&self, other: &Self) -> bool {
        self.code == other.code
            && self.kind == other.kind
            && self.duration == other.duration
            && self.base_charge == other.base_charge
            && self.discounted_charge == other.discounted_charge
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
                if remaining <= total.get() => {}
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
