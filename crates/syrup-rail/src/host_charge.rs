use chrono::{DateTime, Utc};

use crate::{
    ApprovedProcessorEvidence, BillingContact, BillingContactSnapshot, BillingScopeId,
    ChargeAmount, GatewayConfigurationId, HostChargeTargetId, IdempotencyKey, PaymentAttempt,
    PaymentAttemptFingerprint, PaymentAttemptId, PaymentAttemptIdentity, PaymentAttemptKind,
    PaymentAttemptRequest, PaymentAttemptStatus, PaymentAttemptTarget, PaymentReversalKind,
    PaymentToken, ProcessorEvidence, ResolvedGateway, SubscriberId,
};
use thiserror::Error;

/// Provider-neutral request to charge one host-owned target.
///
/// The host extension supplies the authoritative price and eligibility; the
/// caller cannot choose either value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChargeHostTarget {
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    target_id: HostChargeTargetId,
    gateway_configuration_id: GatewayConfigurationId,
    payment_token: PaymentToken,
    idempotency_key: IdempotencyKey,
    billing_contact: Option<BillingContact>,
}

impl ChargeHostTarget {
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        billing_scope_id: BillingScopeId,
        subscriber_id: SubscriberId,
        target_id: HostChargeTargetId,
        gateway_configuration_id: GatewayConfigurationId,
        payment_token: PaymentToken,
        idempotency_key: IdempotencyKey,
        billing_contact: Option<BillingContact>,
    ) -> Self {
        Self {
            billing_scope_id,
            subscriber_id,
            target_id,
            gateway_configuration_id,
            payment_token,
            idempotency_key,
            billing_contact,
        }
    }

    pub const fn billing_scope_id(&self) -> BillingScopeId {
        self.billing_scope_id
    }

    pub const fn subscriber_id(&self) -> SubscriberId {
        self.subscriber_id
    }

    pub const fn target_id(&self) -> HostChargeTargetId {
        self.target_id
    }

    pub const fn gateway_configuration_id(&self) -> GatewayConfigurationId {
        self.gateway_configuration_id
    }

    pub const fn payment_token(&self) -> &PaymentToken {
        &self.payment_token
    }

    pub const fn idempotency_key(&self) -> &IdempotencyKey {
        &self.idempotency_key
    }

    pub const fn billing_contact(&self) -> Option<&BillingContact> {
        self.billing_contact.as_ref()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HostChargeTargetSnapshot {
    target_id: HostChargeTargetId,
    charge: ChargeAmount,
}

impl HostChargeTargetSnapshot {
    pub const fn new(target_id: HostChargeTargetId, charge: ChargeAmount) -> Self {
        Self { target_id, charge }
    }

    pub const fn target_id(self) -> HostChargeTargetId {
        self.target_id
    }

    pub const fn charge(self) -> ChargeAmount {
        self.charge
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum HostChargeReservationBuildError {
    #[error("resolved gateway identity does not match the host charge command")]
    GatewayIdentityMismatch,
    #[error("host target snapshot does not match the command target")]
    TargetIdentityMismatch,
    #[error("payment attempt is not a host charge")]
    AttemptKindMismatch,
    #[error("host charge attempt has invalid economics")]
    InvalidCharge,
}

/// Token-free durable reservation assembled after gateway resolution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostChargeReservation {
    identity: PaymentAttemptIdentity,
    request: PaymentAttemptRequest,
    snapshot: HostChargeTargetSnapshot,
}

impl HostChargeReservation {
    pub fn from_command(
        command: &ChargeHostTarget,
        snapshot: HostChargeTargetSnapshot,
        gateway: &ResolvedGateway,
        attempt_id: PaymentAttemptId,
    ) -> Result<Self, HostChargeReservationBuildError> {
        if snapshot.target_id() != command.target_id() {
            return Err(HostChargeReservationBuildError::TargetIdentityMismatch);
        }
        if gateway.billing_scope_id() != command.billing_scope_id()
            || gateway.gateway_configuration_id() != command.gateway_configuration_id()
        {
            return Err(HostChargeReservationBuildError::GatewayIdentityMismatch);
        }
        let identity = PaymentAttemptIdentity::new(
            attempt_id,
            command.billing_scope_id(),
            command.subscriber_id(),
            gateway.gateway_account_id(),
            command.gateway_configuration_id(),
        );
        let amount = snapshot.charge().money();
        let request = PaymentAttemptRequest::new(
            PaymentAttemptTarget::HostCharge {
                target_id: command.target_id(),
            },
            command.idempotency_key().clone(),
            PaymentAttemptFingerprint::for_host_charge(command.target_id(), amount),
            amount,
            gateway
                .mutation_reference_factory()
                .for_attempt(PaymentAttemptKind::HostCharge, attempt_id),
            command
                .billing_contact()
                .map(BillingContactSnapshot::from_billing_contact)
                .unwrap_or_else(|| BillingContactSnapshot::new(None, None)),
        );
        Ok(Self {
            identity,
            request,
            snapshot,
        })
    }

    pub const fn identity(&self) -> PaymentAttemptIdentity {
        self.identity
    }

    pub const fn request(&self) -> &PaymentAttemptRequest {
        &self.request
    }

    pub const fn snapshot(&self) -> HostChargeTargetSnapshot {
        self.snapshot
    }

    pub fn from_attempt(attempt: &PaymentAttempt) -> Result<Self, HostChargeReservationBuildError> {
        let target_id = attempt
            .request()
            .target()
            .host_charge_target_id()
            .ok_or(HostChargeReservationBuildError::AttemptKindMismatch)?;
        let charge = ChargeAmount::try_from(attempt.request().amount())
            .map_err(|_| HostChargeReservationBuildError::InvalidCharge)?;
        Ok(Self {
            identity: attempt.identity(),
            request: attempt.request().clone(),
            snapshot: HostChargeTargetSnapshot::new(target_id, charge),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostChargeTargetRejection {
    TargetUnavailable,
    ChargeChanged,
    LedgerUnsafe,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostChargeTargetTransitionKind {
    Paid,
    PaymentFailed,
    ReleasedAfterExternalReversal { kind: PaymentReversalKind },
    Reversed { kind: PaymentReversalKind },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HostChargeTargetTransition {
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    attempt_id: PaymentAttemptId,
    target_id: HostChargeTargetId,
    kind: HostChargeTargetTransitionKind,
    effective_at: DateTime<Utc>,
}

impl HostChargeTargetTransition {
    pub const fn new(
        billing_scope_id: BillingScopeId,
        subscriber_id: SubscriberId,
        attempt_id: PaymentAttemptId,
        target_id: HostChargeTargetId,
        kind: HostChargeTargetTransitionKind,
        effective_at: DateTime<Utc>,
    ) -> Self {
        Self {
            billing_scope_id,
            subscriber_id,
            attempt_id,
            target_id,
            kind,
            effective_at,
        }
    }

    pub const fn billing_scope_id(self) -> BillingScopeId {
        self.billing_scope_id
    }

    pub const fn subscriber_id(self) -> SubscriberId {
        self.subscriber_id
    }

    pub const fn attempt_id(self) -> PaymentAttemptId {
        self.attempt_id
    }

    pub const fn target_id(self) -> HostChargeTargetId {
        self.target_id
    }

    pub const fn kind(self) -> HostChargeTargetTransitionKind {
        self.kind
    }

    pub const fn effective_at(self) -> DateTime<Utc> {
        self.effective_at
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostChargeTargetNoChange {
    Missing,
    InapplicableState,
    ReleaseUnsafe,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostChargeTargetTransitionOutcome {
    Applied,
    ExactReplay,
    StaleTarget,
    Unchanged { reason: HostChargeTargetNoChange },
}

/// Durable result of applying one host-target payment outcome.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostChargePaymentResult {
    attempt: PaymentAttempt,
    pending_confirmation_evidence: Option<ApprovedProcessorEvidence>,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum HostChargePaymentResultBuildError {
    #[error("a host-charge payment result requires a host-charge attempt")]
    AttemptNotHostCharge,
    #[error("an approved attempt cannot produce a confirmation-pending payment result")]
    ConfirmationPendingAttemptApproved,
}

impl HostChargePaymentResult {
    pub fn new(attempt: PaymentAttempt) -> Result<Self, HostChargePaymentResultBuildError> {
        require_host_charge_attempt(&attempt)?;
        Ok(Self {
            attempt,
            pending_confirmation_evidence: None,
        })
    }

    pub fn confirmation_pending(
        attempt: PaymentAttempt,
        evidence: ApprovedProcessorEvidence,
    ) -> Result<Self, HostChargePaymentResultBuildError> {
        require_host_charge_attempt(&attempt)?;
        if attempt.status() == PaymentAttemptStatus::Approved {
            return Err(HostChargePaymentResultBuildError::ConfirmationPendingAttemptApproved);
        }
        Ok(Self {
            attempt,
            pending_confirmation_evidence: Some(evidence),
        })
    }

    pub const fn attempt(&self) -> &PaymentAttempt {
        &self.attempt
    }

    pub const fn status(&self) -> PaymentAttemptStatus {
        if self.pending_confirmation_evidence.is_some() {
            PaymentAttemptStatus::Unknown
        } else {
            self.attempt.status()
        }
    }

    pub fn processor_evidence(&self) -> &ProcessorEvidence {
        self.pending_confirmation_evidence
            .as_ref()
            .map(ApprovedProcessorEvidence::evidence)
            .unwrap_or_else(|| self.attempt.state().processor_evidence())
    }

    pub const fn is_confirmation_pending(&self) -> bool {
        self.pending_confirmation_evidence.is_some()
    }
}

fn require_host_charge_attempt(
    attempt: &PaymentAttempt,
) -> Result<(), HostChargePaymentResultBuildError> {
    if attempt.kind() == PaymentAttemptKind::HostCharge {
        Ok(())
    } else {
        Err(HostChargePaymentResultBuildError::AttemptNotHostCharge)
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;
    use crate::{CurrencyCode, Money, PaymentAttemptFingerprint};

    #[test]
    fn host_charge_fingerprint_is_target_and_economics_exact() {
        let target = HostChargeTargetId::new(Uuid::from_u128(1));
        let money = Money::new(1_250, CurrencyCode::new("USD").unwrap()).unwrap();
        let fingerprint = PaymentAttemptFingerprint::for_host_charge(target, money);

        assert_eq!(
            fingerprint.expose(),
            "host_charge:00000000-0000-0000-0000-000000000001:1250:USD"
        );
        assert_eq!(fingerprint.to_string(), "[redacted]");
    }
}
