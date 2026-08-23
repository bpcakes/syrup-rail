use std::borrow::Cow;

use crate::{PaymentOutcome, PaymentOutcomeDiagnostic, PaymentStatus, SensitiveText};

use super::super::{
    MAX_NMI_FIELD_CHARS, WireError,
    text::{parse_provider_scalar, truncate_gateway_text},
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::client) enum ResolvedScalar {
    Missing,
    OneConsistent(String),
    InvalidOrConflicting,
}

pub(in crate::client) enum ScalarOccurrence<'a> {
    Scalar(Cow<'a, str>),
    Null,
    InvalidShape,
}

#[derive(Clone, Copy)]
pub(super) enum IdentifierPresence {
    Required,
    Optional,
}

#[derive(Default)]
pub(in crate::client) struct ScalarOccurrenceCollector {
    values: Vec<String>,
    comparison_values: Vec<String>,
    saw_occurrence: bool,
    invalid: bool,
}

impl ScalarOccurrenceCollector {
    pub(in crate::client) fn record(&mut self, occurrence: ScalarOccurrence<'_>) {
        self.record_with(occurrence, parse_provider_scalar);
    }

    pub(super) fn record_bounded(&mut self, occurrence: ScalarOccurrence<'_>) {
        self.saw_occurrence = true;
        let ScalarOccurrence::Scalar(raw) = occurrence else {
            self.invalid = true;
            return;
        };
        let raw = raw.trim();
        if raw.is_empty() {
            self.invalid = true;
            return;
        }
        self.values
            .push(truncate_gateway_text(raw, MAX_NMI_FIELD_CHARS));
        self.comparison_values.push(raw.to_owned());
    }

    pub(super) fn record_identifier(
        &mut self,
        occurrence: ScalarOccurrence<'_>,
        presence: IdentifierPresence,
    ) {
        match occurrence {
            ScalarOccurrence::Null if matches!(presence, IdentifierPresence::Optional) => {}
            ScalarOccurrence::Scalar(raw)
                if matches!(presence, IdentifierPresence::Optional) && raw.trim().is_empty() => {}
            ScalarOccurrence::Null => self.record(ScalarOccurrence::InvalidShape),
            occurrence => self.record(occurrence),
        }
    }

    fn record_with(&mut self, occurrence: ScalarOccurrence<'_>, parse: fn(&str) -> Option<String>) {
        self.saw_occurrence = true;
        let ScalarOccurrence::Scalar(raw) = occurrence else {
            self.invalid = true;
            return;
        };
        let Some(value) = parse(&raw) else {
            self.invalid = true;
            return;
        };
        self.comparison_values.push(value.clone());
        self.values.push(value);
    }

    pub(in crate::client) fn finish(self) -> ResolvedScalar {
        self.finish_normalized(identity_scalar)
    }

    pub(super) fn finish_normalized(self, normalize: fn(&str) -> String) -> ResolvedScalar {
        if self.invalid {
            ResolvedScalar::InvalidOrConflicting
        } else if self.values.is_empty() {
            debug_assert!(!self.saw_occurrence);
            ResolvedScalar::Missing
        } else {
            let selected = &self.values[0];
            // Normalize the retained, bounded value rather than the original
            // comparison copy. A future bounded-field normalizer must never
            // reintroduce an unbounded provider value into the result.
            let selected_normalized = normalize(selected);
            let comparison_normalized = normalize(&self.comparison_values[0]);
            let raw_is_consistent = self
                .comparison_values
                .iter()
                .skip(1)
                .all(|value| value == &self.comparison_values[0]);
            if self
                .comparison_values
                .iter()
                .skip(1)
                .any(|value| normalize(value) != comparison_normalized)
            {
                ResolvedScalar::InvalidOrConflicting
            } else if raw_is_consistent {
                ResolvedScalar::OneConsistent(selected.clone())
            } else {
                ResolvedScalar::OneConsistent(selected_normalized)
            }
        }
    }

    fn finish_decision(self, kind: DecisionFieldKind) -> DecisionField {
        if self.invalid {
            return DecisionField::InvalidOrConflicting;
        }
        let Some(selected) = self.values.first() else {
            debug_assert!(!self.saw_occurrence);
            return DecisionField::Missing;
        };
        let selected_evidence = kind.classify(selected);
        let selected_normalized = kind.normalize(selected);
        let conflicting_status = self
            .values
            .iter()
            .skip(1)
            .any(|value| kind.classify(value) != selected_evidence);
        let conflicting_raw = self
            .values
            .iter()
            .skip(1)
            .any(|value| kind.normalize(value) != selected_normalized);
        let raw = (!conflicting_raw).then(|| selected.clone());
        match (conflicting_status, selected_evidence) {
            (true, _) => DecisionField::InvalidOrConflicting,
            (false, Some(status)) => DecisionField::Classified { raw, status },
            (false, None) => DecisionField::Unrecognized { raw },
        }
    }
}

#[derive(Clone, Copy)]
enum DecisionFieldKind {
    Response,
    ResponseCode,
    GatewayState,
}

impl DecisionFieldKind {
    fn classify(self, value: &str) -> Option<PaymentStatus> {
        match self {
            Self::Response => match value.trim() {
                "1" => Some(PaymentStatus::Approved),
                "2" => Some(PaymentStatus::Declined),
                "3" => Some(PaymentStatus::Failed),
                _ => None,
            },
            Self::ResponseCode => classified_payment_status_from_response_code(value),
            Self::GatewayState => classified_payment_status_from_gateway_state(value),
        }
    }

    fn normalize(self, value: &str) -> String {
        match self {
            Self::Response | Self::ResponseCode => value.trim().to_owned(),
            Self::GatewayState => normalize_gateway_state(value),
        }
    }
}

enum DecisionField {
    Missing,
    InvalidOrConflicting,
    Unrecognized {
        raw: Option<String>,
    },
    Classified {
        raw: Option<String>,
        status: PaymentStatus,
    },
}

impl DecisionField {
    fn evidence(&self) -> Option<PaymentStatus> {
        match self {
            Self::Classified { status, .. } => Some(*status),
            Self::Missing | Self::InvalidOrConflicting | Self::Unrecognized { .. } => None,
        }
    }

    fn is_present(&self) -> bool {
        !matches!(self, Self::Missing)
    }

    fn raw(&self) -> Option<&str> {
        match self {
            Self::Unrecognized { raw } | Self::Classified { raw, .. } => raw.as_deref(),
            Self::Missing | Self::InvalidOrConflicting => None,
        }
    }

    fn into_raw(self) -> Option<String> {
        match self {
            Self::Unrecognized { raw } | Self::Classified { raw, .. } => raw,
            Self::Missing | Self::InvalidOrConflicting => None,
        }
    }
}

pub(super) struct PaymentDecisionFields {
    response: DecisionField,
    response_code: DecisionField,
    status: DecisionField,
    condition: DecisionField,
}

impl PaymentDecisionFields {
    pub(super) fn new(
        response: ScalarOccurrenceCollector,
        response_code: ScalarOccurrenceCollector,
        status: ScalarOccurrenceCollector,
        condition: ScalarOccurrenceCollector,
    ) -> Self {
        Self {
            response: response.finish_decision(DecisionFieldKind::Response),
            response_code: response_code.finish_decision(DecisionFieldKind::ResponseCode),
            status: status.finish_decision(DecisionFieldKind::GatewayState),
            condition: condition.finish_decision(DecisionFieldKind::GatewayState),
        }
    }

    pub(super) fn payment_status(&self) -> (PaymentStatus, Option<PaymentOutcomeDiagnostic>) {
        let fields = [
            &self.response,
            &self.response_code,
            &self.status,
            &self.condition,
        ];
        if fields
            .iter()
            .any(|field| matches!(field, &&DecisionField::InvalidOrConflicting))
        {
            return (
                PaymentStatus::Unknown,
                Some(PaymentOutcomeDiagnostic::InvalidOrConflictingDecisionField),
            );
        }
        if fields
            .iter()
            .any(|field| matches!(field, &&DecisionField::Unrecognized { .. }))
        {
            return (
                PaymentStatus::Unknown,
                Some(PaymentOutcomeDiagnostic::UnrecognizedDecisionEvidence),
            );
        }
        let mut evidence = fields.iter().filter_map(|field| field.evidence());
        let Some(status) = evidence.next() else {
            return (
                PaymentStatus::Unknown,
                Some(PaymentOutcomeDiagnostic::MissingDecisionEvidence),
            );
        };
        if evidence.any(|candidate| candidate != status) {
            (
                PaymentStatus::Unknown,
                Some(PaymentOutcomeDiagnostic::ConflictingDecisionEvidence),
            )
        } else {
            (status, None)
        }
    }

    pub(super) fn has_structured_evidence(&self) -> bool {
        self.response.is_present()
            || self.response_code.is_present()
            || self.status.is_present()
            || self.condition.is_present()
    }

    pub(super) fn is_rate_limited(&self) -> bool {
        self.response.raw() == Some("3")
            && self.response_code.raw() == Some("301")
            && !self.status.is_present()
            && !self.condition.is_present()
    }

    pub(super) fn into_public_raw_fields(self) -> (Option<String>, Option<String>, Option<String>) {
        (
            self.response.into_raw(),
            self.response_code.into_raw(),
            self.condition.into_raw(),
        )
    }
}

pub(super) fn resolve_optional_scalar(value: ResolvedScalar) -> (Option<String>, bool) {
    match value {
        ResolvedScalar::Missing => (None, false),
        ResolvedScalar::OneConsistent(value) => (Some(value), false),
        ResolvedScalar::InvalidOrConflicting => (None, true),
    }
}

pub(super) fn finalize_foreground_identifiers(
    status: &mut PaymentStatus,
    transaction: ResolvedScalar,
    vault: ResolvedScalar,
    diagnostics: &mut Vec<PaymentOutcomeDiagnostic>,
) -> (Option<SensitiveText>, Option<SensitiveText>) {
    let (transaction, invalid_transaction) = resolve_optional_scalar(transaction);
    let (vault, invalid_vault) = resolve_optional_scalar(vault);
    if invalid_transaction {
        diagnostics.push(PaymentOutcomeDiagnostic::InvalidOrConflictingTransactionIdentifier);
    }
    if invalid_vault {
        diagnostics.push(PaymentOutcomeDiagnostic::InvalidOrConflictingCustomerVaultIdentifier);
    }
    if invalid_transaction || invalid_vault {
        *status = PaymentStatus::Unknown;
        return (None, None);
    }
    (transaction.map(Into::into), vault.map(Into::into))
}

#[derive(Clone, Copy)]
pub(in crate::client) enum ApprovedIdentityRequirement {
    Transaction,
    TransactionAndCustomerVault,
}

pub(in crate::client) fn require_approved_identities(
    mut outcome: PaymentOutcome,
    requirement: ApprovedIdentityRequirement,
) -> PaymentOutcome {
    if outcome.status != PaymentStatus::Approved {
        return outcome;
    }

    let missing_transaction = outcome.transaction_id.is_none();
    let missing_customer_vault = matches!(
        requirement,
        ApprovedIdentityRequirement::TransactionAndCustomerVault
    ) && outcome.customer_vault_id.is_none();
    if missing_transaction {
        outcome
            .diagnostics
            .push(PaymentOutcomeDiagnostic::MissingTransactionIdentifier);
    }
    if missing_customer_vault {
        outcome
            .diagnostics
            .push(PaymentOutcomeDiagnostic::MissingCustomerVaultIdentifier);
    }
    if missing_transaction || missing_customer_vault {
        outcome.status = PaymentStatus::Unknown;
    }
    outcome
}

pub(super) fn normalize_gateway_state(value: &str) -> String {
    value
        .trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|character| !character.is_ascii_whitespace() && !matches!(character, '_' | '-'))
        .collect()
}

fn identity_scalar(value: &str) -> String {
    value.to_owned()
}

fn classified_payment_status_from_gateway_state(value: &str) -> Option<PaymentStatus> {
    match normalize_gateway_state(value).as_str() {
        "approved" | "complete" | "completed" | "captured" | "success" | "successful"
        | "pendingsettlement" => Some(PaymentStatus::Approved),
        "declined" => Some(PaymentStatus::Declined),
        "failed" | "error" => Some(PaymentStatus::Failed),
        "voided" | "canceled" | "refunded" | "chargeback" | "unknown" | "pending" | "queued"
        | "inprogress" | "processing" | "review" | "underreview" => Some(PaymentStatus::Unknown),
        _ => None,
    }
}

fn classified_payment_status_from_response_code(value: &str) -> Option<PaymentStatus> {
    match value.trim().parse::<u16>() {
        Ok(100) => Some(PaymentStatus::Approved),
        Ok(200..=299) => Some(PaymentStatus::Declined),
        Ok(300 | 400 | 410 | 411 | 440 | 441 | 460 | 461) => Some(PaymentStatus::Failed),
        Ok(420 | 421 | 430) => Some(PaymentStatus::Unknown),
        _ => None,
    }
}

pub(super) fn rate_limited_wire_error() -> WireError {
    // NMI documents this exact Payment API response as a pre-processing
    // throttle: response=3, response_code=301, and no transaction or lifecycle
    // evidence.
    // https://docs.nmi.com/reference/rate-limiting
    WireError::RateLimited("NMI rate limited the payment request before processing.".to_owned())
}

#[cfg(test)]
pub(in crate::client) fn payment_status_from_response_code(value: &str) -> PaymentStatus {
    classified_payment_status_from_response_code(value).unwrap_or(PaymentStatus::Unknown)
}
