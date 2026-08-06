use std::{fmt, num::NonZeroU32, sync::Arc};

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use thiserror::Error;

use crate::{
    BillingContact, ChargeAmount, CumulativeRefundCents, GatewayDiagnostic,
    GatewayLifecycleCursorKey, GatewayOrderId, GatewayPaymentMethodReference, GatewayTransactionId,
    PaymentAttemptId, PaymentAttemptKind, PaymentToken,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GatewayAccountMode {
    Live,
    Test,
}

#[derive(Clone)]
pub enum GatewaySaleIntent {
    OneTime {
        payment_token: PaymentToken,
    },
    InitialStoredCredential {
        payment_token: PaymentToken,
    },
    RecurringStoredCredential {
        payment_method_reference: GatewayPaymentMethodReference,
        initial_transaction_id: GatewayTransactionId,
    },
}

impl fmt::Debug for GatewaySaleIntent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OneTime { payment_token } => formatter
                .debug_struct("OneTime")
                .field("payment_token", payment_token)
                .finish(),
            Self::InitialStoredCredential { payment_token } => formatter
                .debug_struct("InitialStoredCredential")
                .field("payment_token", payment_token)
                .finish(),
            Self::RecurringStoredCredential {
                payment_method_reference,
                initial_transaction_id,
            } => formatter
                .debug_struct("RecurringStoredCredential")
                .field("payment_method_reference", payment_method_reference)
                .field("initial_transaction_id", initial_transaction_id)
                .finish(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct GatewaySaleRequest {
    charge: ChargeAmount,
    order_id: GatewayOrderId,
    intent: GatewaySaleIntent,
    billing_contact: Option<BillingContact>,
}

impl GatewaySaleRequest {
    pub const fn new(
        charge: ChargeAmount,
        order_id: GatewayOrderId,
        intent: GatewaySaleIntent,
        billing_contact: Option<BillingContact>,
    ) -> Self {
        Self {
            charge,
            order_id,
            intent,
            billing_contact,
        }
    }

    pub const fn charge(&self) -> ChargeAmount {
        self.charge
    }

    pub const fn order_id(&self) -> &GatewayOrderId {
        &self.order_id
    }

    pub const fn intent(&self) -> &GatewaySaleIntent {
        &self.intent
    }

    pub const fn billing_contact(&self) -> Option<&BillingContact> {
        self.billing_contact.as_ref()
    }

    pub fn into_parts(
        self,
    ) -> (
        ChargeAmount,
        GatewayOrderId,
        GatewaySaleIntent,
        Option<BillingContact>,
    ) {
        (
            self.charge,
            self.order_id,
            self.intent,
            self.billing_contact,
        )
    }
}

#[derive(Clone, Debug)]
pub struct GatewayStorePaymentMethodRequest {
    payment_token: PaymentToken,
    order_id: GatewayOrderId,
    billing_contact: Option<BillingContact>,
}

impl GatewayStorePaymentMethodRequest {
    pub const fn new(
        payment_token: PaymentToken,
        order_id: GatewayOrderId,
        billing_contact: Option<BillingContact>,
    ) -> Self {
        Self {
            payment_token,
            order_id,
            billing_contact,
        }
    }

    pub fn into_parts(self) -> (PaymentToken, GatewayOrderId, Option<BillingContact>) {
        (self.payment_token, self.order_id, self.billing_contact)
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum GatewayRequestError {
    #[error("gateway query requires a transaction ID or order ID")]
    MissingQuerySelector,
    #[error("gateway report window end must be after its start")]
    InvalidReportWindow,
    #[error("gateway report page size must be positive")]
    InvalidPageSize,
}

#[derive(Clone, Debug)]
pub struct GatewayQueryRequest {
    transaction_id: Option<GatewayTransactionId>,
    order_id: Option<GatewayOrderId>,
}

impl GatewayQueryRequest {
    pub fn new(
        transaction_id: Option<GatewayTransactionId>,
        order_id: Option<GatewayOrderId>,
    ) -> Result<Self, GatewayRequestError> {
        if transaction_id.is_none() && order_id.is_none() {
            return Err(GatewayRequestError::MissingQuerySelector);
        }
        Ok(Self {
            transaction_id,
            order_id,
        })
    }

    pub const fn transaction_id(&self) -> Option<&GatewayTransactionId> {
        self.transaction_id.as_ref()
    }

    pub const fn order_id(&self) -> Option<&GatewayOrderId> {
        self.order_id.as_ref()
    }

    pub fn into_parts(self) -> (Option<GatewayTransactionId>, Option<GatewayOrderId>) {
        (self.transaction_id, self.order_id)
    }
}

#[derive(Clone, Debug)]
pub struct GatewayTransactionReportRequest {
    start_at: DateTime<Utc>,
    end_at: DateTime<Utc>,
    page_size: NonZeroU32,
    page_index: u32,
}

impl GatewayTransactionReportRequest {
    pub fn new(
        start_at: DateTime<Utc>,
        end_at: DateTime<Utc>,
        page_size: u32,
        page_index: u32,
    ) -> Result<Self, GatewayRequestError> {
        if end_at <= start_at {
            return Err(GatewayRequestError::InvalidReportWindow);
        }
        let page_size = NonZeroU32::new(page_size).ok_or(GatewayRequestError::InvalidPageSize)?;
        Ok(Self {
            start_at,
            end_at,
            page_size,
            page_index,
        })
    }

    pub const fn start_at(&self) -> &DateTime<Utc> {
        &self.start_at
    }

    pub const fn end_at(&self) -> &DateTime<Utc> {
        &self.end_at
    }

    pub const fn page_size(&self) -> NonZeroU32 {
        self.page_size
    }

    pub const fn page_index(&self) -> u32 {
        self.page_index
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GatewayPaymentStatus {
    Approved,
    Declined,
    Unknown,
    Failed,
}

#[derive(Clone, Eq, PartialEq)]
pub struct CardLastFour(String);

impl CardLastFour {
    pub fn from_provider(value: &str) -> Option<Self> {
        let value = value.trim();
        (value.len() == 4 && value.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| Self(value.to_owned()))
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for CardLastFour {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CardLastFour([redacted])")
    }
}

impl fmt::Display for CardLastFour {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[redacted]")
    }
}

#[derive(Clone, Default, Eq, PartialEq)]
pub struct GatewayPaymentDescriptor {
    payment_type: Option<GatewayDiagnostic>,
    card_brand: Option<GatewayDiagnostic>,
    card_last_four: Option<CardLastFour>,
    card_exp_month: Option<i16>,
    card_exp_year: Option<i16>,
}

impl GatewayPaymentDescriptor {
    pub fn from_provider_parts(
        payment_type: Option<GatewayDiagnostic>,
        card_brand: Option<GatewayDiagnostic>,
        card_last_four: Option<&str>,
        card_exp_month: Option<i16>,
        card_exp_year: Option<i16>,
    ) -> Self {
        Self {
            payment_type,
            card_brand,
            card_last_four: card_last_four.and_then(CardLastFour::from_provider),
            card_exp_month: card_exp_month.filter(|month| (1..=12).contains(month)),
            card_exp_year: card_exp_year.filter(|year| (2000..=2100).contains(year)),
        }
    }

    pub const fn payment_type(&self) -> Option<&GatewayDiagnostic> {
        self.payment_type.as_ref()
    }

    pub const fn card_brand(&self) -> Option<&GatewayDiagnostic> {
        self.card_brand.as_ref()
    }

    pub const fn card_last_four(&self) -> Option<&CardLastFour> {
        self.card_last_four.as_ref()
    }

    pub const fn card_exp_month(&self) -> Option<i16> {
        self.card_exp_month
    }

    pub const fn card_exp_year(&self) -> Option<i16> {
        self.card_exp_year
    }
}

impl fmt::Debug for GatewayPaymentDescriptor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GatewayPaymentDescriptor")
            .field("has_payment_type", &self.payment_type.is_some())
            .field("has_card_brand", &self.card_brand.is_some())
            .field("has_card_last_four", &self.card_last_four.is_some())
            .field("card_exp_month", &self.card_exp_month)
            .field("card_exp_year", &self.card_exp_year)
            .finish()
    }
}

#[derive(Clone)]
pub struct ProcessorEvidence {
    transaction_id: Option<GatewayTransactionId>,
    payment_method_reference: Option<GatewayPaymentMethodReference>,
    response: Option<GatewayDiagnostic>,
    response_code: Option<GatewayDiagnostic>,
    response_text: Option<GatewayDiagnostic>,
    condition: Option<GatewayDiagnostic>,
    descriptor: GatewayPaymentDescriptor,
}

impl ProcessorEvidence {
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        transaction_id: Option<GatewayTransactionId>,
        payment_method_reference: Option<GatewayPaymentMethodReference>,
        response: Option<GatewayDiagnostic>,
        response_code: Option<GatewayDiagnostic>,
        response_text: Option<GatewayDiagnostic>,
        condition: Option<GatewayDiagnostic>,
        descriptor: GatewayPaymentDescriptor,
    ) -> Self {
        Self {
            transaction_id,
            payment_method_reference,
            response,
            response_code,
            response_text,
            condition,
            descriptor,
        }
    }

    pub const fn transaction_id(&self) -> Option<&GatewayTransactionId> {
        self.transaction_id.as_ref()
    }

    pub const fn payment_method_reference(&self) -> Option<&GatewayPaymentMethodReference> {
        self.payment_method_reference.as_ref()
    }

    pub const fn response(&self) -> Option<&GatewayDiagnostic> {
        self.response.as_ref()
    }

    pub const fn response_code(&self) -> Option<&GatewayDiagnostic> {
        self.response_code.as_ref()
    }

    pub const fn response_text(&self) -> Option<&GatewayDiagnostic> {
        self.response_text.as_ref()
    }

    pub const fn condition(&self) -> Option<&GatewayDiagnostic> {
        self.condition.as_ref()
    }

    pub const fn descriptor(&self) -> &GatewayPaymentDescriptor {
        &self.descriptor
    }
}

impl fmt::Debug for ProcessorEvidence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProcessorEvidence")
            .field("has_transaction_id", &self.transaction_id.is_some())
            .field(
                "has_payment_method_reference",
                &self.payment_method_reference.is_some(),
            )
            .field("has_response", &self.response.is_some())
            .field("has_response_code", &self.response_code.is_some())
            .field("has_response_text", &self.response_text.is_some())
            .field("has_condition", &self.condition.is_some())
            .field("descriptor", &self.descriptor)
            .finish()
    }
}

#[derive(Clone, Debug)]
#[must_use = "gateway payment outcomes contain authoritative provider decisions"]
pub struct GatewayPaymentOutcome {
    status: GatewayPaymentStatus,
    evidence: ProcessorEvidence,
}

impl GatewayPaymentOutcome {
    pub const fn new(status: GatewayPaymentStatus, evidence: ProcessorEvidence) -> Self {
        Self { status, evidence }
    }

    pub const fn status(&self) -> GatewayPaymentStatus {
        self.status
    }

    pub const fn evidence(&self) -> &ProcessorEvidence {
        &self.evidence
    }

    pub fn into_parts(self) -> (GatewayPaymentStatus, ProcessorEvidence) {
        (self.status, self.evidence)
    }

    pub const fn transaction_id(&self) -> Option<&GatewayTransactionId> {
        self.evidence.transaction_id()
    }

    pub const fn payment_method_reference(&self) -> Option<&GatewayPaymentMethodReference> {
        self.evidence.payment_method_reference()
    }

    pub const fn response(&self) -> Option<&GatewayDiagnostic> {
        self.evidence.response()
    }

    pub const fn response_code(&self) -> Option<&GatewayDiagnostic> {
        self.evidence.response_code()
    }

    pub const fn response_text(&self) -> Option<&GatewayDiagnostic> {
        self.evidence.response_text()
    }

    pub const fn condition(&self) -> Option<&GatewayDiagnostic> {
        self.evidence.condition()
    }

    pub const fn descriptor(&self) -> &GatewayPaymentDescriptor {
        self.evidence.descriptor()
    }
}

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

#[derive(Debug, Error)]
pub enum GatewayError {
    #[error("gateway rejected the query request")]
    RequestRejected(GatewayDiagnostic),
    #[error("gateway returned a malformed query response")]
    Malformed(GatewayDiagnostic),
    #[error("gateway query configuration is invalid")]
    Configuration(GatewayDiagnostic),
    #[error("gateway query service is unavailable")]
    Unavailable(GatewayDiagnostic),
    #[error("gateway query was rate limited")]
    RateLimited(GatewayDiagnostic),
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

#[derive(Debug, Error)]
pub enum GatewayNotSubmittedError {
    #[error("gateway rejected the mutation request")]
    RequestRejected(GatewayDiagnostic),
    #[error("gateway mutation request is malformed")]
    Malformed(GatewayDiagnostic),
    #[error("gateway mutation configuration is invalid")]
    Configuration(GatewayDiagnostic),
    #[error("gateway mutation service is unavailable")]
    Unavailable(GatewayDiagnostic),
    #[error("gateway mutation was rate limited before submission")]
    RateLimited(GatewayDiagnostic),
}

impl GatewayNotSubmittedError {
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

#[derive(Debug, Error)]
pub enum GatewayMutationError {
    #[error("gateway mutation was not submitted")]
    NotSubmitted(#[source] GatewayNotSubmittedError),
    #[error("gateway mutation was rate limited with an indeterminate outcome")]
    RateLimitedIndeterminate(GatewayDiagnostic),
    #[error("gateway mutation outcome is indeterminate")]
    Indeterminate(GatewayDiagnostic),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MutationCertainty {
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

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum GatewayLifecycleQueryPolicyError {
    #[error("gateway lifecycle query overlap must be positive")]
    NonPositiveOverlap,
    #[error("gateway lifecycle query limit must be positive")]
    NonPositiveLimit,
    #[error("gateway lifecycle query limits overflow their bounded work calculation")]
    Overflow,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GatewayLifecycleQueryPolicy {
    cursor_key: GatewayLifecycleCursorKey,
    overlap: Duration,
    page_size: NonZeroU32,
    ordinary_page_limit: NonZeroU32,
    max_window_splits: NonZeroU32,
    narrow_window_drain_page_limit: NonZeroU32,
}

impl GatewayLifecycleQueryPolicy {
    pub fn new(
        cursor_key: GatewayLifecycleCursorKey,
        overlap: Duration,
        page_size: u32,
        ordinary_page_limit: u32,
        max_window_splits: u32,
        narrow_window_drain_page_limit: u32,
    ) -> Result<Self, GatewayLifecycleQueryPolicyError> {
        if overlap <= Duration::zero() {
            return Err(GatewayLifecycleQueryPolicyError::NonPositiveOverlap);
        }
        let page_size =
            NonZeroU32::new(page_size).ok_or(GatewayLifecycleQueryPolicyError::NonPositiveLimit)?;
        let ordinary_page_limit = NonZeroU32::new(ordinary_page_limit)
            .ok_or(GatewayLifecycleQueryPolicyError::NonPositiveLimit)?;
        let max_window_splits = NonZeroU32::new(max_window_splits)
            .ok_or(GatewayLifecycleQueryPolicyError::NonPositiveLimit)?;
        let narrow_window_drain_page_limit = NonZeroU32::new(narrow_window_drain_page_limit)
            .ok_or(GatewayLifecycleQueryPolicyError::NonPositiveLimit)?;
        page_size
            .get()
            .checked_mul(ordinary_page_limit.get())
            .and_then(|value| value.checked_mul(max_window_splits.get()))
            .and_then(|_| {
                page_size
                    .get()
                    .checked_mul(narrow_window_drain_page_limit.get())
            })
            .ok_or(GatewayLifecycleQueryPolicyError::Overflow)?;
        Ok(Self {
            cursor_key,
            overlap,
            page_size,
            ordinary_page_limit,
            max_window_splits,
            narrow_window_drain_page_limit,
        })
    }

    pub const fn cursor_key(&self) -> &GatewayLifecycleCursorKey {
        &self.cursor_key
    }

    pub const fn overlap(&self) -> Duration {
        self.overlap
    }

    pub const fn page_size(&self) -> NonZeroU32 {
        self.page_size
    }

    pub const fn ordinary_page_limit(&self) -> NonZeroU32 {
        self.ordinary_page_limit
    }

    pub const fn max_window_splits(&self) -> NonZeroU32 {
        self.max_window_splits
    }

    pub const fn narrow_window_drain_page_limit(&self) -> NonZeroU32 {
        self.narrow_window_drain_page_limit
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;
    use crate::{CurrencyCode, GatewayReferenceValueError};

    #[test]
    fn query_and_report_requests_reject_invalid_shapes() {
        assert!(matches!(
            GatewayQueryRequest::new(None, None),
            Err(GatewayRequestError::MissingQuerySelector)
        ));
        let start = Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap();
        assert!(matches!(
            GatewayTransactionReportRequest::new(start, start, 100, 0),
            Err(GatewayRequestError::InvalidReportWindow)
        ));
        assert!(matches!(
            GatewayTransactionReportRequest::new(start, start + Duration::minutes(1), 0, 0),
            Err(GatewayRequestError::InvalidPageSize)
        ));
    }

    #[test]
    fn sale_request_cannot_represent_zero_money() {
        let usd = CurrencyCode::new("USD").unwrap();
        let charge = ChargeAmount::new(100, usd).unwrap();
        let attempt_id: PaymentAttemptId = "00000000-0000-0000-0000-000000000000".parse().unwrap();
        let order = GatewayOrderId::from_generated_attempt(
            "ck_order_00000000000000000000000000000000",
            attempt_id,
        )
        .unwrap();
        let token = PaymentToken::new("tok_safe").unwrap();
        let request = GatewaySaleRequest::new(
            charge,
            order,
            GatewaySaleIntent::OneTime {
                payment_token: token,
            },
            None,
        );
        assert_eq!(request.charge().cents(), 100);
    }

    #[test]
    fn descriptor_drops_invalid_display_fields() {
        let descriptor = GatewayPaymentDescriptor::from_provider_parts(
            Some(GatewayDiagnostic::new("visa")),
            Some(GatewayDiagnostic::new("brand")),
            Some("12x4"),
            Some(13),
            Some(2101),
        );
        assert!(descriptor.card_last_four().is_none());
        assert_eq!(descriptor.card_exp_month(), None);
        assert_eq!(descriptor.card_exp_year(), None);
    }

    #[test]
    fn lifecycle_admission_prevents_invalid_locator_combinations() {
        assert_eq!(
            GatewayLifecycleEvidence::new(
                None,
                None,
                GatewayLifecycleState::Unknown,
                None,
                None,
                None,
            ),
            Err(GatewayLifecycleEvidenceError::MissingLocator)
        );
        assert!(
            GatewayLifecycleQuarantine::new(
                None,
                None,
                GatewayLifecycleQuarantineReason::MalformedReportStructure,
            )
            .is_ok()
        );
        assert_eq!(
            GatewayLifecycleQuarantine::new(
                None,
                None,
                GatewayLifecycleQuarantineReason::InvalidRefundEconomics,
            ),
            Err(GatewayLifecycleQuarantineError::MissingLocator)
        );
    }

    #[test]
    fn only_full_lifecycle_states_derive_reversals() {
        let refunded = CumulativeRefundCents::new(100).unwrap();
        assert_eq!(
            GatewayLifecycleState::Settled {
                cumulative_refunded_cents: Some(refunded)
            }
            .full_reversal_kind(),
            None
        );
        assert_eq!(
            GatewayLifecycleState::Refunded {
                cumulative_refunded_cents: refunded
            }
            .full_reversal_kind(),
            Some(PaymentReversalKind::Refunded)
        );
    }

    #[test]
    fn query_policy_preserves_positive_overflow_safe_limits() {
        let key = GatewayLifecycleCursorKey::new("nmi_approved_lifecycle").unwrap();
        let policy =
            GatewayLifecycleQueryPolicy::new(key, Duration::minutes(5), 100, 20, 12, 2_000)
                .unwrap();
        assert_eq!(policy.page_size().get(), 100);
        assert_eq!(policy.narrow_window_drain_page_limit().get(), 2_000);
        assert_eq!(
            GatewayLifecycleQueryPolicy::new(
                GatewayLifecycleCursorKey::new("cursor").unwrap(),
                Duration::zero(),
                100,
                20,
                12,
                2_000,
            ),
            Err(GatewayLifecycleQueryPolicyError::NonPositiveOverlap)
        );
    }

    #[test]
    fn sensitive_debug_output_is_value_free() {
        let identifier = GatewayTransactionId::new("txn_sentinel").unwrap();
        let debug = format!("{identifier:?}");
        assert!(!debug.contains("txn_sentinel"));
        assert!(debug.contains("redacted"));
        assert_eq!(
            GatewayTransactionId::from_correlation("bad selector"),
            Err(GatewayReferenceValueError::UnsupportedCorrelationCharacter)
        );
    }
}
