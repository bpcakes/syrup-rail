use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PaymentAttemptTimestamps {
    submitted_at: Option<DateTime<Utc>>,
    resolved_at: Option<DateTime<Utc>>,
    review_required_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl PaymentAttemptTimestamps {
    pub const fn new(
        submitted_at: Option<DateTime<Utc>>,
        resolved_at: Option<DateTime<Utc>>,
        review_required_at: Option<DateTime<Utc>>,
        created_at: DateTime<Utc>,
        updated_at: DateTime<Utc>,
    ) -> Self {
        Self {
            submitted_at,
            resolved_at,
            review_required_at,
            created_at,
            updated_at,
        }
    }

    pub const fn submitted_at(self) -> Option<DateTime<Utc>> {
        self.submitted_at
    }

    pub const fn resolved_at(self) -> Option<DateTime<Utc>> {
        self.resolved_at
    }

    pub const fn review_required_at(self) -> Option<DateTime<Utc>> {
        self.review_required_at
    }

    pub const fn created_at(self) -> DateTime<Utc> {
        self.created_at
    }

    pub const fn updated_at(self) -> DateTime<Utc> {
        self.updated_at
    }

    pub const fn submitted_or_created_at(self) -> DateTime<Utc> {
        match self.submitted_at {
            Some(submitted_at) => submitted_at,
            None => self.created_at,
        }
    }
}

/// Durable provider-neutral contact metadata attached to an attempt.
///
/// First and last name remain separate because this value participates in
/// immutable replay identity and provider request reconstruction. `name` is a
/// derived receipt/support projection only. Ordinary formatting remains
/// value-free.
#[derive(Clone, Eq, PartialEq)]
pub struct BillingContactSnapshot {
    first_name: Option<String>,
    last_name: Option<String>,
    name: Option<String>,
    email: Option<String>,
}

impl BillingContactSnapshot {
    /// Builds a canonical snapshot from a historical combined display name.
    ///
    /// The combined name becomes `first_name` and `last_name` remains absent.
    /// New command and persistence boundaries should prefer
    /// [`Self::from_parts`] or [`Self::from_billing_contact`].
    pub fn new(name: Option<String>, email: Option<String>) -> Self {
        Self::from_parts(name, None, email)
    }

    pub fn from_parts(
        first_name: Option<String>,
        last_name: Option<String>,
        email: Option<String>,
    ) -> Self {
        let first_name = normalize_optional(first_name);
        let last_name = normalize_optional(last_name);
        let name = [first_name.as_deref(), last_name.as_deref()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ");
        Self {
            first_name,
            last_name,
            name: (!name.is_empty()).then_some(name),
            email: normalize_optional(email),
        }
    }

    pub fn from_billing_contact(contact: &BillingContact) -> Self {
        Self::from_parts(
            contact.first_name().map(ToOwned::to_owned),
            contact.last_name().map(ToOwned::to_owned),
            contact.email().map(ToOwned::to_owned),
        )
    }

    pub fn first_name(&self) -> Option<&str> {
        self.first_name.as_deref()
    }

    pub fn last_name(&self) -> Option<&str> {
        self.last_name.as_deref()
    }

    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    pub fn email(&self) -> Option<&str> {
        self.email.as_deref()
    }

    pub const fn is_empty(&self) -> bool {
        self.first_name.is_none() && self.last_name.is_none() && self.email.is_none()
    }
}

impl fmt::Debug for BillingContactSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BillingContactSnapshot")
            .field("has_first_name", &self.first_name.is_some())
            .field("has_last_name", &self.last_name.is_some())
            .field("has_name", &self.name.is_some())
            .field("has_email", &self.email.is_some())
            .finish()
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum PaymentAttemptSnapshotError {
    #[error("subscription payment-state snapshot cannot use a terminal subscription")]
    TerminalSubscription,
    #[error("subscription enrollment terms version must be 1 or 2")]
    InvalidEnrollmentTermsVersion,
}

#[derive(Clone, Eq, PartialEq)]
pub struct PaymentMethodUpdateSnapshot {
    subscription_id: SubscriptionId,
    payment_method_id: PaymentMethodId,
    expected_initial_transaction_id: GatewayTransactionId,
}

impl PaymentMethodUpdateSnapshot {
    pub const fn new(
        subscription_id: SubscriptionId,
        payment_method_id: PaymentMethodId,
        expected_initial_transaction_id: GatewayTransactionId,
    ) -> Self {
        Self {
            subscription_id,
            payment_method_id,
            expected_initial_transaction_id,
        }
    }

    pub const fn subscription_id(&self) -> SubscriptionId {
        self.subscription_id
    }

    pub const fn payment_method_id(&self) -> PaymentMethodId {
        self.payment_method_id
    }

    pub const fn expected_initial_transaction_id(&self) -> &GatewayTransactionId {
        &self.expected_initial_transaction_id
    }
}

impl fmt::Debug for PaymentMethodUpdateSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PaymentMethodUpdateSnapshot")
            .field("subscription_id", &self.subscription_id)
            .field("payment_method_id", &self.payment_method_id)
            .field("has_expected_initial_transaction_id", &true)
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct SubscriptionPaymentStateSnapshot {
    subscription_id: SubscriptionId,
    payment_method_id: PaymentMethodId,
    initial_transaction_id: GatewayTransactionId,
    status: SubscriptionStatus,
}

impl SubscriptionPaymentStateSnapshot {
    pub fn new(
        subscription_id: SubscriptionId,
        payment_method_id: PaymentMethodId,
        initial_transaction_id: GatewayTransactionId,
        status: SubscriptionStatus,
    ) -> Result<Self, PaymentAttemptSnapshotError> {
        if matches!(
            status,
            SubscriptionStatus::Canceled | SubscriptionStatus::Unpaid
        ) {
            return Err(PaymentAttemptSnapshotError::TerminalSubscription);
        }
        Ok(Self {
            subscription_id,
            payment_method_id,
            initial_transaction_id,
            status,
        })
    }

    pub const fn subscription_id(&self) -> SubscriptionId {
        self.subscription_id
    }

    pub const fn payment_method_id(&self) -> PaymentMethodId {
        self.payment_method_id
    }

    pub const fn initial_transaction_id(&self) -> &GatewayTransactionId {
        &self.initial_transaction_id
    }

    pub const fn status(&self) -> SubscriptionStatus {
        self.status
    }
}

impl fmt::Debug for SubscriptionPaymentStateSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SubscriptionPaymentStateSnapshot")
            .field("subscription_id", &self.subscription_id)
            .field("payment_method_id", &self.payment_method_id)
            .field("has_initial_transaction_id", &true)
            .field("status", &self.status)
            .finish()
    }
}
