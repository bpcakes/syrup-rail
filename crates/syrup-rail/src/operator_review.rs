use std::fmt;

use chrono::{DateTime, Utc};
use thiserror::Error;

use crate::{
    ActorId, BillingScopeId, ChargeAmount, GatewayAccountId, GatewayConfigurationId,
    GatewayOrderId, GatewayTransactionId, HostChargeTargetId, PaymentAttempt, PaymentAttemptId,
    PaymentAttemptKind, PaymentResolutionCode, ProcessorChargeId, ProcessorEvidence, SubscriberId,
};

pub const OPERATOR_REVIEW_PAGE_LIMIT: i64 = 100;

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

#[derive(Clone, Eq, PartialEq)]
pub struct ExternalReversalReason(String);

impl ExternalReversalReason {
    pub fn new(value: impl Into<String>) -> Result<Self, ExternalReversalReasonError> {
        let value = value.into();
        let value = value.trim();
        if value.is_empty() {
            return Err(ExternalReversalReasonError::Empty);
        }
        if value.chars().count() > 500 {
            return Err(ExternalReversalReasonError::TooLong);
        }
        if crate::string_contains_raw_card_data(value) {
            return Err(ExternalReversalReasonError::ContainsRawCardData);
        }
        Ok(Self(value.to_owned()))
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
    amount: ChargeAmount,
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
        amount: ChargeAmount,
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
    pub const fn amount(&self) -> ChargeAmount {
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExternalReversalAttestation {
    processor_charge_id: ProcessorChargeId,
    attempt_id: PaymentAttemptId,
    actor_id: ActorId,
    kind: ExternalReversalKind,
    reason: ExternalReversalReason,
    prior_resolution_code: String,
    final_resolution_code: PaymentResolutionCode,
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
        kind: ExternalReversalKind,
        reason: ExternalReversalReason,
        prior_resolution_code: String,
        final_resolution_code: PaymentResolutionCode,
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
            kind,
            reason,
            prior_resolution_code,
            final_resolution_code,
            gateway_account_id,
            gateway_configuration_id,
            gateway_order_id,
            amount,
            gateway_transaction_id,
            processor_evidence,
            attested_at,
        }
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
        self.kind
    }
    pub const fn reason(&self) -> &ExternalReversalReason {
        &self.reason
    }
    pub fn prior_resolution_code(&self) -> &str {
        &self.prior_resolution_code
    }
    pub const fn final_resolution_code(&self) -> PaymentResolutionCode {
        self.final_resolution_code
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
    use super::*;

    #[test]
    fn external_reversal_reason_is_normalized_bounded_and_card_safe() {
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
}
