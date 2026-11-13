use std::fmt;

use crate::{SensitiveText, TransactionAction};

/// Both original identifiers are required; receipt recovery never searches a
/// merchant-wide report or the customer's current payment method.
#[derive(Clone)]
pub struct TransactionReceiptQuery {
    pub transaction_id: String,
    pub order_id: String,
}

impl fmt::Debug for TransactionReceiptQuery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TransactionReceiptQuery([redacted])")
    }
}

/// Receipt evidence after binding both original selectors to one transaction.
/// Errors and missing evidence are never promoted to an attributed conflict.
#[derive(Debug)]
#[non_exhaustive]
pub enum TransactionReceiptLookup {
    Found(TransactionReceipt),
    NotFound,
    /// Identity-bound facts for comparison only, never receipt completion.
    /// Missing currency or malformed fields prevent a complete observation.
    /// Each retained field is individually valid and consistent; any malformed
    /// occurrence suppresses that field, while independent fields survive.
    Incomplete {
        currency: Option<SensitiveText>,
        card_last4: Option<SensitiveText>,
        actions: Vec<TransactionAction>,
    },
    /// At least two individually valid values for the same field disagree
    /// after normalization. Contains field kinds only, never provider values.
    /// Other fields may be missing or malformed; no partial receipt is returned.
    BoundConflict {
        fields: Vec<ReceiptConflictField>,
    },
}

/// A contradictory field within the selected transaction or one of its actions.
/// Different actions are history, not contradictory occurrences of one field.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ReceiptConflictField {
    Currency,
    CardLast4,
    ActionType,
    ActionDate,
    ActionAmount,
    ActionSuccess,
    ActionResponseCode,
}

/// Historical receipt evidence, independent of the current financial status.
/// The caller must select and validate the original successful payment action
/// and compare its amount and currency to the recorded charge. Action types
/// are not restricted to a known list; ignore the ones you do not recognize.
#[derive(Debug)]
pub struct TransactionReceipt {
    pub(crate) parts: TransactionReceiptParts,
}

impl TransactionReceipt {
    pub fn into_parts(self) -> TransactionReceiptParts {
        self.parts
    }
}

#[derive(Debug)]
#[non_exhaustive]
pub struct TransactionReceiptParts {
    pub currency: SensitiveText,
    /// Only four visible trailing digits of a valid masked card, or an exact
    /// four-digit value. A missing field remains absent; malformed data fails
    /// the lookup rather than inventing digits from a card prefix.
    pub card_last4: Option<SensitiveText>,
    pub actions: Vec<TransactionAction>,
}
