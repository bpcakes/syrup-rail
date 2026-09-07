use std::borrow::Cow;

use crate::{
    AccountMode, PaymentDescriptor, PaymentOutcome, PaymentOutcomeDiagnostic, PaymentStatus,
    SensitiveText, TransactionAction, TransactionQuery, TransactionReport,
    TransactionReportDiagnostic,
};

use super::super::{
    MAX_NMI_REPORT_ACTIONS, MAX_NMI_REPORT_RESPONSE_BYTES, MAX_NMI_TRANSACTION_REPORTS, WireError,
    text::{last4, parse_expiry, sensitive_gateway_field},
    validation::trimmed_optional,
};
use super::common::{
    DecisionFieldKind, IdentifierPresence, PaymentDecisionFields, ResolvedScalar, ScalarOccurrence,
    ScalarOccurrenceCollector, finalize_payment_identifiers, normalize_gateway_state,
    resolve_optional_scalar,
};

fn normalize_report_success(value: &str) -> String {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" => "1".to_owned(),
        "0" | "false" => "0".to_owned(),
        value => value.to_owned(),
    }
}

fn normalize_report_amount(value: &str) -> String {
    let value = value.trim();
    let (negative, unsigned) = value
        .strip_prefix('-')
        .map_or((false, value), |unsigned| (true, unsigned.trim_start()));
    let unsigned = unsigned.strip_prefix('$').unwrap_or(unsigned);
    let mut parts = unsigned.split('.');
    let Some(whole) = parts.next() else {
        return value.to_owned();
    };
    let fraction = parts.next().unwrap_or("");
    if parts.next().is_some()
        || whole.is_empty()
        || !whole.chars().all(|character| character.is_ascii_digit())
        || fraction.len() > 2
        || !fraction.chars().all(|character| character.is_ascii_digit())
    {
        return value.to_owned();
    }
    let whole = whole.trim_start_matches('0');
    let whole = if whole.is_empty() { "0" } else { whole };
    let fraction = match fraction.len() {
        0 => "00".to_owned(),
        1 => format!("{fraction}0"),
        2 => fraction.to_owned(),
        _ => unreachable!(),
    };
    let negative = negative && (whole != "0" || fraction != "00");
    format!("{}{whole}.{fraction}", if negative { "-" } else { "" })
}

fn unique_nmi_response_root<'document, 'input>(
    document: &'document roxmltree::Document<'input>,
    response_kind: &str,
) -> Result<roxmltree::Node<'document, 'input>, WireError> {
    let root = document.root_element();
    let envelope_count = document
        .descendants()
        .filter(|node| node.is_element() && node.has_tag_name("nm_response"))
        .count();
    if !root.has_tag_name("nm_response") || envelope_count != 1 {
        return Err(WireError::MalformedResponse(format!(
            "NMI {response_kind} did not include exactly one root nm_response envelope."
        )));
    }
    Ok(root)
}

fn successful_query_response_root<'document, 'input>(
    document: &'document roxmltree::Document<'input>,
    response_kind: &str,
) -> Result<roxmltree::Node<'document, 'input>, WireError> {
    let root = unique_nmi_response_root(document, response_kind)?;
    let mut saw_error_response = false;
    let mut credentials_were_rejected = false;
    for error_response in root
        .descendants()
        .filter(|node| node.is_element() && node.has_tag_name("error_response"))
    {
        saw_error_response = true;
        credentials_were_rejected |= error_response
            .text()
            .is_some_and(query_error_reports_invalid_credentials);
    }
    if !saw_error_response {
        return Ok(root);
    }
    if credentials_were_rejected {
        return Err(WireError::Configuration(
            "NMI Query API rejected the configured query credentials.".to_owned(),
        ));
    }
    Err(WireError::RequestRejected(
        "NMI Query API rejected the query request.".to_owned(),
    ))
}

fn query_error_reports_invalid_credentials(value: &str) -> bool {
    // The legacy Query API exposes only free-form <error_response> text, not
    // a stable machine-readable authentication code. This heuristic affects
    // operator routing only: every error_response remains a known
    // non-submission whether it maps to Configuration or RequestRejected.
    let value = value.trim().to_ascii_lowercase();
    value.contains("api key") || value.contains("security key") || value.contains("authentication")
}

struct ExactQueryResponse {
    outcome: PaymentOutcome,
    transaction_id: ResolvedScalar,
    order_id: ResolvedScalar,
}

fn direct_element_count_without_nested_matches<'document, 'input>(
    parent: roxmltree::Node<'document, 'input>,
    element_name: &'static str,
    response_kind: &'static str,
) -> Result<usize, WireError> {
    let direct_count = element_children_named(parent, element_name).count();
    let matching_descendants = parent
        .descendants()
        .filter(|node| node.is_element() && node.has_tag_name(element_name))
        .count();
    if matching_descendants != direct_count {
        return Err(WireError::MalformedResponse(format!(
            "NMI {response_kind} included a nested or wrapped {element_name} element."
        )));
    }
    Ok(direct_count)
}

pub(in crate::client) fn query_outcome_for_request_from_xml(
    text: &str,
    request: &TransactionQuery,
) -> Result<Option<PaymentOutcome>, WireError> {
    let Some(response) = exact_query_response_from_xml(text)? else {
        return Ok(None);
    };
    // The gateway response must echo every selector supplied by the caller.
    // A missing field cannot prove that query.php returned the requested
    // transaction rather than an unrelated record. Either selector can provide
    // that correlation independently; transaction-identity usability remains a
    // separate property of the returned payment evidence.
    bind_exact_query_identifier(
        trimmed_optional(&request.transaction_id),
        &response.transaction_id,
        "transaction",
    )?;
    bind_exact_query_identifier(
        trimmed_optional(&request.order_id),
        &response.order_id,
        "order",
    )?;
    Ok(Some(response.outcome))
}

fn bind_exact_query_identifier(
    expected: Option<&str>,
    response: &ResolvedScalar,
    identifier_name: &'static str,
) -> Result<(), WireError> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let problem = match response {
        ResolvedScalar::Missing => "did not include",
        ResolvedScalar::InvalidOrConflicting => "contained an invalid or conflicting",
        ResolvedScalar::OneConsistent(actual) if actual != expected => {
            "did not match the requested"
        }
        ResolvedScalar::OneConsistent(_) => return Ok(()),
    };
    Err(WireError::MalformedResponse(format!(
        "NMI exact query response {problem} {identifier_name} identifier."
    )))
}

fn exact_query_response_from_xml(text: &str) -> Result<Option<ExactQueryResponse>, WireError> {
    let document = parse_transaction_report_xml(text)?;
    let envelope = successful_query_response_root(&document, "exact query")?;
    let transaction_count = direct_element_count_without_nested_matches(
        envelope,
        "transaction",
        "exact query response",
    )?;
    if transaction_count > 1 {
        return Err(WireError::MalformedResponse(
            "NMI exact query response included an invalid transaction structure.".to_owned(),
        ));
    }
    let Some(transaction) = element_children_named(envelope, "transaction").next() else {
        return Ok(None);
    };
    let decision = PaymentDecisionFields::new(
        collect_xml_scalar(transaction, &["response"], false)
            .finish_decision(DecisionFieldKind::Response),
        collect_xml_scalar(transaction, &["response_code"], false)
            .finish_decision(DecisionFieldKind::ResponseCode),
        collect_xml_scalar(transaction, &["status"], false)
            .finish_decision(DecisionFieldKind::GatewayState),
        collect_xml_scalar(transaction, &["condition"], false)
            .finish_decision(DecisionFieldKind::GatewayState),
    )
    .resolve();
    let (response_text, _) =
        resolve_optional_scalar(collect_xml_scalar(transaction, &["response_text"], true).finish());
    let mut status = decision.status;
    let mut diagnostics = decision.diagnostics;
    let order_identifier =
        collect_xml_identifier(transaction, &["order_id"], IdentifierPresence::Optional);
    let transaction_identifier = collect_xml_identifier(
        transaction,
        &["transaction_id"],
        IdentifierPresence::Required,
    );
    let response_transaction_identifier = transaction_identifier.clone();
    if matches!(transaction_identifier, ResolvedScalar::Missing) {
        diagnostics.push(PaymentOutcomeDiagnostic::MissingTransactionIdentifier);
        if status == PaymentStatus::Approved {
            status = PaymentStatus::Unknown;
        }
    }
    let (transaction_id, customer_vault_id) = finalize_payment_identifiers(
        &mut status,
        transaction_identifier,
        collect_xml_identifier(
            transaction,
            &["customer_vault_id"],
            IdentifierPresence::Optional,
        ),
        &mut diagnostics,
    );
    let (payment_type, _) = resolve_optional_scalar(
        collect_xml_scalar(transaction, &["transaction_type"], true).finish(),
    );
    let (card_brand, _) = resolve_optional_scalar(
        collect_xml_scalar(transaction, &["cc_type"], true)
            .finish_normalized(str::to_ascii_lowercase),
    );
    let (card_number, _) =
        resolve_optional_scalar(collect_xml_scalar(transaction, &["cc_number"], true).finish());
    let (expiry, _) =
        resolve_optional_scalar(collect_xml_scalar(transaction, &["cc_exp"], true).finish());
    let (card_exp_month, card_exp_year) = parse_expiry(expiry.as_deref());
    Ok(Some(ExactQueryResponse {
        outcome: PaymentOutcome {
            status,
            transaction_id,
            customer_vault_id,
            response: sensitive_gateway_field(decision.response),
            response_code: sensitive_gateway_field(decision.response_code),
            response_text: sensitive_gateway_field(response_text),
            condition: sensitive_gateway_field(decision.condition),
            descriptor: PaymentDescriptor {
                payment_type: sensitive_gateway_field(payment_type),
                card_brand: sensitive_gateway_field(card_brand),
                card_last4: card_number.and_then(last4).map(SensitiveText::new),
                card_exp_month,
                card_exp_year,
            },
            diagnostics,
        }
        .normalize_diagnostics(),
        transaction_id: response_transaction_identifier,
        order_id: order_identifier,
    }))
}

pub(in crate::client) fn query_account_mode_from_xml(text: &str) -> Result<AccountMode, WireError> {
    let document = parse_transaction_report_xml(text)?;
    let envelope = successful_query_response_root(&document, "account mode response")?;
    let mut modes = Vec::new();
    let account_details: Vec<_> = element_children_named(envelope, "account_details").collect();
    let status_nodes = element_children_named(envelope, "test_mode_enabled")
        .chain(element_children_named(envelope, "test_mode_status"))
        .chain(
            account_details
                .iter()
                .flat_map(|details| element_children_named(*details, "test_mode_enabled")),
        )
        .chain(
            account_details
                .iter()
                .flat_map(|details| element_children_named(*details, "test_mode_status")),
        );
    for node in status_nodes {
        let value = strict_xml_element_scalar(node).ok_or_else(|| {
            WireError::MalformedResponse(
                "NMI account mode response included an invalid test mode value.".to_owned(),
            )
        })?;
        let parsed = if node.has_tag_name("test_mode_enabled") {
            match value.as_str() {
                "true" | "1" => Some(AccountMode::Test),
                "false" | "0" => Some(AccountMode::Live),
                _ => None,
            }
        } else if node.has_tag_name("test_mode_status") {
            match value.as_str() {
                "active" | "enabled" | "test" => Some(AccountMode::Test),
                "inactive" | "disabled" | "live" => Some(AccountMode::Live),
                _ => None,
            }
        } else {
            continue;
        };
        let Some(mode) = parsed else {
            return Err(WireError::MalformedResponse(
                "NMI account mode response included an invalid test mode value.".to_owned(),
            ));
        };
        modes.push(mode);
    }

    let Some(mode) = modes.first().copied() else {
        return Err(WireError::MalformedResponse(
            "NMI account mode response did not include test mode status.".to_owned(),
        ));
    };
    if modes.iter().any(|candidate| *candidate != mode) {
        return Err(WireError::MalformedResponse(
            "NMI account mode response included conflicting test mode status.".to_owned(),
        ));
    }
    Ok(mode)
}

pub(in crate::client) fn query_transaction_reports_from_xml(
    text: &str,
) -> Result<Vec<TransactionReport>, WireError> {
    if text.len() > MAX_NMI_REPORT_RESPONSE_BYTES {
        return Err(WireError::MalformedResponse(format!(
            "NMI transaction report response exceeded {MAX_NMI_REPORT_RESPONSE_BYTES} bytes."
        )));
    }
    let document = parse_transaction_report_xml(text)?;
    let envelope = successful_query_response_root(&document, "transaction report response")?;
    let transaction_count = direct_element_count_without_nested_matches(
        envelope,
        "transaction",
        "transaction report response",
    )?;
    if transaction_count > MAX_NMI_TRANSACTION_REPORTS {
        return Err(WireError::MalformedResponse(format!(
            "NMI transaction report response exceeded {MAX_NMI_TRANSACTION_REPORTS} transactions."
        )));
    }
    let mut reports = Vec::with_capacity(transaction_count);
    let mut remaining_action_capacity = MAX_NMI_REPORT_ACTIONS;
    for transaction in element_children_named(envelope, "transaction") {
        let mut malformed_structure = false;
        let transaction_id = report_field(
            collect_xml_scalar(transaction, &["transaction_id"], false).finish(),
            &mut malformed_structure,
        );
        let order_id = report_field(
            collect_xml_scalar(transaction, &["order_id"], false).finish(),
            &mut malformed_structure,
        );
        let condition = report_field(
            collect_xml_scalar(transaction, &["condition"], false)
                .finish_normalized(normalize_gateway_state),
            &mut malformed_structure,
        );
        let reserved_action_count = if malformed_structure {
            0
        } else {
            match direct_element_count_without_nested_matches(
                transaction,
                "action",
                "transaction report response",
            ) {
                Ok(action_count) if action_count <= remaining_action_capacity => {
                    remaining_action_capacity -= action_count;
                    action_count
                }
                Ok(_) | Err(_) => {
                    malformed_structure = true;
                    0
                }
            }
        };
        let mut actions = Vec::with_capacity(reserved_action_count);
        if !malformed_structure {
            for action in element_children_named(transaction, "action") {
                let action_type = report_field(
                    collect_xml_scalar(action, &["action_type"], false)
                        .finish_normalized(normalize_gateway_state),
                    &mut malformed_structure,
                );
                let date = report_field(
                    collect_xml_scalar(action, &["date"], false).finish(),
                    &mut malformed_structure,
                );
                let amount = report_field(
                    collect_xml_scalar(action, &["amount"], false)
                        .finish_normalized(normalize_report_amount),
                    &mut malformed_structure,
                );
                let success = report_field(
                    collect_xml_scalar(action, &["success"], false)
                        .finish_normalized(normalize_report_success),
                    &mut malformed_structure,
                );
                if malformed_structure {
                    break;
                }
                let (response_code, _) = resolve_optional_scalar(
                    collect_xml_scalar(action, &["response_code"], true).finish(),
                );
                let (response_text, _) = resolve_optional_scalar(
                    collect_xml_scalar(action, &["response_text"], true).finish(),
                );
                actions.push(TransactionAction {
                    action_type: action_type.map(SensitiveText::new),
                    date: date.map(SensitiveText::new),
                    amount: amount.map(SensitiveText::new),
                    success: success.map(SensitiveText::new),
                    response_code: response_code.map(SensitiveText::new),
                    response_text: response_text.map(SensitiveText::new),
                });
            }
        }
        let diagnostics = if malformed_structure {
            // Keep only independently resolved identifiers for quarantine
            // correlation. No partial condition or action evidence may escape
            // a malformed transaction as lifecycle authority. Release any
            // reserved vector allocation and make its capacity available to
            // later independent transactions in this page.
            if reserved_action_count > 0 {
                remaining_action_capacity += reserved_action_count;
            }
            actions = Vec::new();
            vec![TransactionReportDiagnostic::MalformedStructure]
        } else {
            Vec::new()
        };
        reports.push(TransactionReport {
            transaction_id: transaction_id.map(SensitiveText::new),
            order_id: order_id.map(SensitiveText::new),
            condition: (!malformed_structure)
                .then_some(condition)
                .flatten()
                .map(SensitiveText::new),
            actions,
            diagnostics,
        });
    }
    Ok(reports)
}

fn collect_xml_scalar(
    node: roxmltree::Node<'_, '_>,
    aliases: &[&'static str],
    bounded: bool,
) -> ScalarOccurrenceCollector {
    let mut collector = ScalarOccurrenceCollector::default();
    for child in node.children().filter(|child| child.is_element()) {
        if !aliases.iter().any(|alias| child.has_tag_name(*alias)) {
            continue;
        }
        let occurrence = xml_scalar_occurrence(child);
        if bounded {
            collector.record_bounded(occurrence);
        } else {
            collector.record(occurrence);
        }
    }
    collector
}

fn collect_xml_identifier(
    parent: roxmltree::Node<'_, '_>,
    aliases: &[&'static str],
    presence: IdentifierPresence,
) -> ResolvedScalar {
    let mut collector = ScalarOccurrenceCollector::default();
    for child in parent.children().filter(|child| child.is_element()) {
        if aliases.iter().any(|alias| child.has_tag_name(*alias)) {
            collector.record_identifier(xml_scalar_occurrence(child), presence);
        }
    }
    collector.finish()
}

fn xml_scalar_occurrence(node: roxmltree::Node<'_, '_>) -> ScalarOccurrence<'static> {
    if node.children().any(|child| child.is_element()) {
        return ScalarOccurrence::InvalidShape;
    }
    let mut text = String::new();
    for text_node in node.children().filter(|child| child.is_text()) {
        if let Some(value) = text_node.text() {
            text.push_str(value);
        }
    }
    ScalarOccurrence::Scalar(Cow::Owned(text))
}

fn strict_xml_element_scalar(node: roxmltree::Node<'_, '_>) -> Option<String> {
    let mut collector = ScalarOccurrenceCollector::default();
    collector.record(xml_scalar_occurrence(node));
    match collector.finish() {
        ResolvedScalar::OneConsistent(value) => Some(value),
        ResolvedScalar::Missing | ResolvedScalar::InvalidOrConflicting => None,
    }
}

fn report_field(value: ResolvedScalar, malformed_structure: &mut bool) -> Option<String> {
    match value {
        ResolvedScalar::Missing => None,
        ResolvedScalar::OneConsistent(value) => Some(value),
        ResolvedScalar::InvalidOrConflicting => {
            *malformed_structure = true;
            None
        }
    }
}

fn parse_transaction_report_xml(text: &str) -> Result<roxmltree::Document<'_>, WireError> {
    roxmltree::Document::parse(text).map_err(|_| malformed_xml(text))
}

fn malformed_xml_block(tag: &str) -> WireError {
    WireError::MalformedResponse(format!(
        "NMI transaction report response included an unclosed <{tag}> block."
    ))
}

fn malformed_xml(text: &str) -> WireError {
    if text.contains("<action") && !text.contains("</action>") {
        return malformed_xml_block("action");
    }
    if text.contains("<transaction") && !text.contains("</transaction>") {
        return malformed_xml_block("transaction");
    }
    WireError::MalformedResponse("NMI transaction report response XML was malformed.".to_owned())
}

fn element_children_named<'a, 'input>(
    node: roxmltree::Node<'a, 'input>,
    tag: &'static str,
) -> impl Iterator<Item = roxmltree::Node<'a, 'input>> {
    node.children()
        .filter(move |child| child.is_element() && child.has_tag_name(tag))
}
