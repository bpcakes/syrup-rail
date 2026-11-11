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

/// Historical receipt evidence, independent of the current financial status.
/// The caller must select and validate the original successful payment action
/// and compare its amount and currency to the recorded charge.
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
