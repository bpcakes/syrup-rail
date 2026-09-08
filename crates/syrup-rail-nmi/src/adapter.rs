use async_trait::async_trait;
use chrono::Duration;
use syrup_rail::{
    BillingContact, GatewayAccountMode, GatewayDiagnostic, GatewayError, GatewayLifecycleCursorKey,
    GatewayLifecycleQueryPolicy, GatewayMutationError, GatewayNotSubmittedError, GatewayOrderId,
    GatewayPaymentDescriptor, GatewayPaymentDiagnostic, GatewayPaymentMethodMetadata,
    GatewayPaymentOutcome, GatewayPaymentStatus, GatewayProviderKey, GatewayQueryRequest,
    GatewaySaleIntent, GatewaySaleRequest, GatewayStorePaymentMethodRequest, GatewayTransactionId,
    GatewayTransactionReport, GatewayTransactionReportRequest, PaymentAttemptId, PaymentGateway,
    ProcessorEvidence,
};
use syrup_rail_nmi_client::{
    AccountMode, Client, MutationError, PaymentDescriptorParts, PaymentOutcomeDiagnostic,
    PaymentOutcomeParts, PaymentSource, PaymentStatus, QueryError, ReportQuery, SaleRequest,
    SensitiveText, StorePaymentMethodRequest, StoredCredential, TransactionActionParts,
    TransactionQuery, TransactionReportDiagnostic, TransactionReportParts, VaultAction,
};

use crate::lifecycle::{NmiAction, NmiReport, admit_report};

pub struct NmiPaymentGateway {
    client: Client,
}

impl NmiPaymentGateway {
    pub const fn new(client: Client) -> Self {
        Self { client }
    }

    pub fn provider_key() -> GatewayProviderKey {
        GatewayProviderKey::new("nmi").expect("static NMI provider key is valid")
    }

    pub fn lifecycle_query_policy() -> GatewayLifecycleQueryPolicy {
        GatewayLifecycleQueryPolicy::new(
            GatewayLifecycleCursorKey::new("nmi_approved_lifecycle")
                .expect("static NMI cursor key is valid"),
            Duration::minutes(5),
            100,
            20,
            12,
            2_000,
        )
        .expect("static NMI lifecycle query policy is valid")
    }
}

impl std::fmt::Debug for NmiPaymentGateway {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NmiPaymentGateway")
            .field("client", &self.client)
            .finish()
    }
}

#[async_trait]
impl PaymentGateway for NmiPaymentGateway {
    async fn account_mode(&self) -> Result<GatewayAccountMode, GatewayError> {
        self.client
            .account_mode()
            .await
            .map(map_account_mode)
            .map_err(map_query_error)
    }

    async fn sale(
        &self,
        request: GatewaySaleRequest,
    ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
        let request = map_sale_request(request)?;
        self.client
            .sale(request)
            .await
            .map(map_payment_outcome)
            .map_err(map_mutation_error)
    }

    async fn store_payment_method(
        &self,
        request: GatewayStorePaymentMethodRequest,
    ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
        let (payment_token, order_id, billing_contact) = request.into_parts();
        self.client
            .store_payment_method(StorePaymentMethodRequest {
                payment_token: payment_token.into_inner(),
                order_id: order_id.into_inner(),
                billing_contact: billing_contact.map(map_billing_contact),
            })
            .await
            .map(map_payment_outcome)
            .map_err(map_mutation_error)
    }

    async fn query_transaction(
        &self,
        request: GatewayQueryRequest,
    ) -> Result<Option<GatewayPaymentOutcome>, GatewayError> {
        let (transaction_id, order_id) = request.into_parts();
        self.client
            .query_transaction(TransactionQuery {
                transaction_id: transaction_id.map(GatewayTransactionId::into_inner),
                order_id: order_id.map(GatewayOrderId::into_inner),
            })
            .await
            .map(|outcome| outcome.map(map_payment_outcome))
            .map_err(map_query_error)
    }

    async fn query_transaction_reports(
        &self,
        request: GatewayTransactionReportRequest,
    ) -> Result<Vec<GatewayTransactionReport>, GatewayError> {
        let reports = self
            .client
            .query_transaction_reports(ReportQuery {
                start_date: request.start_at().format("%Y%m%d%H%M%S").to_string(),
                end_date: request.end_at().format("%Y%m%d%H%M%S").to_string(),
                result_limit: i64::from(request.page_size().get()),
                page_number: i64::from(request.page_index()),
            })
            .await
            .map_err(map_query_error)?;
        Ok(reports
            .into_iter()
            .map(|report| map_transaction_report_parts(report.into_parts()))
            .collect())
    }

    async fn query_payment_method_metadata(
        &self,
        request: GatewayQueryRequest,
    ) -> Result<Option<GatewayPaymentMethodMetadata>, GatewayError> {
        let (transaction_id, order_id) = request.into_parts();
        self.client
            .query_payment_method_metadata(TransactionQuery {
                transaction_id: transaction_id.map(GatewayTransactionId::into_inner),
                order_id: order_id.map(GatewayOrderId::into_inner),
            })
            .await
            .map(|metadata| metadata.map(map_payment_method_metadata))
            .map_err(map_query_error)
    }
}

fn map_payment_method_metadata(
    metadata: syrup_rail_nmi_client::PaymentMethodMetadata,
) -> GatewayPaymentMethodMetadata {
    let parts = metadata.into_parts();
    // Reuse identity admission and diagnostic normalization, then discard all
    // financial authority before returning across the metadata query boundary.
    GatewayPaymentMethodMetadata::from_query_outcome(map_payment_outcome_parts(
        PaymentOutcomeParts {
            status: parts.status,
            transaction_id: parts.transaction_id,
            customer_vault_id: parts.customer_vault_id,
            descriptor: parts.descriptor,
            diagnostics: parts.diagnostics,
            response: None,
            response_code: None,
            response_text: None,
            condition: None,
        },
    ))
}

fn map_account_mode(mode: AccountMode) -> GatewayAccountMode {
    match mode {
        AccountMode::Live => GatewayAccountMode::Live,
        AccountMode::Test => GatewayAccountMode::Test,
    }
}

fn map_sale_request(request: GatewaySaleRequest) -> Result<SaleRequest, GatewayMutationError> {
    let (charge, order_id, intent, billing_contact) = request.into_parts();
    if charge.currency().as_str() != "USD" {
        return Err(GatewayMutationError::NotSubmitted(
            GatewayNotSubmittedError::Malformed(GatewayDiagnostic::new(
                "NMI sale currency is unsupported",
            )),
        ));
    }
    let (source, vault_action, stored_credential) = match intent {
        GatewaySaleIntent::OneTime { payment_token } => (
            PaymentSource::PaymentToken(payment_token.into_inner()),
            None,
            None,
        ),
        GatewaySaleIntent::InitialStoredCredential { payment_token } => (
            PaymentSource::PaymentToken(payment_token.into_inner()),
            Some(VaultAction::AddCustomer),
            Some(StoredCredential::InitialCustomer),
        ),
        GatewaySaleIntent::RecurringStoredCredential {
            payment_method_reference,
            initial_transaction_id,
        } => (
            PaymentSource::CustomerVault(payment_method_reference.into_inner()),
            None,
            Some(StoredCredential::RecurringMerchant {
                initial_transaction_id: initial_transaction_id.into_inner(),
            }),
        ),
    };
    Ok(SaleRequest {
        amount_cents: charge.cents(),
        currency: charge.currency().as_str().to_owned(),
        order_id: order_id.into_inner(),
        source,
        vault_action,
        stored_credential,
        billing_contact: billing_contact.map(map_billing_contact),
    })
}

fn map_billing_contact(contact: BillingContact) -> syrup_rail_nmi_client::BillingContact {
    let (first_name, last_name, email) = contact.into_parts();
    syrup_rail_nmi_client::BillingContact {
        first_name,
        last_name,
        email,
    }
}

fn map_payment_outcome(outcome: syrup_rail_nmi_client::PaymentOutcome) -> GatewayPaymentOutcome {
    map_payment_outcome_parts(outcome.into_parts())
}

fn map_payment_outcome_parts(parts: PaymentOutcomeParts) -> GatewayPaymentOutcome {
    for diagnostic in &parts.diagnostics {
        match diagnostic {
            PaymentOutcomeDiagnostic::DuplicateTransactionAtProcessor => {
                tracing::warn!(
                    provider = "nmi",
                    diagnostic = ?diagnostic,
                    "NMI reported a duplicate transaction at the processor; reconciliation is required"
                );
            }
            _ => {
                tracing::warn!(
                    provider = "nmi",
                    diagnostic = ?diagnostic,
                    "NMI returned anomalous payment outcome evidence"
                );
            }
        }
    }
    let mut diagnostics = parts
        .diagnostics
        .iter()
        .map(map_payment_diagnostic)
        .collect();
    let status = map_payment_status(parts.status);
    let mut identifiers = adapt_payment_identifiers(
        parts.transaction_id,
        parts.customer_vault_id,
        &mut diagnostics,
    );
    let invalid_identity_bundle = diagnostics.iter().any(|diagnostic| {
        matches!(
            diagnostic,
            GatewayPaymentDiagnostic::InvalidOrConflictingTransactionIdentifier
                | GatewayPaymentDiagnostic::InvalidOrConflictingPaymentMethodReference
        )
    });
    if invalid_identity_bundle {
        identifiers.transaction_id = None;
        identifiers.payment_method_reference = None;
    }
    let evidence = ProcessorEvidence::new(
        identifiers.transaction_id,
        identifiers.payment_method_reference,
        sanitized_text(parts.response),
        sanitized_text(parts.response_code),
        sanitized_text(parts.response_text),
        sanitized_text(parts.condition),
        map_payment_descriptor_parts(parts.descriptor.into_parts()),
    );
    GatewayPaymentOutcome::new(status, evidence).with_diagnostics(diagnostics)
}

fn map_payment_diagnostic(diagnostic: &PaymentOutcomeDiagnostic) -> GatewayPaymentDiagnostic {
    match diagnostic {
        PaymentOutcomeDiagnostic::MissingTransactionIdentifier => {
            GatewayPaymentDiagnostic::MissingTransactionIdentifier
        }
        PaymentOutcomeDiagnostic::MissingCustomerVaultIdentifier => {
            GatewayPaymentDiagnostic::MissingPaymentMethodReference
        }
        PaymentOutcomeDiagnostic::InvalidOrConflictingTransactionIdentifier => {
            GatewayPaymentDiagnostic::InvalidOrConflictingTransactionIdentifier
        }
        PaymentOutcomeDiagnostic::InvalidOrConflictingCustomerVaultIdentifier => {
            GatewayPaymentDiagnostic::InvalidOrConflictingPaymentMethodReference
        }
        PaymentOutcomeDiagnostic::InvalidOrConflictingDecisionField => {
            GatewayPaymentDiagnostic::InvalidOrConflictingDecisionField
        }
        PaymentOutcomeDiagnostic::IndeterminatePaymentOutcome => {
            GatewayPaymentDiagnostic::IndeterminatePaymentOutcome
        }
        PaymentOutcomeDiagnostic::DuplicateTransactionAtProcessor => {
            GatewayPaymentDiagnostic::ProcessorReportedDuplicate
        }
        PaymentOutcomeDiagnostic::ConflictingDecisionEvidence => {
            GatewayPaymentDiagnostic::ConflictingDecisionEvidence
        }
        PaymentOutcomeDiagnostic::UnrecognizedDecisionEvidence => {
            GatewayPaymentDiagnostic::UnrecognizedDecisionEvidence
        }
        PaymentOutcomeDiagnostic::MissingDecisionEvidence => {
            GatewayPaymentDiagnostic::MissingDecisionEvidence
        }
        _ => GatewayPaymentDiagnostic::UnmappedProviderDiagnostic,
    }
}

fn map_payment_status(status: PaymentStatus) -> GatewayPaymentStatus {
    match status {
        PaymentStatus::Approved => GatewayPaymentStatus::Approved,
        PaymentStatus::Declined => GatewayPaymentStatus::Declined,
        PaymentStatus::Unknown => GatewayPaymentStatus::Unknown,
        PaymentStatus::Failed => GatewayPaymentStatus::Failed,
    }
}

struct AdaptedPaymentIdentifiers {
    transaction_id: Option<GatewayTransactionId>,
    payment_method_reference: Option<syrup_rail::GatewayPaymentMethodReference>,
}

fn adapt_payment_identifiers(
    transaction_id: Option<SensitiveText>,
    payment_method_reference: Option<SensitiveText>,
    diagnostics: &mut Vec<GatewayPaymentDiagnostic>,
) -> AdaptedPaymentIdentifiers {
    let transaction_id = match validated_transaction_id(transaction_id) {
        Ok(transaction_id) => transaction_id,
        Err(()) => {
            tracing::warn!(
                provider = "nmi",
                identifier_kind = "transaction",
                "NMI returned an identifier rejected by Syrup Rail policy"
            );
            diagnostics.push(GatewayPaymentDiagnostic::InvalidOrConflictingTransactionIdentifier);
            None
        }
    };
    let payment_method_reference =
        match validated_payment_method_reference(payment_method_reference) {
            Ok(payment_method_reference) => payment_method_reference,
            Err(()) => {
                tracing::warn!(
                    provider = "nmi",
                    identifier_kind = "customer_vault",
                    "NMI returned an identifier rejected by Syrup Rail policy"
                );
                diagnostics
                    .push(GatewayPaymentDiagnostic::InvalidOrConflictingPaymentMethodReference);
                None
            }
        };
    AdaptedPaymentIdentifiers {
        transaction_id,
        payment_method_reference,
    }
}

fn map_payment_descriptor_parts(parts: PaymentDescriptorParts) -> GatewayPaymentDescriptor {
    let card_last_four = parts
        .card_last4
        .as_ref()
        .map(SensitiveText::expose)
        .map(str::to_owned);
    GatewayPaymentDescriptor::from_provider_parts(
        sanitized_text(parts.payment_type),
        sanitized_text(parts.card_brand),
        card_last_four.as_deref(),
        parts.card_exp_month,
        parts.card_exp_year,
    )
}

fn validated_transaction_id(
    value: Option<SensitiveText>,
) -> Result<Option<GatewayTransactionId>, ()> {
    value
        .map(|value| GatewayTransactionId::from_correlation(value.expose()).map_err(|_| ()))
        .transpose()
}

fn validated_payment_method_reference(
    value: Option<SensitiveText>,
) -> Result<Option<syrup_rail::GatewayPaymentMethodReference>, ()> {
    value
        .map(|value| {
            syrup_rail::GatewayPaymentMethodReference::from_correlation(value.expose())
                .map_err(|_| ())
        })
        .transpose()
}

fn map_transaction_report_parts(parts: TransactionReportParts) -> GatewayTransactionReport {
    for diagnostic in &parts.diagnostics {
        tracing::warn!(
            provider = "nmi",
            diagnostic = ?diagnostic,
            "NMI returned anomalous transaction report evidence"
        );
    }
    let transaction_id = validated_report_transaction_id(parts.transaction_id);
    let order_id = validated_report_order_id(parts.order_id);
    let malformed_structure = parts
        .diagnostics
        .iter()
        .any(|diagnostic| matches!(diagnostic, TransactionReportDiagnostic::MalformedStructure));
    let report = NmiReport {
        transaction_id,
        order_id,
        condition: sanitized_text(parts.condition),
        actions: parts
            .actions
            .into_iter()
            .map(|action| map_transaction_action_parts(action.into_parts()))
            .collect(),
        malformed_structure,
    };
    admit_report(report)
}

fn validated_report_transaction_id(value: Option<SensitiveText>) -> Option<GatewayTransactionId> {
    match validated_transaction_id(value) {
        Ok(identifier) => identifier,
        Err(()) => {
            tracing::warn!(
                provider = "nmi",
                identifier_kind = "transaction",
                "NMI transaction report identifier was rejected by Syrup Rail policy"
            );
            None
        }
    }
}

fn validated_report_order_id(value: Option<SensitiveText>) -> Option<GatewayOrderId> {
    match value
        .map(|value| {
            let value = value.expose().trim();
            if let Some(attempt_id) = nmi_mutation_reference_attempt_id(value) {
                GatewayOrderId::from_generated_attempt(value, attempt_id)
            } else {
                GatewayOrderId::from_correlation(value)
            }
            .map_err(|_| ())
        })
        .transpose()
    {
        Ok(identifier) => identifier,
        Err(()) => {
            tracing::warn!(
                provider = "nmi",
                identifier_kind = "order",
                "NMI transaction report identifier was rejected by Syrup Rail policy"
            );
            None
        }
    }
}

fn nmi_mutation_reference_attempt_id(value: &str) -> Option<PaymentAttemptId> {
    let mut parts = value.split('_');
    let namespace = parts.next()?;
    let kind = parts.next()?;
    let attempt_id = parts.next()?;
    let has_canonical_shape = parts.next().is_none()
        && namespace.len() == 2
        && namespace.bytes().all(|byte| byte.is_ascii_lowercase())
        && matches!(
            kind,
            "order" | "base-sub" | "renewal" | "recovery" | "payment-method"
        )
        && attempt_id.len() == 32
        && attempt_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'));
    has_canonical_shape
        .then(|| attempt_id.parse::<PaymentAttemptId>().ok())
        .flatten()
}

fn map_transaction_action_parts(parts: TransactionActionParts) -> NmiAction {
    NmiAction {
        action_type: sanitized_text(parts.action_type),
        date: sanitized_text(parts.date),
        amount: sanitized_text(parts.amount),
        success: sanitized_text(parts.success),
    }
}

fn sanitized_text(value: Option<SensitiveText>) -> Option<GatewayDiagnostic> {
    value.map(|value| GatewayDiagnostic::new(value.expose()))
}

fn map_mutation_error(error: MutationError) -> GatewayMutationError {
    let detail = GatewayDiagnostic::new(error.detail().expose());
    match error {
        MutationError::InvalidRequest(_) => {
            GatewayMutationError::NotSubmitted(GatewayNotSubmittedError::Malformed(detail))
        }
        MutationError::RequestRejected(_) => {
            GatewayMutationError::NotSubmitted(GatewayNotSubmittedError::RequestRejected(detail))
        }
        MutationError::Configuration(_) => {
            GatewayMutationError::NotSubmitted(GatewayNotSubmittedError::Configuration(detail))
        }
        MutationError::Unavailable(_) => {
            GatewayMutationError::NotSubmitted(GatewayNotSubmittedError::NotTransmitted(detail))
        }
        MutationError::RateLimited(_) => {
            GatewayMutationError::NotSubmitted(GatewayNotSubmittedError::RateLimited(detail))
        }
        MutationError::RateLimitedIndeterminate(_) => {
            GatewayMutationError::RateLimitedIndeterminate(detail)
        }
        MutationError::Indeterminate(_) => GatewayMutationError::Indeterminate(detail),
    }
}

fn map_query_error(error: QueryError) -> GatewayError {
    let detail = GatewayDiagnostic::new(error.detail().expose());
    match error {
        QueryError::InvalidRequest(_) => GatewayError::RequestRejected(detail),
        QueryError::MalformedResponse(_) => GatewayError::Malformed(detail),
        QueryError::Configuration(_) => GatewayError::Configuration(detail),
        QueryError::RateLimited(_) => GatewayError::RateLimited(detail),
        QueryError::Unavailable(_) => GatewayError::Unavailable(detail),
    }
}

#[cfg(test)]
mod tests;
