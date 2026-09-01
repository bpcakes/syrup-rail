use std::fmt;

use syrup_rail::{
    GatewayAccountMode, GatewayDiagnostic, GatewayError, GatewayMutationError,
    GatewayNotSubmittedError, GatewayPaymentOutcome, GatewaySaleRequest,
    GatewayStorePaymentMethodRequest, PaymentAttemptIdentity, ResolvedGateway,
};
use thiserror::Error;

/// Why a resolved gateway could not be authorized for a provider submission.
#[derive(Error)]
#[non_exhaustive]
pub enum GatewayAccountModeVerificationError {
    /// The account answered successfully but is not in the trusted deployment mode.
    #[error("gateway account mode does not match the required deployment mode")]
    AccountModeMismatch {
        /// Mode required by trusted service or low-level caller configuration.
        required: GatewayAccountMode,
        /// Mode observed from the provider account for this submission authority.
        observed: GatewayAccountMode,
    },
    /// The provider account-mode query failed.
    #[error("gateway account mode query failed")]
    Gateway(#[source] GatewayError),
}

impl fmt::Debug for GatewayAccountModeVerificationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AccountModeMismatch { required, observed } => formatter
                .debug_struct("GatewayAccountModeVerificationError::AccountModeMismatch")
                .field("required", required)
                .field("observed", observed)
                .finish(),
            Self::Gateway(error) => formatter
                .debug_tuple("GatewayAccountModeVerificationError::Gateway")
                .field(error)
                .finish(),
        }
    }
}

/// Opaque readiness capability for a resolved gateway and required account mode.
///
/// This value is intentionally non-cloneable and can be constructed only by
/// [`verify_gateway_account_mode`]. Supported low-level submission functions
/// consume it, compare its mode and gateway identity with the durable admitted
/// attempt, and re-query the account mode immediately before the real provider
/// mutation.
///
/// Account-mode lookup and provider mutation are separate NMI requests, so the
/// first lookup is only an early readiness observation. The mandatory second
/// lookup narrows, but cannot eliminate, a provider-side race. Deployments that
/// require hard test/live isolation should use separate gateway accounts.
#[must_use = "a verified gateway must be consumed by an admitted provider submission"]
pub struct ModeVerifiedGateway<'gateway> {
    gateway: &'gateway ResolvedGateway,
    required_mode: GatewayAccountMode,
}

impl fmt::Debug for ModeVerifiedGateway<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModeVerifiedGateway")
            .field("required_mode", &self.required_mode)
            .finish_non_exhaustive()
    }
}

#[must_use = "an attempt-verified gateway must perform its provider submission"]
pub(crate) struct AttemptVerifiedGateway<'gateway> {
    gateway: &'gateway ResolvedGateway,
    required_mode: GatewayAccountMode,
}

pub(crate) fn gateway_account_mode_mismatch_detail() -> GatewayDiagnostic {
    GatewayDiagnostic::new(
        "Payment was not submitted because the payment processor account mode did not match this deployment.",
    )
}

fn normalize_adapter_mutation_error(error: GatewayMutationError) -> GatewayMutationError {
    match error {
        GatewayMutationError::NotSubmitted(GatewayNotSubmittedError::AccountModeMismatch {
            detail,
            ..
        }) => GatewayMutationError::NotSubmitted(GatewayNotSubmittedError::Malformed(detail)),
        GatewayMutationError::NotSubmitted(GatewayNotSubmittedError::AccountModeVerification(
            error,
        )) => GatewayMutationError::NotSubmitted(GatewayNotSubmittedError::Malformed(
            error.detail().clone(),
        )),
        other => other,
    }
}

impl<'gateway> ModeVerifiedGateway<'gateway> {
    pub(crate) const fn resolved_gateway(&self) -> &'gateway ResolvedGateway {
        self.gateway
    }

    pub(crate) fn authorize_attempt(
        self,
        identity: &PaymentAttemptIdentity,
    ) -> Option<AttemptVerifiedGateway<'gateway>> {
        (self.required_mode == identity.required_gateway_account_mode()
            && self.gateway.billing_scope_id() == identity.billing_scope_id()
            && self.gateway.gateway_account_id() == identity.gateway_account_id()
            && self.gateway.gateway_configuration_id() == identity.gateway_configuration_id())
        .then_some(AttemptVerifiedGateway {
            gateway: self.gateway,
            required_mode: self.required_mode,
        })
    }
}

impl AttemptVerifiedGateway<'_> {
    pub(crate) const fn provider_key(&self) -> &syrup_rail::GatewayProviderKey {
        self.gateway.provider_key()
    }

    async fn verify_current_mode(&self) -> Result<(), GatewayMutationError> {
        let observed = self.gateway.account_mode().await.map_err(|error| {
            GatewayMutationError::NotSubmitted(GatewayNotSubmittedError::AccountModeVerification(
                error,
            ))
        })?;
        if observed != self.required_mode {
            return Err(GatewayMutationError::NotSubmitted(
                GatewayNotSubmittedError::AccountModeMismatch {
                    required: self.required_mode,
                    observed,
                    detail: gateway_account_mode_mismatch_detail(),
                },
            ));
        }
        Ok(())
    }

    pub(crate) async fn sale(
        self,
        request: GatewaySaleRequest,
    ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
        self.verify_current_mode().await?;
        self.gateway
            .sale(request)
            .await
            .map_err(normalize_adapter_mutation_error)
    }

    pub(crate) async fn store_payment_method(
        self,
        request: GatewayStorePaymentMethodRequest,
    ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
        self.verify_current_mode().await?;
        self.gateway
            .store_payment_method(request)
            .await
            .map_err(normalize_adapter_mutation_error)
    }
}

/// Queries the real provider account and mints one readiness capability only
/// when its observed mode exactly matches the trusted required mode. Consuming
/// the capability for submission performs another account-mode query.
///
/// A successful `Test` verification still authorizes the real gateway API
/// request; NMI's account-wide TEST setting determines simulated processing.
pub async fn verify_gateway_account_mode(
    gateway: &ResolvedGateway,
    required_mode: GatewayAccountMode,
) -> Result<ModeVerifiedGateway<'_>, GatewayAccountModeVerificationError> {
    let observed_mode = gateway
        .account_mode()
        .await
        .map_err(GatewayAccountModeVerificationError::Gateway)?;
    if observed_mode != required_mode {
        return Err(GatewayAccountModeVerificationError::AccountModeMismatch {
            required: required_mode,
            observed: observed_mode,
        });
    }
    Ok(ModeVerifiedGateway {
        gateway,
        required_mode,
    })
}
