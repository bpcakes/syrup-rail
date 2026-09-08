use super::{
    GatewayPaymentDescriptor, GatewayPaymentDiagnostic, GatewayPaymentMethodReference,
    GatewayPaymentOutcome, GatewayPaymentStatus, GatewayTransactionId,
};

/// An exact-query observation for saved-method display, without payment authority.
///
/// Identity, status and diagnostics allow the caller to reject unsuitable display
/// observations. Only an already approved durable attempt can authorize a refresh.
/// There is no conversion from this type to [`super::ProcessorEvidence`]; the
/// original financial response fields are discarded by the projection.
///
/// ```
/// use syrup_rail::{
///     GatewayPaymentMethodMetadata, GatewayPaymentOutcome, GatewayPaymentStatus,
///     ProcessorEvidence,
/// };
/// let outcome = GatewayPaymentOutcome::new(
///     GatewayPaymentStatus::Unknown, ProcessorEvidence::default(),
/// );
/// let metadata = GatewayPaymentMethodMetadata::from_query_outcome(outcome);
/// assert_eq!(metadata.status(), GatewayPaymentStatus::Unknown);
/// ```
///
/// ```compile_fail
/// use syrup_rail::{GatewayPaymentMethodMetadata, ProcessorEvidence};
/// fn financial_evidence(metadata: GatewayPaymentMethodMetadata) -> ProcessorEvidence {
///     metadata.into()
/// }
/// ```
#[derive(Clone, Debug)]
pub struct GatewayPaymentMethodMetadata {
    status: GatewayPaymentStatus,
    transaction_id: Option<GatewayTransactionId>,
    payment_method_reference: Option<GatewayPaymentMethodReference>,
    descriptor: GatewayPaymentDescriptor,
    diagnostics: Vec<GatewayPaymentDiagnostic>,
}

impl GatewayPaymentMethodMetadata {
    /// Discards financial response fields and retains only query correlation and
    /// display. The outcome's normalized diagnostics and status remain intact.
    pub fn from_query_outcome(outcome: GatewayPaymentOutcome) -> Self {
        let (status, evidence, diagnostics) = outcome.into_parts_with_diagnostics();
        Self {
            status,
            transaction_id: evidence.transaction_id,
            payment_method_reference: evidence.payment_method_reference,
            descriptor: evidence.descriptor,
            diagnostics,
        }
    }

    /// Observed status for rejecting contradictions, never approval authority.
    pub const fn status(&self) -> GatewayPaymentStatus {
        self.status
    }

    /// Provider transaction identity for exact correlation.
    pub const fn transaction_id(&self) -> Option<&GatewayTransactionId> {
        self.transaction_id.as_ref()
    }

    /// Optional provider reference for checking the current saved method.
    pub const fn payment_method_reference(&self) -> Option<&GatewayPaymentMethodReference> {
        self.payment_method_reference.as_ref()
    }

    /// Card-safe display fields observed by the provider query.
    pub const fn descriptor(&self) -> &GatewayPaymentDescriptor {
        &self.descriptor
    }

    /// Payload-free reasons the observation may be unsuitable for display repair.
    pub fn diagnostics(&self) -> &[GatewayPaymentDiagnostic] {
        &self.diagnostics
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GatewayDiagnostic, ProcessorEvidence};

    #[test]
    fn projection_preserves_diagnostic_quarantine_and_redaction() {
        let outcome = GatewayPaymentOutcome::new(
            GatewayPaymentStatus::Approved,
            ProcessorEvidence::new(
                Some(GatewayTransactionId::new("txn_private_metadata").unwrap()),
                Some(GatewayPaymentMethodReference::new("vault_private_metadata").unwrap()),
                None,
                None,
                Some(GatewayDiagnostic::new("private response")),
                None,
                GatewayPaymentDescriptor::from_provider_parts(
                    None,
                    Some(GatewayDiagnostic::new("Visa")),
                    Some("1111"),
                    Some(10),
                    Some(2029),
                ),
            ),
        )
        .with_diagnostics(vec![
            GatewayPaymentDiagnostic::InvalidOrConflictingTransactionIdentifier,
        ]);
        let metadata = GatewayPaymentMethodMetadata::from_query_outcome(outcome);
        assert_eq!(metadata.status(), GatewayPaymentStatus::Unknown);
        assert!(metadata.transaction_id().is_none());
        assert_eq!(
            metadata.diagnostics(),
            &[GatewayPaymentDiagnostic::InvalidOrConflictingTransactionIdentifier]
        );
        assert_eq!(metadata.descriptor().card_exp_year(), Some(2029));
        let debug = format!("{metadata:?}");
        for sensitive in [
            "txn_private_metadata",
            "vault_private_metadata",
            "private response",
        ] {
            assert!(!debug.contains(sensitive));
        }
    }
}
