use super::*;
use crate::{ReceiptConflictField, TransactionReceiptLookup};

mod fields;
use fields::{Evidence, action_type, amount, currency, date, response_code, success};

pub(in crate::client) fn parse(
    text: &str,
    query: &TransactionQuery,
) -> Result<TransactionReceiptLookup, WireError> {
    if text.len() > crate::client::MAX_NMI_STANDARD_RESPONSE_BYTES {
        return Err(invalid());
    }
    let document = parse_transaction_report_xml(text)?;
    let Some(transaction) = bound_transaction(&document, query)? else {
        return Ok(TransactionReceiptLookup::NotFound);
    };
    let count = direct_element_count_without_nested_matches(transaction, "action", "receipt")?;
    if count > MAX_RECEIPT_ACTIONS {
        return Err(invalid());
    }

    // Identity, cardinality and work bounds precede attribution. Within that
    // boundary, inspect every field before considering ordinary malformation:
    // a missing currency or bad mask cannot erase a proven action conflict.
    // Current condition is lifecycle metadata, not original-payment evidence.
    let mut evidence = Evidence {
        malformed: field(transaction, "condition").is_err(),
        ..Evidence::default()
    };
    let currency = evidence.field(
        transaction,
        "currency",
        ReceiptConflictField::Currency,
        currency,
    );
    let card_last4 = evidence.field(
        transaction,
        "cc_number",
        ReceiptConflictField::CardLast4,
        masked_last_four,
    );
    let mut actions = Vec::with_capacity(count);
    for action in element_children_named(transaction, "action") {
        actions.push(TransactionAction {
            action_type: evidence.field(
                action,
                "action_type",
                ReceiptConflictField::ActionType,
                action_type,
            ),
            date: evidence.field(action, "date", ReceiptConflictField::ActionDate, date),
            amount: evidence.field(action, "amount", ReceiptConflictField::ActionAmount, amount),
            success: evidence.field(
                action,
                "success",
                ReceiptConflictField::ActionSuccess,
                success,
            ),
            response_code: evidence.field(
                action,
                "response_code",
                ReceiptConflictField::ActionResponseCode,
                response_code,
            ),
            response_text: None,
        });
    }
    if !evidence.conflicts.is_empty() {
        return Ok(TransactionReceiptLookup::BoundConflict {
            fields: evidence.conflicts,
        });
    }
    if evidence.malformed || currency.is_none() {
        return Ok(TransactionReceiptLookup::Incomplete {
            currency,
            card_last4,
            actions,
        });
    }
    Ok(TransactionReceiptLookup::Found(TransactionReceipt {
        parts: TransactionReceiptParts {
            currency: currency.ok_or_else(invalid)?,
            card_last4,
            actions,
        },
    }))
}
