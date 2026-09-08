use syrup_rail::{
    ChargeAmount, CurrencyCode, GatewayLifecycleQuarantineReason, PaymentAttemptId,
    PaymentCardBrand, PaymentToken,
};
use syrup_rail_nmi_client::{
    ClientFactory, Credentials, DuplicateCheck, Endpoint, PaymentDescriptor, PaymentOutcomeParts,
    TransactionReportParts,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;

mod card_metadata;

fn text(value: &str) -> SensitiveText {
    SensitiveText::new(value)
}

fn empty_outcome(status: PaymentStatus) -> PaymentOutcomeParts {
    PaymentOutcomeParts {
        status,
        approval_evidence: if status == PaymentStatus::Approved {
            syrup_rail_nmi_client::PaymentApprovalEvidence::Structured
        } else {
            syrup_rail_nmi_client::PaymentApprovalEvidence::Absent
        },
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

async fn gateway_with_response(
    response_body: &'static str,
) -> (NmiPaymentGateway, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let endpoint = Endpoint::parse_loopback_http(format!(
        "http://{}",
        listener.local_addr().expect("test listener address")
    ))
    .expect("loopback endpoint should validate");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("sale should connect");
        let mut request = Vec::new();
        let mut chunk = [0_u8; 1024];
        let header_end = loop {
            let read = stream.read(&mut chunk).await.expect("request should read");
            assert!(read > 0, "request closed before headers");
            request.extend_from_slice(&chunk[..read]);
            if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                break end + 4;
            }
        };
        let content_length = String::from_utf8_lossy(&request[..header_end])
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or_default();
        while request.len() < header_end + content_length {
            let read = stream
                .read(&mut chunk)
                .await
                .expect("request body should read");
            assert!(read > 0, "request closed before body");
            request.extend_from_slice(&chunk[..read]);
        }
        let headers = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            response_body.len()
        );
        stream
            .write_all(headers.as_bytes())
            .await
            .expect("response headers should write");
        stream
            .write_all(response_body.as_bytes())
            .await
            .expect("response body should write");
    });
    let credentials = Credentials::new("private_key".to_owned(), "query_key".to_owned())
        .expect("test credentials should validate");
    let client = ClientFactory::new_with_loopback_http()
        .expect("test factory should construct")
        .client_with_duplicate_check(endpoint, credentials, DuplicateCheck::ProcessorConfigured)
        .expect("test client should construct");
    (NmiPaymentGateway::new(client), server)
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
fn invalid_identifiers_downgrade_approval_and_quarantine_identity_bundle() {
    for (transaction_id, payment_method_reference, expected_diagnostics) in [
        (
            "txn_safe",
            "bad vault",
            vec![GatewayPaymentDiagnostic::InvalidOrConflictingPaymentMethodReference],
        ),
        (
            "bad transaction",
            "vault_safe",
            vec![GatewayPaymentDiagnostic::InvalidOrConflictingTransactionIdentifier],
        ),
        (
            "bad transaction",
            "bad vault",
            vec![
                GatewayPaymentDiagnostic::InvalidOrConflictingTransactionIdentifier,
                GatewayPaymentDiagnostic::InvalidOrConflictingPaymentMethodReference,
            ],
        ),
    ] {
        let mut parts = empty_outcome(PaymentStatus::Approved);
        parts.transaction_id = Some(text(transaction_id));
        parts.customer_vault_id = Some(text(payment_method_reference));

        let outcome = map_payment_outcome_parts(parts);

        assert_eq!(outcome.status(), GatewayPaymentStatus::Unknown);
        assert!(outcome.transaction_id().is_none());
        assert!(outcome.payment_method_reference().is_none());
        assert_eq!(outcome.diagnostics(), expected_diagnostics);
        assert!(outcome.approved_evidence().is_none());
    }
}

#[test]
fn rejected_optional_identifier_preserves_determinate_status_but_quarantines_identity_bundle() {
    for (source_status, expected_status) in [
        (PaymentStatus::Declined, GatewayPaymentStatus::Declined),
        (PaymentStatus::Failed, GatewayPaymentStatus::Failed),
    ] {
        let mut parts = empty_outcome(source_status);
        parts.transaction_id = Some(text("txn_reconciliation"));
        parts.customer_vault_id = Some(text("bad vault"));

        let outcome = map_payment_outcome_parts(parts);

        assert_eq!(outcome.status(), expected_status);
        assert!(outcome.transaction_id().is_none());
        assert!(outcome.payment_method_reference().is_none());
        assert_eq!(
            outcome.diagnostics(),
            &[GatewayPaymentDiagnostic::InvalidOrConflictingPaymentMethodReference]
        );
    }
}

#[test]
fn raw_identity_conflict_diagnostic_quarantines_parseable_sibling() {
    let mut parts = empty_outcome(PaymentStatus::Unknown);
    parts.customer_vault_id = Some(text("vault_parseable_but_untrusted"));
    parts.diagnostics = vec![PaymentOutcomeDiagnostic::InvalidOrConflictingTransactionIdentifier];

    let outcome = map_payment_outcome_parts(parts);

    assert_eq!(outcome.status(), GatewayPaymentStatus::Unknown);
    assert!(outcome.transaction_id().is_none());
    assert!(outcome.payment_method_reference().is_none());
    assert_eq!(
        outcome.diagnostics(),
        &[GatewayPaymentDiagnostic::InvalidOrConflictingTransactionIdentifier]
    );
}

#[test]
fn missing_transaction_identity_preserves_valid_vault_evidence() {
    let mut parts = empty_outcome(PaymentStatus::Unknown);
    parts.customer_vault_id = Some(text("vault_reconciliation"));
    parts.diagnostics = vec![PaymentOutcomeDiagnostic::MissingTransactionIdentifier];

    let outcome = map_payment_outcome_parts(parts);

    assert_eq!(outcome.status(), GatewayPaymentStatus::Unknown);
    assert!(outcome.transaction_id().is_none());
    assert_eq!(
        outcome
            .payment_method_reference()
            .map(syrup_rail::GatewayPaymentMethodReference::expose),
        Some("vault_reconciliation")
    );
    assert_eq!(
        outcome.diagnostics(),
        &[GatewayPaymentDiagnostic::MissingTransactionIdentifier]
    );
    assert!(outcome.approved_evidence().is_none());
}

#[test]
fn processor_duplicate_diagnostic_crosses_the_provider_neutral_boundary() {
    let mut parts = empty_outcome(PaymentStatus::Unknown);
    parts.response_code = Some(text("430"));
    parts.diagnostics = vec![PaymentOutcomeDiagnostic::DuplicateTransactionAtProcessor];

    let outcome = map_payment_outcome_parts(parts);

    assert_eq!(outcome.status(), GatewayPaymentStatus::Unknown);
    assert_eq!(
        outcome.diagnostics(),
        &[GatewayPaymentDiagnostic::ProcessorReportedDuplicate]
    );
    assert_eq!(
        outcome.response_code().map(GatewayDiagnostic::expose),
        Some("430")
    );
}

#[test]
fn every_payment_anomaly_crosses_the_provider_neutral_boundary() {
    let mappings = [
        (
            PaymentOutcomeDiagnostic::MissingTransactionIdentifier,
            GatewayPaymentDiagnostic::MissingTransactionIdentifier,
        ),
        (
            PaymentOutcomeDiagnostic::MissingCustomerVaultIdentifier,
            GatewayPaymentDiagnostic::MissingPaymentMethodReference,
        ),
        (
            PaymentOutcomeDiagnostic::InvalidOrConflictingTransactionIdentifier,
            GatewayPaymentDiagnostic::InvalidOrConflictingTransactionIdentifier,
        ),
        (
            PaymentOutcomeDiagnostic::InvalidOrConflictingCustomerVaultIdentifier,
            GatewayPaymentDiagnostic::InvalidOrConflictingPaymentMethodReference,
        ),
        (
            PaymentOutcomeDiagnostic::InvalidOrConflictingDecisionField,
            GatewayPaymentDiagnostic::InvalidOrConflictingDecisionField,
        ),
        (
            PaymentOutcomeDiagnostic::IndeterminatePaymentOutcome,
            GatewayPaymentDiagnostic::IndeterminatePaymentOutcome,
        ),
        (
            PaymentOutcomeDiagnostic::DuplicateTransactionAtProcessor,
            GatewayPaymentDiagnostic::ProcessorReportedDuplicate,
        ),
        (
            PaymentOutcomeDiagnostic::ConflictingDecisionEvidence,
            GatewayPaymentDiagnostic::ConflictingDecisionEvidence,
        ),
        (
            PaymentOutcomeDiagnostic::UnrecognizedDecisionEvidence,
            GatewayPaymentDiagnostic::UnrecognizedDecisionEvidence,
        ),
        (
            PaymentOutcomeDiagnostic::MissingDecisionEvidence,
            GatewayPaymentDiagnostic::MissingDecisionEvidence,
        ),
    ];

    for (source, expected) in mappings {
        assert_eq!(map_payment_diagnostic(&source), expected);
    }

    for source in PaymentOutcomeDiagnostic::ALL {
        assert_ne!(
            map_payment_diagnostic(source),
            GatewayPaymentDiagnostic::UnmappedProviderDiagnostic
        );
    }
}

#[tokio::test]
async fn parsed_processor_duplicate_reaches_the_gateway_boundary() {
    let (gateway, server) = gateway_with_response(
        r#"{"response":"3","response_code":"430","response_text":"Duplicate transaction"}"#,
    )
    .await;
    let attempt_id: PaymentAttemptId = "00000000-0000-0000-0000-000000000430".parse().unwrap();
    let request = GatewaySaleRequest::new(
        ChargeAmount::new(100, CurrencyCode::new("USD").unwrap()).unwrap(),
        GatewayOrderId::from_generated_attempt(
            "ck_order_00000000000000000000000000000430",
            attempt_id,
        )
        .unwrap(),
        GatewaySaleIntent::OneTime {
            payment_token: PaymentToken::new("tok_duplicate").unwrap(),
        },
        None,
    );

    let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), gateway.sale(request))
        .await
        .expect("test sale should not hang")
        .expect("430 should be an outcome");

    assert_eq!(outcome.status(), GatewayPaymentStatus::Unknown);
    assert_eq!(
        outcome.diagnostics(),
        &[GatewayPaymentDiagnostic::ProcessorReportedDuplicate]
    );
    assert_eq!(
        outcome.response_code().map(GatewayDiagnostic::expose),
        Some("430")
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), server)
        .await
        .expect("test server should not hang")
        .expect("test server assertions should pass");
}

#[tokio::test]
async fn parsed_processor_error_without_identity_remains_reconcilable() {
    let (gateway, server) = gateway_with_response(
        r#"{"response":"3","response_code":"400","response_text":"Processor error","id":""}"#,
    )
    .await;
    let attempt_id: PaymentAttemptId = "00000000-0000-0000-0000-000000000400".parse().unwrap();
    let request = GatewaySaleRequest::new(
        ChargeAmount::new(100, CurrencyCode::new("USD").unwrap()).unwrap(),
        GatewayOrderId::from_generated_attempt(
            "ck_order_00000000000000000000000000000400",
            attempt_id,
        )
        .unwrap(),
        GatewaySaleIntent::OneTime {
            payment_token: PaymentToken::new("tok_processor_error").unwrap(),
        },
        None,
    );

    let outcome = gateway
        .sale(request)
        .await
        .expect("processor error should remain an outcome");

    assert_eq!(outcome.status(), GatewayPaymentStatus::Unknown);
    assert!(outcome.transaction_id().is_none());
    assert_eq!(
        outcome.diagnostics(),
        &[GatewayPaymentDiagnostic::IndeterminatePaymentOutcome]
    );
    server.await.expect("test server assertions should pass");
}

#[tokio::test]
async fn parsed_terminal_failure_with_empty_identity_stays_terminal() {
    let (gateway, server) = gateway_with_response(
        r#"{"response":"3","response_code":"300","response_text":"Transaction was rejected by gateway.","id":""}"#,
    )
    .await;
    let attempt_id: PaymentAttemptId = "00000000-0000-0000-0000-000000000300".parse().unwrap();
    let request = GatewaySaleRequest::new(
        ChargeAmount::new(100, CurrencyCode::new("USD").unwrap()).unwrap(),
        GatewayOrderId::from_generated_attempt(
            "ck_order_00000000000000000000000000000300",
            attempt_id,
        )
        .unwrap(),
        GatewaySaleIntent::OneTime {
            payment_token: PaymentToken::new("tok_terminal_failure").unwrap(),
        },
        None,
    );

    let outcome = gateway
        .sale(request)
        .await
        .expect("300 should be an outcome");

    assert_eq!(outcome.status(), GatewayPaymentStatus::Failed);
    assert!(outcome.transaction_id().is_none());
    assert!(outcome.diagnostics().is_empty());
    server.await.expect("test server assertions should pass");
}

#[tokio::test]
async fn parsed_conflicting_terminal_evidence_remains_reconcilable() {
    let (gateway, server) = gateway_with_response(
        r#"{"response":"3","response_code":"300","status":"approved","id":""}"#,
    )
    .await;
    let attempt_id: PaymentAttemptId = "00000000-0000-0000-0000-000000000301".parse().unwrap();
    let request = GatewaySaleRequest::new(
        ChargeAmount::new(100, CurrencyCode::new("USD").unwrap()).unwrap(),
        GatewayOrderId::from_generated_attempt(
            "ck_order_00000000000000000000000000000301",
            attempt_id,
        )
        .unwrap(),
        GatewaySaleIntent::OneTime {
            payment_token: PaymentToken::new("tok_conflicting_failure").unwrap(),
        },
        None,
    );

    let outcome = gateway
        .sale(request)
        .await
        .expect("conflicting evidence should be an outcome");

    assert_eq!(outcome.status(), GatewayPaymentStatus::Unknown);
    assert_eq!(
        outcome.diagnostics(),
        &[
            GatewayPaymentDiagnostic::IndeterminatePaymentOutcome,
            GatewayPaymentDiagnostic::ConflictingDecisionEvidence,
        ]
    );
    server.await.expect("test server assertions should pass");
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
        order_id: Some(text(value)),
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

#[tokio::test]
async fn numeric_approval_aliases_survive_conflicting_payment_decisions() {
    for body in [
        r#"{"response":"2","response_code":"100","id":"txn_alias"}"#,
        r#"{"response":"2","response_code":"0100","id":"txn_alias"}"#,
        r#"{"response":"2","response_code":"+0100","id":"txn_alias"}"#,
    ] {
        let (gateway, server) = gateway_with_response(body).await;
        let request = GatewaySaleRequest::new(
            ChargeAmount::new(100, CurrencyCode::new("USD").unwrap()).unwrap(),
            GatewayOrderId::from_correlation("approval-alias").unwrap(),
            GatewaySaleIntent::OneTime {
                payment_token: PaymentToken::new("tok_alias").unwrap(),
            },
            None,
        );
        let outcome = gateway.sale(request).await.unwrap();
        assert_eq!(outcome.status(), GatewayPaymentStatus::Unknown);
        assert_eq!(
            outcome.evidence().approval_evidence(),
            syrup_rail::ProcessorApprovalEvidence::Structured
        );
        assert!(outcome.evidence().indicates_approved_payment());
        assert!(outcome.evidence().may_indicate_approval());
        assert!(body.contains(outcome.response_code().unwrap().expose()));
        server.await.unwrap();
    }
}

#[tokio::test]
async fn lifecycle_condition_alone_supplies_structured_approval_evidence() {
    let body = r#"{"response":"2","condition":"complete","id":"txn_condition"}"#;
    let (gateway, server) = gateway_with_response(body).await;
    let outcome = gateway.sale(approval_signal_sale_request()).await.unwrap();
    assert_eq!(outcome.status(), GatewayPaymentStatus::Unknown);
    assert_eq!(
        outcome.evidence().approval_evidence(),
        syrup_rail::ProcessorApprovalEvidence::Structured
    );
    assert!(outcome.evidence().indicates_approved_payment());
    server.await.unwrap();
}

#[tokio::test]
async fn approval_text_is_a_review_hint_and_never_a_payment_decision() {
    for (body, expected) in [
        (
            r#"{"response":"3","response_text":"Approved by processor","id":"txn_text_hint"}"#,
            syrup_rail::ProcessorApprovalEvidence::TextOnly,
        ),
        (
            r#"{"response":"3","response_text":"not-approved","id":"txn_text_hint"}"#,
            syrup_rail::ProcessorApprovalEvidence::TextOnly,
        ),
        (
            r#"{"response":"3","response_text":"Not approved","id":"txn_text_hint"}"#,
            syrup_rail::ProcessorApprovalEvidence::TextOnly,
        ),
        (
            r#"{"response":"3","response_text":"Processor error","id":"txn_text_hint"}"#,
            syrup_rail::ProcessorApprovalEvidence::Unclassified,
        ),
    ] {
        let (gateway, server) = gateway_with_response(body).await;
        let outcome = gateway.sale(approval_signal_sale_request()).await.unwrap();
        assert_eq!(outcome.evidence().approval_evidence(), expected);
        assert_eq!(outcome.status(), GatewayPaymentStatus::Unknown);
        assert!(!outcome.evidence().indicates_approved_payment());
        server.await.unwrap();
    }
}

fn approval_signal_sale_request() -> GatewaySaleRequest {
    GatewaySaleRequest::new(
        ChargeAmount::new(100, CurrencyCode::new("USD").unwrap()).unwrap(),
        GatewayOrderId::from_correlation("approval-signal").unwrap(),
        GatewaySaleIntent::OneTime {
            payment_token: PaymentToken::new("tok_signal").unwrap(),
        },
        None,
    )
}

#[tokio::test]
async fn discarded_and_status_only_decisions_retain_approval_signals() {
    use syrup_rail::ProcessorApprovalEvidence as Signal;
    for (body, expected) in [
        (
            r#"{"status":"approved","condition":"declined","id":"txn_signal"}"#,
            Signal::Structured,
        ),
        (
            r#"{"response":"1","response":"2","id":"txn_signal"}"#,
            Signal::Structured,
        ),
        (
            r#"{"response":"2","response":"1","id":"txn_signal"}"#,
            Signal::Structured,
        ),
        (
            r#"{"status":"approved","id":"invalid identity"}"#,
            Signal::Structured,
        ),
        (r#"{"response":"3"}"#, Signal::Unclassified),
        (r#"{"condition":"error"}"#, Signal::Unclassified),
        (r#"{"status":"pending"}"#, Signal::Unclassified),
        (r#"{"response_code":"400"}"#, Signal::Unclassified),
        (r#"{"response_code":"420"}"#, Signal::Unclassified),
        (r#"{"response_code":"430"}"#, Signal::Unclassified),
        (r#"{"response":{},"id":null}"#, Signal::Unclassified),
        (
            r#"{"response_code":"provider-extension","id":null}"#,
            Signal::Unclassified,
        ),
        (
            r#"{"response_text":"Approved","response_text":"Processor error","id":null}"#,
            Signal::TextOnly,
        ),
    ] {
        let (gateway, server) = gateway_with_response(body).await;
        let outcome = gateway.sale(approval_signal_sale_request()).await.unwrap();
        assert_eq!(outcome.status(), GatewayPaymentStatus::Unknown, "{body}");
        assert_eq!(outcome.evidence().approval_evidence(), expected, "{body}");
        assert!(outcome.evidence().may_indicate_approval(), "{body}");
        server.await.unwrap();
    }
}
