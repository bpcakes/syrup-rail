use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PaymentResolutionCode {
    SubscriptionInitialCurrentSubscriptionConflict,
    SubscriptionInitialCurrentGrantConflict,
    SubscriptionInitialExternallyRefunded,
    SubscriptionInitialExternallyVoided,
    ProcessorChargeExternallyRefunded,
    ProcessorChargeExternallyVoided,
    SubscriptionInitialPreparedAttemptExpired,
    SubscriptionRenewalRetryStateChangedBeforeCharge,
    GatewayLiveReadinessFailedBeforeSubmission,
    GatewayMalformedBeforeSubmission,
    GatewayRequestRejectedBeforeSubmission,
    GatewayConfigurationBeforeSubmission,
    GatewayUnavailableBeforeSubmission,
    GatewayProviderRateLimitedBeforeSubmission,
    GatewayAccountMutationCooldownBeforeSubmission,
    HostChargeApprovedStaleState,
    SubscriptionApprovedRenewalStaleState,
    SubscriptionApprovedRecoveryStaleState,
    SubscriptionApprovedRecoveryInactiveReplacementMethod,
    SubscriptionApprovedPaymentMethodUpdateSubscriptionIneligible,
    SubscriptionApprovedPaymentMethodUpdateStaleState,
    SubscriptionApprovedPaymentMethodUpdateInactiveReplacementMethod,
}

impl PaymentResolutionCode {
    pub const ALL: &'static [Self] = &[
        Self::SubscriptionInitialCurrentSubscriptionConflict,
        Self::SubscriptionInitialCurrentGrantConflict,
        Self::SubscriptionInitialExternallyRefunded,
        Self::SubscriptionInitialExternallyVoided,
        Self::ProcessorChargeExternallyRefunded,
        Self::ProcessorChargeExternallyVoided,
        Self::SubscriptionInitialPreparedAttemptExpired,
        Self::SubscriptionRenewalRetryStateChangedBeforeCharge,
        Self::GatewayLiveReadinessFailedBeforeSubmission,
        Self::GatewayMalformedBeforeSubmission,
        Self::GatewayRequestRejectedBeforeSubmission,
        Self::GatewayConfigurationBeforeSubmission,
        Self::GatewayUnavailableBeforeSubmission,
        Self::GatewayProviderRateLimitedBeforeSubmission,
        Self::GatewayAccountMutationCooldownBeforeSubmission,
        Self::HostChargeApprovedStaleState,
        Self::SubscriptionApprovedRenewalStaleState,
        Self::SubscriptionApprovedRecoveryStaleState,
        Self::SubscriptionApprovedRecoveryInactiveReplacementMethod,
        Self::SubscriptionApprovedPaymentMethodUpdateSubscriptionIneligible,
        Self::SubscriptionApprovedPaymentMethodUpdateStaleState,
        Self::SubscriptionApprovedPaymentMethodUpdateInactiveReplacementMethod,
    ];

    pub const RENEWAL_RETRY_ACCOUNTING_EXCLUDED: &'static [Self] = &[
        Self::SubscriptionRenewalRetryStateChangedBeforeCharge,
        Self::GatewayLiveReadinessFailedBeforeSubmission,
        Self::GatewayMalformedBeforeSubmission,
        Self::GatewayRequestRejectedBeforeSubmission,
        Self::GatewayConfigurationBeforeSubmission,
        Self::GatewayUnavailableBeforeSubmission,
        Self::GatewayProviderRateLimitedBeforeSubmission,
        Self::GatewayAccountMutationCooldownBeforeSubmission,
    ];

    pub const RENEWAL_INFRASTRUCTURE_RETRY_CODES: &'static [Self] = &[
        Self::GatewayLiveReadinessFailedBeforeSubmission,
        Self::GatewayMalformedBeforeSubmission,
        Self::GatewayRequestRejectedBeforeSubmission,
        Self::GatewayConfigurationBeforeSubmission,
        Self::GatewayUnavailableBeforeSubmission,
        Self::GatewayProviderRateLimitedBeforeSubmission,
        Self::GatewayAccountMutationCooldownBeforeSubmission,
    ];

    pub const RENEWAL_RETRY_PACING_EXCLUDED: &'static [Self] = &[
        Self::SubscriptionRenewalRetryStateChangedBeforeCharge,
        Self::GatewayProviderRateLimitedBeforeSubmission,
        Self::GatewayAccountMutationCooldownBeforeSubmission,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SubscriptionInitialCurrentSubscriptionConflict => {
                "subscription_initial_current_subscription_conflict"
            }
            Self::SubscriptionInitialCurrentGrantConflict => {
                "subscription_initial_current_grant_conflict"
            }
            Self::SubscriptionInitialExternallyRefunded => {
                "subscription_initial_externally_refunded"
            }
            Self::SubscriptionInitialExternallyVoided => "subscription_initial_externally_voided",
            Self::ProcessorChargeExternallyRefunded => "processor_charge_externally_refunded",
            Self::ProcessorChargeExternallyVoided => "processor_charge_externally_voided",
            Self::SubscriptionInitialPreparedAttemptExpired => {
                "subscription_initial_prepared_attempt_expired"
            }
            Self::SubscriptionRenewalRetryStateChangedBeforeCharge => {
                "subscription_renewal_retry_state_changed_before_charge"
            }
            Self::GatewayLiveReadinessFailedBeforeSubmission => {
                "gateway_live_readiness_failed_before_submission"
            }
            Self::GatewayMalformedBeforeSubmission => "gateway_malformed_before_submission",
            Self::GatewayRequestRejectedBeforeSubmission => {
                "gateway_request_rejected_before_submission"
            }
            Self::GatewayConfigurationBeforeSubmission => "gateway_configuration_before_submission",
            Self::GatewayUnavailableBeforeSubmission => "gateway_unavailable_before_submission",
            Self::GatewayProviderRateLimitedBeforeSubmission => {
                "gateway_provider_rate_limited_before_submission"
            }
            Self::GatewayAccountMutationCooldownBeforeSubmission => {
                "gateway_account_mutation_cooldown_before_submission"
            }
            Self::HostChargeApprovedStaleState => "host_charge_approved_stale_state",
            Self::SubscriptionApprovedRenewalStaleState => {
                "subscription_approved_renewal_stale_state"
            }
            Self::SubscriptionApprovedRecoveryStaleState => {
                "subscription_approved_recovery_stale_state"
            }
            Self::SubscriptionApprovedRecoveryInactiveReplacementMethod => {
                "subscription_approved_recovery_inactive_replacement_method"
            }
            Self::SubscriptionApprovedPaymentMethodUpdateSubscriptionIneligible => {
                "subscription_approved_payment_method_update_subscription_ineligible"
            }
            Self::SubscriptionApprovedPaymentMethodUpdateStaleState => {
                "subscription_approved_payment_method_update_stale_state"
            }
            Self::SubscriptionApprovedPaymentMethodUpdateInactiveReplacementMethod => {
                "subscription_approved_payment_method_update_inactive_replacement_method"
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("unknown payment resolution code")]
pub struct PaymentResolutionCodeParseError;

impl TryFrom<&str> for PaymentResolutionCode {
    type Error = PaymentResolutionCodeParseError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "subscription_initial_current_subscription_conflict" => {
                Ok(Self::SubscriptionInitialCurrentSubscriptionConflict)
            }
            "subscription_initial_current_grant_conflict" => {
                Ok(Self::SubscriptionInitialCurrentGrantConflict)
            }
            "subscription_initial_externally_refunded" => {
                Ok(Self::SubscriptionInitialExternallyRefunded)
            }
            "subscription_initial_externally_voided" => {
                Ok(Self::SubscriptionInitialExternallyVoided)
            }
            "processor_charge_externally_refunded" => Ok(Self::ProcessorChargeExternallyRefunded),
            "processor_charge_externally_voided" => Ok(Self::ProcessorChargeExternallyVoided),
            "subscription_initial_prepared_attempt_expired" => {
                Ok(Self::SubscriptionInitialPreparedAttemptExpired)
            }
            "subscription_renewal_retry_state_changed_before_charge" => {
                Ok(Self::SubscriptionRenewalRetryStateChangedBeforeCharge)
            }
            "gateway_live_readiness_failed_before_submission" => {
                Ok(Self::GatewayLiveReadinessFailedBeforeSubmission)
            }
            "gateway_malformed_before_submission" => Ok(Self::GatewayMalformedBeforeSubmission),
            "gateway_request_rejected_before_submission" => {
                Ok(Self::GatewayRequestRejectedBeforeSubmission)
            }
            "gateway_configuration_before_submission" => {
                Ok(Self::GatewayConfigurationBeforeSubmission)
            }
            "gateway_unavailable_before_submission" => Ok(Self::GatewayUnavailableBeforeSubmission),
            "gateway_provider_rate_limited_before_submission" => {
                Ok(Self::GatewayProviderRateLimitedBeforeSubmission)
            }
            "gateway_account_mutation_cooldown_before_submission" => {
                Ok(Self::GatewayAccountMutationCooldownBeforeSubmission)
            }
            "host_charge_approved_stale_state" => Ok(Self::HostChargeApprovedStaleState),
            "subscription_approved_renewal_stale_state" => {
                Ok(Self::SubscriptionApprovedRenewalStaleState)
            }
            "subscription_approved_recovery_stale_state" => {
                Ok(Self::SubscriptionApprovedRecoveryStaleState)
            }
            "subscription_approved_recovery_inactive_replacement_method" => {
                Ok(Self::SubscriptionApprovedRecoveryInactiveReplacementMethod)
            }
            "subscription_approved_payment_method_update_subscription_ineligible" => {
                Ok(Self::SubscriptionApprovedPaymentMethodUpdateSubscriptionIneligible)
            }
            "subscription_approved_payment_method_update_stale_state" => {
                Ok(Self::SubscriptionApprovedPaymentMethodUpdateStaleState)
            }
            "subscription_approved_payment_method_update_inactive_replacement_method" => {
                Ok(Self::SubscriptionApprovedPaymentMethodUpdateInactiveReplacementMethod)
            }
            _ => Err(PaymentResolutionCodeParseError),
        }
    }
}

impl TryFrom<String> for PaymentResolutionCode {
    type Error = PaymentResolutionCodeParseError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_from(value.as_str())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn all_twenty_two_canonical_values_round_trip_exhaustively() {
        assert_eq!(PaymentResolutionCode::ALL.len(), 22);
        let values = PaymentResolutionCode::ALL
            .iter()
            .map(|code| code.as_str())
            .collect::<HashSet<_>>();
        assert_eq!(values.len(), 22);
        for code in PaymentResolutionCode::ALL {
            assert_eq!(PaymentResolutionCode::try_from(code.as_str()), Ok(*code));
        }
        assert_eq!(
            PaymentResolutionCode::try_from("base_subscription_initial_prepared_attempt_expired"),
            Err(PaymentResolutionCodeParseError)
        );
        assert_eq!(
            PaymentResolutionCode::try_from("nmi_provider_rate_limited_before_submission"),
            Err(PaymentResolutionCodeParseError)
        );
    }

    #[test]
    fn retry_policy_sets_preserve_the_characterized_membership() {
        assert_eq!(
            PaymentResolutionCode::RENEWAL_RETRY_ACCOUNTING_EXCLUDED.len(),
            8
        );
        assert_eq!(
            PaymentResolutionCode::RENEWAL_INFRASTRUCTURE_RETRY_CODES.len(),
            7
        );
        assert_eq!(
            PaymentResolutionCode::RENEWAL_RETRY_PACING_EXCLUDED.len(),
            3
        );
        assert!(
            PaymentResolutionCode::RENEWAL_RETRY_PACING_EXCLUDED
                .contains(&PaymentResolutionCode::GatewayProviderRateLimitedBeforeSubmission)
        );
    }
}
