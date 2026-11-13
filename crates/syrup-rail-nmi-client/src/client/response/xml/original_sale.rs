//! Decline evidence carried by a queried transaction's original sale action.
//!
//! NMI's Query API reports a transaction's `condition` on the transaction
//! element and the processor's `response_code` and `response_text` on each
//! `<action>` beneath it. An exact query that reconciles a sale whose response
//! was lost therefore sees a failed transaction with no transaction-level
//! response code.
//! <https://docs.nmi.com/reference/query>

use crate::{PaymentOutcomeDiagnostic, PaymentStatus};

use super::super::common::ScalarBounds::{GatewayField, Protocol};
use super::super::common::{
    DecisionFieldKind, PaymentDecisionFields, ResolvedScalar, ScalarOccurrenceCollector,
    normalize_gateway_state, resolve_optional_scalar,
};
use super::{collect_xml_scalar, normalize_report_success};

/// Response code and text of the one failed sale that produced a failed
/// transaction.
struct FailedSaleEvidence {
    response_code: String,
    response_text: Option<String>,
}

/// The response code and text an exact query reports for `transaction`.
///
/// The transaction-level values are returned unchanged unless all of these
/// hold, in which case the failed original sale supplies both:
///
/// - `status`, already resolved from the transaction-level fields alone, is a
///   decline or determinate failure with no decision diagnostics; and
/// - the transaction has no `response`, `response_code`, or `response_text`
///   element of its own, in any shape.
///
/// A missing or contradictory transaction identity does not block the
/// evidence. Its identifiers stay quarantined and diagnosed, and the query
/// boundary accepts the failure only after a requested selector, such as the
/// order ID, binds it to the request.
///
/// An approved or indeterminate transaction therefore keeps exactly the
/// evidence it had, and an action never replaces transaction-level evidence.
pub(super) fn response_evidence(
    transaction: roxmltree::Node<'_, '_>,
    status: PaymentStatus,
    diagnostics: &[PaymentOutcomeDiagnostic],
    response_code: Option<String>,
    response_text: Option<String>,
) -> (Option<String>, Option<String>) {
    let settled_failure = matches!(status, PaymentStatus::Declined | PaymentStatus::Failed)
        && diagnostics.iter().copied().all(concerns_identity_only);
    let has_own_evidence = transaction.children().any(|child| {
        child.is_element()
            && ["response", "response_code", "response_text"]
                .iter()
                .any(|name| child.has_tag_name(*name))
    });
    if settled_failure
        && !has_own_evidence
        && let Some(evidence) = failed_sale_evidence(transaction)
    {
        return (Some(evidence.response_code), evidence.response_text);
    }
    (response_code, response_text)
}

/// Whether `diagnostic` describes only the transaction's identifiers, which
/// are quarantined independently of its payment decision.
///
/// The match is exhaustive so that a new diagnostic must be classified here
/// before it can reach this gate.
const fn concerns_identity_only(diagnostic: PaymentOutcomeDiagnostic) -> bool {
    match diagnostic {
        PaymentOutcomeDiagnostic::MissingTransactionIdentifier
        | PaymentOutcomeDiagnostic::MissingCustomerVaultIdentifier
        | PaymentOutcomeDiagnostic::InvalidOrConflictingTransactionIdentifier
        | PaymentOutcomeDiagnostic::InvalidOrConflictingCustomerVaultIdentifier => true,
        PaymentOutcomeDiagnostic::InvalidOrConflictingDecisionField
        | PaymentOutcomeDiagnostic::IndeterminatePaymentOutcome
        | PaymentOutcomeDiagnostic::DuplicateTransactionAtProcessor
        | PaymentOutcomeDiagnostic::ConflictingDecisionEvidence
        | PaymentOutcomeDiagnostic::UnrecognizedDecisionEvidence
        | PaymentOutcomeDiagnostic::MissingDecisionEvidence => false,
    }
}

/// Reads the failed original sale of `transaction`.
///
/// This is descriptive evidence for a transaction whose own fields already
/// prove a determinate non-approval; it never decides the payment status.
/// It is returned only when nothing about the action is open to
/// interpretation:
///
/// - the transaction has exactly one `<action>`, directly beneath it;
/// - that action is a `sale` whose `success` is `0`;
/// - the action has exactly one `response_code` element, directly beneath
///   it, so a repeated code is rejected even when its copies agree;
/// - that code is by itself a determinate decline or failure, so an
///   approval, an indeterminate processor error, a duplicate, or an
///   unpublished code is never attached to a failed transaction; and
/// - its `response_text` elements, when there are several, agree. A single
///   empty or malformed text is dropped without discarding the code.
///
/// Anything else returns `None`, which leaves the outcome exactly as the
/// transaction-level fields describe it.
fn failed_sale_evidence(transaction: roxmltree::Node<'_, '_>) -> Option<FailedSaleEvidence> {
    let action = sole_direct_element(transaction, "action")?;
    // The shared collector reduces identical or numerically equivalent
    // repeats to one value, so the single element is required here.
    sole_direct_element(action, "response_code")?;

    let action_type = collect_xml_scalar(action, &["action_type"], Protocol)
        .finish_normalized(normalize_gateway_state);
    if !matches!(&action_type, ResolvedScalar::OneConsistent(value) if normalize_gateway_state(value) == "sale")
    {
        return None;
    }
    let success = collect_xml_scalar(action, &["success"], Protocol)
        .finish_normalized(normalize_report_success);
    if !matches!(&success, ResolvedScalar::OneConsistent(value) if normalize_report_success(value) == "0")
    {
        return None;
    }

    let missing = |kind| ScalarOccurrenceCollector::default().finish_decision(kind);
    let decision = PaymentDecisionFields::new(
        missing(DecisionFieldKind::Response),
        collect_xml_scalar(action, &["response_code"], Protocol)
            .finish_decision(DecisionFieldKind::ResponseCode),
        missing(DecisionFieldKind::GatewayState),
        missing(DecisionFieldKind::GatewayState),
    )
    .resolve();
    if !matches!(
        decision.status,
        PaymentStatus::Declined | PaymentStatus::Failed
    ) || !decision.diagnostics.is_empty()
    {
        return None;
    }
    let response_code = decision.response_code?;

    let (response_text, invalid_text) = resolve_optional_scalar(
        collect_xml_scalar(action, &["response_text"], GatewayField).finish(),
    );
    // The collector cannot tell an empty or malformed text from a conflict.
    // Only several texts can disagree; a single unusable one is dropped, as it
    // is at the transaction level, and leaves the code intact.
    let text_elements = action
        .children()
        .filter(|child| child.is_element() && child.has_tag_name("response_text"))
        .count();
    if invalid_text && text_elements > 1 {
        return None;
    }
    Some(FailedSaleEvidence {
        response_code,
        response_text,
    })
}

/// The one `name` element beneath `parent`, when it is a direct child and no
/// other `name` element appears at any depth.
fn sole_direct_element<'a, 'input>(
    parent: roxmltree::Node<'a, 'input>,
    name: &str,
) -> Option<roxmltree::Node<'a, 'input>> {
    let mut matches = parent
        .descendants()
        .filter(|node| node.is_element() && node.has_tag_name(name));
    let only = matches.next()?;
    (only.parent() == Some(parent) && matches.next().is_none()).then_some(only)
}
