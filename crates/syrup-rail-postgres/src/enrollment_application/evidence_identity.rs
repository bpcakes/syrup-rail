//! Durable processor identity across separate observations of one attempt:
//! which identifiers a non-approved outcome may keep, replace, or must report
//! as conflicting.

use super::*;

pub(crate) struct ReconciledNonApprovedEvidence {
    pub(crate) evidence: ProcessorEvidence,
    pub(crate) transaction_id_conflict: bool,
    pub(crate) payment_method_reference_conflict: bool,
    discarded_transaction_id: bool,
    discarded_payment_method_reference: bool,
}

impl ReconciledNonApprovedEvidence {
    pub(crate) const fn has_identity_conflict(&self) -> bool {
        self.transaction_id_conflict || self.payment_method_reference_conflict
    }

    pub(crate) fn identity_conflict_diagnostics(&self) -> Vec<GatewayPaymentDiagnostic> {
        [
            (self.transaction_id_conflict || self.discarded_transaction_id)
                .then_some(GatewayPaymentDiagnostic::InvalidOrConflictingTransactionIdentifier),
            (self.payment_method_reference_conflict || self.discarded_payment_method_reference)
                .then_some(GatewayPaymentDiagnostic::InvalidOrConflictingPaymentMethodReference),
        ]
        .into_iter()
        .flatten()
        .collect()
    }
}

pub(crate) fn processor_identity_conflict_diagnostics(
    attempt: &PaymentAttempt,
    observed: &ProcessorEvidence,
) -> Vec<GatewayPaymentDiagnostic> {
    let persisted = attempt.state().processor_evidence();
    [
        identifiers_conflict(persisted.transaction_id(), observed.transaction_id())
            .then_some(GatewayPaymentDiagnostic::InvalidOrConflictingTransactionIdentifier),
        identifiers_conflict(
            persisted.payment_method_reference(),
            observed.payment_method_reference(),
        )
        .then_some(GatewayPaymentDiagnostic::InvalidOrConflictingPaymentMethodReference),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// Builds durable non-approved evidence without treating fields from separate
/// processor observations as one approving response.
///
/// A concrete identity conflict preserves the established forensic bundle and
/// forces callers to leave the attempt unresolved. Otherwise the current
/// observation owns the complete decision and descriptor bundles, while an
/// absent AVS result keeps any previously stored AVS code. Missing identity
/// fields may retain established values, but an unanchored partial identity
/// cannot delete or replace an existing durable bundle.
pub(crate) fn reconcile_non_approved_evidence(
    attempt: &PaymentAttempt,
    observed: &ProcessorEvidence,
) -> ReconciledNonApprovedEvidence {
    let persisted = attempt.state().processor_evidence();
    let transaction_id_conflict =
        identifiers_conflict(persisted.transaction_id(), observed.transaction_id());
    let payment_method_reference_conflict = identifiers_conflict(
        persisted.payment_method_reference(),
        observed.payment_method_reference(),
    );
    if !attempt.status().is_resolvable() {
        return ReconciledNonApprovedEvidence {
            evidence: observed.clone(),
            transaction_id_conflict,
            payment_method_reference_conflict,
            discarded_transaction_id: false,
            discarded_payment_method_reference: false,
        };
    }
    if transaction_id_conflict || payment_method_reference_conflict {
        return ReconciledNonApprovedEvidence {
            evidence: persisted.clone(),
            transaction_id_conflict,
            payment_method_reference_conflict,
            discarded_transaction_id: false,
            discarded_payment_method_reference: false,
        };
    }

    let observed_transaction_id = observed.transaction_id();
    let observed_payment_method_reference = observed.payment_method_reference();
    let persisted_transaction_id = persisted.transaction_id();
    let persisted_payment_method_reference = persisted.payment_method_reference();
    // A partial identity from a different observation cannot be spliced onto
    // the established sibling. Preserve the durable bundle, but make the
    // rejected field observable to host policy instead of silently dropping
    // it.
    let discarded_transaction_id = observed_transaction_id.is_some()
        && observed_payment_method_reference.is_none()
        && persisted_transaction_id.is_none()
        && persisted_payment_method_reference.is_some();
    let discarded_payment_method_reference = observed_transaction_id.is_none()
        && observed_payment_method_reference.is_some()
        && persisted_transaction_id.is_some()
        && persisted_payment_method_reference.is_none();
    let (transaction_id, payment_method_reference) =
        match (observed_transaction_id, observed_payment_method_reference) {
            (None, None) => (
                persisted_transaction_id.cloned(),
                persisted_payment_method_reference.cloned(),
            ),
            (Some(transaction_id), Some(payment_method_reference)) => (
                Some(transaction_id.clone()),
                Some(payment_method_reference.clone()),
            ),
            (Some(transaction_id), None) if persisted_transaction_id == Some(transaction_id) => (
                Some(transaction_id.clone()),
                persisted_payment_method_reference.cloned(),
            ),
            (None, Some(payment_method_reference))
                if persisted_payment_method_reference == Some(payment_method_reference) =>
            {
                (
                    persisted_transaction_id.cloned(),
                    Some(payment_method_reference.clone()),
                )
            }
            (Some(transaction_id), None) if !persisted.has_gateway_reference() => {
                (Some(transaction_id.clone()), None)
            }
            (None, Some(payment_method_reference)) if !persisted.has_gateway_reference() => {
                (None, Some(payment_method_reference.clone()))
            }
            (Some(_), None) | (None, Some(_)) => (
                persisted_transaction_id.cloned(),
                persisted_payment_method_reference.cloned(),
            ),
        };
    ReconciledNonApprovedEvidence {
        evidence: ProcessorEvidence::new(
            transaction_id,
            payment_method_reference,
            observed.response().cloned(),
            observed.response_code().cloned(),
            observed.response_text().cloned(),
            observed.condition().cloned(),
            observed.descriptor().clone(),
        )
        .with_avs_response(observed.avs_response().or(persisted.avs_response())),
        transaction_id_conflict: false,
        payment_method_reference_conflict: false,
        discarded_transaction_id,
        discarded_payment_method_reference,
    }
}

fn identifiers_conflict<T: Eq>(persisted: Option<&T>, observed: Option<&T>) -> bool {
    matches!((persisted, observed), (Some(persisted), Some(observed)) if persisted != observed)
}
