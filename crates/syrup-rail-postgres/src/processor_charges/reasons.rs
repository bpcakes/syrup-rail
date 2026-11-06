//! Reasons accepted by charge transition operations and compatibility callers.
use super::*;

/// Why an approved payment-method update still needs reconciliation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChargeReconciliationReason {
    /// The subscription no longer accepts a payment-method update.
    PaymentMethodSubscriptionIneligible,
    /// The subscription's payment-method baseline changed.
    PaymentMethodStaleState,
}

impl ChargeReconciliationReason {
    const fn attempt_kind(self) -> PaymentAttemptKind {
        match self {
            Self::PaymentMethodSubscriptionIneligible | Self::PaymentMethodStaleState => {
                PaymentAttemptKind::SubscriptionPaymentMethodUpdate
            }
        }
    }

    pub(crate) const fn code(self) -> PaymentResolutionCode {
        match self {
            Self::PaymentMethodSubscriptionIneligible => {
                PaymentResolutionCode::SubscriptionApprovedPaymentMethodUpdateSubscriptionIneligible
            }
            Self::PaymentMethodStaleState => {
                PaymentResolutionCode::SubscriptionApprovedPaymentMethodUpdateStaleState
            }
        }
    }
}

/// Why an identified approval requires external-reversal review.
/// Expired periods use the separate permanent parking operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChargeReversalReason {
    /// Another subscription prevents initial application.
    InitialSubscriptionConflict,
    /// An active grant prevents initial application.
    InitialGrantConflict,
    /// The renewal's expected subscription state changed.
    RenewalStaleState,
    /// The recovery's expected subscription state changed.
    RecoveryStaleState,
    /// The host target no longer accepts the payment.
    HostStaleState,
}

impl ChargeReversalReason {
    const fn attempt_kind(self) -> PaymentAttemptKind {
        match self {
            Self::InitialSubscriptionConflict | Self::InitialGrantConflict => {
                PaymentAttemptKind::SubscriptionInitial
            }
            Self::RenewalStaleState => PaymentAttemptKind::SubscriptionRenewal,
            Self::RecoveryStaleState => PaymentAttemptKind::SubscriptionRecovery,
            Self::HostStaleState => PaymentAttemptKind::HostCharge,
        }
    }

    pub(crate) const fn code(self) -> PaymentResolutionCode {
        match self {
            Self::InitialSubscriptionConflict => {
                PaymentResolutionCode::SubscriptionInitialCurrentSubscriptionConflict
            }
            Self::InitialGrantConflict => {
                PaymentResolutionCode::SubscriptionInitialCurrentGrantConflict
            }
            Self::RenewalStaleState => PaymentResolutionCode::SubscriptionApprovedRenewalStaleState,
            Self::RecoveryStaleState => {
                PaymentResolutionCode::SubscriptionApprovedRecoveryStaleState
            }
            Self::HostStaleState => PaymentResolutionCode::HostChargeApprovedStaleState,
        }
    }
}

/// Additional owner prerequisites for a newly supplied reason. Persistence
/// checks these against the owning attempt in the same guarded UPDATE.
pub(super) enum ChargeReasonOwner {
    Any,
    AttemptKind(PaymentAttemptKind),
    ExpiredSubscriptionPeriod,
}

impl ChargeReasonOwner {
    pub(super) const fn sql_key(self) -> &'static str {
        match self {
            Self::Any => "any",
            Self::AttemptKind(kind) => kind.as_str(),
            Self::ExpiredSubscriptionPeriod => "expired_subscription_period",
        }
    }
}

/// Validate newly supplied reasons. Retained reasons are historical audit facts;
/// changing progression does not rewrite them. SQL separately protects expiry.
pub(super) fn validate_reason(
    destination: ProcessorChargeProgression,
    reason: Option<ProcessorChargeStateCode>,
) -> Result<ChargeReasonOwner, ProcessorChargeStoreError> {
    use ProcessorChargeProgression as P;
    use ProcessorChargeStateCode as C;
    let Some(reason) = reason else {
        return Ok(ChargeReasonOwner::Any);
    };
    let reconciliation_reason = [
        ChargeReconciliationReason::PaymentMethodSubscriptionIneligible,
        ChargeReconciliationReason::PaymentMethodStaleState,
    ]
    .into_iter()
    .find(|candidate| reason == C::PaymentResolution(candidate.code()));
    let reversal_reason = [
        ChargeReversalReason::InitialSubscriptionConflict,
        ChargeReversalReason::InitialGrantConflict,
        ChargeReversalReason::RenewalStaleState,
        ChargeReversalReason::RecoveryStaleState,
        ChargeReversalReason::HostStaleState,
    ]
    .into_iter()
    .find(|candidate| reason == C::PaymentResolution(candidate.code()));
    let reconciliation = match reason {
        C::TransactionIdentityRequired
        | C::ApprovedChargeWaitingForApplication
        | C::ZeroAmountAdditionalApprovedCharge => true,
        C::PaymentResolution(_) => reconciliation_reason.is_some(),
        C::ExternalReversalRequired | C::AdditionalApprovedChargeIdentified => false,
    };
    let reversal = match reason {
        C::ExternalReversalRequired | C::AdditionalApprovedChargeIdentified => true,
        C::PaymentResolution(_) => reversal_reason.is_some(),
        C::TransactionIdentityRequired
        | C::ApprovedChargeWaitingForApplication
        | C::ZeroAmountAdditionalApprovedCharge => false,
    };
    let allowed = match destination {
        P::Pending => false,
        P::ReconciliationRequired => reconciliation,
        P::ExternalReversalRequired => {
            reversal
                || reason
                    == C::PaymentResolution(
                        PaymentResolutionCode::SubscriptionApprovedPeriodExpired,
                    )
        }
        // Compatibility callers may preserve a prior application review reason.
        P::Applied => reconciliation || reversal,
        // The matching attestation, checked in SQL, owns the terminal reason.
        P::ExternallyReversed => {
            matches!(
                reason,
                C::ExternalReversalRequired | C::PaymentResolution(_)
            )
        }
    };
    if allowed {
        Ok(match reason {
            C::PaymentResolution(PaymentResolutionCode::SubscriptionApprovedPeriodExpired) => {
                ChargeReasonOwner::ExpiredSubscriptionPeriod
            }
            _ => reconciliation_reason
                .map(|reason| reason.attempt_kind())
                .or_else(|| reversal_reason.map(|reason| reason.attempt_kind()))
                .map_or(ChargeReasonOwner::Any, ChargeReasonOwner::AttemptKind),
        })
    } else {
        Err(ProcessorChargeStoreError::InvalidState(
            "processor charge reason does not belong to the requested transition",
        ))
    }
}
