use std::fmt;

use chrono::{DateTime, Utc};
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
    !evidence.has_gateway_reference() && !evidence.may_indicate_approval()
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
    .with_approval_evidence(current.approval_evidence())
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
        || evidence.response_text().is_some()
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

include!("operator_review/reversal.rs");
