use super::*;

/// Complete persistence behavior for a host charge resolved before provider
/// submission.
#[derive(Clone, Copy)]
pub(crate) struct HostChargeBeforeSubmissionResolution {
    pub(super) boundary: OutcomeResolutionBoundary,
    pub(super) cooldown: Option<RateLimitCooldown>,
}

#[derive(Clone, Copy)]
pub(super) struct HostChargeNonApprovedResolution<'provider> {
    pub(super) command: OutcomeResolutionCommand,
    pub(super) cooldown: Option<(&'provider GatewayProviderKey, RateLimitCooldown)>,
}

impl HostChargeNonApprovedResolution<'_> {
    pub(super) const fn submitted(status: AttemptResolutionStatus) -> Self {
        Self {
            command: OutcomeResolutionCommand::submitted(status, None, None),
            cooldown: None,
        }
    }
}

impl HostChargeBeforeSubmissionResolution {
    pub(crate) const fn prepared() -> Self {
        Self {
            boundary: OutcomeResolutionBoundary::Prepared,
            cooldown: None,
        }
    }

    pub(crate) const fn admitted_not_submitted(proof: NonSubmissionProof) -> Self {
        Self {
            boundary: OutcomeResolutionBoundary::AdmittedNotSubmitted(proof),
            cooldown: None,
        }
    }

    pub(crate) const fn with_not_submitted_policy(self, policy: GatewayNotSubmittedPolicy) -> Self {
        Self {
            boundary: self.boundary,
            cooldown: policy.cooldown(),
        }
    }

    pub(super) const fn with_provider(
        self,
        provider_key: &GatewayProviderKey,
        code: PaymentResolutionCode,
    ) -> HostChargeNonApprovedResolution<'_> {
        HostChargeNonApprovedResolution {
            command: OutcomeResolutionCommand::local_failure(
                self.boundary,
                Some(code),
                self.cooldown,
            ),
            cooldown: match self.cooldown {
                Some(cooldown) => Some((provider_key, cooldown)),
                None => None,
            },
        }
    }
}
