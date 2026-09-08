use super::{
    PaymentDescriptor, PaymentOutcome, PaymentOutcomeDiagnostic, PaymentStatus, SensitiveText,
};

/// Enriched exact-query card display, separate from durable payment outcomes.
#[derive(Debug)]
pub struct PaymentMethodMetadata {
    parts: PaymentMethodMetadataParts,
}

impl PaymentMethodMetadata {
    pub(crate) fn from_query_outcome(outcome: PaymentOutcome) -> Self {
        let parts = outcome.into_parts();
        Self {
            parts: PaymentMethodMetadataParts {
                status: parts.status,
                transaction_id: parts.transaction_id,
                customer_vault_id: parts.customer_vault_id,
                descriptor: parts.descriptor,
                diagnostics: parts.diagnostics,
            },
        }
    }

    /// Transfers the display observation to a provider adapter.
    pub fn into_parts(self) -> PaymentMethodMetadataParts {
        self.parts
    }
}

/// Correlation and display fields from a metadata query, without payment response
/// evidence. Status and diagnostics are rejection signals, not payment authority.
#[derive(Debug)]
#[non_exhaustive]
pub struct PaymentMethodMetadataParts {
    pub status: PaymentStatus,
    pub transaction_id: Option<SensitiveText>,
    pub customer_vault_id: Option<SensitiveText>,
    pub descriptor: PaymentDescriptor,
    pub diagnostics: Vec<PaymentOutcomeDiagnostic>,
}
