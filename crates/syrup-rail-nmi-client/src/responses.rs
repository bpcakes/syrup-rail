use std::fmt;

use zeroize::Zeroizing;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccountMode {
    Live,
    Test,
}

/// Provider-owned text whose ordinary formatting never reveals its value.
#[derive(Default, Eq, PartialEq)]
pub struct SensitiveText(Zeroizing<String>);

impl SensitiveText {
    pub fn new(value: impl Into<String>) -> Self {
        Self(Zeroizing::new(value.into()))
    }

    /// Explicitly exposes the provider value to a caller-owned policy boundary.
    pub fn expose(&self) -> &str {
        self.0.as_str()
    }

    /// Explicitly transfers the provider value out of its zeroizing owner.
    ///
    /// The caller becomes responsible for the returned buffer's lifetime and
    /// disclosure behavior.
    pub fn into_inner(mut self) -> String {
        std::mem::take(&mut *self.0)
    }
}

impl fmt::Debug for SensitiveText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SensitiveText([redacted])")
    }
}

impl fmt::Display for SensitiveText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[redacted]")
    }
}

impl From<String> for SensitiveText {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for SensitiveText {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use = "a payment status is an authoritative provider decision and must be handled"]
pub enum PaymentStatus {
    Approved,
    Declined,
    Unknown,
    Failed,
}

macro_rules! define_payment_outcome_diagnostics {
    (
        $(#[$enum_meta:meta])*
        pub enum $name:ident {
            $(
                $(#[$variant_meta:meta])*
                $variant:ident
            ),+ $(,)?
        }
    ) => {
        $(#[$enum_meta])*
        pub enum $name {
            $(
                $(#[$variant_meta])*
                $variant,
            )+
        }

        impl $name {
            /// Every diagnostic variant supported by this client version.
            ///
            /// Provider adapters can use this list to prove that their typed
            /// mapping covers the complete current diagnostic vocabulary while
            /// retaining a fallback for future non-exhaustive variants. This is
            /// not a closed set: compatible releases may append new variants.
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];
        }
    };
}

define_payment_outcome_diagnostics! {
    /// A payload-free reason that provider evidence could not be trusted normally.
    ///
    /// Callers may safely use these values in logs and metrics. Raw provider values
    /// remain available only through the redacted [`SensitiveText`] boundary.
    /// Variant declaration order defines the canonical order returned by
    /// [`PaymentOutcome::diagnostics`]; append new variants.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    #[non_exhaustive]
    pub enum PaymentOutcomeDiagnostic {
        MissingTransactionIdentifier,
        MissingCustomerVaultIdentifier,
        InvalidOrConflictingTransactionIdentifier,
        InvalidOrConflictingCustomerVaultIdentifier,
        InvalidOrConflictingDecisionField,
        /// NMI reported an error without guaranteeing that the attempted
        /// payment had no financial effect.
        IndeterminatePaymentOutcome,
        /// NMI reported response code `430`, "Duplicate transaction at processor".
        ///
        /// This remains an unknown payment outcome that requires reconciliation;
        /// the diagnostic only makes the provider's duplicate decision observable.
        DuplicateTransactionAtProcessor,
        ConflictingDecisionEvidence,
        UnrecognizedDecisionEvidence,
        MissingDecisionEvidence,
    }
}

#[derive(Default)]
pub struct PaymentDescriptor {
    pub(crate) payment_type: Option<SensitiveText>,
    pub(crate) card_brand: Option<SensitiveText>,
    pub(crate) card_last4: Option<SensitiveText>,
    pub(crate) card_exp_month: Option<i16>,
    pub(crate) card_exp_year: Option<i16>,
}

impl PaymentDescriptor {
    pub fn into_parts(self) -> PaymentDescriptorParts {
        PaymentDescriptorParts {
            payment_type: self.payment_type,
            card_brand: self.card_brand,
            card_last4: self.card_last4,
            card_exp_month: self.card_exp_month,
            card_exp_year: self.card_exp_year,
        }
    }
}

impl fmt::Debug for PaymentDescriptor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PaymentDescriptor")
            .field("has_payment_type", &self.payment_type.is_some())
            .field("has_card_brand", &self.card_brand.is_some())
            .field("has_card_last4", &self.card_last4.is_some())
            .field("has_card_exp_month", &self.card_exp_month.is_some())
            .field("has_card_exp_year", &self.card_exp_year.is_some())
            .finish()
    }
}

pub struct PaymentDescriptorParts {
    pub payment_type: Option<SensitiveText>,
    pub card_brand: Option<SensitiveText>,
    pub card_last4: Option<SensitiveText>,
    pub card_exp_month: Option<i16>,
    pub card_exp_year: Option<i16>,
}

/// The provider decision cannot be silently discarded after unwrapping a
/// successful operation.
///
/// ```compile_fail
/// #![deny(unused_must_use)]
/// fn discard(outcome: syrup_rail_nmi_client::PaymentOutcome) {
///     outcome;
/// }
/// ```
#[must_use = "payment outcomes must be inspected before the operation is considered handled"]
pub struct PaymentOutcome {
    pub(crate) status: PaymentStatus,
    pub(crate) transaction_id: Option<SensitiveText>,
    pub(crate) customer_vault_id: Option<SensitiveText>,
    pub(crate) response: Option<SensitiveText>,
    pub(crate) response_code: Option<SensitiveText>,
    pub(crate) response_text: Option<SensitiveText>,
    pub(crate) condition: Option<SensitiveText>,
    pub(crate) descriptor: PaymentDescriptor,
    pub(crate) diagnostics: Vec<PaymentOutcomeDiagnostic>,
}

impl PaymentOutcome {
    pub fn status(&self) -> PaymentStatus {
        self.status
    }

    /// Returns payload-free diagnostics as a canonically ordered set.
    ///
    /// Order carries no provider chronology or policy precedence. Route by
    /// membership rather than sequence.
    pub fn diagnostics(&self) -> &[PaymentOutcomeDiagnostic] {
        &self.diagnostics
    }

    pub(crate) fn normalize_diagnostics(mut self) -> Self {
        self.diagnostics
            .sort_unstable_by_key(|diagnostic| *diagnostic as usize);
        self.diagnostics.dedup();
        self
    }

    pub fn into_parts(self) -> PaymentOutcomeParts {
        PaymentOutcomeParts {
            status: self.status,
            transaction_id: self.transaction_id,
            customer_vault_id: self.customer_vault_id,
            response: self.response,
            response_code: self.response_code,
            response_text: self.response_text,
            condition: self.condition,
            descriptor: self.descriptor,
            diagnostics: self.diagnostics,
        }
    }
}

impl fmt::Debug for PaymentOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PaymentOutcome")
            .field("status", &self.status)
            .field("has_transaction_id", &self.transaction_id.is_some())
            .field("has_customer_vault_id", &self.customer_vault_id.is_some())
            .field("has_response", &self.response.is_some())
            .field("has_response_code", &self.response_code.is_some())
            .field("has_response_text", &self.response_text.is_some())
            .field("has_condition", &self.condition.is_some())
            .field("descriptor", &self.descriptor)
            .field("diagnostics", &self.diagnostics)
            .finish()
    }
}

#[must_use = "payment outcome parts retain the provider decision and must be inspected"]
pub struct PaymentOutcomeParts {
    pub status: PaymentStatus,
    pub transaction_id: Option<SensitiveText>,
    pub customer_vault_id: Option<SensitiveText>,
    pub response: Option<SensitiveText>,
    pub response_code: Option<SensitiveText>,
    pub response_text: Option<SensitiveText>,
    pub condition: Option<SensitiveText>,
    pub descriptor: PaymentDescriptor,
    pub diagnostics: Vec<PaymentOutcomeDiagnostic>,
}

pub struct TransactionAction {
    pub(crate) action_type: Option<SensitiveText>,
    pub(crate) date: Option<SensitiveText>,
    pub(crate) amount: Option<SensitiveText>,
    pub(crate) success: Option<SensitiveText>,
    pub(crate) response_code: Option<SensitiveText>,
    pub(crate) response_text: Option<SensitiveText>,
}

impl TransactionAction {
    pub fn into_parts(self) -> TransactionActionParts {
        TransactionActionParts {
            action_type: self.action_type,
            date: self.date,
            amount: self.amount,
            success: self.success,
            response_code: self.response_code,
            response_text: self.response_text,
        }
    }
}

impl fmt::Debug for TransactionAction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TransactionAction")
            .field("has_action_type", &self.action_type.is_some())
            .field("has_date", &self.date.is_some())
            .field("has_amount", &self.amount.is_some())
            .field("has_success", &self.success.is_some())
            .field("has_response_code", &self.response_code.is_some())
            .field("has_response_text", &self.response_text.is_some())
            .finish()
    }
}

pub struct TransactionActionParts {
    pub action_type: Option<SensitiveText>,
    pub date: Option<SensitiveText>,
    pub amount: Option<SensitiveText>,
    pub success: Option<SensitiveText>,
    pub response_code: Option<SensitiveText>,
    pub response_text: Option<SensitiveText>,
}

/// Payload-free evidence that one well-formed XML transaction could not be
/// interpreted without fabricating authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransactionReportDiagnostic {
    MalformedStructure,
}

pub struct TransactionReport {
    pub(crate) transaction_id: Option<SensitiveText>,
    pub(crate) order_id: Option<SensitiveText>,
    pub(crate) condition: Option<SensitiveText>,
    pub(crate) actions: Vec<TransactionAction>,
    pub(crate) diagnostics: Vec<TransactionReportDiagnostic>,
}

impl TransactionReport {
    pub fn into_parts(self) -> TransactionReportParts {
        TransactionReportParts {
            transaction_id: self.transaction_id,
            order_id: self.order_id,
            condition: self.condition,
            actions: self.actions,
            diagnostics: self.diagnostics,
        }
    }
}

impl fmt::Debug for TransactionReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TransactionReport")
            .field("has_transaction_id", &self.transaction_id.is_some())
            .field("has_order_id", &self.order_id.is_some())
            .field("has_condition", &self.condition.is_some())
            .field("action_count", &self.actions.len())
            .field("diagnostics", &self.diagnostics)
            .finish()
    }
}

pub struct TransactionReportParts {
    pub transaction_id: Option<SensitiveText>,
    pub order_id: Option<SensitiveText>,
    pub condition: Option<SensitiveText>,
    pub actions: Vec<TransactionAction>,
    pub diagnostics: Vec<TransactionReportDiagnostic>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payment_outcome_diagnostic_vocabulary_is_append_only() {
        assert_eq!(
            PaymentOutcomeDiagnostic::ALL,
            &[
                PaymentOutcomeDiagnostic::MissingTransactionIdentifier,
                PaymentOutcomeDiagnostic::MissingCustomerVaultIdentifier,
                PaymentOutcomeDiagnostic::InvalidOrConflictingTransactionIdentifier,
                PaymentOutcomeDiagnostic::InvalidOrConflictingCustomerVaultIdentifier,
                PaymentOutcomeDiagnostic::InvalidOrConflictingDecisionField,
                PaymentOutcomeDiagnostic::IndeterminatePaymentOutcome,
                PaymentOutcomeDiagnostic::DuplicateTransactionAtProcessor,
                PaymentOutcomeDiagnostic::ConflictingDecisionEvidence,
                PaymentOutcomeDiagnostic::UnrecognizedDecisionEvidence,
                PaymentOutcomeDiagnostic::MissingDecisionEvidence,
            ]
        );
    }

    #[test]
    fn payment_outcome_diagnostics_have_set_semantics() {
        let outcome = PaymentOutcome {
            status: PaymentStatus::Unknown,
            transaction_id: None,
            customer_vault_id: None,
            response: None,
            response_code: None,
            response_text: None,
            condition: None,
            descriptor: PaymentDescriptor::default(),
            diagnostics: vec![
                PaymentOutcomeDiagnostic::DuplicateTransactionAtProcessor,
                PaymentOutcomeDiagnostic::InvalidOrConflictingDecisionField,
                PaymentOutcomeDiagnostic::DuplicateTransactionAtProcessor,
            ],
        }
        .normalize_diagnostics();

        assert_eq!(
            outcome.diagnostics(),
            &[
                PaymentOutcomeDiagnostic::InvalidOrConflictingDecisionField,
                PaymentOutcomeDiagnostic::DuplicateTransactionAtProcessor,
            ]
        );
    }
}
