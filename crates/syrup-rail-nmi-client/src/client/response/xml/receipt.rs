use super::*;
use crate::{TransactionReceipt, TransactionReceiptParts};

mod evidence;
pub(in crate::client) use evidence::parse as parse_evidence;

const MAX_RECEIPT_ACTIONS: usize = 100;

pub(in crate::client) fn parse(
    text: &str,
    query: &TransactionQuery,
) -> Result<Option<TransactionReceipt>, WireError> {
    if text.len() > crate::client::MAX_NMI_STANDARD_RESPONSE_BYTES {
        return Err(invalid());
    }
    let document = parse_transaction_report_xml(text)?;
    let Some(transaction) = bound_transaction(&document, query)? else {
        return Ok(None);
    };
    // Validate scalar conflicts even though current status is not payment-date
    // evidence. Do not map refunded/canceled into a failed original payment.
    field(transaction, "condition")?;
    let currency = field(transaction, "currency")?.ok_or_else(invalid)?;
    if currency.len() != 3 || !currency.bytes().all(|byte| byte.is_ascii_alphabetic()) {
        return Err(invalid());
    }
    let card_last4 = field(transaction, "cc_number")?
        .map(|card| masked_last_four(&card).ok_or_else(invalid))
        .transpose()?;
    let count = direct_element_count_without_nested_matches(transaction, "action", "receipt")?;
    if count > MAX_RECEIPT_ACTIONS {
        return Err(invalid());
    }
    let mut actions = Vec::with_capacity(count);
    for action in element_children_named(transaction, "action") {
        actions.push(TransactionAction {
            action_type: field(action, "action_type")?.map(SensitiveText::new),
            date: field(action, "date")?.map(SensitiveText::new),
            amount: field(action, "amount")?.map(SensitiveText::new),
            success: field(action, "success")?.map(SensitiveText::new),
            // Conflicting/invalid codes must not become a merely missing code.
            response_code: field(action, "response_code")?.map(SensitiveText::new),
            response_text: None,
        });
    }
    Ok(Some(TransactionReceipt {
        parts: TransactionReceiptParts {
            currency: SensitiveText::new(currency.to_ascii_uppercase()),
            card_last4: card_last4.map(SensitiveText::new),
            actions,
        },
    }))
}

fn bound_transaction<'document, 'input>(
    document: &'document roxmltree::Document<'input>,
    query: &TransactionQuery,
) -> Result<Option<roxmltree::Node<'document, 'input>>, WireError> {
    let envelope = successful_query_response_root(document, "receipt query")?;
    let count = direct_element_count_without_nested_matches(envelope, "transaction", "receipt")?;
    if count > 1 {
        return Err(invalid());
    }
    let Some(transaction) = element_children_named(envelope, "transaction").next() else {
        return Ok(None);
    };
    for (name, expected) in [
        ("transaction_id", &query.transaction_id),
        ("order_id", &query.order_id),
    ] {
        let expected = trimmed_optional(expected).ok_or_else(invalid)?;
        bind_exact_query_identifier(
            Some(expected),
            &collect_xml_identifier(transaction, &[name], IdentifierPresence::Required),
            name,
        )?;
    }
    Ok(Some(transaction))
}

fn field(node: roxmltree::Node<'_, '_>, name: &'static str) -> Result<Option<String>, WireError> {
    // The general display-field collector truncates values. Receipt evidence
    // must remain exact: reject excess work before collecting, never truncate.
    let mut collector = ScalarOccurrenceCollector::default();
    let mut empty = false;
    let mut populated = false;
    for child in element_children_named(node, name) {
        let occurrence = xml_scalar_occurrence(child);
        if let ScalarOccurrence::Scalar(value) = &occurrence {
            if value.chars().count() > crate::client::MAX_NMI_FIELD_CHARS {
                return Err(invalid());
            }
            if value.trim().is_empty() {
                empty = true;
                continue;
            }
        }
        populated = true;
        collector.record(occurrence);
    }
    if empty && populated {
        return Err(invalid());
    }
    match collector.finish() {
        ResolvedScalar::OneConsistent(value) => Ok(Some(value)),
        ResolvedScalar::Missing => Ok(None),
        ResolvedScalar::InvalidOrConflicting => Err(invalid()),
    }
}

fn masked_last_four(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    if bytes.len() < 4 || !bytes[bytes.len() - 4..].iter().all(u8::is_ascii_digit) {
        return None;
    }
    let prefix = &bytes[..bytes.len() - 4];
    let mask = |byte: &u8| matches!(byte, b'*' | b'x' | b'X' | b'#');
    if !prefix.is_empty()
        && (!prefix.iter().any(mask)
            || !prefix
                .iter()
                .all(|byte| byte.is_ascii_digit() || mask(byte)))
    {
        return None;
    }
    Some(value[value.len() - 4..].to_owned())
}

fn invalid() -> WireError {
    WireError::MalformedResponse("NMI receipt evidence is malformed or conflicting.".to_owned())
}
