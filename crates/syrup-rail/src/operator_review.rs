use std::{fmt, sync::LazyLock};

use chrono::{DateTime, Utc};
use regex::Regex;
use thiserror::Error;

use crate::{
    ActorId, BillingScopeId, ChargeAmount, GatewayAccountId, GatewayConfigurationId,
    GatewayDiagnostic, GatewayOrderId, GatewayTransactionId, HostChargeTargetId, Money,
    PaymentAttempt, PaymentAttemptId, PaymentAttemptKind, PaymentAttemptStatus,
    PaymentResolutionCode, ProcessorChargeId, ProcessorEvidence, SubscriberId,
};

pub const OPERATOR_REVIEW_PAGE_LIMIT: i64 = 100;
pub const MANUAL_ATTEMPT_FAILURE_NOTE: &str = "Manual review confirmed no processor transaction.";
pub const PAYMENT_METHOD_UPDATE_MANUAL_CLOSURE_NOTE: &str =
    "Manual review closed payment method update without changing subscription.";
const PROCESSOR_RESPONSE_MARKER: &str = "Processor response:";

static APPROVED_WORD_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(^|[^[:alnum:]-])approved([^[:alnum:]]|$)")
        .expect("approved word regex should compile")
});

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OperatorReviewPageLimit(i64);

impl OperatorReviewPageLimit {
    pub fn new(value: i64) -> Result<Self, OperatorReviewPageLimitError> {
        if !(1..=OPERATOR_REVIEW_PAGE_LIMIT).contains(&value) {
            return Err(OperatorReviewPageLimitError);
        }
        Ok(Self(value))
    }

    pub const fn get(self) -> i64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("operator review page limit must be between 1 and 100")]
pub struct OperatorReviewPageLimitError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AttemptReviewCursor {
    reviewed_at: DateTime<Utc>,
    attempt_id: PaymentAttemptId,
}

impl AttemptReviewCursor {
    pub const fn new(reviewed_at: DateTime<Utc>, attempt_id: PaymentAttemptId) -> Self {
        Self {
            reviewed_at,
            attempt_id,
        }
    }
    pub const fn reviewed_at(self) -> DateTime<Utc> {
        self.reviewed_at
    }
    pub const fn attempt_id(self) -> PaymentAttemptId {
        self.attempt_id
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttemptReviewPage {
    items: Vec<PaymentAttempt>,
    next_cursor: Option<AttemptReviewCursor>,
}

impl AttemptReviewPage {
    pub fn new(items: Vec<PaymentAttempt>, next_cursor: Option<AttemptReviewCursor>) -> Self {
        Self { items, next_cursor }
    }
    pub fn into_items(self) -> Vec<PaymentAttempt> {
        self.items
    }
    pub const fn next_cursor(&self) -> Option<AttemptReviewCursor> {
        self.next_cursor
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessorChargeReviewCursor {
    reviewed_at: DateTime<Utc>,
    processor_charge_id: ProcessorChargeId,
}

impl ProcessorChargeReviewCursor {
    pub const fn new(reviewed_at: DateTime<Utc>, processor_charge_id: ProcessorChargeId) -> Self {
        Self {
            reviewed_at,
            processor_charge_id,
        }
    }
    pub const fn reviewed_at(self) -> DateTime<Utc> {
        self.reviewed_at
    }
    pub const fn processor_charge_id(self) -> ProcessorChargeId {
        self.processor_charge_id
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExternalReversalKind {
    Refund,
    Void,
}

impl ExternalReversalKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Refund => "refund",
            Self::Void => "void",
        }
    }
}

/// The durable classification that immediately preceded an external reversal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExternalReversalPriorClassification {
    SubscriptionInitialCurrentGrantConflict,
    ProcessorChargeExternalReversalRequired,
}

impl ExternalReversalPriorClassification {
    pub const fn resolution_code(self) -> &'static str {
        match self {
            Self::SubscriptionInitialCurrentGrantConflict => {
                "subscription_initial_current_grant_conflict"
            }
            Self::ProcessorChargeExternalReversalRequired => {
                "processor_charge_external_reversal_required"
            }
        }
    }

    pub fn from_resolution_code(value: &str) -> Result<Self, ExternalReversalResolutionError> {
        match value {
            "subscription_initial_current_grant_conflict" => {
                Ok(Self::SubscriptionInitialCurrentGrantConflict)
            }
            "processor_charge_external_reversal_required" => {
                Ok(Self::ProcessorChargeExternalReversalRequired)
            }
            _ => Err(ExternalReversalResolutionError::InvalidPriorClassification),
        }
    }
}

/// The closed final result of an externally reversed processor charge.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExternalReversalOutcome {
    SubscriptionInitialRefunded,
    SubscriptionInitialVoided,
    ProcessorChargeRefunded,
    ProcessorChargeVoided,
}

impl ExternalReversalOutcome {
    pub const ALL: &'static [Self] = &[
        Self::SubscriptionInitialRefunded,
        Self::SubscriptionInitialVoided,
        Self::ProcessorChargeRefunded,
        Self::ProcessorChargeVoided,
    ];

    pub const fn kind(self) -> ExternalReversalKind {
        match self {
            Self::SubscriptionInitialRefunded | Self::ProcessorChargeRefunded => {
                ExternalReversalKind::Refund
            }
            Self::SubscriptionInitialVoided | Self::ProcessorChargeVoided => {
                ExternalReversalKind::Void
            }
        }
    }

    pub const fn final_resolution_code(self) -> PaymentResolutionCode {
        match self {
            Self::SubscriptionInitialRefunded => {
                PaymentResolutionCode::SubscriptionInitialExternallyRefunded
            }
            Self::SubscriptionInitialVoided => {
                PaymentResolutionCode::SubscriptionInitialExternallyVoided
            }
            Self::ProcessorChargeRefunded => {
                PaymentResolutionCode::ProcessorChargeExternallyRefunded
            }
            Self::ProcessorChargeVoided => PaymentResolutionCode::ProcessorChargeExternallyVoided,
        }
    }

    pub fn from_kind_and_final_resolution_code(
        kind: ExternalReversalKind,
        final_resolution_code: PaymentResolutionCode,
    ) -> Result<Self, ExternalReversalResolutionError> {
        match (kind, final_resolution_code) {
            (
                ExternalReversalKind::Refund,
                PaymentResolutionCode::SubscriptionInitialExternallyRefunded,
            ) => Ok(Self::SubscriptionInitialRefunded),
            (
                ExternalReversalKind::Void,
                PaymentResolutionCode::SubscriptionInitialExternallyVoided,
            ) => Ok(Self::SubscriptionInitialVoided),
            (
                ExternalReversalKind::Refund,
                PaymentResolutionCode::ProcessorChargeExternallyRefunded,
            ) => Ok(Self::ProcessorChargeRefunded),
            (
                ExternalReversalKind::Void,
                PaymentResolutionCode::ProcessorChargeExternallyVoided,
            ) => Ok(Self::ProcessorChargeVoided),
            _ => Err(ExternalReversalResolutionError::InvalidOutcome),
        }
    }
}

/// A valid durable external-reversal resolution tuple.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExternalReversalResolution {
    prior: ExternalReversalPriorClassification,
    outcome: ExternalReversalOutcome,
}

impl ExternalReversalResolution {
    pub fn new(
        prior: ExternalReversalPriorClassification,
        outcome: ExternalReversalOutcome,
    ) -> Result<Self, ExternalReversalResolutionError> {
        if prior == ExternalReversalPriorClassification::SubscriptionInitialCurrentGrantConflict
            && matches!(
                outcome,
                ExternalReversalOutcome::ProcessorChargeRefunded
                    | ExternalReversalOutcome::ProcessorChargeVoided
            )
        {
            return Err(ExternalReversalResolutionError::IncompatiblePriorOutcome);
        }
        Ok(Self { prior, outcome })
    }

    pub const fn prior(self) -> ExternalReversalPriorClassification {
        self.prior
    }

    pub const fn outcome(self) -> ExternalReversalOutcome {
        self.outcome
    }

    pub const fn kind(self) -> ExternalReversalKind {
        self.outcome.kind()
    }

    pub const fn prior_resolution_code(self) -> &'static str {
        self.prior.resolution_code()
    }

    pub const fn final_resolution_code(self) -> PaymentResolutionCode {
        self.outcome.final_resolution_code()
    }
}

/// Why a persisted external-reversal tuple cannot be represented safely.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ExternalReversalResolutionError {
    #[error("external reversal prior classification is invalid")]
    InvalidPriorClassification,
    #[error("external reversal outcome is invalid")]
    InvalidOutcome,
    #[error("external reversal prior classification is incompatible with its outcome")]
    IncompatiblePriorOutcome,
}

#[derive(Clone, Eq, PartialEq)]
pub struct ExternalReversalReason(String);

impl ExternalReversalReason {
    pub fn new(value: impl Into<String>) -> Result<Self, ExternalReversalReasonError> {
        crate::audit_reason::normalize_audit_reason(value)
            .map(Self)
            .map_err(|error| match error {
                crate::audit_reason::ReasonValidationError::Empty => {
                    ExternalReversalReasonError::Empty
                }
                crate::audit_reason::ReasonValidationError::TooLong => {
                    ExternalReversalReasonError::TooLong
                }
                crate::audit_reason::ReasonValidationError::ContainsRawCardData => {
                    ExternalReversalReasonError::ContainsRawCardData
                }
            })
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ExternalReversalReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ExternalReversalReason([redacted])")
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ExternalReversalReasonError {
    #[error("external reversal reason is empty")]
    Empty,
    #[error("external reversal reason exceeds 500 characters")]
    TooLong,
    #[error("external reversal reason contains raw payment card data")]
    ContainsRawCardData,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessorChargeRole {
    Primary,
    Additional,
}

impl ProcessorChargeRole {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Additional => "additional",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessorChargeProgression {
    Pending,
    ReconciliationRequired,
    ExternalReversalRequired,
    Applied,
    ExternallyReversed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessorChargeStateCode {
    PaymentResolution(PaymentResolutionCode),
    ExternalReversalRequired,
    AdditionalApprovedChargeIdentified,
    TransactionIdentityRequired,
    ApprovedChargeWaitingForApplication,
    ZeroAmountAdditionalApprovedCharge,
}

impl ProcessorChargeStateCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PaymentResolution(code) => code.as_str(),
            Self::ExternalReversalRequired => "processor_charge_external_reversal_required",
            Self::AdditionalApprovedChargeIdentified => "additional_approved_charge_identified",
            Self::TransactionIdentityRequired => "processor_charge_transaction_identity_required",
            Self::ApprovedChargeWaitingForApplication => "approved_charge_waiting_for_application",
            Self::ZeroAmountAdditionalApprovedCharge => "zero_amount_additional_approved_charge",
        }
    }
}

impl ProcessorChargeProgression {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::ReconciliationRequired => "reconciliation_required",
            Self::ExternalReversalRequired => "external_reversal_required",
            Self::Applied => "applied",
            Self::ExternallyReversed => "externally_reversed",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessorCharge {
    id: ProcessorChargeId,
    attempt_id: PaymentAttemptId,
    billing_scope_id: BillingScopeId,
    gateway_account_id: GatewayAccountId,
    gateway_order_id: GatewayOrderId,
    attempt_kind: PaymentAttemptKind,
    amount: Money,
    role: ProcessorChargeRole,
    progression: ProcessorChargeProgression,
    state_code: Option<ProcessorChargeStateCode>,
    evidence: ProcessorEvidence,
    observed_at: DateTime<Utc>,
}

impl ProcessorCharge {
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        id: ProcessorChargeId,
        attempt_id: PaymentAttemptId,
        billing_scope_id: BillingScopeId,
        gateway_account_id: GatewayAccountId,
        gateway_order_id: GatewayOrderId,
        attempt_kind: PaymentAttemptKind,
        amount: Money,
        role: ProcessorChargeRole,
        progression: ProcessorChargeProgression,
        state_code: Option<ProcessorChargeStateCode>,
        evidence: ProcessorEvidence,
        observed_at: DateTime<Utc>,
    ) -> Self {
        Self {
            id,
            attempt_id,
            billing_scope_id,
            gateway_account_id,
            gateway_order_id,
            attempt_kind,
            amount,
            role,
            progression,
            state_code,
            evidence,
            observed_at,
        }
    }

    pub const fn id(&self) -> ProcessorChargeId {
        self.id
    }
    pub const fn attempt_id(&self) -> PaymentAttemptId {
        self.attempt_id
    }
    pub const fn billing_scope_id(&self) -> BillingScopeId {
        self.billing_scope_id
    }
    pub const fn gateway_account_id(&self) -> GatewayAccountId {
        self.gateway_account_id
    }
    pub const fn gateway_order_id(&self) -> &GatewayOrderId {
        &self.gateway_order_id
    }
    pub const fn attempt_kind(&self) -> PaymentAttemptKind {
        self.attempt_kind
    }
    pub const fn amount(&self) -> Money {
        self.amount
    }
    pub const fn role(&self) -> ProcessorChargeRole {
        self.role
    }
    pub const fn progression(&self) -> ProcessorChargeProgression {
        self.progression
    }
    pub const fn state_code(&self) -> Option<ProcessorChargeStateCode> {
        self.state_code
    }
    pub const fn evidence(&self) -> &ProcessorEvidence {
        &self.evidence
    }
    pub const fn observed_at(&self) -> DateTime<Utc> {
        self.observed_at
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessorChargeReviewItem {
    attempt: PaymentAttempt,
    charge: ProcessorCharge,
    external_reversal_required_at: DateTime<Utc>,
}

impl ProcessorChargeReviewItem {
    pub const fn new(
        attempt: PaymentAttempt,
        charge: ProcessorCharge,
        external_reversal_required_at: DateTime<Utc>,
    ) -> Self {
        Self {
            attempt,
            charge,
            external_reversal_required_at,
        }
    }
    pub const fn attempt(&self) -> &PaymentAttempt {
        &self.attempt
    }
    pub const fn charge(&self) -> &ProcessorCharge {
        &self.charge
    }
    pub const fn external_reversal_required_at(&self) -> DateTime<Utc> {
        self.external_reversal_required_at
    }
    pub fn into_parts(self) -> (PaymentAttempt, ProcessorCharge, DateTime<Utc>) {
        (
            self.attempt,
            self.charge,
            self.external_reversal_required_at,
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessorChargeReviewPage {
    items: Vec<ProcessorChargeReviewItem>,
    next_cursor: Option<ProcessorChargeReviewCursor>,
}

impl ProcessorChargeReviewPage {
    pub fn new(
        items: Vec<ProcessorChargeReviewItem>,
        next_cursor: Option<ProcessorChargeReviewCursor>,
    ) -> Self {
        Self { items, next_cursor }
    }
    pub fn into_items(self) -> Vec<ProcessorChargeReviewItem> {
        self.items
    }
    pub const fn next_cursor(&self) -> Option<ProcessorChargeReviewCursor> {
        self.next_cursor
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ManualFailureHostCharge {
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    target_id: HostChargeTargetId,
}

impl ManualFailureHostCharge {
    pub const fn new(
        billing_scope_id: BillingScopeId,
        subscriber_id: SubscriberId,
        target_id: HostChargeTargetId,
    ) -> Self {
        Self {
            billing_scope_id,
            subscriber_id,
            target_id,
        }
    }

    pub const fn billing_scope_id(self) -> BillingScopeId {
        self.billing_scope_id
    }

    pub const fn subscriber_id(self) -> SubscriberId {
        self.subscriber_id
    }

    pub const fn target_id(self) -> HostChargeTargetId {
        self.target_id
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ManualAttemptFailureOutcome {
    Failed(PaymentAttempt),
    KeptOpen(PaymentAttempt),
    NotFound,
}

pub fn review_required_attempt_can_be_manually_failed(attempt: &PaymentAttempt) -> bool {
    if attempt.status() != PaymentAttemptStatus::ReviewRequired {
        return false;
    }
    if attempt.kind() == PaymentAttemptKind::SubscriptionPaymentMethodUpdate {
        return attempt
            .request()
            .target()
            .payment_method_update_snapshot()
            .is_some();
    }
    if attempt.kind() == PaymentAttemptKind::HostCharge
        && attempt.state().timestamps().submitted_at().is_some()
    {
        return false;
    }
    let evidence = attempt.state().processor_evidence();
    !evidence.has_gateway_reference() && !processor_evidence_indicates_approval(evidence)
}

pub fn review_required_manual_failure_evidence(attempt: &PaymentAttempt) -> ProcessorEvidence {
    let current = attempt.state().processor_evidence();
    let response_text = if attempt.kind() == PaymentAttemptKind::SubscriptionPaymentMethodUpdate
        && payment_method_update_has_processor_evidence(current)
    {
        payment_method_update_manual_failure_response_text(
            current.response_text().map(GatewayDiagnostic::expose),
        )
    } else {
        GatewayDiagnostic::new(MANUAL_ATTEMPT_FAILURE_NOTE)
    };
    let condition = if attempt.kind() == PaymentAttemptKind::SubscriptionPaymentMethodUpdate
        && payment_method_update_has_processor_evidence(current)
    {
        current.condition().cloned()
    } else {
        Some(GatewayDiagnostic::new("failed"))
    };
    ProcessorEvidence::new(
        current.transaction_id().cloned(),
        current.payment_method_reference().cloned(),
        current.response().cloned(),
        current.response_code().cloned(),
        Some(response_text),
        condition,
        current.descriptor().clone(),
    )
}

fn payment_method_update_has_processor_evidence(evidence: &ProcessorEvidence) -> bool {
    evidence.has_gateway_reference()
        || evidence.response().is_some()
        || evidence.response_code().is_some()
        || evidence.condition().is_some()
        || evidence.descriptor().payment_type().is_some()
        || evidence.descriptor().card_brand().is_some()
        || evidence.descriptor().card_last_four().is_some()
        || evidence.descriptor().card_exp_month().is_some()
        || evidence.descriptor().card_exp_year().is_some()
        || evidence.response_text().is_some_and(|value| {
            value
                .expose()
                .trim()
                .to_ascii_lowercase()
                .contains(&PROCESSOR_RESPONSE_MARKER.to_ascii_lowercase())
                || gateway_text_says_approved(value.expose())
        })
}

fn payment_method_update_manual_failure_response_text(existing: Option<&str>) -> GatewayDiagnostic {
    let Some(existing) = existing.map(str::trim).filter(|value| !value.is_empty()) else {
        return GatewayDiagnostic::new(PAYMENT_METHOD_UPDATE_MANUAL_CLOSURE_NOTE);
    };
    if existing.contains(PAYMENT_METHOD_UPDATE_MANUAL_CLOSURE_NOTE) {
        return GatewayDiagnostic::new(existing);
    }
    GatewayDiagnostic::new(&format!(
        "{existing} {PAYMENT_METHOD_UPDATE_MANUAL_CLOSURE_NOTE}"
    ))
}

fn processor_evidence_indicates_approval(evidence: &ProcessorEvidence) -> bool {
    crate::gateway_response_is_approved(evidence.response().map(GatewayDiagnostic::expose))
        || crate::gateway_response_is_approved(
            evidence.response_code().map(GatewayDiagnostic::expose),
        )
        || evidence
            .condition()
            .is_some_and(|value| crate::gateway_state_is_approved(value.expose()))
        || evidence
            .response_text()
            .is_some_and(|value| gateway_text_says_approved(value.expose()))
}

fn gateway_text_says_approved(value: &str) -> bool {
    APPROVED_WORD_RE.is_match(value.trim())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExternalReversalAttestation {
    processor_charge_id: ProcessorChargeId,
    attempt_id: PaymentAttemptId,
    actor_id: ActorId,
    reason: ExternalReversalReason,
    resolution: ExternalReversalResolution,
    gateway_account_id: GatewayAccountId,
    gateway_configuration_id: GatewayConfigurationId,
    gateway_order_id: GatewayOrderId,
    amount: ChargeAmount,
    gateway_transaction_id: GatewayTransactionId,
    processor_evidence: ProcessorEvidence,
    attested_at: DateTime<Utc>,
}

impl ExternalReversalAttestation {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        processor_charge_id: ProcessorChargeId,
        attempt_id: PaymentAttemptId,
        actor_id: ActorId,
        reason: ExternalReversalReason,
        resolution: ExternalReversalResolution,
        gateway_account_id: GatewayAccountId,
        gateway_configuration_id: GatewayConfigurationId,
        gateway_order_id: GatewayOrderId,
        amount: ChargeAmount,
        gateway_transaction_id: GatewayTransactionId,
        processor_evidence: ProcessorEvidence,
        attested_at: DateTime<Utc>,
    ) -> Self {
        Self {
            processor_charge_id,
            attempt_id,
            actor_id,
            reason,
            resolution,
            gateway_account_id,
            gateway_configuration_id,
            gateway_order_id,
            amount,
            gateway_transaction_id,
            processor_evidence,
            attested_at,
        }
    }

    /// Validates the historical raw resolution fields before constructing the typed form.
    #[allow(clippy::too_many_arguments)]
    pub fn from_legacy_parts(
        processor_charge_id: ProcessorChargeId,
        attempt_id: PaymentAttemptId,
        actor_id: ActorId,
        kind: ExternalReversalKind,
        reason: ExternalReversalReason,
        prior_resolution_code: impl AsRef<str>,
        final_resolution_code: PaymentResolutionCode,
        gateway_account_id: GatewayAccountId,
        gateway_configuration_id: GatewayConfigurationId,
        gateway_order_id: GatewayOrderId,
        amount: ChargeAmount,
        gateway_transaction_id: GatewayTransactionId,
        processor_evidence: ProcessorEvidence,
        attested_at: DateTime<Utc>,
    ) -> Result<Self, ExternalReversalResolutionError> {
        let prior = ExternalReversalPriorClassification::from_resolution_code(
            prior_resolution_code.as_ref(),
        )?;
        let outcome = ExternalReversalOutcome::from_kind_and_final_resolution_code(
            kind,
            final_resolution_code,
        )?;
        let resolution = ExternalReversalResolution::new(prior, outcome)?;
        Ok(Self::new(
            processor_charge_id,
            attempt_id,
            actor_id,
            reason,
            resolution,
            gateway_account_id,
            gateway_configuration_id,
            gateway_order_id,
            amount,
            gateway_transaction_id,
            processor_evidence,
            attested_at,
        ))
    }

    pub const fn processor_charge_id(&self) -> ProcessorChargeId {
        self.processor_charge_id
    }
    pub const fn attempt_id(&self) -> PaymentAttemptId {
        self.attempt_id
    }
    pub const fn actor_id(&self) -> ActorId {
        self.actor_id
    }
    pub const fn kind(&self) -> ExternalReversalKind {
        self.resolution.kind()
    }
    pub const fn reason(&self) -> &ExternalReversalReason {
        &self.reason
    }
    pub const fn resolution(&self) -> ExternalReversalResolution {
        self.resolution
    }
    pub const fn prior_classification(&self) -> ExternalReversalPriorClassification {
        self.resolution.prior()
    }
    pub const fn outcome(&self) -> ExternalReversalOutcome {
        self.resolution.outcome()
    }
    pub const fn prior_resolution_code(&self) -> &'static str {
        self.resolution.prior_resolution_code()
    }
    pub const fn final_resolution_code(&self) -> PaymentResolutionCode {
        self.resolution.final_resolution_code()
    }
    pub const fn gateway_account_id(&self) -> GatewayAccountId {
        self.gateway_account_id
    }
    pub const fn gateway_configuration_id(&self) -> GatewayConfigurationId {
        self.gateway_configuration_id
    }
    pub const fn gateway_order_id(&self) -> &GatewayOrderId {
        &self.gateway_order_id
    }
    pub const fn amount(&self) -> ChargeAmount {
        self.amount
    }
    pub const fn gateway_transaction_id(&self) -> &GatewayTransactionId {
        &self.gateway_transaction_id
    }
    pub const fn processor_evidence(&self) -> &ProcessorEvidence {
        &self.processor_evidence
    }
    pub const fn attested_at(&self) -> DateTime<Utc> {
        self.attested_at
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExternalReversalHostChargeRelease {
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    target_id: HostChargeTargetId,
}

impl ExternalReversalHostChargeRelease {
    pub const fn new(
        billing_scope_id: BillingScopeId,
        subscriber_id: SubscriberId,
        target_id: HostChargeTargetId,
    ) -> Self {
        Self {
            billing_scope_id,
            subscriber_id,
            target_id,
        }
    }
    pub const fn billing_scope_id(self) -> BillingScopeId {
        self.billing_scope_id
    }
    pub const fn subscriber_id(self) -> SubscriberId {
        self.subscriber_id
    }
    pub const fn target_id(self) -> HostChargeTargetId {
        self.target_id
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use uuid::Uuid;

    use super::*;

    fn review_attempt(
        target: crate::PaymentAttemptTarget,
        cents: i32,
        submitted: bool,
        evidence: ProcessorEvidence,
    ) -> PaymentAttempt {
        let created_at = Utc.with_ymd_and_hms(2026, 8, 8, 0, 0, 0).unwrap();
        PaymentAttempt::new(
            crate::PaymentAttemptIdentity::new(
                PaymentAttemptId::new(Uuid::from_u128(1)),
                BillingScopeId::new(Uuid::from_u128(2)),
                SubscriberId::new(Uuid::from_u128(3)),
                GatewayAccountId::new(Uuid::from_u128(4)),
                GatewayConfigurationId::new(Uuid::from_u128(5)),
                crate::GatewayAccountMode::Live,
            ),
            crate::PaymentAttemptRequest::new(
                target,
                crate::IdempotencyKey::new("manual-review").unwrap(),
                crate::PaymentAttemptFingerprint::new("manual-review-fingerprint").unwrap(),
                crate::Money::new(cents, crate::CurrencyCode::new("USD").unwrap()).unwrap(),
                GatewayOrderId::from_correlation("manual-review-order").unwrap(),
                crate::BillingContactSnapshot::new(None, None),
            ),
            crate::PaymentAttemptState::new(
                PaymentAttemptStatus::ReviewRequired,
                None,
                evidence,
                crate::PaymentAttemptLifecycle::default(),
                crate::PaymentAttemptTimestamps::new(
                    submitted.then_some(created_at),
                    None,
                    Some(created_at),
                    created_at,
                    created_at,
                ),
            ),
        )
        .unwrap()
    }

    #[test]
    fn external_reversal_reason_is_normalized_bounded_and_card_safe() {
        assert!(ExternalReversalReason::new(format!("  {}  ", "é".repeat(500))).is_ok());
        assert_eq!(
            ExternalReversalReason::new("é".repeat(501)),
            Err(ExternalReversalReasonError::TooLong)
        );
        assert_eq!(
            ExternalReversalReason::new(format!("{}4111111111111111", "x".repeat(500))),
            Err(ExternalReversalReasonError::TooLong)
        );
        let reason = ExternalReversalReason::new("  processor refund verified  ").unwrap();
        assert_eq!(reason.expose(), "processor refund verified");
        assert!(!format!("{reason:?}").contains("processor refund verified"));
        assert_eq!(
            ExternalReversalReason::new(" "),
            Err(ExternalReversalReasonError::Empty)
        );
        assert_eq!(
            ExternalReversalReason::new("x".repeat(501)),
            Err(ExternalReversalReasonError::TooLong)
        );
        assert_eq!(
            ExternalReversalReason::new("card 4111111111111111"),
            Err(ExternalReversalReasonError::ContainsRawCardData)
        );
    }

    #[test]
    fn external_reversal_resolution_exhaustively_closes_prior_and_outcome_labels() {
        let priors = [
            ExternalReversalPriorClassification::SubscriptionInitialCurrentGrantConflict,
            ExternalReversalPriorClassification::ProcessorChargeExternalReversalRequired,
        ];
        assert_eq!(ExternalReversalOutcome::ALL.len(), 4);

        for prior in priors {
            for outcome in ExternalReversalOutcome::ALL {
                let resolution = ExternalReversalResolution::new(prior, *outcome);
                let is_invalid = prior
                    == ExternalReversalPriorClassification::SubscriptionInitialCurrentGrantConflict
                    && matches!(
                        *outcome,
                        ExternalReversalOutcome::ProcessorChargeRefunded
                            | ExternalReversalOutcome::ProcessorChargeVoided
                    );
                if is_invalid {
                    assert_eq!(
                        resolution,
                        Err(ExternalReversalResolutionError::IncompatiblePriorOutcome)
                    );
                    continue;
                }

                let resolution = resolution.expect("the remaining tuple labels are valid");
                assert_eq!(resolution.prior(), prior);
                assert_eq!(resolution.outcome(), *outcome);
                assert_eq!(resolution.prior_resolution_code(), prior.resolution_code());
                assert_eq!(resolution.kind(), outcome.kind());
                assert_eq!(
                    resolution.final_resolution_code(),
                    outcome.final_resolution_code()
                );
                assert_eq!(
                    ExternalReversalOutcome::from_kind_and_final_resolution_code(
                        resolution.kind(),
                        resolution.final_resolution_code(),
                    ),
                    Ok(*outcome)
                );
            }
        }

        assert_eq!(
            ExternalReversalPriorClassification::from_resolution_code("unexpected"),
            Err(ExternalReversalResolutionError::InvalidPriorClassification)
        );
        assert_eq!(
            ExternalReversalOutcome::from_kind_and_final_resolution_code(
                ExternalReversalKind::Refund,
                PaymentResolutionCode::SubscriptionInitialExternallyVoided,
            ),
            Err(ExternalReversalResolutionError::InvalidOutcome)
        );
    }

    #[test]
    fn external_reversal_legacy_parts_are_fallible() {
        let attestation = ExternalReversalAttestation::from_legacy_parts(
            ProcessorChargeId::new(Uuid::from_u128(10)),
            PaymentAttemptId::new(Uuid::from_u128(11)),
            ActorId::new(Uuid::from_u128(12)),
            ExternalReversalKind::Refund,
            ExternalReversalReason::new("processor refund verified").unwrap(),
            "processor_charge_external_reversal_required",
            PaymentResolutionCode::ProcessorChargeExternallyRefunded,
            GatewayAccountId::new(Uuid::from_u128(13)),
            GatewayConfigurationId::new(Uuid::from_u128(14)),
            GatewayOrderId::from_correlation("legacy-reversal-order").unwrap(),
            ChargeAmount::new(500, crate::CurrencyCode::new("USD").unwrap()).unwrap(),
            GatewayTransactionId::new("txn-legacy-reversal").unwrap(),
            ProcessorEvidence::default(),
            Utc.with_ymd_and_hms(2026, 8, 23, 0, 0, 0).unwrap(),
        )
        .unwrap();

        assert_eq!(
            attestation.resolution(),
            ExternalReversalResolution::new(
                ExternalReversalPriorClassification::ProcessorChargeExternalReversalRequired,
                ExternalReversalOutcome::ProcessorChargeRefunded,
            )
            .unwrap()
        );
        assert_eq!(attestation.kind(), ExternalReversalKind::Refund);
        assert_eq!(
            attestation.final_resolution_code(),
            PaymentResolutionCode::ProcessorChargeExternallyRefunded
        );

        assert_eq!(
            ExternalReversalAttestation::from_legacy_parts(
                ProcessorChargeId::new(Uuid::from_u128(10)),
                PaymentAttemptId::new(Uuid::from_u128(11)),
                ActorId::new(Uuid::from_u128(12)),
                ExternalReversalKind::Refund,
                ExternalReversalReason::new("processor refund verified").unwrap(),
                "subscription_initial_current_grant_conflict",
                PaymentResolutionCode::ProcessorChargeExternallyRefunded,
                GatewayAccountId::new(Uuid::from_u128(13)),
                GatewayConfigurationId::new(Uuid::from_u128(14)),
                GatewayOrderId::from_correlation("legacy-reversal-order").unwrap(),
                ChargeAmount::new(500, crate::CurrencyCode::new("USD").unwrap()).unwrap(),
                GatewayTransactionId::new("txn-legacy-reversal").unwrap(),
                ProcessorEvidence::default(),
                Utc.with_ymd_and_hms(2026, 8, 23, 0, 0, 0).unwrap(),
            ),
            Err(ExternalReversalResolutionError::IncompatiblePriorOutcome)
        );
    }

    #[test]
    fn processor_charge_state_codes_cover_charge_specific_workflow_states() {
        assert_eq!(
            ProcessorChargeStateCode::AdditionalApprovedChargeIdentified.as_str(),
            "additional_approved_charge_identified"
        );
        assert_eq!(
            ProcessorChargeStateCode::TransactionIdentityRequired.as_str(),
            "processor_charge_transaction_identity_required"
        );
        assert_eq!(
            ProcessorChargeStateCode::ApprovedChargeWaitingForApplication.as_str(),
            "approved_charge_waiting_for_application"
        );
        assert_eq!(
            ProcessorChargeStateCode::ZeroAmountAdditionalApprovedCharge.as_str(),
            "zero_amount_additional_approved_charge"
        );
    }

    #[test]
    fn operator_review_page_limit_is_closed_and_bounded() {
        assert_eq!(OperatorReviewPageLimit::new(1).unwrap().get(), 1);
        assert_eq!(
            OperatorReviewPageLimit::new(OPERATOR_REVIEW_PAGE_LIMIT)
                .unwrap()
                .get(),
            OPERATOR_REVIEW_PAGE_LIMIT
        );
        assert_eq!(
            OperatorReviewPageLimit::new(0),
            Err(OperatorReviewPageLimitError)
        );
        assert_eq!(
            OperatorReviewPageLimit::new(OPERATOR_REVIEW_PAGE_LIMIT + 1),
            Err(OperatorReviewPageLimitError)
        );
    }

    #[test]
    fn manual_failure_policy_refuses_charge_risk_and_preserves_update_evidence() {
        let host = || crate::PaymentAttemptTarget::HostCharge {
            target_id: HostChargeTargetId::new(Uuid::from_u128(6)),
        };
        assert!(review_required_attempt_can_be_manually_failed(
            &review_attempt(host(), 500, false, ProcessorEvidence::default())
        ));
        assert!(!review_required_attempt_can_be_manually_failed(
            &review_attempt(host(), 500, true, ProcessorEvidence::default())
        ));
        let approved = ProcessorEvidence::new(
            None,
            None,
            None,
            None,
            Some(GatewayDiagnostic::new("not approved by support")),
            None,
            crate::GatewayPaymentDescriptor::default(),
        );
        assert!(!review_required_attempt_can_be_manually_failed(
            &review_attempt(host(), 500, false, approved)
        ));

        let update_evidence = ProcessorEvidence::new(
            Some(GatewayTransactionId::new("txn-update-review").unwrap()),
            None,
            Some(GatewayDiagnostic::new("1")),
            Some(GatewayDiagnostic::new("100")),
            Some(GatewayDiagnostic::new("Processor response: Approved")),
            Some(GatewayDiagnostic::new("complete")),
            crate::GatewayPaymentDescriptor::default(),
        );
        let update = review_attempt(
            crate::PaymentAttemptTarget::SubscriptionPaymentMethodUpdate {
                plan_key: crate::PlanKey::new("test_plan").unwrap(),
                payment_method_id: crate::PaymentMethodId::new(Uuid::from_u128(7)),
                expected_state: crate::PaymentMethodUpdateSnapshot::new(
                    crate::SubscriptionId::new(Uuid::from_u128(8)),
                    crate::PaymentMethodId::new(Uuid::from_u128(7)),
                    GatewayTransactionId::new("txn-initial").unwrap(),
                ),
            },
            0,
            true,
            update_evidence,
        );
        assert!(review_required_attempt_can_be_manually_failed(&update));
        let preserved = review_required_manual_failure_evidence(&update);
        assert_eq!(
            preserved.transaction_id().map(GatewayTransactionId::expose),
            Some("txn-update-review")
        );
        assert_eq!(
            preserved.condition().map(GatewayDiagnostic::expose),
            Some("complete")
        );
        let response_text = preserved
            .response_text()
            .expect("manual closure note")
            .expose();
        assert!(response_text.contains("Processor response: Approved"));
        assert!(response_text.contains(PAYMENT_METHOD_UPDATE_MANUAL_CLOSURE_NOTE));
    }
}
