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
    /// A textual wire value whose original spelling can participate in an
    /// exact protocol proof.
    Scalar(Cow<'a, str>),
    /// A scalar coerced from another wire representation (currently a JSON
    /// number). It remains usable as ordinary provider evidence, but can
    /// never satisfy an exact textual protocol contract.
    CoercedScalar(Cow<'a, str>),
    Null,
    InvalidShape,
}

#[derive(Clone, Copy)]
pub(super) enum IdentifierPresence {
    /// Exact-query transaction records are expected to carry their processor
    /// identity. Absence or contradiction is reported as unusable identity;
    /// the query boundary separately binds every supplied selector before a
    /// non-approved decision can be accepted.
    Required,
    /// Foreground mutation responses may explicitly report that no
    /// transaction identity was created. Public operation policy separately
    /// requires an identity for approved outcomes.
    ForegroundRequired,
    Optional,
}

#[derive(Default)]
pub(in crate::client) struct ScalarOccurrenceCollector {
    values: Vec<String>,
    comparison_values: Vec<String>,
    exact_wire_spellings: Vec<Option<String>>,
    saw_occurrence: bool,
    saw_absent_required_identifier: bool,
    saw_bounded_occurrence: bool,
    invalid: bool,
}

impl ScalarOccurrenceCollector {
    pub(in crate::client) fn record(&mut self, occurrence: ScalarOccurrence<'_>) {
        self.record_with(occurrence, parse_provider_scalar);
    }

    pub(super) fn record_bounded(&mut self, occurrence: ScalarOccurrence<'_>) {
        self.saw_occurrence = true;
        self.saw_bounded_occurrence = true;
        let raw = match occurrence {
            ScalarOccurrence::Scalar(raw) | ScalarOccurrence::CoercedScalar(raw) => raw,
            ScalarOccurrence::Null | ScalarOccurrence::InvalidShape => {
                self.invalid = true;
                return;
            }
        };
        let raw = raw.trim();
        if raw.is_empty() {
            self.invalid = true;
            return;
        }
        self.values
            .push(truncate_gateway_text(raw, MAX_NMI_FIELD_CHARS));
        self.comparison_values.push(raw.to_owned());
        // Bounded values are not exact protocol spellings: truncation may
        // have changed the retained value. Keep the occurrence vectors in
        // lockstep so a future decision field cannot accidentally treat a
        // missing entry as exact textual evidence.
        self.exact_wire_spellings.push(None);
    }

    pub(super) fn record_identifier(
        &mut self,
        occurrence: ScalarOccurrence<'_>,
        presence: IdentifierPresence,
    ) {
        match occurrence {
            ScalarOccurrence::Null if matches!(presence, IdentifierPresence::Optional) => {}
            ScalarOccurrence::Scalar(raw) | ScalarOccurrence::CoercedScalar(raw)
                if matches!(presence, IdentifierPresence::Optional) && raw.trim().is_empty() => {}
            ScalarOccurrence::Null
                if matches!(presence, IdentifierPresence::ForegroundRequired) =>
            {
                self.saw_occurrence = true;
                self.saw_absent_required_identifier = true;
            }
            ScalarOccurrence::Scalar(raw) | ScalarOccurrence::CoercedScalar(raw)
                if matches!(presence, IdentifierPresence::ForegroundRequired)
                    && raw.trim().is_empty() =>
            {
                self.saw_occurrence = true;
                self.saw_absent_required_identifier = true;
            }
            ScalarOccurrence::Null => self.record(ScalarOccurrence::InvalidShape),
            occurrence => self.record(occurrence),
        }
    }

    fn record_with(&mut self, occurrence: ScalarOccurrence<'_>, parse: fn(&str) -> Option<String>) {
        self.saw_occurrence = true;
        let (raw, exact_text) = match occurrence {
            ScalarOccurrence::Scalar(raw) => (raw, true),
            ScalarOccurrence::CoercedScalar(raw) => (raw, false),
            ScalarOccurrence::Null | ScalarOccurrence::InvalidShape => {
                self.invalid = true;
                return;
            }
        };
        let Some(value) = parse(&raw) else {
            self.invalid = true;
            return;
        };
        let exact_wire_spelling =
            (exact_text && raw.chars().count() <= MAX_NMI_FIELD_CHARS).then(|| raw.into_owned());
        self.comparison_values.push(value.clone());
        self.values.push(value);
        self.exact_wire_spellings.push(exact_wire_spelling);
    }

    pub(in crate::client) fn finish(self) -> ResolvedScalar {
        self.finish_normalized(identity_scalar)
    }

    pub(super) fn finish_normalized(self, normalize: fn(&str) -> String) -> ResolvedScalar {
        if self.invalid || (self.saw_absent_required_identifier && !self.values.is_empty()) {
            ResolvedScalar::InvalidOrConflicting
        } else if self.values.is_empty() {
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
                // Preserve a spelling the provider actually sent while
                // keeping equivalent duplicates independent of wire order.
                // Prefer the canonical normalized spelling when present.
                ResolvedScalar::OneConsistent(
                    self.values
                        .iter()
                        .find(|value| value.as_str() == selected_normalized)
                        .or_else(|| self.values.iter().min())
                        .cloned()
                        .expect("a selected scalar has at least one value"),
                )
            }
        }
    }

    pub(super) fn finish_decision(self, kind: DecisionFieldKind) -> DecisionField {
        debug_assert_eq!(self.values.len(), self.exact_wire_spellings.len());
        let observed_normalized = self
            .values
            .iter()
            .map(|value| kind.normalize(value))
            .collect::<Vec<_>>();
        if self.invalid {
            return DecisionField {
                saw_occurrence: true,
                invalid_or_conflicting: true,
                observed_normalized,
                wire_spellings: self.exact_wire_spellings,
                saw_bounded_occurrence: self.saw_bounded_occurrence,
                ..DecisionField::empty(kind)
            };
        }
        let Some(selected) = self.values.first() else {
            debug_assert!(!self.saw_occurrence);
            return DecisionField::empty(kind);
        };
        let selected_evidence = kind.classify(selected);
        let selected_normalized = kind.normalize(selected);
        let conflicting_status = self
            .values
            .iter()
            .skip(1)
            .any(|value| kind.classify(value) != selected_evidence);
        let conflicting_normalized = observed_normalized
            .iter()
            .skip(1)
            .any(|value| value != &selected_normalized);
        let invalid_or_conflicting = conflicting_status
            || matches!(kind, DecisionFieldKind::ResponseCode) && conflicting_normalized;
        let semantically_exact_spelling = self
            .comparison_values
            .iter()
            .skip(1)
            .all(|value| value == &self.comparison_values[0]);
        let exact_wire_spelling = self
            .exact_wire_spellings
            .first()
            .and_then(Clone::clone)
            .filter(|selected| {
                self.exact_wire_spellings
                    .iter()
                    .all(|value| value.as_deref() == Some(selected.as_str()))
            });
        let raw = if conflicting_normalized {
            None
        } else if semantically_exact_spelling {
            Some(selected.clone())
        } else {
            // Preserve a value the provider actually sent while keeping the
            // result independent of equivalent duplicate-field order. Prefer
            // the canonical spelling when it was one of those observations.
            self.values
                .iter()
                .find(|value| value.as_str() == selected_normalized)
                .or_else(|| self.values.iter().min())
                .cloned()
        };
        DecisionField {
            kind,
            raw,
            evidence: (!invalid_or_conflicting)
                .then_some(selected_evidence)
                .flatten(),
            saw_occurrence: true,
            invalid_or_conflicting,
            unrecognized: !invalid_or_conflicting && selected_evidence.is_none(),
            exact_wire_spelling,
            observed_normalized,
            wire_spellings: self.exact_wire_spellings,
            saw_bounded_occurrence: self.saw_bounded_occurrence,
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum DecisionFieldKind {
    Response,
    ResponseCode,
    GatewayState,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum DecisionEvidence {
    PaymentStatus(PaymentStatus),
    ProviderError,
    IndeterminatePaymentOutcome,
    ProcessorDuplicate,
}

impl DecisionEvidence {
    const fn payment_status(self) -> Option<PaymentStatus> {
        match self {
            Self::PaymentStatus(status) => Some(status),
            Self::IndeterminatePaymentOutcome | Self::ProcessorDuplicate => {
                Some(PaymentStatus::Unknown)
            }
            Self::ProviderError => None,
        }
    }

    const fn diagnostic(self) -> Option<PaymentOutcomeDiagnostic> {
        match self {
            Self::IndeterminatePaymentOutcome => {
                Some(PaymentOutcomeDiagnostic::IndeterminatePaymentOutcome)
            }
            Self::ProcessorDuplicate => {
                Some(PaymentOutcomeDiagnostic::DuplicateTransactionAtProcessor)
            }
            Self::PaymentStatus(_) | Self::ProviderError => None,
        }
    }
}

impl DecisionFieldKind {
    fn classify(self, value: &str) -> Option<DecisionEvidence> {
        match self {
            Self::Response => match value.trim() {
                "1" => Some(DecisionEvidence::PaymentStatus(PaymentStatus::Approved)),
                "2" => Some(DecisionEvidence::PaymentStatus(PaymentStatus::Declined)),
                // NMI defines response=3 as either a transaction-data error or
                // a system error. It is recognized evidence, but it does not
                // by itself prove whether processing had a financial effect.
                "3" => Some(DecisionEvidence::ProviderError),
                _ => None,
            },
            Self::ResponseCode => classified_decision_evidence_from_response_code(value),
            Self::GatewayState if normalize_gateway_state(value) == "error" => {
                Some(DecisionEvidence::ProviderError)
            }
            Self::GatewayState => classified_payment_status_from_gateway_state(value)
                .map(DecisionEvidence::PaymentStatus),
        }
    }

    fn normalize(self, value: &str) -> String {
        match self {
            Self::Response => value.trim().to_owned(),
            Self::ResponseCode => value
                .trim()
                .parse::<u16>()
                .map(|code| code.to_string())
                .unwrap_or_else(|_| value.trim().to_owned()),
            Self::GatewayState => normalize_gateway_state(value),
        }
    }
}

pub(super) struct DecisionField {
    kind: DecisionFieldKind,
    raw: Option<String>,
    evidence: Option<DecisionEvidence>,
    saw_occurrence: bool,
    invalid_or_conflicting: bool,
    unrecognized: bool,
    exact_wire_spelling: Option<String>,
    observed_normalized: Vec<String>,
    wire_spellings: Vec<Option<String>>,
    saw_bounded_occurrence: bool,
}

impl DecisionField {
    fn empty(kind: DecisionFieldKind) -> Self {
        Self {
            kind,
            raw: None,
            evidence: None,
            saw_occurrence: false,
            invalid_or_conflicting: false,
            unrecognized: false,
            exact_wire_spelling: None,
            observed_normalized: Vec::new(),
            wire_spellings: Vec::new(),
            saw_bounded_occurrence: false,
        }
    }

    fn observed_evidence(&self, expected: DecisionEvidence) -> bool {
        self.observed_normalized
            .iter()
            .any(|value| self.kind.classify(value) == Some(expected))
    }

    fn exactly_matches(&self, expected: &str) -> bool {
        !self.invalid_or_conflicting && self.exact_wire_spelling.as_deref() == Some(expected)
    }

    fn generic_http_status(&self) -> Option<u16> {
        if self.invalid_or_conflicting || !self.saw_occurrence || self.saw_bounded_occurrence {
            return None;
        }
        let normalized = self.observed_normalized.first()?;
        let status = normalized.parse::<u16>().ok()?;
        if !(100..=599).contains(&status)
            || self
                .observed_normalized
                .iter()
                .any(|value| value != normalized)
        {
            return None;
        }
        let canonical = status.to_string();
        (!self.wire_spellings.is_empty()
            && self.wire_spellings.iter().all(|spelling| {
                spelling
                    .as_deref()
                    .is_none_or(|spelling| spelling == canonical)
            }))
        .then_some(status)
    }
}

pub(super) struct PaymentDecisionFields {
    response: DecisionField,
    response_code: DecisionField,
    status: DecisionField,
    condition: DecisionField,
}

pub(super) struct ResolvedPaymentDecision {
    pub(super) status: PaymentStatus,
    pub(super) diagnostics: Vec<PaymentOutcomeDiagnostic>,
    pub(super) response: Option<String>,
    pub(super) response_code: Option<String>,
    pub(super) condition: Option<String>,
    has_structured_evidence: bool,
    has_non_status_payment_processing_evidence: bool,
    status_saw_occurrence: bool,
    generic_http_status: Option<u16>,
    known_preprocessing_rate_limit: bool,
}

impl ResolvedPaymentDecision {
    pub(super) const fn has_structured_evidence(&self) -> bool {
        self.has_structured_evidence
    }

    pub(super) fn has_payment_processing_evidence(&self, http_status: u16) -> bool {
        self.has_non_status_payment_processing_evidence
            || self.status_saw_occurrence && self.generic_http_status != Some(http_status)
    }

    pub(super) const fn is_known_preprocessing_rate_limit(&self) -> bool {
        self.known_preprocessing_rate_limit
    }
}

impl PaymentDecisionFields {
    pub(super) fn new(
        response: DecisionField,
        response_code: DecisionField,
        status: DecisionField,
        condition: DecisionField,
    ) -> Self {
        Self {
            response,
            response_code,
            status,
            condition,
        }
    }

    pub(super) fn resolve(self) -> ResolvedPaymentDecision {
        let fields = [
            &self.response,
            &self.response_code,
            &self.status,
            &self.condition,
        ];
        let specific_error_evidence = [
            DecisionEvidence::IndeterminatePaymentOutcome,
            DecisionEvidence::ProcessorDuplicate,
        ];
        let has_specific_error_diagnostic = specific_error_evidence
            .into_iter()
            .any(|evidence| self.response_code.observed_evidence(evidence));
        let mut diagnostics: Vec<_> = specific_error_evidence
            .into_iter()
            .filter(|evidence| self.response_code.observed_evidence(*evidence))
            .filter_map(DecisionEvidence::diagnostic)
            .collect();
        let saw_provider_error = self
            .response
            .observed_evidence(DecisionEvidence::ProviderError)
            || self
                .status
                .observed_evidence(DecisionEvidence::ProviderError)
            || self
                .condition
                .observed_evidence(DecisionEvidence::ProviderError);
        let status = if fields.iter().any(|field| field.invalid_or_conflicting) {
            diagnostics.push(PaymentOutcomeDiagnostic::InvalidOrConflictingDecisionField);
            PaymentStatus::Unknown
        } else if fields.iter().any(|field| field.unrecognized) {
            diagnostics.push(PaymentOutcomeDiagnostic::UnrecognizedDecisionEvidence);
            PaymentStatus::Unknown
        } else {
            let mut evidence = fields
                .iter()
                .filter_map(|field| field.evidence.and_then(DecisionEvidence::payment_status));
            match evidence.next() {
                None if saw_provider_error => PaymentStatus::Unknown,
                None => {
                    diagnostics.push(PaymentOutcomeDiagnostic::MissingDecisionEvidence);
                    PaymentStatus::Unknown
                }
                Some(status)
                    if evidence.any(|candidate| candidate != status)
                        || saw_provider_error
                            && matches!(
                                status,
                                PaymentStatus::Approved | PaymentStatus::Declined
                            ) =>
                {
                    diagnostics.push(PaymentOutcomeDiagnostic::ConflictingDecisionEvidence);
                    PaymentStatus::Unknown
                }
                Some(status) => status,
            }
        };
        // A generic provider error is safely superseded only when a compatible,
        // determinate failure wins the complete reduction. Merely observing a
        // failed field is insufficient when malformed or conflicting sibling
        // evidence forces the aggregate result to remain unknown.
        if saw_provider_error && status != PaymentStatus::Failed && !has_specific_error_diagnostic {
            diagnostics.push(PaymentOutcomeDiagnostic::IndeterminatePaymentOutcome);
        }
        let has_structured_evidence = self.response.saw_occurrence
            || self.response_code.saw_occurrence
            || self.status.saw_occurrence
            || self.condition.saw_occurrence;
        // A generic HTTP error envelope may repeat the outer status as a
        // canonical numeric `status`. Every other occurrence of that key is
        // anomalous payment-decision evidence and must fail closed.
        let has_non_status_payment_processing_evidence = self.response.saw_occurrence
            || self.response_code.saw_occurrence
            || self.condition.saw_occurrence;
        let status_saw_occurrence = self.status.saw_occurrence;
        let generic_http_status = self.status.generic_http_status();
        let known_preprocessing_rate_limit = self.response.exactly_matches("3")
            && self.response_code.exactly_matches("301")
            && !self.status.saw_occurrence
            && !self.condition.saw_occurrence;
        ResolvedPaymentDecision {
            status,
            diagnostics,
            response: self.response.raw,
            response_code: self.response_code.raw,
            condition: self.condition.raw,
            has_structured_evidence,
            has_non_status_payment_processing_evidence,
            status_saw_occurrence,
            generic_http_status,
            known_preprocessing_rate_limit,
        }
    }
}

pub(super) fn resolve_optional_scalar(value: ResolvedScalar) -> (Option<String>, bool) {
    match value {
        ResolvedScalar::Missing => (None, false),
        ResolvedScalar::OneConsistent(value) => (Some(value), false),
        ResolvedScalar::InvalidOrConflicting => (None, true),
    }
}

/// Resolves missing identities independently, but rejects a structurally
/// invalid identity bundle as a unit for every payment response shape.
///
/// A missing field says nothing about the trustworthiness of its sibling. An
/// invalid or conflicting field instead means the response cannot safely bind
/// either identifier to the same processor decision. Identity usability is
/// distinct from payment certainty: coherent declines and determinate failures
/// remain terminal, while approvals fail closed.
pub(super) fn finalize_payment_identifiers(
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
        if *status == PaymentStatus::Approved {
            *status = PaymentStatus::Unknown;
        }
        return (None, None);
    }
    (transaction.map(Into::into), vault.map(Into::into))
}

#[derive(Clone, Copy)]
pub(in crate::client) enum ApprovedIdentityRequirement {
    ApprovedTransaction,
    ApprovedTransactionAndCustomerVault,
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
        ApprovedIdentityRequirement::ApprovedTransactionAndCustomerVault
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
    if outcome.status == PaymentStatus::Approved && (missing_transaction || missing_customer_vault)
    {
        outcome.status = PaymentStatus::Unknown;
    }
    outcome.normalize_diagnostics()
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
        "failed" => Some(PaymentStatus::Failed),
        "voided" | "canceled" | "refunded" | "chargeback" | "unknown" | "pending" | "queued"
        | "inprogress" | "processing" | "review" | "underreview" => Some(PaymentStatus::Unknown),
        _ => None,
    }
}

fn classified_decision_evidence_from_response_code(value: &str) -> Option<DecisionEvidence> {
    match value.trim().parse::<u16>() {
        Ok(100) => Some(DecisionEvidence::PaymentStatus(PaymentStatus::Approved)),
        Ok(200..=299) => Some(DecisionEvidence::PaymentStatus(PaymentStatus::Declined)),
        Ok(300 | 410 | 411 | 460 | 461) => {
            Some(DecisionEvidence::PaymentStatus(PaymentStatus::Failed))
        }
        // NMI documents these as processor-originated errors, but does not
        // guarantee that they had no financial effect. Keep them reconcilable.
        // https://docs.nmi.com/reference/response-codes
        Ok(400 | 420 | 421 | 440 | 441) => Some(DecisionEvidence::IndeterminatePaymentOutcome),
        // NMI documents 430 only as "Duplicate transaction at processor". It
        // does not guarantee that the processor produced no transaction or
        // other financial evidence, so this must remain reconcilable rather
        // than becoming a known non-submission.
        // https://docs.nmi.com/reference/response-codes
        Ok(430) => Some(DecisionEvidence::ProcessorDuplicate),
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
    classified_decision_evidence_from_response_code(value)
        .and_then(DecisionEvidence::payment_status)
        .unwrap_or(PaymentStatus::Unknown)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_decision_occurrences_cannot_become_exact_http_status_proof() {
        let mut collector = ScalarOccurrenceCollector::default();
        collector.record_bounded(ScalarOccurrence::Scalar(Cow::Borrowed("404")));

        let decision = collector.finish_decision(DecisionFieldKind::GatewayState);

        assert_eq!(decision.observed_normalized.len(), 1);
        assert_eq!(decision.wire_spellings, vec![None]);
        assert_eq!(decision.generic_http_status(), None);
    }
}
