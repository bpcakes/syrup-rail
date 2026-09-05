use std::fmt;

use chrono::{DateTime, Utc};
use thiserror::Error;

use crate::{CumulativeRefundCents, GatewayDiagnostic, GatewayOrderId, GatewayTransactionId};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PaymentReversalKind {
    Refunded,
    Voided,
    Chargeback,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GatewayLifecycleState {
    Unknown,
    PendingSettlement,
    Settled {
        cumulative_refunded_cents: Option<CumulativeRefundCents>,
    },
    Voided,
    Refunded {
        cumulative_refunded_cents: CumulativeRefundCents,
    },
    Chargeback {
        cumulative_refunded_cents: Option<CumulativeRefundCents>,
    },
}

impl GatewayLifecycleState {
    pub const fn full_reversal_kind(&self) -> Option<PaymentReversalKind> {
        match self {
            Self::Voided => Some(PaymentReversalKind::Voided),
            Self::Refunded { .. } => Some(PaymentReversalKind::Refunded),
            Self::Chargeback { .. } => Some(PaymentReversalKind::Chargeback),
            Self::Unknown | Self::PendingSettlement | Self::Settled { .. } => None,
        }
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum GatewayLifecycleEvidenceError {
    #[error("gateway lifecycle evidence requires a transaction ID or order ID")]
    MissingLocator,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GatewayLifecycleEvidence {
    transaction_id: Option<GatewayTransactionId>,
    order_id: Option<GatewayOrderId>,
    state: GatewayLifecycleState,
    condition: Option<GatewayDiagnostic>,
    action: Option<GatewayDiagnostic>,
    effective_at: Option<DateTime<Utc>>,
}

impl GatewayLifecycleEvidence {
    pub fn new(
        transaction_id: Option<GatewayTransactionId>,
        order_id: Option<GatewayOrderId>,
        state: GatewayLifecycleState,
        condition: Option<GatewayDiagnostic>,
        action: Option<GatewayDiagnostic>,
        effective_at: Option<DateTime<Utc>>,
    ) -> Result<Self, GatewayLifecycleEvidenceError> {
        if transaction_id.is_none() && order_id.is_none() {
            return Err(GatewayLifecycleEvidenceError::MissingLocator);
        }
        Ok(Self {
            transaction_id,
            order_id,
            state,
            condition,
            action,
            effective_at,
        })
    }

    pub const fn transaction_id(&self) -> Option<&GatewayTransactionId> {
        self.transaction_id.as_ref()
    }

    pub const fn order_id(&self) -> Option<&GatewayOrderId> {
        self.order_id.as_ref()
    }

    pub const fn state(&self) -> &GatewayLifecycleState {
        &self.state
    }

    pub const fn condition(&self) -> Option<&GatewayDiagnostic> {
        self.condition.as_ref()
    }

    pub const fn action(&self) -> Option<&GatewayDiagnostic> {
        self.action.as_ref()
    }

    pub const fn effective_at(&self) -> Option<&DateTime<Utc>> {
        self.effective_at.as_ref()
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum GatewayLifecycleQuarantineReason {
    AmbiguousReversalSuccess,
    InvalidRefundEconomics,
    MalformedReportStructure,
}

impl GatewayLifecycleQuarantineReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AmbiguousReversalSuccess => "ambiguous_reversal_success",
            Self::InvalidRefundEconomics => "invalid_refund_economics",
            Self::MalformedReportStructure => "malformed_report_structure",
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct GatewayLifecycleQuarantineResolutionReason(String);

impl GatewayLifecycleQuarantineResolutionReason {
    pub fn new(
        value: impl Into<String>,
    ) -> Result<Self, GatewayLifecycleQuarantineResolutionReasonError> {
        crate::audit_reason::normalize_audit_reason(value)
            .map(Self)
            .map_err(|error| match error {
                crate::audit_reason::ReasonValidationError::Empty => {
                    GatewayLifecycleQuarantineResolutionReasonError::Empty
                }
                crate::audit_reason::ReasonValidationError::TooLong => {
                    GatewayLifecycleQuarantineResolutionReasonError::TooLong
                }
                crate::audit_reason::ReasonValidationError::ContainsRawCardData => {
                    GatewayLifecycleQuarantineResolutionReasonError::ContainsRawCardData
                }
            })
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for GatewayLifecycleQuarantineResolutionReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("GatewayLifecycleQuarantineResolutionReason([redacted])")
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum GatewayLifecycleQuarantineResolutionReasonError {
    #[error("gateway lifecycle quarantine resolution reason is empty")]
    Empty,
    #[error("gateway lifecycle quarantine resolution reason exceeds 500 characters")]
    TooLong,
    #[error("gateway lifecycle quarantine resolution reason contains raw payment card data")]
    ContainsRawCardData,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum GatewayLifecycleQuarantineError {
    #[error("gateway lifecycle quarantine requires a locator unless the report is malformed")]
    MissingLocator,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GatewayLifecycleQuarantine {
    transaction_id: Option<GatewayTransactionId>,
    order_id: Option<GatewayOrderId>,
    reason: GatewayLifecycleQuarantineReason,
}

impl GatewayLifecycleQuarantine {
    pub fn new(
        transaction_id: Option<GatewayTransactionId>,
        order_id: Option<GatewayOrderId>,
        reason: GatewayLifecycleQuarantineReason,
    ) -> Result<Self, GatewayLifecycleQuarantineError> {
        if transaction_id.is_none()
            && order_id.is_none()
            && reason != GatewayLifecycleQuarantineReason::MalformedReportStructure
        {
            return Err(GatewayLifecycleQuarantineError::MissingLocator);
        }
        Ok(Self {
            transaction_id,
            order_id,
            reason,
        })
    }

    pub const fn transaction_id(&self) -> Option<&GatewayTransactionId> {
        self.transaction_id.as_ref()
    }

    pub const fn order_id(&self) -> Option<&GatewayOrderId> {
        self.order_id.as_ref()
    }

    pub const fn reason(&self) -> GatewayLifecycleQuarantineReason {
        self.reason
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GatewayTransactionReport {
    Ignore,
    Evidence(GatewayLifecycleEvidence),
    Quarantine(GatewayLifecycleQuarantine),
}
