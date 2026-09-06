/// Processor evidence refined by an authoritative approved gateway outcome.
///
/// Raw processor fields are intentionally not reclassified here: some valid
/// approved outcomes carry incomplete evidence until exact reconciliation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovedProcessorEvidence {
    evidence: ProcessorEvidence,
}

impl ApprovedProcessorEvidence {
    pub const fn evidence(&self) -> &ProcessorEvidence {
        &self.evidence
    }

    pub fn into_evidence(self) -> ProcessorEvidence {
        self.evidence
    }
}

impl fmt::Debug for ProcessorEvidence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProcessorEvidence")
            .field("approval_evidence", &self.approval_evidence)
            .field("has_transaction_id", &self.transaction_id.is_some())
            .field(
                "has_payment_method_reference",
                &self.payment_method_reference.is_some(),
            )
            .field("has_response", &self.response.is_some())
            .field("has_response_code", &self.response_code.is_some())
            .field("has_response_text", &self.response_text.is_some())
            .field("has_condition", &self.condition.is_some())
            .field("descriptor", &self.descriptor)
            .finish()
    }
}

#[derive(Clone, Debug)]
#[must_use = "gateway payment outcomes contain authoritative provider decisions"]
pub struct GatewayPaymentOutcome {
    status: GatewayPaymentStatus,
    evidence: ProcessorEvidence,
    diagnostics: Vec<GatewayPaymentDiagnostic>,
}

impl GatewayPaymentOutcome {
    pub const fn new(status: GatewayPaymentStatus, evidence: ProcessorEvidence) -> Self {
        Self {
            status,
            evidence,
            diagnostics: Vec::new(),
        }
    }

    /// Attaches provider-neutral diagnostics derived by the current gateway
    /// observation.
    ///
    /// Diagnostics have set semantics: duplicate values are removed and the
    /// returned slice uses a canonical order. Callers should route by membership
    /// rather than treating that order as provider chronology or precedence.
    /// Diagnostics about decision certainty and unreconciled processor
    /// duplicates conservatively force an unknown status regardless of the
    /// provider-reported status. Missing-identity diagnostics quarantine the
    /// corresponding field while leaving an approval visible for workflow
    /// parking. Invalid or conflicting identity instead prevents an approval
    /// from remaining authoritative, while preserving a determinate decline or
    /// failure.
    ///
    /// A certainty downgrade is monotonic. Replacing the diagnostic set cannot
    /// restore a terminal decision after earlier evidence made it unknown.
    pub fn with_diagnostics(mut self, diagnostics: Vec<GatewayPaymentDiagnostic>) -> Self {
        self.diagnostics = normalize_gateway_payment_diagnostics(diagnostics);
        if self.diagnostics.iter().any(|diagnostic| {
            matches!(
                diagnostic,
                GatewayPaymentDiagnostic::MissingTransactionIdentifier
                    | GatewayPaymentDiagnostic::InvalidOrConflictingTransactionIdentifier
            )
        }) {
            self.evidence.transaction_id = None;
        }
        if self.diagnostics.iter().any(|diagnostic| {
            matches!(
                diagnostic,
                GatewayPaymentDiagnostic::MissingPaymentMethodReference
                    | GatewayPaymentDiagnostic::InvalidOrConflictingPaymentMethodReference
            )
        }) {
            self.evidence.payment_method_reference = None;
        }
        if self.diagnostics.iter().any(|diagnostic| {
            diagnostic.requires_unknown_status()
                || self.status == GatewayPaymentStatus::Approved && diagnostic.prevents_approval()
        }) {
            self.status = GatewayPaymentStatus::Unknown;
        }
        self
    }

    pub const fn status(&self) -> GatewayPaymentStatus {
        self.status
    }

    pub const fn evidence(&self) -> &ProcessorEvidence {
        &self.evidence
    }

    /// Returns payload-free diagnostics suitable for host policy and routing.
    ///
    /// The slice is deduplicated and canonically ordered. Its order carries no
    /// provider chronology or policy precedence; use [`Self::has_diagnostic`]
    /// for membership-based routing.
    pub fn diagnostics(&self) -> &[GatewayPaymentDiagnostic] {
        &self.diagnostics
    }

    /// Returns whether this outcome contains a particular payload-free
    /// diagnostic.
    pub fn has_diagnostic(&self, diagnostic: GatewayPaymentDiagnostic) -> bool {
        self.diagnostics.contains(&diagnostic)
    }

    /// Refines this outcome's evidence only when the provider decision is
    /// authoritatively approved.
    pub fn approved_evidence(&self) -> Option<ApprovedProcessorEvidence> {
        (self.status == GatewayPaymentStatus::Approved).then(|| ApprovedProcessorEvidence {
            evidence: self.evidence.clone(),
        })
    }

    /// Consumes the outcome into its original durable decision parts.
    ///
    /// This compatibility method does not return provider-neutral diagnostics;
    /// use [`Self::into_parts_with_diagnostics`] when routing them matters.
    #[deprecated(
        note = "this drops gateway diagnostics; use GatewayPaymentOutcome::into_parts_with_diagnostics"
    )]
    pub fn into_parts(self) -> (GatewayPaymentStatus, ProcessorEvidence) {
        (self.status, self.evidence)
    }

    /// Consumes the outcome without discarding provider-neutral diagnostics.
    pub fn into_parts_with_diagnostics(
        self,
    ) -> (
        GatewayPaymentStatus,
        ProcessorEvidence,
        Vec<GatewayPaymentDiagnostic>,
    ) {
        (self.status, self.evidence, self.diagnostics)
    }

    pub const fn transaction_id(&self) -> Option<&GatewayTransactionId> {
        self.evidence.transaction_id()
    }

    pub const fn payment_method_reference(&self) -> Option<&GatewayPaymentMethodReference> {
        self.evidence.payment_method_reference()
    }

    pub const fn response(&self) -> Option<&GatewayDiagnostic> {
        self.evidence.response()
    }

    pub const fn response_code(&self) -> Option<&GatewayDiagnostic> {
        self.evidence.response_code()
    }

    pub const fn response_text(&self) -> Option<&GatewayDiagnostic> {
        self.evidence.response_text()
    }

    pub const fn condition(&self) -> Option<&GatewayDiagnostic> {
        self.evidence.condition()
    }

    pub const fn descriptor(&self) -> &GatewayPaymentDescriptor {
        self.evidence.descriptor()
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum GatewayLifecycleQueryPolicyError {
    #[error("gateway lifecycle query overlap must be positive")]
    NonPositiveOverlap,
    #[error("gateway lifecycle query limit must be positive")]
    NonPositiveLimit,
    #[error("gateway lifecycle query limits overflow their bounded work calculation")]
    Overflow,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GatewayLifecycleQueryPolicy {
    cursor_key: GatewayLifecycleCursorKey,
    overlap: Duration,
    page_size: NonZeroU32,
    ordinary_page_limit: NonZeroU32,
    max_window_splits: NonZeroU32,
    narrow_window_drain_page_limit: NonZeroU32,
}

impl GatewayLifecycleQueryPolicy {
    pub fn new(
        cursor_key: GatewayLifecycleCursorKey,
        overlap: Duration,
        page_size: u32,
        ordinary_page_limit: u32,
        max_window_splits: u32,
        narrow_window_drain_page_limit: u32,
    ) -> Result<Self, GatewayLifecycleQueryPolicyError> {
        if overlap <= Duration::zero() {
            return Err(GatewayLifecycleQueryPolicyError::NonPositiveOverlap);
        }
        let page_size =
            NonZeroU32::new(page_size).ok_or(GatewayLifecycleQueryPolicyError::NonPositiveLimit)?;
        let ordinary_page_limit = NonZeroU32::new(ordinary_page_limit)
            .ok_or(GatewayLifecycleQueryPolicyError::NonPositiveLimit)?;
        let max_window_splits = NonZeroU32::new(max_window_splits)
            .ok_or(GatewayLifecycleQueryPolicyError::NonPositiveLimit)?;
        let narrow_window_drain_page_limit = NonZeroU32::new(narrow_window_drain_page_limit)
            .ok_or(GatewayLifecycleQueryPolicyError::NonPositiveLimit)?;
        page_size
            .get()
            .checked_mul(ordinary_page_limit.get())
            .and_then(|value| value.checked_mul(max_window_splits.get()))
            .and_then(|_| {
                page_size
                    .get()
                    .checked_mul(narrow_window_drain_page_limit.get())
            })
            .ok_or(GatewayLifecycleQueryPolicyError::Overflow)?;
        Ok(Self {
            cursor_key,
            overlap,
            page_size,
            ordinary_page_limit,
            max_window_splits,
            narrow_window_drain_page_limit,
        })
    }

    pub const fn cursor_key(&self) -> &GatewayLifecycleCursorKey {
        &self.cursor_key
    }

    pub const fn overlap(&self) -> Duration {
        self.overlap
    }

    pub const fn page_size(&self) -> NonZeroU32 {
        self.page_size
    }

    pub const fn ordinary_page_limit(&self) -> NonZeroU32 {
        self.ordinary_page_limit
    }

    pub const fn max_window_splits(&self) -> NonZeroU32 {
        self.max_window_splits
    }

    pub const fn narrow_window_drain_page_limit(&self) -> NonZeroU32 {
        self.narrow_window_drain_page_limit
    }
}

#[cfg(test)]
mod tests;
