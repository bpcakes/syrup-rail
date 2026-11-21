//! Exact transaction-diagnostics lookups: read-only observations of one
//! transaction's actions, never payment evidence.

use super::*;

/// Parses an exact diagnostic lookup. Structural envelope problems and
/// `error_response` keep their ordinary query error classification; problems
/// inside the one returned transaction are reported through the `malformed`
/// and `incomplete` flags so the adapter can decline to select an action.
pub(in crate::client) fn query_diagnostics_from_xml(
    text: &str,
) -> Result<TransactionDiagnosticsLookup, WireError> {
    let document = parse_transaction_report_xml(text)?;
    let envelope = successful_query_response_root(&document, "diagnostic query")?;
    let transaction_count = direct_element_count_without_nested_matches(
        envelope,
        "transaction",
        "diagnostic query response",
    )?;
    let transaction = match transaction_count {
        0 => return Ok(TransactionDiagnosticsLookup::NotFound),
        1 => element_children_named(envelope, "transaction")
            .next()
            .ok_or_else(|| {
                WireError::MalformedResponse(
                    "NMI diagnostic query response lost its transaction.".to_owned(),
                )
            })?,
        _ => return Ok(TransactionDiagnosticsLookup::MultipleTransactions),
    };
    let mut malformed = false;
    let mut incomplete = false;
    let transaction_id = report_field(
        collect_xml_identifier(
            transaction,
            &["transaction_id"],
            IdentifierPresence::Required,
        ),
        &mut malformed,
    );
    let order_id = report_field(
        collect_xml_identifier(transaction, &["order_id"], IdentifierPresence::Optional),
        &mut malformed,
    );
    let currency = report_field(
        collect_xml_scalar(transaction, &["currency"], Protocol).finish(),
        &mut malformed,
    );
    let avs_response = optional_diagnostic_code(transaction, "avs_response", &mut incomplete);
    let csc_response = optional_diagnostic_code(transaction, "csc_response", &mut incomplete);
    let action_count = direct_element_count_without_nested_matches(
        transaction,
        "action",
        "diagnostic query response",
    )
    .unwrap_or_else(|_| {
        malformed = true;
        0
    });
    if action_count > MAX_NMI_DIAGNOSTIC_ACTIONS {
        malformed = true;
    }
    let mut actions = Vec::new();
    if !malformed {
        for action in element_children_named(transaction, "action") {
            // Resolution preserves a provider spelling; selection compares
            // the canonical forms. The action type is a code: bound every
            // occurrence before normalization removes separators or duplicate
            // resolution picks an equivalent spelling, and treat an oversized
            // type like a conflicting one, because an unclassifiable action
            // makes exactly one match unprovable.
            let action_type = if diagnostic_code_oversized(action, "action_type") {
                malformed = true;
                None
            } else {
                report_field(
                    collect_xml_scalar(action, &["action_type"], Protocol)
                        .finish_normalized(normalize_gateway_state),
                    &mut malformed,
                )
                .map(|value| normalize_gateway_state(&value))
            };
            let amount = report_field(
                collect_xml_scalar(action, &["amount"], Protocol)
                    .finish_normalized(normalize_report_amount),
                &mut malformed,
            )
            .map(|value| normalize_report_amount(&value));
            let mut action_incomplete = false;
            actions.push(TransactionDiagnosticsActionParts {
                action_type: action_type.map(SensitiveText::new),
                amount: amount.map(SensitiveText::new),
                response_code: optional_diagnostic_code(
                    action,
                    "response_code",
                    &mut action_incomplete,
                ),
                response_text: optional_diagnostic_field(
                    action,
                    "response_text",
                    &mut action_incomplete,
                ),
                processor_response_code: optional_diagnostic_code(
                    action,
                    "processor_response_code",
                    &mut action_incomplete,
                ),
                processor_response_text: optional_diagnostic_field(
                    action,
                    "processor_response_text",
                    &mut action_incomplete,
                ),
                incomplete: action_incomplete,
            });
        }
    }
    if malformed {
        // No partial action may escape a transaction whose selection fields
        // cannot be trusted.
        actions = Vec::new();
    }
    Ok(TransactionDiagnosticsLookup::Found(
        TransactionDiagnostics::new(TransactionDiagnosticsParts {
            transaction_id: transaction_id.map(SensitiveText::new),
            order_id: order_id.map(SensitiveText::new),
            currency: currency.map(SensitiveText::new),
            avs_response,
            csc_response,
            actions,
            malformed,
            incomplete,
        }),
    ))
}

/// Reads one optional provider code without lossy truncation. A code longer
/// than [`MAX_NMI_DIAGNOSTIC_CODE_BYTES`] after trimming, or one too long to
/// parse at all, is omitted and marks the observation incomplete rather than
/// being shortened into a different, apparently valid code.
fn optional_diagnostic_code(
    node: roxmltree::Node<'_, '_>,
    name: &'static str,
    incomplete: &mut bool,
) -> Option<SensitiveText> {
    if all_diagnostic_occurrences_empty(node, name)? {
        return None;
    }
    if diagnostic_code_oversized(node, name) {
        *incomplete = true;
        return None;
    }
    let (value, invalid) =
        resolve_optional_scalar(collect_xml_scalar(node, &[name], Protocol).finish());
    *incomplete |= invalid || value.is_none();
    value.map(SensitiveText::new)
}

/// Whether any occurrence of a code element exceeds
/// [`MAX_NMI_DIAGNOSTIC_CODE_BYTES`] after trimming. Checked on every raw
/// occurrence before duplicate resolution, which may select a different,
/// equivalent spelling and so hide an oversized one.
fn diagnostic_code_oversized(node: roxmltree::Node<'_, '_>, name: &'static str) -> bool {
    element_children_named(node, name).any(|occurrence| {
        occurrence
            .children()
            .filter(|child| child.is_text())
            .filter_map(|child| child.text())
            .collect::<String>()
            .trim()
            .len()
            > MAX_NMI_DIAGNOSTIC_CODE_BYTES
    })
}

/// Returns `None` when the element is absent and whether every occurrence is
/// empty otherwise.
fn all_diagnostic_occurrences_empty(
    node: roxmltree::Node<'_, '_>,
    name: &'static str,
) -> Option<bool> {
    let mut occurrences = element_children_named(node, name).peekable();
    occurrences.peek()?;
    Some(occurrences.all(|occurrence| {
        occurrence
            .children()
            .all(|child| child.is_text() && child.text().is_none_or(|text| text.trim().is_empty()))
    }))
}

/// Reads one optional, bounded diagnostic text field. Empty elements are
/// absent; a conflicting, nested, or otherwise unusable value is omitted and
/// marks the observation incomplete.
fn optional_diagnostic_field(
    node: roxmltree::Node<'_, '_>,
    name: &'static str,
    incomplete: &mut bool,
) -> Option<SensitiveText> {
    if all_diagnostic_occurrences_empty(node, name)? {
        return None;
    }
    let (value, invalid) =
        resolve_optional_scalar(collect_xml_scalar(node, &[name], GatewayField).finish());
    *incomplete |= invalid;
    value.map(SensitiveText::new)
}
