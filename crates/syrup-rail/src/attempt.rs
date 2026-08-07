use std::fmt;

use chrono::{DateTime, Utc};
use thiserror::Error;

use crate::{
    BillingPeriod, BillingScopeId, GatewayAccountId, GatewayConfigurationId, GatewayDiagnostic,
    GatewayLifecycleState, GatewayOrderId, GatewayTransactionId, HostChargeTargetId,
    IdempotencyKey, Money, PaymentAttemptId, PaymentAttemptKind, PaymentAttemptStatus,
    PaymentMethodId, PaymentResolutionCode, PlanKey, ProcessorEvidence, SubscriberId,
    SubscriptionEnrollmentDiscountSnapshot, SubscriptionId, SubscriptionStatus,
};

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum PaymentAttemptFingerprintError {
    #[error("payment attempt fingerprint is empty")]
    Empty,
}

/// Opaque durable equality key for one payment request.
///
/// Fingerprints contain only canonical billing identities and economic/state
/// snapshots. They never contain a payment token, credential, or raw billing
/// contact. Ordinary formatting is redacted so callers must opt into exposing
/// the value at the persistence boundary.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct PaymentAttemptFingerprint(String);

impl PaymentAttemptFingerprint {
    pub fn new(value: impl Into<String>) -> Result<Self, PaymentAttemptFingerprintError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(PaymentAttemptFingerprintError::Empty);
        }
        Ok(Self(value))
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for PaymentAttemptFingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PaymentAttemptFingerprint([redacted])")
    }
}

impl fmt::Display for PaymentAttemptFingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[redacted]")
    }
}

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

/// Durable contact metadata attached to an attempt.
///
/// The command-side [`crate::BillingContact`] remains the provider-neutral
/// structured input. This snapshot mirrors the deliberately smaller durable
/// projection used for receipts and support, and keeps ordinary formatting
/// value-free.
#[derive(Clone, Eq, PartialEq)]
pub struct BillingContactSnapshot {
    name: Option<String>,
    email: Option<String>,
}

impl BillingContactSnapshot {
    pub fn new(name: Option<String>, email: Option<String>) -> Self {
        Self {
            name: normalize_optional(name),
            email: normalize_optional(email),
        }
    }

    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    pub fn email(&self) -> Option<&str> {
        self.email.as_deref()
    }

    pub const fn is_empty(&self) -> bool {
        self.name.is_none() && self.email.is_none()
    }
}

impl fmt::Debug for BillingContactSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BillingContactSnapshot")
            .field("has_name", &self.name.is_some())
            .field("has_email", &self.email.is_some())
            .finish()
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum PaymentAttemptSnapshotError {
    #[error("subscription payment-state snapshot cannot use a canceled subscription")]
    CanceledSubscription,
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
        if status == SubscriptionStatus::Canceled {
            return Err(PaymentAttemptSnapshotError::CanceledSubscription);
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

/// Initial-enrollment rows gain application identities only after provider
/// submission has produced a payment method and the host has attempted its
/// atomic subscription projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SubscriptionInitialApplication {
    subscription_id: Option<SubscriptionId>,
    payment_method_id: PaymentMethodId,
}

impl SubscriptionInitialApplication {
    pub const fn new(
        subscription_id: Option<SubscriptionId>,
        payment_method_id: PaymentMethodId,
    ) -> Self {
        Self {
            subscription_id,
            payment_method_id,
        }
    }

    pub const fn subscription_id(self) -> Option<SubscriptionId> {
        self.subscription_id
    }

    pub const fn payment_method_id(self) -> PaymentMethodId {
        self.payment_method_id
    }
}

/// The mutually exclusive business target and optimistic snapshot for an
/// attempt. This replaces the persistence table's nullable-column bag at the
/// domain boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PaymentAttemptTarget {
    HostCharge {
        target_id: HostChargeTargetId,
    },
    SubscriptionInitial {
        plan_key: PlanKey,
        discount: Option<SubscriptionEnrollmentDiscountSnapshot>,
        application: Option<SubscriptionInitialApplication>,
    },
    SubscriptionRenewal {
        plan_key: PlanKey,
        payment_method_id: PaymentMethodId,
        period: BillingPeriod,
        expected_state: SubscriptionPaymentStateSnapshot,
    },
    SubscriptionRecovery {
        plan_key: PlanKey,
        payment_method_id: PaymentMethodId,
        period: BillingPeriod,
        expected_state: SubscriptionPaymentStateSnapshot,
    },
    SubscriptionPaymentMethodUpdate {
        plan_key: PlanKey,
        payment_method_id: PaymentMethodId,
        expected_state: PaymentMethodUpdateSnapshot,
    },
}

impl PaymentAttemptTarget {
    pub const fn kind(&self) -> PaymentAttemptKind {
        match self {
            Self::HostCharge { .. } => PaymentAttemptKind::HostCharge,
            Self::SubscriptionInitial { .. } => PaymentAttemptKind::SubscriptionInitial,
            Self::SubscriptionRenewal { .. } => PaymentAttemptKind::SubscriptionRenewal,
            Self::SubscriptionRecovery { .. } => PaymentAttemptKind::SubscriptionRecovery,
            Self::SubscriptionPaymentMethodUpdate { .. } => {
                PaymentAttemptKind::SubscriptionPaymentMethodUpdate
            }
        }
    }

    pub const fn plan_key(&self) -> Option<&PlanKey> {
        match self {
            Self::HostCharge { .. } => None,
            Self::SubscriptionInitial { plan_key, .. }
            | Self::SubscriptionRenewal { plan_key, .. }
            | Self::SubscriptionRecovery { plan_key, .. }
            | Self::SubscriptionPaymentMethodUpdate { plan_key, .. } => Some(plan_key),
        }
    }

    pub const fn host_charge_target_id(&self) -> Option<HostChargeTargetId> {
        match self {
            Self::HostCharge { target_id } => Some(*target_id),
            _ => None,
        }
    }

    pub const fn subscription_id(&self) -> Option<SubscriptionId> {
        match self {
            Self::SubscriptionInitial {
                application: Some(application),
                ..
            } => application.subscription_id(),
            Self::SubscriptionRenewal { expected_state, .. }
            | Self::SubscriptionRecovery { expected_state, .. } => {
                Some(expected_state.subscription_id())
            }
            Self::SubscriptionPaymentMethodUpdate { expected_state, .. } => {
                Some(expected_state.subscription_id())
            }
            Self::HostCharge { .. } | Self::SubscriptionInitial { .. } => None,
        }
    }

    pub const fn payment_method_id(&self) -> Option<PaymentMethodId> {
        match self {
            Self::SubscriptionInitial {
                application: Some(application),
                ..
            } => Some(application.payment_method_id()),
            Self::SubscriptionRenewal {
                payment_method_id, ..
            }
            | Self::SubscriptionRecovery {
                payment_method_id, ..
            }
            | Self::SubscriptionPaymentMethodUpdate {
                payment_method_id, ..
            } => Some(*payment_method_id),
            Self::HostCharge { .. } | Self::SubscriptionInitial { .. } => None,
        }
    }

    pub const fn period(&self) -> Option<&BillingPeriod> {
        match self {
            Self::SubscriptionRenewal { period, .. }
            | Self::SubscriptionRecovery { period, .. } => Some(period),
            _ => None,
        }
    }

    pub const fn enrollment_discount(&self) -> Option<&SubscriptionEnrollmentDiscountSnapshot> {
        match self {
            Self::SubscriptionInitial { discount, .. } => discount.as_ref(),
            _ => None,
        }
    }

    pub const fn initial_application(&self) -> Option<SubscriptionInitialApplication> {
        match self {
            Self::SubscriptionInitial { application, .. } => *application,
            _ => None,
        }
    }

    pub const fn payment_method_update_snapshot(&self) -> Option<&PaymentMethodUpdateSnapshot> {
        match self {
            Self::SubscriptionPaymentMethodUpdate { expected_state, .. } => Some(expected_state),
            _ => None,
        }
    }

    pub const fn subscription_payment_state_snapshot(
        &self,
    ) -> Option<&SubscriptionPaymentStateSnapshot> {
        match self {
            Self::SubscriptionRenewal { expected_state, .. }
            | Self::SubscriptionRecovery { expected_state, .. } => Some(expected_state),
            _ => None,
        }
    }
}

fn normalize_optional(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let value = value.trim();
        (!value.is_empty()).then(|| value.to_owned())
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PaymentAttemptIdentity {
    attempt_id: PaymentAttemptId,
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    gateway_account_id: GatewayAccountId,
    gateway_configuration_id: GatewayConfigurationId,
}

impl PaymentAttemptIdentity {
    pub const fn new(
        attempt_id: PaymentAttemptId,
        billing_scope_id: BillingScopeId,
        subscriber_id: SubscriberId,
        gateway_account_id: GatewayAccountId,
        gateway_configuration_id: GatewayConfigurationId,
    ) -> Self {
        Self {
            attempt_id,
            billing_scope_id,
            subscriber_id,
            gateway_account_id,
            gateway_configuration_id,
        }
    }

    pub const fn attempt_id(self) -> PaymentAttemptId {
        self.attempt_id
    }

    pub const fn billing_scope_id(self) -> BillingScopeId {
        self.billing_scope_id
    }

    pub const fn subscriber_id(self) -> SubscriberId {
        self.subscriber_id
    }

    pub const fn gateway_account_id(self) -> GatewayAccountId {
        self.gateway_account_id
    }

    pub const fn gateway_configuration_id(self) -> GatewayConfigurationId {
        self.gateway_configuration_id
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct PaymentAttemptRequest {
    target: PaymentAttemptTarget,
    idempotency_key: IdempotencyKey,
    fingerprint: PaymentAttemptFingerprint,
    amount: Money,
    gateway_order_id: GatewayOrderId,
    billing_contact: BillingContactSnapshot,
}

impl PaymentAttemptRequest {
    pub const fn new(
        target: PaymentAttemptTarget,
        idempotency_key: IdempotencyKey,
        fingerprint: PaymentAttemptFingerprint,
        amount: Money,
        gateway_order_id: GatewayOrderId,
        billing_contact: BillingContactSnapshot,
    ) -> Self {
        Self {
            target,
            idempotency_key,
            fingerprint,
            amount,
            gateway_order_id,
            billing_contact,
        }
    }

    pub const fn target(&self) -> &PaymentAttemptTarget {
        &self.target
    }

    pub const fn idempotency_key(&self) -> &IdempotencyKey {
        &self.idempotency_key
    }

    pub const fn fingerprint(&self) -> &PaymentAttemptFingerprint {
        &self.fingerprint
    }

    pub const fn amount(&self) -> Money {
        self.amount
    }

    pub const fn gateway_order_id(&self) -> &GatewayOrderId {
        &self.gateway_order_id
    }

    pub const fn billing_contact(&self) -> &BillingContactSnapshot {
        &self.billing_contact
    }
}

impl fmt::Debug for PaymentAttemptRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PaymentAttemptRequest")
            .field("target", &self.target)
            .field("has_idempotency_key", &true)
            .field("has_fingerprint", &true)
            .field("amount", &self.amount)
            .field("has_gateway_order_id", &true)
            .field("billing_contact", &self.billing_contact)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaymentAttemptLifecycle {
    state: GatewayLifecycleState,
    action: Option<GatewayDiagnostic>,
    effective_at: Option<DateTime<Utc>>,
    reconciled_at: Option<DateTime<Utc>>,
}

impl PaymentAttemptLifecycle {
    pub const fn new(
        state: GatewayLifecycleState,
        action: Option<GatewayDiagnostic>,
        effective_at: Option<DateTime<Utc>>,
        reconciled_at: Option<DateTime<Utc>>,
    ) -> Self {
        Self {
            state,
            action,
            effective_at,
            reconciled_at,
        }
    }

    pub const fn state(&self) -> &GatewayLifecycleState {
        &self.state
    }

    pub const fn action(&self) -> Option<&GatewayDiagnostic> {
        self.action.as_ref()
    }

    pub const fn effective_at(&self) -> Option<DateTime<Utc>> {
        self.effective_at
    }

    pub const fn reconciled_at(&self) -> Option<DateTime<Utc>> {
        self.reconciled_at
    }
}

impl Default for PaymentAttemptLifecycle {
    fn default() -> Self {
        Self::new(GatewayLifecycleState::Unknown, None, None, None)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaymentAttemptState {
    status: PaymentAttemptStatus,
    resolution_code: Option<PaymentResolutionCode>,
    processor_evidence: ProcessorEvidence,
    lifecycle: PaymentAttemptLifecycle,
    timestamps: PaymentAttemptTimestamps,
}

impl PaymentAttemptState {
    pub const fn new(
        status: PaymentAttemptStatus,
        resolution_code: Option<PaymentResolutionCode>,
        processor_evidence: ProcessorEvidence,
        lifecycle: PaymentAttemptLifecycle,
        timestamps: PaymentAttemptTimestamps,
    ) -> Self {
        Self {
            status,
            resolution_code,
            processor_evidence,
            lifecycle,
            timestamps,
        }
    }

    pub const fn status(&self) -> PaymentAttemptStatus {
        self.status
    }

    pub const fn resolution_code(&self) -> Option<PaymentResolutionCode> {
        self.resolution_code
    }

    pub const fn processor_evidence(&self) -> &ProcessorEvidence {
        &self.processor_evidence
    }

    pub const fn lifecycle(&self) -> &PaymentAttemptLifecycle {
        &self.lifecycle
    }

    pub const fn timestamps(&self) -> PaymentAttemptTimestamps {
        self.timestamps
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum PaymentAttemptError {
    #[error("only payment-method update attempts may have a zero amount")]
    ZeroAmountRequiresPaymentMethodUpdate,
    #[error("payment-method update attempts must have a zero amount")]
    PaymentMethodUpdateRequiresZeroAmount,
    #[error("pending or unknown initial attempts cannot have application identities")]
    InitialApplicationBeforeResolution,
    #[error("approved initial attempts require payment-method application evidence")]
    ApprovedInitialMissingApplication,
    #[error("terminal attempts require a resolved timestamp")]
    TerminalAttemptMissingResolvedAt,
    #[error("review-required attempts require a review timestamp")]
    ReviewAttemptMissingReviewRequiredAt,
    #[error("attempt update timestamp precedes its creation timestamp")]
    UpdatedBeforeCreated,
    #[error("gateway lifecycle refund amount is inconsistent with the attempt amount")]
    InvalidLifecycleAmount,
}

#[derive(Clone, Eq, PartialEq)]
pub struct PaymentAttempt {
    identity: PaymentAttemptIdentity,
    request: PaymentAttemptRequest,
    state: PaymentAttemptState,
}

impl PaymentAttempt {
    pub fn new(
        identity: PaymentAttemptIdentity,
        request: PaymentAttemptRequest,
        state: PaymentAttemptState,
    ) -> Result<Self, PaymentAttemptError> {
        let is_update =
            request.target().kind() == PaymentAttemptKind::SubscriptionPaymentMethodUpdate;
        match (is_update, request.amount().cents()) {
            (false, 0) => return Err(PaymentAttemptError::ZeroAmountRequiresPaymentMethodUpdate),
            (true, cents) if cents != 0 => {
                return Err(PaymentAttemptError::PaymentMethodUpdateRequiresZeroAmount);
            }
            _ => {}
        }

        if let PaymentAttemptTarget::SubscriptionInitial { application, .. } = request.target() {
            if matches!(
                state.status(),
                PaymentAttemptStatus::Pending | PaymentAttemptStatus::Unknown
            ) && application.is_some()
            {
                return Err(PaymentAttemptError::InitialApplicationBeforeResolution);
            }
            if state.status() == PaymentAttemptStatus::Approved && application.is_none() {
                return Err(PaymentAttemptError::ApprovedInitialMissingApplication);
            }
        }

        let timestamps = state.timestamps();
        if state.status().is_terminal() && timestamps.resolved_at().is_none() {
            return Err(PaymentAttemptError::TerminalAttemptMissingResolvedAt);
        }
        if state.status() == PaymentAttemptStatus::ReviewRequired
            && timestamps.review_required_at().is_none()
        {
            return Err(PaymentAttemptError::ReviewAttemptMissingReviewRequiredAt);
        }
        if timestamps.updated_at() < timestamps.created_at() {
            return Err(PaymentAttemptError::UpdatedBeforeCreated);
        }
        if !lifecycle_amount_is_valid(state.lifecycle().state(), request.amount().cents()) {
            return Err(PaymentAttemptError::InvalidLifecycleAmount);
        }

        Ok(Self {
            identity,
            request,
            state,
        })
    }

    pub const fn identity(&self) -> PaymentAttemptIdentity {
        self.identity
    }

    pub const fn request(&self) -> &PaymentAttemptRequest {
        &self.request
    }

    pub const fn state(&self) -> &PaymentAttemptState {
        &self.state
    }

    pub const fn kind(&self) -> PaymentAttemptKind {
        self.request.target().kind()
    }

    pub const fn status(&self) -> PaymentAttemptStatus {
        self.state.status()
    }
}

impl fmt::Debug for PaymentAttempt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PaymentAttempt")
            .field("identity", &self.identity)
            .field("request", &self.request)
            .field("status", &self.state.status)
            .field("resolution_code", &self.state.resolution_code)
            .field("processor_evidence", &self.state.processor_evidence)
            .field("lifecycle", &self.state.lifecycle)
            .field("timestamps", &self.state.timestamps)
            .finish()
    }
}

fn lifecycle_amount_is_valid(state: &GatewayLifecycleState, amount_cents: i32) -> bool {
    match state {
        GatewayLifecycleState::Unknown
        | GatewayLifecycleState::PendingSettlement
        | GatewayLifecycleState::Voided => true,
        GatewayLifecycleState::Settled {
            cumulative_refunded_cents,
        } => cumulative_refunded_cents.is_none_or(|refunded| refunded.get() < amount_cents),
        GatewayLifecycleState::Refunded {
            cumulative_refunded_cents,
        } => amount_cents > 0 && cumulative_refunded_cents.get() == amount_cents,
        GatewayLifecycleState::Chargeback {
            cumulative_refunded_cents,
        } => cumulative_refunded_cents.is_none_or(|refunded| refunded.get() <= amount_cents),
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use uuid::Uuid;

    use super::*;
    use crate::{
        CumulativeRefundCents, CurrencyCode, GatewayPaymentDescriptor,
        GatewayPaymentMethodReference,
    };

    fn subscription(value: u128) -> SubscriptionId {
        SubscriptionId::new(Uuid::from_u128(value))
    }

    fn method(value: u128) -> PaymentMethodId {
        PaymentMethodId::new(Uuid::from_u128(value))
    }

    fn target(value: u128) -> HostChargeTargetId {
        HostChargeTargetId::new(Uuid::from_u128(value))
    }

    fn identity() -> PaymentAttemptIdentity {
        PaymentAttemptIdentity::new(
            PaymentAttemptId::new(Uuid::from_u128(10)),
            BillingScopeId::new(Uuid::from_u128(11)),
            SubscriberId::new(Uuid::from_u128(12)),
            GatewayAccountId::new(Uuid::from_u128(13)),
            GatewayConfigurationId::new(Uuid::from_u128(14)),
        )
    }

    fn request(target: PaymentAttemptTarget, cents: i32) -> PaymentAttemptRequest {
        PaymentAttemptRequest::new(
            target,
            IdempotencyKey::new("idempotency-secret").unwrap(),
            PaymentAttemptFingerprint::new("fingerprint-secret").unwrap(),
            Money::new(cents, CurrencyCode::new("USD").unwrap()).unwrap(),
            GatewayOrderId::from_correlation("order-secret").unwrap(),
            BillingContactSnapshot::new(
                Some("Sensitive Name".to_owned()),
                Some("secret@example.test".to_owned()),
            ),
        )
    }

    fn instant(second: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, second).unwrap()
    }

    fn state(
        status: PaymentAttemptStatus,
        resolved_at: Option<DateTime<Utc>>,
        review_required_at: Option<DateTime<Utc>>,
    ) -> PaymentAttemptState {
        PaymentAttemptState::new(
            status,
            None,
            ProcessorEvidence::default(),
            PaymentAttemptLifecycle::default(),
            PaymentAttemptTimestamps::new(
                None,
                resolved_at,
                review_required_at,
                instant(0),
                instant(1),
            ),
        )
    }

    fn initial_target(application: Option<SubscriptionInitialApplication>) -> PaymentAttemptTarget {
        PaymentAttemptTarget::SubscriptionInitial {
            plan_key: PlanKey::new("basic").unwrap(),
            discount: None,
            application,
        }
    }

    #[test]
    fn fingerprints_are_nonempty_and_value_safe_to_format() {
        assert_eq!(
            PaymentAttemptFingerprint::new(" "),
            Err(PaymentAttemptFingerprintError::Empty),
        );
        let fingerprint = PaymentAttemptFingerprint::new("secret:economics").unwrap();
        assert_eq!(fingerprint.expose(), "secret:economics");
        assert!(!format!("{fingerprint:?}").contains("secret:economics"));
        assert_eq!(fingerprint.to_string(), "[redacted]");
    }

    #[test]
    fn payment_state_snapshots_are_typed_and_redact_transaction_identity() {
        let transaction = GatewayTransactionId::new("txn-secret").unwrap();
        let update =
            PaymentMethodUpdateSnapshot::new(subscription(1), method(2), transaction.clone());
        let state = SubscriptionPaymentStateSnapshot::new(
            subscription(1),
            method(2),
            transaction,
            SubscriptionStatus::PastDue,
        )
        .unwrap();
        assert_eq!(state.status(), SubscriptionStatus::PastDue);
        assert!(!format!("{update:?}").contains("txn-secret"));
        assert!(!format!("{state:?}").contains("txn-secret"));
        assert_eq!(
            SubscriptionPaymentStateSnapshot::new(
                subscription(1),
                method(2),
                GatewayTransactionId::new("txn").unwrap(),
                SubscriptionStatus::Canceled,
            ),
            Err(PaymentAttemptSnapshotError::CanceledSubscription),
        );
    }

    #[test]
    fn attempt_timestamps_choose_submission_as_the_economic_boundary() {
        let created = Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap();
        let submitted = Utc.with_ymd_and_hms(2026, 8, 1, 0, 1, 0).unwrap();
        assert_eq!(
            PaymentAttemptTimestamps::new(Some(submitted), None, None, created, submitted,)
                .submitted_or_created_at(),
            submitted,
        );
        assert_eq!(
            PaymentAttemptTimestamps::new(None, None, None, created, created)
                .submitted_or_created_at(),
            created,
        );
    }

    #[test]
    fn amount_shape_follows_attempt_kind() {
        assert_eq!(
            PaymentAttempt::new(
                identity(),
                request(
                    PaymentAttemptTarget::HostCharge {
                        target_id: target(20),
                    },
                    0,
                ),
                state(PaymentAttemptStatus::Pending, None, None),
            ),
            Err(PaymentAttemptError::ZeroAmountRequiresPaymentMethodUpdate),
        );

        let update = PaymentAttemptTarget::SubscriptionPaymentMethodUpdate {
            plan_key: PlanKey::new("basic").unwrap(),
            payment_method_id: method(22),
            expected_state: PaymentMethodUpdateSnapshot::new(
                subscription(21),
                method(22),
                GatewayTransactionId::new("txn-initial").unwrap(),
            ),
        };
        assert_eq!(
            PaymentAttempt::new(
                identity(),
                request(update.clone(), 1),
                state(PaymentAttemptStatus::Pending, None, None),
            ),
            Err(PaymentAttemptError::PaymentMethodUpdateRequiresZeroAmount),
        );
        let accepted = PaymentAttempt::new(
            identity(),
            request(update, 0),
            state(PaymentAttemptStatus::Pending, None, None),
        )
        .unwrap();
        assert_eq!(
            accepted.kind(),
            PaymentAttemptKind::SubscriptionPaymentMethodUpdate
        );
    }

    #[test]
    fn initial_application_identity_follows_resolution_state() {
        let application = SubscriptionInitialApplication::new(Some(subscription(30)), method(31));
        assert_eq!(
            PaymentAttempt::new(
                identity(),
                request(initial_target(Some(application)), 1_000),
                state(PaymentAttemptStatus::Pending, None, None),
            ),
            Err(PaymentAttemptError::InitialApplicationBeforeResolution),
        );
        assert_eq!(
            PaymentAttempt::new(
                identity(),
                request(initial_target(None), 1_000),
                state(PaymentAttemptStatus::Approved, Some(instant(2)), None),
            ),
            Err(PaymentAttemptError::ApprovedInitialMissingApplication),
        );
        let approved = PaymentAttempt::new(
            identity(),
            request(initial_target(Some(application)), 1_000),
            state(PaymentAttemptStatus::Approved, Some(instant(2)), None),
        )
        .unwrap();
        assert_eq!(
            approved.request().target().subscription_id(),
            Some(subscription(30))
        );
        assert_eq!(
            approved.request().target().payment_method_id(),
            Some(method(31))
        );
    }

    #[test]
    fn terminal_and_review_states_require_their_durable_boundaries() {
        let host = || PaymentAttemptTarget::HostCharge {
            target_id: target(40),
        };
        assert_eq!(
            PaymentAttempt::new(
                identity(),
                request(host(), 1_000),
                state(PaymentAttemptStatus::Failed, None, None),
            ),
            Err(PaymentAttemptError::TerminalAttemptMissingResolvedAt),
        );
        assert_eq!(
            PaymentAttempt::new(
                identity(),
                request(host(), 1_000),
                state(PaymentAttemptStatus::ReviewRequired, None, None),
            ),
            Err(PaymentAttemptError::ReviewAttemptMissingReviewRequiredAt),
        );
        assert!(
            PaymentAttempt::new(
                identity(),
                request(host(), 1_000),
                state(PaymentAttemptStatus::ReviewRequired, None, Some(instant(2)),),
            )
            .is_ok()
        );
    }

    #[test]
    fn lifecycle_refund_amount_must_match_the_attempt_amount() {
        let lifecycle = PaymentAttemptLifecycle::new(
            GatewayLifecycleState::Refunded {
                cumulative_refunded_cents: CumulativeRefundCents::new(999).unwrap(),
            },
            None,
            None,
            None,
        );
        let invalid_state = PaymentAttemptState::new(
            PaymentAttemptStatus::Pending,
            None,
            ProcessorEvidence::default(),
            lifecycle,
            PaymentAttemptTimestamps::new(None, None, None, instant(0), instant(1)),
        );
        assert_eq!(
            PaymentAttempt::new(
                identity(),
                request(
                    PaymentAttemptTarget::HostCharge {
                        target_id: target(50),
                    },
                    1_000,
                ),
                invalid_state,
            ),
            Err(PaymentAttemptError::InvalidLifecycleAmount),
        );
    }

    #[test]
    fn typed_targets_preserve_exact_relationships() {
        let period = BillingPeriod::new(instant(0), instant(2)).unwrap();
        let expected_state = SubscriptionPaymentStateSnapshot::new(
            subscription(60),
            method(61),
            GatewayTransactionId::new("txn-original").unwrap(),
            SubscriptionStatus::PastDue,
        )
        .unwrap();
        let renewal = PaymentAttemptTarget::SubscriptionRenewal {
            plan_key: PlanKey::new("premium").unwrap(),
            payment_method_id: method(62),
            period: period.clone(),
            expected_state: expected_state.clone(),
        };
        assert_eq!(renewal.kind(), PaymentAttemptKind::SubscriptionRenewal);
        assert_eq!(renewal.plan_key().unwrap().as_str(), "premium");
        assert_eq!(renewal.subscription_id(), Some(subscription(60)));
        assert_eq!(renewal.payment_method_id(), Some(method(62)));
        assert_eq!(
            renewal
                .subscription_payment_state_snapshot()
                .unwrap()
                .payment_method_id(),
            method(61)
        );
        assert_eq!(renewal.period(), Some(&period));
        assert_eq!(
            renewal.subscription_payment_state_snapshot(),
            Some(&expected_state)
        );
        assert_eq!(renewal.host_charge_target_id(), None);
    }

    #[test]
    fn durable_attempt_debug_is_value_free() {
        let evidence = ProcessorEvidence::new(
            Some(GatewayTransactionId::new("transaction-secret").unwrap()),
            Some(GatewayPaymentMethodReference::new("method-secret").unwrap()),
            Some(GatewayDiagnostic::new("response-secret")),
            Some(GatewayDiagnostic::new("code-secret")),
            Some(GatewayDiagnostic::new("text-secret")),
            Some(GatewayDiagnostic::new("condition-secret")),
            GatewayPaymentDescriptor::default(),
        );
        let state = PaymentAttemptState::new(
            PaymentAttemptStatus::Pending,
            None,
            evidence,
            PaymentAttemptLifecycle::new(
                GatewayLifecycleState::Unknown,
                Some(GatewayDiagnostic::new("action-secret")),
                None,
                None,
            ),
            PaymentAttemptTimestamps::new(None, None, None, instant(0), instant(1)),
        );
        let attempt = PaymentAttempt::new(
            identity(),
            request(
                PaymentAttemptTarget::HostCharge {
                    target_id: target(70),
                },
                1_000,
            ),
            state,
        )
        .unwrap();
        let debug = format!("{attempt:?}");
        for secret in [
            "idempotency-secret",
            "fingerprint-secret",
            "order-secret",
            "Sensitive Name",
            "secret@example.test",
            "transaction-secret",
            "method-secret",
            "response-secret",
            "code-secret",
            "text-secret",
            "condition-secret",
            "action-secret",
        ] {
            assert!(!debug.contains(secret), "debug leaked {secret}");
        }
        assert!(debug.contains("has_idempotency_key"));
        assert!(debug.contains("has_transaction_id"));
    }
}
