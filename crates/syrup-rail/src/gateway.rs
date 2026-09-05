use std::{fmt, num::NonZeroU32};

use chrono::{DateTime, Duration, Utc};
use thiserror::Error;

use crate::{
    BillingContact, ChargeAmount, GatewayDiagnostic, GatewayLifecycleCursorKey, GatewayOrderId,
    GatewayPaymentMethodReference, GatewayTransactionId, PaymentToken,
};

/// Cooldown applied after a determinate account- or provider-scoped mutation
/// rate limit.
pub const GATEWAY_MUTATION_RATE_LIMIT_RETRY_AFTER_SECONDS: i64 = 60;

pub use self::lifecycle::{
    GatewayLifecycleEvidence, GatewayLifecycleEvidenceError, GatewayLifecycleQuarantine,
    GatewayLifecycleQuarantineError, GatewayLifecycleQuarantineReason,
    GatewayLifecycleQuarantineResolutionReason, GatewayLifecycleQuarantineResolutionReasonError,
    GatewayLifecycleState, GatewayTransactionReport, PaymentReversalKind,
};
pub use self::port::{
    GatewayError, GatewayMutationError, GatewayMutationReferenceFactory, GatewayNotSubmittedError,
    MutationCertainty, PaymentGateway, SharedGatewayMutationReferenceFactory, SharedPaymentGateway,
};

mod lifecycle;
mod port;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GatewayAccountMode {
    Live,
    Test,
}

impl GatewayAccountMode {
    /// Canonical provider-neutral storage representation.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Test => "test",
        }
    }
}

impl fmt::Display for GatewayAccountMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("gateway account mode is not recognized")]
pub struct GatewayAccountModeParseError;

impl std::str::FromStr for GatewayAccountMode {
    type Err = GatewayAccountModeParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "live" => Ok(Self::Live),
            "test" => Ok(Self::Test),
            _ => Err(GatewayAccountModeParseError),
        }
    }
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

    pub const fn payment_token(&self) -> &PaymentToken {
        &self.payment_token
    }

    pub const fn order_id(&self) -> &GatewayOrderId {
        &self.order_id
    }

    pub const fn billing_contact(&self) -> Option<&BillingContact> {
        self.billing_contact.as_ref()
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

// Keep the public enum and its exhaustive variant list generated from one
// declaration so adding a diagnostic cannot silently escape policy tests.
macro_rules! define_gateway_payment_diagnostics {
    (
        $(#[$enum_meta:meta])*
        pub enum $name:ident {
            $(
                $(#[$variant_meta:meta])*
                $variant:ident
            ),+ $(,)?
        }
    ) => {
        $(#[$enum_meta])*
        pub enum $name {
            $(
                $(#[$variant_meta])*
                $variant,
            )+
        }

        impl $name {
            /// Every provider-neutral payment diagnostic supported by this
            /// version.
            ///
            /// Adapters and policy tests can use this list to prove complete
            /// coverage while retaining a wildcard for future non-exhaustive
            /// variants. This is the current-version vocabulary, not a closed
            /// set: compatible releases may append new variants.
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];
        }
    };
}

define_gateway_payment_diagnostics! {
/// Provider-neutral diagnostics that require host policy beyond the payment
/// status alone.
///
/// These diagnostics intentionally contain no provider payload. Exact gateway
/// response fields remain available through [`ProcessorEvidence`].
/// On [`GatewayPaymentOutcome`] they qualify the current observation and may
/// force its status to [`GatewayPaymentStatus::Unknown`]. Durable application
/// results expose them separately as observation-local annotations and never
/// reinterpret an already persisted attempt status.
///
/// Variant declaration order defines the canonical order returned by payment
/// outcomes. Append new variants, and classify their certainty effect in the
/// adjacent policy method.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum GatewayPaymentDiagnostic {
    /// An approved processor decision did not include a transaction identity.
    MissingTransactionIdentifier,
    /// An approved processor decision did not include the requested stored
    /// payment-method identity.
    MissingPaymentMethodReference,
    /// Transaction-identity evidence was malformed, contradictory, or rejected
    /// by provider-neutral identifier admission.
    ///
    /// Attaching this diagnostic to a [`GatewayPaymentOutcome`] quarantines the
    /// outcome's transaction identity and prevents an approval from remaining
    /// authoritative. A determinate decline or failure remains terminal.
    InvalidOrConflictingTransactionIdentifier,
    /// Stored-payment-method identity evidence was malformed, contradictory, or
    /// rejected by provider-neutral identifier admission.
    ///
    /// Attaching this diagnostic to a [`GatewayPaymentOutcome`] quarantines the
    /// outcome's payment-method reference and prevents an approval from
    /// remaining authoritative. A determinate decline or failure remains
    /// terminal.
    InvalidOrConflictingPaymentMethodReference,
    /// One processor decision field was malformed or internally contradictory.
    InvalidOrConflictingDecisionField,
    /// The provider reported an error without proving that the attempted
    /// payment had no financial effect.
    ///
    /// Reconcile the durable attempt before retrying or treating it as failed.
    IndeterminatePaymentOutcome,
    /// The processor reported the payment as a duplicate.
    ///
    /// This does not prove that the current attempt was submitted or identify
    /// an earlier transaction. It forces an unknown outcome; reconcile the
    /// durable attempt before retrying.
    ProcessorReportedDuplicate,
    /// Individually recognized processor decision fields disagreed.
    ConflictingDecisionEvidence,
    /// A processor decision field contained unrecognized vocabulary.
    UnrecognizedDecisionEvidence,
    /// The processor response contained no recognizable payment decision.
    MissingDecisionEvidence,
    /// The provider adapter received a newer payload-free diagnostic category
    /// that the current provider-neutral contract does not yet name.
    ///
    /// Treat the outcome conservatively and update the adapter before routing
    /// on the new category. This fallback preserves anomaly provenance when a
    /// provider client and its adapter are upgraded independently.
    UnmappedProviderDiagnostic,
}
}

impl GatewayPaymentDiagnostic {
    /// Whether this diagnostic means that no terminal payment decision is safe.
    ///
    /// Identity diagnostics describe evidence usability, not payment certainty.
    /// Workflows separately park approvals that lack an identity they require.
    /// Every decision anomaly and an unreconciled processor duplicate instead
    /// require exact reconciliation regardless of the provider-reported status.
    const fn requires_unknown_status(self) -> bool {
        match self {
            Self::MissingTransactionIdentifier
            | Self::MissingPaymentMethodReference
            | Self::InvalidOrConflictingTransactionIdentifier
            | Self::InvalidOrConflictingPaymentMethodReference => false,
            Self::InvalidOrConflictingDecisionField
            | Self::IndeterminatePaymentOutcome
            | Self::ProcessorReportedDuplicate
            | Self::ConflictingDecisionEvidence
            | Self::UnrecognizedDecisionEvidence
            | Self::MissingDecisionEvidence
            | Self::UnmappedProviderDiagnostic => true,
        }
    }

    /// Whether this diagnostic prevents a provider-reported approval from
    /// remaining authoritative.
    const fn prevents_approval(self) -> bool {
        self.requires_unknown_status()
            || matches!(
                self,
                Self::InvalidOrConflictingTransactionIdentifier
                    | Self::InvalidOrConflictingPaymentMethodReference
            )
    }
}

pub(crate) fn normalize_gateway_payment_diagnostics(
    mut diagnostics: Vec<GatewayPaymentDiagnostic>,
) -> Vec<GatewayPaymentDiagnostic> {
    diagnostics.sort_unstable_by_key(|diagnostic| *diagnostic as usize);
    diagnostics.dedup();
    diagnostics
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

/// Canonical provider-neutral card brand retained for masked presentation.
///
/// Provider text is mapped into this closed vocabulary before it can enter a
/// consumer-facing projection or host event. Unrecognized nonempty values
/// become [`Self::Other`]; their original text is not retained by this value.
#[non_exhaustive]
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub enum PaymentCardBrand {
    /// Visa.
    Visa,
    /// Mastercard.
    Mastercard,
    /// American Express.
    AmericanExpress,
    /// Discover.
    Discover,
    /// Japan Credit Bureau.
    Jcb,
    /// Diners Club.
    DinersClub,
    /// UnionPay.
    UnionPay,
    /// Maestro.
    Maestro,
    /// A nonempty provider value outside the recognized vocabulary.
    Other,
}

const PAYMENT_CARD_BRAND_ALIASES: &[(&str, PaymentCardBrand)] = &[
    ("visa", PaymentCardBrand::Visa),
    ("mastercard", PaymentCardBrand::Mastercard),
    ("master card", PaymentCardBrand::Mastercard),
    ("american express", PaymentCardBrand::AmericanExpress),
    ("amex", PaymentCardBrand::AmericanExpress),
    ("discover", PaymentCardBrand::Discover),
    ("jcb", PaymentCardBrand::Jcb),
    ("diners", PaymentCardBrand::DinersClub),
    ("diners club", PaymentCardBrand::DinersClub),
    ("dinersclub", PaymentCardBrand::DinersClub),
    ("unionpay", PaymentCardBrand::UnionPay),
    ("union pay", PaymentCardBrand::UnionPay),
    ("maestro", PaymentCardBrand::Maestro),
];

impl PaymentCardBrand {
    /// Canonicalizes an untrusted provider value without retaining unknown
    /// text.
    pub fn from_provider(value: &str) -> Option<Self> {
        let value = value.trim();
        if value.is_empty() {
            return None;
        }
        let brand = PAYMENT_CARD_BRAND_ALIASES
            .iter()
            .find_map(|(alias, brand)| value.eq_ignore_ascii_case(alias).then_some(*brand))
            .unwrap_or(Self::Other);
        Some(brand)
    }

    /// Explicitly exposes the stable provider-neutral wire label.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Visa => "visa",
            Self::Mastercard => "mastercard",
            Self::AmericanExpress => "american_express",
            Self::Discover => "discover",
            Self::Jcb => "jcb",
            Self::DinersClub => "diners_club",
            Self::UnionPay => "union_pay",
            Self::Maestro => "maestro",
            Self::Other => "other",
        }
    }
}

impl fmt::Debug for PaymentCardBrand {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PaymentCardBrand([redacted])")
    }
}

impl fmt::Display for PaymentCardBrand {
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

    /// Returns provider payment-type evidence for explicit boundary use.
    pub const fn payment_type(&self) -> Option<&GatewayDiagnostic> {
        self.payment_type.as_ref()
    }

    /// Returns provider card-brand evidence for explicit persistence or
    /// reconciliation use.
    pub const fn card_brand(&self) -> Option<&GatewayDiagnostic> {
        self.card_brand.as_ref()
    }

    /// Reduces provider card-brand evidence to the presentation vocabulary.
    ///
    /// Unknown provider text becomes [`PaymentCardBrand::Other`] and is not
    /// retained in the returned value. Use this method for customer displays
    /// and host events; use [`Self::card_brand`] only where exact provider
    /// evidence is required.
    pub fn canonical_card_brand(&self) -> Option<PaymentCardBrand> {
        self.card_brand
            .as_ref()
            .and_then(|brand| PaymentCardBrand::from_provider(brand.expose()))
    }

    /// Returns the validated masked last four digits.
    pub const fn card_last_four(&self) -> Option<&CardLastFour> {
        self.card_last_four.as_ref()
    }

    /// Returns the validated card expiration month.
    pub const fn card_exp_month(&self) -> Option<i16> {
        self.card_exp_month
    }

    /// Returns the validated card expiration year.
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

#[derive(Clone, Eq, PartialEq)]
pub struct ProcessorEvidence {
    approval_evidence: crate::ProcessorApprovalEvidence,
    transaction_id: Option<GatewayTransactionId>,
    payment_method_reference: Option<GatewayPaymentMethodReference>,
    response: Option<GatewayDiagnostic>,
    response_code: Option<GatewayDiagnostic>,
    response_text: Option<GatewayDiagnostic>,
    condition: Option<GatewayDiagnostic>,
    descriptor: GatewayPaymentDescriptor,
}

impl Default for ProcessorEvidence {
    fn default() -> Self {
        Self::new(
            crate::ProcessorApprovalEvidence::Absent,
            None,
            None,
            None,
            None,
            None,
            None,
            GatewayPaymentDescriptor::default(),
        )
    }
}

impl ProcessorEvidence {
    /// Constructs evidence with an explicit provider-derived review
    /// classification. Requiring this fact at construction prevents adapters
    /// from silently omitting the classification while still compiling.
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        approval_evidence: crate::ProcessorApprovalEvidence,
        transaction_id: Option<GatewayTransactionId>,
        payment_method_reference: Option<GatewayPaymentMethodReference>,
        response: Option<GatewayDiagnostic>,
        response_code: Option<GatewayDiagnostic>,
        response_text: Option<GatewayDiagnostic>,
        condition: Option<GatewayDiagnostic>,
        descriptor: GatewayPaymentDescriptor,
    ) -> Self {
        Self {
            approval_evidence,
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

    pub const fn has_gateway_reference(&self) -> bool {
        self.transaction_id.is_some() || self.payment_method_reference.is_some()
    }

    /// Attaches the provider adapter's conservative approval-signal classification.
    /// This is independent of the authoritative payment decision. Persistence
    /// and evidence-copying code must retain it alongside the raw observation.
    pub const fn with_approval_evidence(
        mut self,
        evidence: crate::ProcessorApprovalEvidence,
    ) -> Self {
        self.approval_evidence = evidence;
        self
    }

    /// Returns the classification supplied when the evidence was constructed.
    pub const fn approval_evidence(&self) -> crate::ProcessorApprovalEvidence {
        self.approval_evidence
    }

    /// Whether a located observation needs a pending processor-charge record.
    /// Unclassified legacy observations remain on the attempt for reconciliation;
    /// they cannot create a new immutable charge solely from uninterpreted text.
    /// This never authorizes applying a payment as approved.
    pub const fn indicates_approved_payment(&self) -> bool {
        self.transaction_id.is_some()
            && matches!(
                self.approval_evidence,
                crate::ProcessorApprovalEvidence::Structured
            )
    }

    /// Whether approval signals prevent an operator's no-financial-effect exit.
    /// This depends only on the durable classification. Local notes, redaction,
    /// and discarded raw fields cannot strengthen or weaken the decision.
    pub const fn may_indicate_approval(&self) -> bool {
        !matches!(
            self.approval_evidence,
            crate::ProcessorApprovalEvidence::Absent
        )
    }
}

include!("gateway/outcome.rs");
