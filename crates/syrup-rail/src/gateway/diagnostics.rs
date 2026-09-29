//! Provider-neutral, read-only diagnostics for one historical transaction.
//!
//! These values explain what a processor reported for an already submitted
//! payment. They are observations, not payment evidence: nothing here can
//! authorize, approve, decline, or reconcile a payment.

use std::fmt;

use thiserror::Error;

use super::GatewayError;
use crate::{GatewayDiagnostic, GatewayOrderId, GatewayTransactionId, Money};

/// Largest provider code, in bytes, that a diagnostic observation retains.
///
/// Response codes, processor codes, address and card-security verification
/// results are short provider codes. A longer value is omitted and the
/// observation is marked partial rather than truncated into a misleading code.
pub const MAX_GATEWAY_DIAGNOSTIC_CODE_BYTES: usize = 64;

/// The original provider operation whose result a diagnostic query selects.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum GatewayDiagnosticOperation {
    /// A charge: initial enrollment, recovery, renewal, or host charge.
    Sale,
    /// A zero-amount card verification used by payment-method replacement.
    Validate,
}

impl GatewayDiagnosticOperation {
    /// Returns the stable persisted spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sale => "sale",
            Self::Validate => "validate",
        }
    }
}

/// Identifies one historical transaction and the exact original operation to
/// read back from the provider.
///
/// Providers must look the transaction up by its transaction ID only and must
/// select exactly one original action of `operation` whose amount equals
/// `amount` (zero for [`GatewayDiagnosticOperation::Validate`]). A returned
/// order ID that differs from `expected_order_id` makes the result unavailable.
#[derive(Clone, Eq, PartialEq)]
pub struct GatewayTransactionDiagnosticsRequest {
    transaction_id: GatewayTransactionId,
    operation: GatewayDiagnosticOperation,
    amount: Money,
    expected_order_id: Option<GatewayOrderId>,
}

impl GatewayTransactionDiagnosticsRequest {
    pub const fn new(
        transaction_id: GatewayTransactionId,
        operation: GatewayDiagnosticOperation,
        amount: Money,
        expected_order_id: Option<GatewayOrderId>,
    ) -> Self {
        Self {
            transaction_id,
            operation,
            amount,
            expected_order_id,
        }
    }

    pub const fn transaction_id(&self) -> &GatewayTransactionId {
        &self.transaction_id
    }

    pub const fn operation(&self) -> GatewayDiagnosticOperation {
        self.operation
    }

    pub const fn amount(&self) -> Money {
        self.amount
    }

    pub const fn expected_order_id(&self) -> Option<&GatewayOrderId> {
        self.expected_order_id.as_ref()
    }

    pub fn into_parts(
        self,
    ) -> (
        GatewayTransactionId,
        GatewayDiagnosticOperation,
        Money,
        Option<GatewayOrderId>,
    ) {
        (
            self.transaction_id,
            self.operation,
            self.amount,
            self.expected_order_id,
        )
    }
}

impl fmt::Debug for GatewayTransactionDiagnosticsRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GatewayTransactionDiagnosticsRequest")
            .field("operation", &self.operation)
            .field("amount", &self.amount)
            .field("has_expected_order_id", &self.expected_order_id.is_some())
            .finish()
    }
}

/// Why a provider diagnostic lookup could not produce a trustworthy
/// observation. Every value has a stable [`Self::as_str`] spelling.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum GatewayDiagnosticsUnavailableReason {
    /// The provider returned more than one transaction.
    MultipleTransactions,
    /// The returned transaction ID was missing or differed from the request.
    TransactionMismatch,
    /// The returned order ID differed from the expected order ID.
    OrderMismatch,
    /// The returned transaction currency differed from the request.
    CurrencyMismatch,
    /// No action matched the requested operation and amount, for example
    /// because only settlement, refund, or void actions were returned.
    NoMatchingAction,
    /// More than one action matched the requested operation and amount.
    AmbiguousAction,
    /// The provider response could not be parsed safely.
    MalformedResponse,
    /// The provider or its transport was unavailable.
    ProviderUnavailable,
    /// The provider rejected the query request.
    ProviderRejected,
    /// The provider reported a credential or configuration problem.
    ProviderConfiguration,
}

impl GatewayDiagnosticsUnavailableReason {
    /// Returns the stable persisted spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MultipleTransactions => "multiple_transactions",
            Self::TransactionMismatch => "transaction_mismatch",
            Self::OrderMismatch => "order_mismatch",
            Self::CurrencyMismatch => "currency_mismatch",
            Self::NoMatchingAction => "no_matching_action",
            Self::AmbiguousAction => "ambiguous_action",
            Self::MalformedResponse => "malformed_response",
            Self::ProviderUnavailable => "provider_unavailable",
            Self::ProviderRejected => "provider_rejected",
            Self::ProviderConfiguration => "provider_configuration",
        }
    }

    /// Classifies a provider query failure other than rate limiting.
    ///
    /// Returns `None` for [`GatewayError::RateLimited`], which callers must
    /// handle separately because it affects shared provider pacing.
    pub const fn for_query_error(error: &GatewayError) -> Option<Self> {
        match error {
            GatewayError::RequestRejected(_) => Some(Self::ProviderRejected),
            GatewayError::Malformed(_) => Some(Self::MalformedResponse),
            GatewayError::Configuration(_) => Some(Self::ProviderConfiguration),
            GatewayError::Unavailable(_) => Some(Self::ProviderUnavailable),
            GatewayError::RateLimited(_) => None,
        }
    }
}

/// Whether every diagnostic field was present and usable.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum GatewayDiagnosticsCompleteness {
    /// All six diagnostic fields were present and retained.
    Complete,
    /// At least one field was absent, invalid, or too long to retain. A
    /// missing field never means a match, mismatch, or disabled check.
    Partial,
}

impl GatewayDiagnosticsCompleteness {
    /// Returns the stable persisted spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Partial => "partial",
        }
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error(
    "gateway diagnostics source must be 1 to 64 lowercase ASCII letters, digits, or underscores"
)]
pub struct GatewayDiagnosticsSourceError;

/// A stable identifier for the provider interface that produced an
/// observation, such as a provider adapter's query API name.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct GatewayDiagnosticsSource(String);

impl GatewayDiagnosticsSource {
    pub fn new(value: &str) -> Result<Self, GatewayDiagnosticsSourceError> {
        let valid = !value.is_empty()
            && value.len() <= 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_');
        if valid {
            Ok(Self(value.to_owned()))
        } else {
            Err(GatewayDiagnosticsSourceError)
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Bounded processor and verification results for the selected original
/// action of one transaction.
///
/// Every value is sanitized provider text with value-free ordinary
/// formatting; read it through [`GatewayDiagnostic::expose`] only at a
/// protected operator boundary. Provider codes longer than
/// [`MAX_GATEWAY_DIAGNOSTIC_CODE_BYTES`] as received or after sanitization are
/// omitted, and free text is sanitized and truncated like other gateway
/// diagnostics. Absent values are never invented.
#[derive(Clone, Eq, PartialEq)]
pub struct GatewayTransactionDiagnosticsObservation {
    operation: GatewayDiagnosticOperation,
    source: GatewayDiagnosticsSource,
    gateway_response_code: Option<GatewayDiagnostic>,
    gateway_response_text: Option<GatewayDiagnostic>,
    processor_response_code: Option<GatewayDiagnostic>,
    processor_response_text: Option<GatewayDiagnostic>,
    avs_response: Option<GatewayDiagnostic>,
    csc_response: Option<GatewayDiagnostic>,
    marked_partial: bool,
}

impl GatewayTransactionDiagnosticsObservation {
    /// Starts an observation of the selected `operation` action from `source`.
    pub const fn new(
        operation: GatewayDiagnosticOperation,
        source: GatewayDiagnosticsSource,
    ) -> Self {
        Self {
            operation,
            source,
            gateway_response_code: None,
            gateway_response_text: None,
            processor_response_code: None,
            processor_response_text: None,
            avs_response: None,
            csc_response: None,
            marked_partial: false,
        }
    }

    /// Records the provider's gateway response code.
    pub fn with_gateway_response_code(mut self, value: Option<&str>) -> Self {
        self.gateway_response_code = self.bounded_code(value);
        self
    }

    /// Records the provider's gateway response text.
    pub fn with_gateway_response_text(mut self, value: Option<&str>) -> Self {
        self.gateway_response_text = sanitized_text(value);
        self
    }

    /// Records the processor's own response code.
    pub fn with_processor_response_code(mut self, value: Option<&str>) -> Self {
        self.processor_response_code = self.bounded_code(value);
        self
    }

    /// Records the processor's own response text.
    pub fn with_processor_response_text(mut self, value: Option<&str>) -> Self {
        self.processor_response_text = sanitized_text(value);
        self
    }

    /// Records the address verification result code.
    pub fn with_avs_response(mut self, value: Option<&str>) -> Self {
        self.avs_response = self.bounded_code(value);
        self
    }

    /// Records the card-security-code verification result code.
    pub fn with_csc_response(mut self, value: Option<&str>) -> Self {
        self.csc_response = self.bounded_code(value);
        self
    }

    /// Marks the observation partial when the provider supplied a field that
    /// could not be read safely.
    pub const fn mark_partial(mut self) -> Self {
        self.marked_partial = true;
        self
    }

    /// Bounds both the provider's code and the retained sanitized code, since
    /// redaction can lengthen a code that fit on arrival.
    fn bounded_code(&mut self, value: Option<&str>) -> Option<GatewayDiagnostic> {
        let value = value.map(str::trim).filter(|value| !value.is_empty())?;
        if value.len() > MAX_GATEWAY_DIAGNOSTIC_CODE_BYTES {
            self.marked_partial = true;
            return None;
        }
        let code = sanitized_text(Some(value))?;
        if code.expose().len() > MAX_GATEWAY_DIAGNOSTIC_CODE_BYTES {
            self.marked_partial = true;
            return None;
        }
        Some(code)
    }

    pub const fn operation(&self) -> GatewayDiagnosticOperation {
        self.operation
    }

    pub const fn source(&self) -> &GatewayDiagnosticsSource {
        &self.source
    }

    pub const fn gateway_response_code(&self) -> Option<&GatewayDiagnostic> {
        self.gateway_response_code.as_ref()
    }

    pub const fn gateway_response_text(&self) -> Option<&GatewayDiagnostic> {
        self.gateway_response_text.as_ref()
    }

    pub const fn processor_response_code(&self) -> Option<&GatewayDiagnostic> {
        self.processor_response_code.as_ref()
    }

    pub const fn processor_response_text(&self) -> Option<&GatewayDiagnostic> {
        self.processor_response_text.as_ref()
    }

    pub const fn avs_response(&self) -> Option<&GatewayDiagnostic> {
        self.avs_response.as_ref()
    }

    pub const fn csc_response(&self) -> Option<&GatewayDiagnostic> {
        self.csc_response.as_ref()
    }

    /// Returns [`GatewayDiagnosticsCompleteness::Partial`] when any field is
    /// absent or was omitted as unusable.
    pub const fn completeness(&self) -> GatewayDiagnosticsCompleteness {
        if self.marked_partial
            || self.gateway_response_code.is_none()
            || self.gateway_response_text.is_none()
            || self.processor_response_code.is_none()
            || self.processor_response_text.is_none()
            || self.avs_response.is_none()
            || self.csc_response.is_none()
        {
            GatewayDiagnosticsCompleteness::Partial
        } else {
            GatewayDiagnosticsCompleteness::Complete
        }
    }
}

fn sanitized_text(value: Option<&str>) -> Option<GatewayDiagnostic> {
    value
        .map(GatewayDiagnostic::new)
        .filter(|diagnostic| !diagnostic.is_empty())
}

impl fmt::Debug for GatewayTransactionDiagnosticsObservation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GatewayTransactionDiagnosticsObservation")
            .field("operation", &self.operation)
            .field("source", &self.source.as_str())
            .field(
                "has_gateway_response_code",
                &self.gateway_response_code.is_some(),
            )
            .field(
                "has_gateway_response_text",
                &self.gateway_response_text.is_some(),
            )
            .field(
                "has_processor_response_code",
                &self.processor_response_code.is_some(),
            )
            .field(
                "has_processor_response_text",
                &self.processor_response_text.is_some(),
            )
            .field("has_avs_response", &self.avs_response.is_some())
            .field("has_csc_response", &self.csc_response.is_some())
            .field("completeness", &self.completeness())
            .finish()
    }
}

/// The result of one read-only provider diagnostic lookup.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum GatewayTransactionDiagnostics {
    /// The provider adapter does not implement diagnostic queries.
    Unsupported,
    /// The provider returned no transaction for the requested ID.
    NotFound,
    /// The provider response could not be bound safely to the request.
    Unavailable(GatewayDiagnosticsUnavailableReason),
    /// Exactly one original action matched the request.
    Observed(Box<GatewayTransactionDiagnosticsObservation>),
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;

    use super::*;
    use crate::{
        CurrencyCode, GatewayAccountMode, GatewayMutationError, GatewayPaymentOutcome,
        GatewayQueryRequest, GatewaySaleRequest, GatewayStorePaymentMethodRequest,
        GatewayTransactionReport, GatewayTransactionReportRequest, PaymentGateway,
    };

    fn source() -> GatewayDiagnosticsSource {
        GatewayDiagnosticsSource::new("test_query_api").unwrap()
    }

    #[test]
    fn diagnostic_vocabulary_has_stable_spellings() {
        use GatewayDiagnosticsUnavailableReason as Reason;
        assert_eq!(GatewayDiagnosticOperation::Sale.as_str(), "sale");
        assert_eq!(GatewayDiagnosticOperation::Validate.as_str(), "validate");
        assert_eq!(
            GatewayDiagnosticsCompleteness::Complete.as_str(),
            "complete"
        );
        assert_eq!(GatewayDiagnosticsCompleteness::Partial.as_str(), "partial");
        for (reason, expected) in [
            (Reason::MultipleTransactions, "multiple_transactions"),
            (Reason::TransactionMismatch, "transaction_mismatch"),
            (Reason::OrderMismatch, "order_mismatch"),
            (Reason::CurrencyMismatch, "currency_mismatch"),
            (Reason::NoMatchingAction, "no_matching_action"),
            (Reason::AmbiguousAction, "ambiguous_action"),
            (Reason::MalformedResponse, "malformed_response"),
            (Reason::ProviderUnavailable, "provider_unavailable"),
            (Reason::ProviderRejected, "provider_rejected"),
            (Reason::ProviderConfiguration, "provider_configuration"),
        ] {
            assert_eq!(reason.as_str(), expected);
        }
        let detail = || GatewayDiagnostic::new("detail");
        assert_eq!(
            Reason::for_query_error(&GatewayError::Unavailable(detail())),
            Some(Reason::ProviderUnavailable)
        );
        assert_eq!(
            Reason::for_query_error(&GatewayError::Malformed(detail())),
            Some(Reason::MalformedResponse)
        );
        assert_eq!(
            Reason::for_query_error(&GatewayError::RequestRejected(detail())),
            Some(Reason::ProviderRejected)
        );
        assert_eq!(
            Reason::for_query_error(&GatewayError::Configuration(detail())),
            Some(Reason::ProviderConfiguration)
        );
        assert_eq!(
            Reason::for_query_error(&GatewayError::RateLimited(detail())),
            None
        );
    }

    #[test]
    fn diagnostics_source_is_a_short_lowercase_identifier() {
        assert_eq!(source().as_str(), "test_query_api");
        for invalid in ["", "Query", "query-api", "query api", &"a".repeat(65)] {
            assert_eq!(
                GatewayDiagnosticsSource::new(invalid),
                Err(GatewayDiagnosticsSourceError),
                "{invalid:?}"
            );
        }
    }

    #[test]
    fn observation_bounds_codes_sanitizes_text_and_reports_completeness() {
        let at_limit = "c".repeat(MAX_GATEWAY_DIAGNOSTIC_CODE_BYTES);
        let complete = GatewayTransactionDiagnosticsObservation::new(
            GatewayDiagnosticOperation::Sale,
            source(),
        )
        .with_gateway_response_code(Some(" 253 "))
        .with_gateway_response_text(Some("Do Not Honor"))
        .with_processor_response_code(Some("59"))
        .with_processor_response_text(Some("Suspected fraud"))
        .with_avs_response(Some("0"))
        .with_csc_response(Some(&at_limit));
        assert_eq!(
            complete.completeness(),
            GatewayDiagnosticsCompleteness::Complete
        );
        assert_eq!(
            complete
                .gateway_response_code()
                .map(GatewayDiagnostic::expose),
            Some("253")
        );
        assert_eq!(
            complete.csc_response().map(GatewayDiagnostic::expose),
            Some(at_limit.as_str())
        );
        assert_eq!(complete.operation(), GatewayDiagnosticOperation::Sale);

        let oversized = complete
            .clone()
            .with_processor_response_code(Some(&"9".repeat(MAX_GATEWAY_DIAGNOSTIC_CODE_BYTES + 1)));
        assert_eq!(oversized.processor_response_code(), None);
        assert_eq!(
            oversized.completeness(),
            GatewayDiagnosticsCompleteness::Partial
        );

        let hostile = GatewayTransactionDiagnosticsObservation::new(
            GatewayDiagnosticOperation::Validate,
            source(),
        )
        .with_gateway_response_text(Some(&format!(
            "declined card 4111 1111 1111 1111 {}",
            "x".repeat(2_000)
        )))
        .with_csc_response(Some("   "))
        .with_avs_response(None);
        let text = hostile.gateway_response_text().unwrap().expose();
        assert!(!text.contains("4111"));
        assert!(text.len() <= crate::MAX_GATEWAY_TEXT_BYTES);
        assert_eq!(hostile.csc_response(), None);
        assert_eq!(
            hostile.completeness(),
            GatewayDiagnosticsCompleteness::Partial
        );

        let marked = complete.clone().mark_partial();
        assert_eq!(
            marked.completeness(),
            GatewayDiagnosticsCompleteness::Partial
        );

        // Redaction can lengthen a code that fits the limit on arrival; the
        // retained code must fit too.
        let expanding = "cvv=1 ".repeat(10);
        assert!(expanding.trim().len() <= MAX_GATEWAY_DIAGNOSTIC_CODE_BYTES);
        assert!(
            GatewayDiagnostic::new(&expanding).expose().len() > MAX_GATEWAY_DIAGNOSTIC_CODE_BYTES
        );
        let expanded = complete.clone().with_avs_response(Some(&expanding));
        assert_eq!(expanded.avs_response(), None);
        assert_eq!(
            expanded.completeness(),
            GatewayDiagnosticsCompleteness::Partial
        );
        let redacted = complete.clone().with_avs_response(Some("cvv=1"));
        assert!(
            redacted
                .avs_response()
                .is_some_and(|code| !code.expose().contains('1'))
        );
        assert_eq!(
            redacted.completeness(),
            GatewayDiagnosticsCompleteness::Complete
        );

        let debug = format!("{complete:?}");
        assert!(debug.contains("has_avs_response: true"));
        for value in ["253", "Do Not Honor", "Suspected fraud"] {
            assert!(!debug.contains(value), "{value} leaked into {debug}");
        }
    }

    struct RequiredOnlyGateway;

    #[async_trait]
    impl PaymentGateway for RequiredOnlyGateway {
        async fn account_mode(&self) -> Result<GatewayAccountMode, GatewayError> {
            panic!("default diagnostics must not call the provider")
        }

        async fn sale(
            &self,
            _request: GatewaySaleRequest,
        ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
            panic!("default diagnostics must not call the provider")
        }

        async fn store_payment_method(
            &self,
            _request: GatewayStorePaymentMethodRequest,
        ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
            panic!("default diagnostics must not call the provider")
        }

        async fn query_transaction(
            &self,
            _request: GatewayQueryRequest,
        ) -> Result<Option<GatewayPaymentOutcome>, GatewayError> {
            panic!("default diagnostics must not call the provider")
        }

        async fn query_transaction_reports(
            &self,
            _request: GatewayTransactionReportRequest,
        ) -> Result<Vec<GatewayTransactionReport>, GatewayError> {
            panic!("default diagnostics must not call the provider")
        }
    }

    #[test]
    fn default_port_method_is_unsupported_without_provider_io() {
        let request = GatewayTransactionDiagnosticsRequest::new(
            GatewayTransactionId::new("txn-secret").unwrap(),
            GatewayDiagnosticOperation::Sale,
            Money::new(1_000, CurrencyCode::new("USD").unwrap()).unwrap(),
            Some(GatewayOrderId::from_correlation("order-secret").unwrap()),
        );
        let debug = format!("{request:?}");
        assert!(!debug.contains("txn-secret"));
        assert!(!debug.contains("order-secret"));
        // The default completes on its first poll: it performs no I/O.
        let mut future = RequiredOnlyGateway.query_transaction_diagnostics(request);
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        let std::task::Poll::Ready(result) = future.as_mut().poll(&mut context) else {
            panic!("default diagnostics must not wait");
        };
        assert_eq!(result.unwrap(), GatewayTransactionDiagnostics::Unsupported);
    }
}
