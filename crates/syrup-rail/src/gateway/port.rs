use std::{fmt, sync::Arc};

use async_trait::async_trait;
use thiserror::Error;

use super::{
    GatewayAccountMode, GatewayPaymentOutcome, GatewayQueryRequest, GatewaySaleRequest,
    GatewayStorePaymentMethodRequest, GatewayTransactionReport, GatewayTransactionReportRequest,
};
use crate::{GatewayDiagnostic, GatewayOrderId, PaymentAttemptId, PaymentAttemptKind};

#[derive(Error)]
pub enum GatewayError {
    #[error("gateway rejected the request before processing")]
    RequestRejected(GatewayDiagnostic),
    #[error("gateway response was malformed")]
    Malformed(GatewayDiagnostic),
    #[error("gateway configuration is invalid")]
    Configuration(GatewayDiagnostic),
    #[error("gateway is unavailable")]
    Unavailable(GatewayDiagnostic),
    #[error("gateway rate limit exceeded")]
    RateLimited(GatewayDiagnostic),
}

impl fmt::Debug for GatewayError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (variant, detail) = match self {
            Self::RequestRejected(detail) => ("RequestRejected", detail),
            Self::Malformed(detail) => ("Malformed", detail),
            Self::Configuration(detail) => ("Configuration", detail),
            Self::Unavailable(detail) => ("Unavailable", detail),
            Self::RateLimited(detail) => ("RateLimited", detail),
        };
        formatter
            .debug_struct(variant)
            .field("has_detail", &(!detail.is_empty()))
            .finish()
    }
}

impl GatewayError {
    pub const fn detail(&self) -> &GatewayDiagnostic {
        match self {
            Self::RequestRejected(detail)
            | Self::Malformed(detail)
            | Self::Configuration(detail)
            | Self::Unavailable(detail)
            | Self::RateLimited(detail) => detail,
        }
    }
}

/// Proof from a gateway adapter that the provider never received a mutation.
///
/// Returning any variant authorizes ledger-aware callers to restore prepared
/// work and retry the same provider mutation when policy permits. Adapters must
/// use [`GatewayMutationError::Indeterminate`] once request transmission may
/// have begun. Misclassifying an in-flight request as not submitted can cause a
/// duplicate provider mutation on same-key retry.
///
/// This enum is intentionally exhaustive: adding a variant must break every
/// persistence adapter at compile time so retry safety, durable resolution,
/// cooldown scope, and host-target consequences are classified together.
#[derive(Error)]
pub enum GatewayNotSubmittedError {
    #[error("gateway rejected the mutation request")]
    RequestRejected(GatewayDiagnostic),
    #[error("gateway mutation request is malformed")]
    Malformed(GatewayDiagnostic),
    #[error("gateway mutation configuration is invalid")]
    Configuration(GatewayDiagnostic),
    /// Transport failed before request transmission began. This is not a
    /// generic transient transport error: returning it certifies that the
    /// provider could not have received the mutation.
    #[error("gateway mutation was not transmitted")]
    NotTransmitted(GatewayDiagnostic),
    #[error("gateway mutation was rate limited before submission")]
    RateLimited(GatewayDiagnostic),
    /// Reserved for a caller-owned account-mode check performed immediately
    /// before invoking the mutation endpoint.
    ///
    /// Gateway adapters must not return this variant from `sale` or
    /// `store_payment_method`; verified submission wrappers normalize any such
    /// adapter response to an ordinary terminal not-submitted error.
    #[error("gateway account mode changed before mutation submission")]
    AccountModeMismatch {
        required: GatewayAccountMode,
        observed: GatewayAccountMode,
        detail: GatewayDiagnostic,
    },
    /// Reserved for a caller-owned account-mode query performed immediately
    /// before invoking the mutation endpoint. Gateway adapters must not return
    /// this variant from mutation methods.
    #[error("gateway account mode could not be verified before mutation submission")]
    AccountModeVerification(#[source] GatewayError),
}

impl fmt::Debug for GatewayNotSubmittedError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (variant, detail) = match self {
            Self::RequestRejected(detail) => ("RequestRejected", detail),
            Self::Malformed(detail) => ("Malformed", detail),
            Self::Configuration(detail) => ("Configuration", detail),
            Self::NotTransmitted(detail) => ("NotTransmitted", detail),
            Self::RateLimited(detail) => ("RateLimited", detail),
            Self::AccountModeMismatch {
                required,
                observed,
                detail,
            } => {
                return formatter
                    .debug_struct("AccountModeMismatch")
                    .field("required", required)
                    .field("observed", observed)
                    .field("has_detail", &(!detail.is_empty()))
                    .finish();
            }
            Self::AccountModeVerification(error) => {
                return formatter
                    .debug_tuple("AccountModeVerification")
                    .field(error)
                    .finish();
            }
        };
        formatter
            .debug_struct(variant)
            .field("has_detail", &(!detail.is_empty()))
            .finish()
    }
}

impl GatewayNotSubmittedError {
    pub const fn detail(&self) -> &GatewayDiagnostic {
        match self {
            Self::RequestRejected(detail)
            | Self::Malformed(detail)
            | Self::Configuration(detail)
            | Self::NotTransmitted(detail)
            | Self::RateLimited(detail) => detail,
            Self::AccountModeMismatch { detail, .. } => detail,
            Self::AccountModeVerification(error) => error.detail(),
        }
    }
}

/// Gateway mutation failure classified by whether provider receipt is possible.
///
/// Adapter implementations must return `NotSubmitted` only with proof that no
/// request bytes could have reached the provider. Once transmission may have
/// begun, return an indeterminate variant even if the transport later reports
/// an ordinary unavailable or rate-limit error.
#[derive(Error)]
pub enum GatewayMutationError {
    #[error("gateway mutation was not submitted")]
    NotSubmitted(#[source] GatewayNotSubmittedError),
    #[error("gateway mutation was rate limited with an indeterminate outcome")]
    RateLimitedIndeterminate(GatewayDiagnostic),
    #[error("gateway mutation outcome is indeterminate")]
    Indeterminate(GatewayDiagnostic),
}

impl fmt::Debug for GatewayMutationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotSubmitted(error) => {
                formatter.debug_tuple("NotSubmitted").field(error).finish()
            }
            Self::RateLimitedIndeterminate(detail) => formatter
                .debug_struct("RateLimitedIndeterminate")
                .field("has_detail", &(!detail.is_empty()))
                .finish(),
            Self::Indeterminate(detail) => formatter
                .debug_struct("Indeterminate")
                .field("has_detail", &(!detail.is_empty()))
                .finish(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MutationCertainty {
    /// The adapter certifies that the provider never received the request.
    /// Ledger-aware callers may use this proof to permit same-key resubmission.
    NotSubmitted,
    Indeterminate,
}

impl GatewayMutationError {
    pub const fn detail(&self) -> &GatewayDiagnostic {
        match self {
            Self::NotSubmitted(error) => error.detail(),
            Self::RateLimitedIndeterminate(detail) | Self::Indeterminate(detail) => detail,
        }
    }

    pub const fn certainty(&self) -> MutationCertainty {
        match self {
            Self::NotSubmitted(_) => MutationCertainty::NotSubmitted,
            Self::RateLimitedIndeterminate(_) | Self::Indeterminate(_) => {
                MutationCertainty::Indeterminate
            }
        }
    }
}

#[async_trait]
pub trait PaymentGateway: Send + Sync {
    async fn account_mode(&self) -> Result<GatewayAccountMode, GatewayError>;

    async fn sale(
        &self,
        request: GatewaySaleRequest,
    ) -> Result<GatewayPaymentOutcome, GatewayMutationError>;

    async fn store_payment_method(
        &self,
        request: GatewayStorePaymentMethodRequest,
    ) -> Result<GatewayPaymentOutcome, GatewayMutationError>;

    async fn query_transaction(
        &self,
        request: GatewayQueryRequest,
    ) -> Result<Option<GatewayPaymentOutcome>, GatewayError>;

    async fn query_transaction_reports(
        &self,
        request: GatewayTransactionReportRequest,
    ) -> Result<Vec<GatewayTransactionReport>, GatewayError>;
}

pub trait GatewayMutationReferenceFactory: Send + Sync {
    fn for_attempt(&self, kind: PaymentAttemptKind, attempt_id: PaymentAttemptId)
    -> GatewayOrderId;
}

pub type SharedPaymentGateway = Arc<dyn PaymentGateway>;
pub type SharedGatewayMutationReferenceFactory = Arc<dyn GatewayMutationReferenceFactory>;
