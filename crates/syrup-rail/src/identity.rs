use std::{fmt, str::FromStr};

use thiserror::Error;
use uuid::Uuid;

macro_rules! uuid_id {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(Uuid);

        impl $name {
            pub const fn new(value: Uuid) -> Self {
                Self(value)
            }

            pub const fn as_uuid(&self) -> &Uuid {
                &self.0
            }

            pub const fn into_uuid(self) -> Uuid {
                self.0
            }
        }

        impl From<Uuid> for $name {
            fn from(value: Uuid) -> Self {
                Self::new(value)
            }
        }

        impl From<$name> for Uuid {
            fn from(value: $name) -> Self {
                value.into_uuid()
            }
        }

        impl FromStr for $name {
            type Err = uuid::Error;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(value).map(Self::new)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

uuid_id!(BillingScopeId);
uuid_id!(SubscriberId);
uuid_id!(SubscriptionId);
uuid_id!(PaymentMethodId);
uuid_id!(PaymentAttemptId);
uuid_id!(HostChargeTargetId);
uuid_id!(GatewayAccountId);
uuid_id!(GatewayConfigurationId);
uuid_id!(SubscriptionGrantId);
uuid_id!(DiscountCodeId);
uuid_id!(DiscountClaimId);
uuid_id!(ActorId);

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum IdempotencyKeyError {
    #[error("idempotency key is empty")]
    Empty,
    #[error("idempotency key exceeds 128 bytes")]
    TooLong,
    #[error("idempotency key contains an unsupported character")]
    InvalidCharacter,
}

#[derive(Clone, Eq, Hash, PartialEq)]
pub struct IdempotencyKey(String);

impl IdempotencyKey {
    pub fn new(value: impl Into<String>) -> Result<Self, IdempotencyKeyError> {
        let value = value.into();
        let value = value.trim();
        if value.is_empty() {
            return Err(IdempotencyKeyError::Empty);
        }
        if value.len() > 128 {
            return Err(IdempotencyKeyError::TooLong);
        }
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-'))
        {
            return Err(IdempotencyKeyError::InvalidCharacter);
        }
        Ok(Self(value.to_owned()))
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for IdempotencyKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("IdempotencyKey([redacted])")
    }
}

impl fmt::Display for IdempotencyKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[redacted]")
    }
}

const MAX_SLUG_BYTES: usize = 64;

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum SlugError {
    #[error("slug is empty")]
    Empty,
    #[error("slug exceeds 64 bytes")]
    TooLong,
    #[error("slug contains an unsupported character")]
    InvalidCharacter,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct BoundedSlug(String);

impl BoundedSlug {
    fn parse(value: impl Into<String>) -> Result<Self, SlugError> {
        let value = value.into();
        if value.is_empty() {
            return Err(SlugError::Empty);
        }
        if value.len() > MAX_SLUG_BYTES {
            return Err(SlugError::TooLong);
        }
        let mut bytes = value.bytes();
        let first = bytes.next().expect("nonempty slug has a first byte");
        if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
            return Err(SlugError::InvalidCharacter);
        }
        if !bytes.all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        }) {
            return Err(SlugError::InvalidCharacter);
        }
        Ok(Self(value))
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}

macro_rules! slug_id {
    ($name:ident) => {
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(BoundedSlug);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, SlugError> {
                BoundedSlug::parse(value).map(Self)
            }

            pub fn as_str(&self) -> &str {
                self.0.as_str()
            }
        }

        impl FromStr for $name {
            type Err = SlugError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }

        impl TryFrom<String> for $name {
            type Error = SlugError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }
    };
}

slug_id!(PlanKey);
slug_id!(GatewayProviderKey);
slug_id!(GatewayLifecycleCursorKey);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PaymentAttemptKind {
    HostCharge,
    SubscriptionInitial,
    SubscriptionRenewal,
    SubscriptionRecovery,
    SubscriptionPaymentMethodUpdate,
}

impl PaymentAttemptKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HostCharge => "host_charge",
            Self::SubscriptionInitial => "subscription_initial",
            Self::SubscriptionRenewal => "subscription_renewal",
            Self::SubscriptionRecovery => "subscription_recovery",
            Self::SubscriptionPaymentMethodUpdate => "subscription_payment_method_update",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PaymentAttemptStatus {
    Pending,
    Approved,
    Declined,
    Unknown,
    ReviewRequired,
    Failed,
}

impl PaymentAttemptStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Declined => "declined",
            Self::Unknown => "unknown",
            Self::ReviewRequired => "review_required",
            Self::Failed => "failed",
        }
    }

    pub const fn is_resolvable(self) -> bool {
        matches!(self, Self::Pending | Self::Unknown | Self::ReviewRequired)
    }

    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Approved | Self::Declined | Self::Failed)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PaymentMethodStatus {
    Active,
    Disabled,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SubscriptionStatus {
    Active,
    PastDue,
    Canceled,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_slugs_match_the_schema_contract() {
        for valid in ["a", "base_subscription", "nmi", "p-1", &"a".repeat(64)] {
            assert_eq!(PlanKey::new(valid).unwrap().as_str(), valid);
        }
        for invalid in ["", "A", "-a", "a.b", "a:b", "a b", &"a".repeat(65)] {
            assert!(PlanKey::new(invalid).is_err(), "accepted {invalid:?}");
        }
    }

    #[test]
    fn neutral_attempt_names_are_closed() {
        assert_eq!(PaymentAttemptKind::HostCharge.as_str(), "host_charge");
        assert_eq!(
            PaymentAttemptKind::SubscriptionPaymentMethodUpdate.as_str(),
            "subscription_payment_method_update"
        );
    }

    #[test]
    fn idempotency_key_matches_the_existing_wire_boundary() {
        let key = IdempotencyKey::new(" renewal:sub_123.4-5 ").unwrap();
        assert_eq!(key.expose(), "renewal:sub_123.4-5");
        assert_eq!(IdempotencyKey::new(" "), Err(IdempotencyKeyError::Empty));
        assert_eq!(
            IdempotencyKey::new("bad/key"),
            Err(IdempotencyKeyError::InvalidCharacter)
        );
        assert!(!format!("{key:?}").contains("renewal"));
    }
}
