use super::SensitiveText;

/// Result of one exact diagnostic lookup by transaction ID.
#[derive(Debug)]
#[non_exhaustive]
pub enum TransactionDiagnosticsLookup {
    /// The Query API returned no transaction.
    NotFound,
    /// The Query API returned more than one transaction for an exact lookup.
    MultipleTransactions,
    /// The Query API returned exactly one transaction.
    Found(TransactionDiagnostics),
}

/// Read-only processor and verification fields for one queried transaction.
///
/// Every value is untrusted provider text with value-free formatting. Only
/// correlation, currency, verification, and per-action response fields are
/// parsed; names, addresses, email, card, and signature fields are never
/// extracted. This is not payment evidence and carries no decision.
#[derive(Debug)]
pub struct TransactionDiagnostics {
    parts: TransactionDiagnosticsParts,
}

impl TransactionDiagnostics {
    pub(crate) const fn new(parts: TransactionDiagnosticsParts) -> Self {
        Self { parts }
    }

    /// Transfers the observation to a provider adapter.
    pub fn into_parts(self) -> TransactionDiagnosticsParts {
        self.parts
    }
}

/// Transaction-level diagnostic fields. Codes (AVS, CSC, and the actions'
/// gateway and processor response codes) are at most 64 bytes; a longer code
/// is omitted and flagged incomplete rather than truncated.
#[derive(Debug)]
#[non_exhaustive]
pub struct TransactionDiagnosticsParts {
    pub transaction_id: Option<SensitiveText>,
    pub order_id: Option<SensitiveText>,
    pub currency: Option<SensitiveText>,
    pub avs_response: Option<SensitiveText>,
    pub csc_response: Option<SensitiveText>,
    /// Actions in response order. Empty when `malformed` is set.
    pub actions: Vec<TransactionDiagnosticsActionParts>,
    /// An identifier, the currency, or an action's type or amount was
    /// malformed or conflicting, or the action structure was invalid. No
    /// action can then be selected safely.
    pub malformed: bool,
    /// A transaction-level verification field was present but unusable and
    /// was omitted.
    pub incomplete: bool,
}

/// One transaction action's diagnostic fields.
#[derive(Debug)]
#[non_exhaustive]
pub struct TransactionDiagnosticsActionParts {
    /// Lowercase action type without separators, such as `sale` or `validate`.
    pub action_type: Option<SensitiveText>,
    /// Decimal amount normalized to two fractional digits, such as `12.34`.
    pub amount: Option<SensitiveText>,
    pub response_code: Option<SensitiveText>,
    pub response_text: Option<SensitiveText>,
    pub processor_response_code: Option<SensitiveText>,
    pub processor_response_text: Option<SensitiveText>,
    /// One of this action's response fields was present but unusable and was
    /// omitted.
    pub incomplete: bool,
}
