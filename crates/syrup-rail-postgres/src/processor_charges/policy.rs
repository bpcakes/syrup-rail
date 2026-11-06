//! Shared classification of immutable approval evidence before application.
use syrup_rail::{
    PaymentAttemptKind, PaymentAttemptStatus, PaymentResolutionCode, ProcessorChargeProgression,
    ProcessorChargeRole, ProcessorChargeStateCode,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ChargeIdentityMatch {
    Same,
    Different,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ApprovalIdentity {
    Unidentified,
    Primary,
    AdditionalCharge,
    AdditionalZeroAmount,
}

impl ApprovalIdentity {
    pub(crate) fn classify(
        role: ProcessorChargeRole,
        amount_cents: i32,
        transaction_id: Option<&str>,
    ) -> Self {
        if transaction_id.is_none() {
            return Self::Unidentified;
        }
        match role {
            ProcessorChargeRole::Primary => Self::Primary,
            ProcessorChargeRole::Additional if amount_cents > 0 => Self::AdditionalCharge,
            ProcessorChargeRole::Additional => Self::AdditionalZeroAmount,
        }
    }
}

/// One decision fixes both the destination and its reason. It cannot describe
/// an applied charge that is simultaneously waiting for transaction identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PendingChargeDecision {
    IdentifyTransaction,
    AwaitApplication,
    ReviewZeroAmountAdditional,
    ReverseAdditional,
    ReverseConflicting,
    Applied,
    AttestedReversal(ProcessorChargeStateCode),
}

impl PendingChargeDecision {
    pub(crate) fn parts(self) -> (ProcessorChargeProgression, Option<ProcessorChargeStateCode>) {
        use ProcessorChargeProgression as P;
        use ProcessorChargeStateCode as C;
        match self {
            Self::IdentifyTransaction => (
                P::ReconciliationRequired,
                Some(C::TransactionIdentityRequired),
            ),
            Self::AwaitApplication => (
                P::ReconciliationRequired,
                Some(C::ApprovedChargeWaitingForApplication),
            ),
            Self::ReviewZeroAmountAdditional => (
                P::ReconciliationRequired,
                Some(C::ZeroAmountAdditionalApprovedCharge),
            ),
            Self::ReverseAdditional => (
                P::ExternalReversalRequired,
                Some(C::AdditionalApprovedChargeIdentified),
            ),
            Self::ReverseConflicting => (
                P::ExternalReversalRequired,
                Some(C::ExternalReversalRequired),
            ),
            Self::Applied => (P::Applied, None),
            Self::AttestedReversal(code) => (P::ExternallyReversed, Some(code)),
        }
    }
}

pub(crate) struct ApprovalApplicationFacts {
    pub(crate) identity: ApprovalIdentity,
    pub(crate) amount_cents: i32,
    pub(crate) attempt_kind: PaymentAttemptKind,
    pub(crate) attempt_status: PaymentAttemptStatus,
    pub(crate) resolution_code: Option<PaymentResolutionCode>,
    pub(crate) attempt_transaction_id: Option<String>,
    pub(crate) identity_match: ChargeIdentityMatch,
}

impl ApprovalApplicationFacts {
    pub(crate) fn pending_decision(&self) -> PendingChargeDecision {
        match self.identity {
            ApprovalIdentity::Unidentified => return PendingChargeDecision::IdentifyTransaction,
            ApprovalIdentity::AdditionalCharge => return PendingChargeDecision::ReverseAdditional,
            ApprovalIdentity::AdditionalZeroAmount => {
                return PendingChargeDecision::ReviewZeroAmountAdditional;
            }
            ApprovalIdentity::Primary => {}
        }
        let terminal_reversal = self.attempt_status == PaymentAttemptStatus::Failed
            && is_external_reversal_resolution(self.resolution_code);
        let grant_conflict = self.attempt_kind == PaymentAttemptKind::SubscriptionInitial
            && self.resolution_code
                == Some(PaymentResolutionCode::SubscriptionInitialCurrentGrantConflict);
        if self.amount_cents > 0
            && (terminal_reversal
                || grant_conflict
                || (self.attempt_transaction_id.is_some()
                    && self.identity_match == ChargeIdentityMatch::Different))
        {
            return PendingChargeDecision::ReverseConflicting;
        }
        if self.identity_match == ChargeIdentityMatch::Same
            && self.attempt_status == PaymentAttemptStatus::Approved
        {
            return PendingChargeDecision::Applied;
        }
        match self.identity {
            ApprovalIdentity::AdditionalZeroAmount => {
                PendingChargeDecision::ReviewZeroAmountAdditional
            }
            ApprovalIdentity::Primary => PendingChargeDecision::AwaitApplication,
            ApprovalIdentity::Unidentified | ApprovalIdentity::AdditionalCharge => {
                unreachable!("handled above")
            }
        }
    }
}

pub(crate) fn is_external_reversal_resolution(code: Option<PaymentResolutionCode>) -> bool {
    matches!(
        code,
        Some(
            PaymentResolutionCode::SubscriptionInitialExternallyRefunded
                | PaymentResolutionCode::SubscriptionInitialExternallyVoided
                | PaymentResolutionCode::ProcessorChargeExternallyRefunded
                | PaymentResolutionCode::ProcessorChargeExternallyVoided
        )
    )
}

/// The purpose of retaining approval evidence. Terminal reversal is created
/// only by the attestation workflow, never by observation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ApprovalObservation {
    AwaitApplication,
    Reconcile,
    ReviewReversal,
    ReplayApplied,
}

impl ApprovalObservation {
    /// Classify evidence for an already-approved attempt without promoting a
    /// sparse observation into an exact replay.
    pub(crate) fn for_approved_attempt(
        attempt: &syrup_rail::PaymentAttempt,
        evidence: &syrup_rail::ProcessorEvidence,
    ) -> Self {
        if attempt.state().processor_evidence().transaction_id() == evidence.transaction_id() {
            Self::ReplayApplied
        } else if evidence.transaction_id().is_some() && attempt.request().amount().cents() > 0 {
            Self::ReviewReversal
        } else {
            Self::Reconcile
        }
    }

    pub(crate) fn progression(self) -> ProcessorChargeProgression {
        match self {
            Self::AwaitApplication => ProcessorChargeProgression::Pending,
            Self::Reconcile => ProcessorChargeProgression::ReconciliationRequired,
            Self::ReviewReversal => ProcessorChargeProgression::ExternalReversalRequired,
            Self::ReplayApplied => ProcessorChargeProgression::Applied,
        }
    }
}

impl TryFrom<ProcessorChargeProgression> for ApprovalObservation {
    type Error = super::ProcessorChargeStoreError;

    fn try_from(value: ProcessorChargeProgression) -> Result<Self, Self::Error> {
        match value {
            ProcessorChargeProgression::Pending => Ok(Self::AwaitApplication),
            ProcessorChargeProgression::ReconciliationRequired => Ok(Self::Reconcile),
            ProcessorChargeProgression::ExternalReversalRequired => Ok(Self::ReviewReversal),
            ProcessorChargeProgression::Applied => Ok(Self::ReplayApplied),
            ProcessorChargeProgression::ExternallyReversed => Err(Self::Error::InvalidState(
                "observing approval cannot attest an external reversal",
            )),
        }
    }
}

#[cfg(test)]
mod tests;
