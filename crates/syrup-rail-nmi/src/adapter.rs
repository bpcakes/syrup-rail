use async_trait::async_trait;
use chrono::Duration;
use syrup_rail::{
    BillingContact, GatewayAccountMode, GatewayDiagnostic, GatewayError, GatewayLifecycleCursorKey,
    GatewayLifecycleQueryPolicy, GatewayMutationError, GatewayNotSubmittedError, GatewayOrderId,
    GatewayPaymentDescriptor, GatewayPaymentOutcome, GatewayPaymentStatus, GatewayProviderKey,
    GatewayQueryRequest, GatewaySaleIntent, GatewaySaleRequest, GatewayStorePaymentMethodRequest,
    GatewayTransactionId, GatewayTransactionReport, GatewayTransactionReportRequest,
    PaymentGateway, ProcessorEvidence,
};
use syrup_rail_nmi_client::{
    AccountMode, Client, MutationError, PaymentDescriptorParts, PaymentOutcomeParts, PaymentStatus,
    QueryError, ReportQuery, SaleIntent, SaleRequest, SensitiveText, StorePaymentMethodRequest,
    TransactionActionParts, TransactionQuery, TransactionReportDiagnostic, TransactionReportParts,
};

use crate::{
    lifecycle::{NmiAction, NmiReport, admit_report},
    reference::nmi_mutation_reference_attempt_id,
};

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
    let intent = match intent {
        GatewaySaleIntent::OneTime { payment_token } => {
            SaleIntent::PaymentToken(payment_token.into_inner())
        }
        GatewaySaleIntent::InitialStoredCredential { payment_token } => {
            SaleIntent::InitialStoredCredential {
                payment_token: payment_token.into_inner(),
            }
        }
        GatewaySaleIntent::RecurringStoredCredential {
            payment_method_reference,
            initial_transaction_id,
        } => SaleIntent::RecurringStoredCredential {
            customer_vault_id: payment_method_reference.into_inner(),
            initial_transaction_id: initial_transaction_id.into_inner(),
        },
    };
    Ok(SaleRequest {
        amount_cents: charge.cents(),
        order_id: order_id.into_inner(),
        intent,
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
        tracing::warn!(
            provider = "nmi",
            diagnostic = ?diagnostic,
            "NMI returned anomalous payment outcome evidence"
        );
    }
    let mut status = map_payment_status(parts.status);
    let transaction_id = validated_transaction_id(parts.transaction_id);
    let payment_method_reference = validated_payment_method_reference(parts.customer_vault_id);
    if transaction_id.is_err() {
        tracing::warn!(
            provider = "nmi",
            identifier_kind = "transaction",
            "NMI returned an identifier rejected by Syrup Rail policy"
        );
    }
    if payment_method_reference.is_err() {
        tracing::warn!(
            provider = "nmi",
            identifier_kind = "customer_vault",
            "NMI returned an identifier rejected by Syrup Rail policy"
        );
    }
    let (transaction_id, payment_method_reference) =
        match (transaction_id, payment_method_reference) {
            (Ok(transaction_id), Ok(payment_method_reference)) => {
                (transaction_id, payment_method_reference)
            }
            _ => {
                status = GatewayPaymentStatus::Unknown;
                (None, None)
            }
        };
    let evidence = ProcessorEvidence::new(
        transaction_id,
        payment_method_reference,
        sanitized_text(parts.response),
        sanitized_text(parts.response_code),
        sanitized_text(parts.response_text),
        sanitized_text(parts.condition),
        map_payment_descriptor_parts(parts.descriptor.into_parts()),
    );
    GatewayPaymentOutcome::new(status, evidence)
}

fn map_payment_status(status: PaymentStatus) -> GatewayPaymentStatus {
    match status {
        PaymentStatus::Approved => GatewayPaymentStatus::Approved,
        PaymentStatus::Declined => GatewayPaymentStatus::Declined,
        PaymentStatus::Unknown => GatewayPaymentStatus::Unknown,
        PaymentStatus::Failed => GatewayPaymentStatus::Failed,
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
mod tests {
    use syrup_rail::{
        ChargeAmount, CurrencyCode, GatewayLifecycleQuarantineReason, PaymentAttemptId,
        PaymentCardBrand, PaymentToken,
    };
    use syrup_rail_nmi_client::{PaymentDescriptor, PaymentOutcomeParts, TransactionReportParts};

    use super::*;

    fn text(value: &str) -> SensitiveText {
        SensitiveText::new(value)
    }

    fn empty_outcome(status: PaymentStatus) -> PaymentOutcomeParts {
        PaymentOutcomeParts {
            status,
            transaction_id: None,
            customer_vault_id: None,
            response: None,
            response_code: None,
            response_text: None,
            condition: None,
            descriptor: PaymentDescriptor::default(),
            diagnostics: Vec::new(),
        }
    }

    #[test]
    fn non_usd_sale_is_rejected_before_transport() {
        let attempt_id: PaymentAttemptId = "00000000-0000-0000-0000-000000000000".parse().unwrap();
        let request = GatewaySaleRequest::new(
            ChargeAmount::new(100, CurrencyCode::new("EUR").unwrap()).unwrap(),
            GatewayOrderId::from_generated_attempt(
                "ck_order_00000000000000000000000000000000",
                attempt_id,
            )
            .unwrap(),
            GatewaySaleIntent::OneTime {
                payment_token: PaymentToken::new("tok_safe").unwrap(),
            },
            None,
        );
        assert!(matches!(
            map_sale_request(request),
            Err(GatewayMutationError::NotSubmitted(
                GatewayNotSubmittedError::Malformed(_)
            ))
        ));
    }

    #[test]
    fn invalid_either_identifier_downgrades_and_clears_both() {
        let mut parts = empty_outcome(PaymentStatus::Approved);
        parts.transaction_id = Some(text("txn_safe"));
        parts.customer_vault_id = Some(text("bad vault"));
        let outcome = map_payment_outcome_parts(parts);
        assert_eq!(outcome.status(), GatewayPaymentStatus::Unknown);
        assert!(outcome.transaction_id().is_none());
        assert!(outcome.payment_method_reference().is_none());
    }

    #[test]
    fn descriptor_admission_preserves_exact_bounds() {
        let descriptor = map_payment_descriptor_parts(PaymentDescriptorParts {
            payment_type: Some(text("creditcard")),
            card_brand: Some(text("visa")),
            card_last4: Some(text("1234")),
            card_exp_month: Some(12),
            card_exp_year: Some(2100),
        });
        assert_eq!(
            descriptor.card_brand().map(GatewayDiagnostic::expose),
            Some("visa")
        );
        assert_eq!(
            descriptor.canonical_card_brand(),
            Some(syrup_rail::PaymentCardBrand::Visa)
        );
        assert_eq!(descriptor.card_last_four().unwrap().expose(), "1234");
        assert_eq!(descriptor.card_exp_month(), Some(12));
        assert_eq!(descriptor.card_exp_year(), Some(2100));

        let invalid = map_payment_descriptor_parts(PaymentDescriptorParts {
            payment_type: None,
            card_brand: None,
            card_last4: Some(text("１２３４")),
            card_exp_month: Some(0),
            card_exp_year: Some(2101),
        });
        assert!(invalid.card_last_four().is_none());
        assert_eq!(invalid.card_exp_month(), None);
        assert_eq!(invalid.card_exp_year(), None);
    }

    #[test]
    fn documented_nmi_card_schemes_have_canonical_projections() {
        let schemes = [
            ("visa", PaymentCardBrand::Visa),
            ("mastercard", PaymentCardBrand::Mastercard),
            ("amex", PaymentCardBrand::AmericanExpress),
            ("discover", PaymentCardBrand::Discover),
            ("diners", PaymentCardBrand::DinersClub),
            ("Diners", PaymentCardBrand::DinersClub),
            ("jcb", PaymentCardBrand::Jcb),
            ("maestro", PaymentCardBrand::Maestro),
        ];

        for (provider_value, expected) in schemes {
            let descriptor = map_payment_descriptor_parts(PaymentDescriptorParts {
                payment_type: Some(text("creditcard")),
                card_brand: Some(text(provider_value)),
                card_last4: None,
                card_exp_month: None,
                card_exp_year: None,
            });
            assert_eq!(
                descriptor.card_brand().map(GatewayDiagnostic::expose),
                Some(provider_value)
            );
            assert_eq!(descriptor.canonical_card_brand(), Some(expected));
        }
    }

    #[test]
    fn invalid_optional_locator_does_not_discard_safe_sibling() {
        assert_eq!(nmi_mutation_reference_attempt_id("ck_order_safe"), None);
        let report = map_transaction_report_parts(TransactionReportParts {
            transaction_id: Some(text("bad transaction")),
            order_id: Some(text("ck_order_safe")),
            condition: Some(text("complete")),
            actions: Vec::new(),
            diagnostics: Vec::new(),
        });
        let GatewayTransactionReport::Evidence(evidence) = report else {
            panic!("safe order locator should admit evidence");
        };
        assert!(evidence.transaction_id().is_none());
        assert_eq!(evidence.order_id().unwrap().expose(), "ck_order_safe");
    }

    #[test]
    fn malformed_report_without_identifiers_is_quarantined() {
        let report = map_transaction_report_parts(TransactionReportParts {
            transaction_id: None,
            order_id: None,
            condition: None,
            actions: Vec::new(),
            diagnostics: vec![TransactionReportDiagnostic::MalformedStructure],
        });
        let GatewayTransactionReport::Quarantine(quarantine) = report else {
            panic!("malformed structure should quarantine");
        };
        assert_eq!(
            quarantine.reason(),
            GatewayLifecycleQuarantineReason::MalformedReportStructure
        );
    }

    #[test]
    fn canonical_generated_order_survives_luhn_false_positive_uuid_digits() {
        let value = "ck_renewal_00000000000000000000000000000000";
        assert!(syrup_rail::string_contains_raw_card_data(value));
        let report = map_transaction_report_parts(TransactionReportParts {
            transaction_id: None,
            order_id: Some(text("  ck_renewal_00000000000000000000000000000000  ")),
            condition: Some(text("complete")),
            actions: Vec::new(),
            diagnostics: Vec::new(),
        });
        let GatewayTransactionReport::Evidence(evidence) = report else {
            panic!("canonical generated order should remain usable");
        };
        assert_eq!(evidence.order_id().unwrap().expose(), value);
    }

    #[test]
    fn descriptor_carries_current_nmi_query_envelope() {
        let policy = NmiPaymentGateway::lifecycle_query_policy();
        assert_eq!(policy.cursor_key().as_str(), "nmi_approved_lifecycle");
        assert_eq!(policy.overlap(), Duration::minutes(5));
        assert_eq!(policy.page_size().get(), 100);
        assert_eq!(policy.ordinary_page_limit().get(), 20);
        assert_eq!(policy.max_window_splits().get(), 12);
        assert_eq!(policy.narrow_window_drain_page_limit().get(), 2_000);
    }
}
