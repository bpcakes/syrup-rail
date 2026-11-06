//! Legal outcome commands: only definite local failures clear admission.
use super::*;
use syrup_rail::PaymentAttemptPhase;

#[derive(Clone, Copy)]
pub(crate) enum OutcomeResolutionCommand {
    PreparedFailure {
        code: Option<PaymentResolutionCode>,
        cooldown: Option<RateLimitCooldown>,
    },
    AdmittedFailure {
        proof: NonSubmissionProof,
        code: Option<PaymentResolutionCode>,
        cooldown: Option<RateLimitCooldown>,
    },
    Submitted {
        status: AttemptResolutionStatus,
        code: Option<PaymentResolutionCode>,
        cooldown: Option<RateLimitCooldown>,
    },
    Unknown {
        cooldown: Option<RateLimitCooldown>,
    },
}

impl OutcomeResolutionCommand {
    pub(crate) const fn local_failure(
        boundary: OutcomeResolutionBoundary,
        code: Option<PaymentResolutionCode>,
        cooldown: Option<RateLimitCooldown>,
    ) -> Self {
        match boundary {
            OutcomeResolutionBoundary::Prepared => Self::PreparedFailure { code, cooldown },
            OutcomeResolutionBoundary::AdmittedNotSubmitted(proof) => Self::AdmittedFailure {
                proof,
                code,
                cooldown,
            },
            OutcomeResolutionBoundary::Submitted => Self::Submitted {
                status: AttemptResolutionStatus::Failed,
                code,
                cooldown,
            },
        }
    }

    pub(crate) const fn submitted(
        status: AttemptResolutionStatus,
        code: Option<PaymentResolutionCode>,
        cooldown: Option<RateLimitCooldown>,
    ) -> Self {
        Self::Submitted {
            status,
            code,
            cooldown,
        }
    }

    pub(crate) const fn unknown(cooldown: Option<RateLimitCooldown>) -> Self {
        Self::Unknown { cooldown }
    }

    pub(crate) const fn boundary(self) -> OutcomeResolutionBoundary {
        match self {
            Self::PreparedFailure { .. } => OutcomeResolutionBoundary::Prepared,
            Self::AdmittedFailure { proof, .. } => {
                OutcomeResolutionBoundary::AdmittedNotSubmitted(proof)
            }
            Self::Submitted { .. } | Self::Unknown { .. } => OutcomeResolutionBoundary::Submitted,
        }
    }

    pub(crate) const fn status(self) -> AttemptResolutionStatus {
        match self {
            Self::PreparedFailure { .. } | Self::AdmittedFailure { .. } => {
                AttemptResolutionStatus::Failed
            }
            Self::Submitted { status, .. } => status,
            Self::Unknown { .. } => AttemptResolutionStatus::Unknown,
        }
    }

    pub(crate) const fn cooldown(self) -> Option<RateLimitCooldown> {
        match self {
            Self::PreparedFailure { cooldown, .. }
            | Self::AdmittedFailure { cooldown, .. }
            | Self::Submitted { cooldown, .. }
            | Self::Unknown { cooldown } => cooldown,
        }
    }

    pub(crate) const fn resolution_code(self) -> Option<PaymentResolutionCode> {
        match self {
            Self::PreparedFailure { code, .. }
            | Self::AdmittedFailure { code, .. }
            | Self::Submitted { code, .. } => code,
            Self::Unknown { .. } => None,
        }
    }

    pub(crate) fn matches_attempt(self, attempt: &PaymentAttempt) -> bool {
        match self {
            Self::AdmittedFailure { proof, .. } => proof.matches(attempt),
            _ => self.may_resolve(
                attempt.status(),
                attempt.state().timestamps().submitted_at(),
            ),
        }
    }

    pub(crate) const fn may_resolve(
        self,
        status: PaymentAttemptStatus,
        submitted_at: Option<DateTime<Utc>>,
    ) -> bool {
        let phase = PaymentAttemptPhase::from_persisted(status, submitted_at);
        match self {
            Self::PreparedFailure { .. } => phase.is_prepared(),
            Self::AdmittedFailure { .. } => phase.is_admitted(),
            Self::Submitted { .. } | Self::Unknown { .. } => phase.is_resolvable(),
        }
    }

    pub(super) fn resolved_status(
        self,
        operation: ReservationOperation,
        current: PaymentAttemptStatus,
    ) -> AttemptResolutionStatus {
        match self {
            Self::PreparedFailure { .. } | Self::AdmittedFailure { .. } => {
                AttemptResolutionStatus::Failed
            }
            Self::Submitted { status, .. } => status,
            Self::Unknown { .. }
                if operation.preserves_review_required_for_unknown()
                    && current == PaymentAttemptStatus::ReviewRequired =>
            {
                AttemptResolutionStatus::ReviewRequired
            }
            Self::Unknown { .. } => AttemptResolutionStatus::Unknown,
        }
    }

    pub(crate) fn clears_submitted_at(self) -> bool {
        matches!(self, Self::AdmittedFailure { .. })
    }

    pub(crate) fn records_pending_evidence(self, status: AttemptResolutionStatus) -> bool {
        matches!(self, Self::Unknown { .. }) && status != AttemptResolutionStatus::ReviewRequired
    }

    pub(crate) fn marks_renewal_past_due(self, status: AttemptResolutionStatus) -> bool {
        matches!(self, Self::Submitted { .. })
            && matches!(
                status,
                AttemptResolutionStatus::Declined | AttemptResolutionStatus::Failed
            )
    }
}
