use std::fmt;

use chrono::{DateTime, Utc};
use thiserror::Error;

use crate::{
    BillingContact, BillingPeriod, BillingScopeId, GatewayAccountId, GatewayConfigurationId,
    GatewayDiagnostic, GatewayLifecycleState, GatewayOrderId, GatewayTransactionId,
    HostChargeTargetId, IdempotencyKey, Money, PaymentAttemptId, PaymentAttemptKind,
    PaymentAttemptStatus, PaymentMethodId, PaymentResolutionCode, PlanKey, ProcessorEvidence,
    SubscriberId, SubscriptionDiscountDuration, SubscriptionDiscountKind,
    SubscriptionEnrollmentDiscountSnapshot, SubscriptionId, SubscriptionOffer, SubscriptionStatus,
};

pub use fingerprint::{PaymentAttemptFingerprint, PaymentAttemptFingerprintError};
pub use snapshots::{
    BillingContactSnapshot, PaymentAttemptSnapshotError, PaymentAttemptTimestamps,
    PaymentMethodUpdateSnapshot, SubscriptionPaymentStateSnapshot,
};

mod fingerprint;
mod snapshots;

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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubscriptionEnrollmentTermsVersion {
    V1,
    V2,
}

impl SubscriptionEnrollmentTermsVersion {
    pub const fn get(self) -> u16 {
        match self {
            Self::V1 => 1,
            Self::V2 => 2,
        }
    }
}

impl TryFrom<u16> for SubscriptionEnrollmentTermsVersion {
    type Error = PaymentAttemptSnapshotError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::V1),
            2 => Ok(Self::V2),
            _ => Err(PaymentAttemptSnapshotError::InvalidEnrollmentTermsVersion),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PaymentAttemptTarget {
    HostCharge {
        target_id: HostChargeTargetId,
    },
    SubscriptionInitial {
        terms_version: SubscriptionEnrollmentTermsVersion,
        offer: SubscriptionOffer,
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
            Self::SubscriptionInitial { offer, .. } => Some(offer.plan_key()),
            Self::SubscriptionRenewal { plan_key, .. }
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

    pub const fn enrollment_terms_version(&self) -> Option<SubscriptionEnrollmentTermsVersion> {
        match self {
            Self::SubscriptionInitial { terms_version, .. } => Some(*terms_version),
            _ => None,
        }
    }

    pub const fn enrollment_offer(&self) -> Option<&SubscriptionOffer> {
        match self {
            Self::SubscriptionInitial { offer, .. } => Some(offer),
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
mod tests;
