use syrup_rail::{
    GatewayDiagnosticOperation, GatewayDiagnosticsCompleteness,
    GatewayDiagnosticsUnavailableReason as Reason, GatewayTransactionDiagnostics,
    GatewayTransactionDiagnosticsRequest, Money,
};

use super::*;

fn request(
    operation: GatewayDiagnosticOperation,
    cents: i32,
    order_id: Option<&str>,
) -> GatewayTransactionDiagnosticsRequest {
    GatewayTransactionDiagnosticsRequest::new(
        GatewayTransactionId::new("txn_1").unwrap(),
        operation,
        Money::new(cents, CurrencyCode::new("USD").unwrap()).unwrap(),
        order_id.map(|value| GatewayOrderId::from_correlation(value).unwrap()),
    )
}

fn sale(cents: i32) -> GatewayTransactionDiagnosticsRequest {
    request(GatewayDiagnosticOperation::Sale, cents, Some("ck_order_1"))
}

fn transaction(body: &str) -> String {
    format!(
        "<nm_response><transaction><transaction_id>txn_1</transaction_id>\
         <order_id>ck_order_1</order_id><currency>USD</currency>{body}</transaction>\
         </nm_response>"
    )
}

fn action(kind: &str, amount: &str, code: &str) -> String {
    format!(
        "<action><amount>{amount}</amount><action_type>{kind}</action_type>\
         <response_code>{code}</response_code></action>"
    )
}

async fn diagnose(
    body: String,
    request: GatewayTransactionDiagnosticsRequest,
) -> Result<GatewayTransactionDiagnostics, GatewayError> {
    let (gateway, server) = gateway_with_response(Box::leak(body.into_boxed_str())).await;
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        gateway.query_transaction_diagnostics(request),
    )
    .await
    .expect("diagnostic query should not hang");
    tokio::time::timeout(std::time::Duration::from_secs(5), server)
        .await
        .expect("test server should not hang")
        .expect("test server assertions should pass");
    result
}

#[tokio::test]
async fn selects_only_the_original_sale_after_settlement_and_refund() {
    let body = transaction(&format!(
        "<avs_response>0</avs_response><csc_response></csc_response>\
         <action><amount>10.00</amount><action_type>sale</action_type><success>0</success>\
         <response_code>253</response_code><response_text>Fraud Suspected</response_text>\
         <processor_response_code>59</processor_response_code>\
         <processor_response_text>Suspected fraud</processor_response_text></action>\
         {}{}",
        action("settle", "10.00", "100"),
        action("refund", "10.00", "100"),
    ));
    let GatewayTransactionDiagnostics::Observed(observation) =
        diagnose(body, sale(1_000)).await.unwrap()
    else {
        panic!("the sale should be observed");
    };
    let code = |value: Option<&GatewayDiagnostic>| value.map(|value| value.expose().to_owned());
    assert_eq!(observation.operation(), GatewayDiagnosticOperation::Sale);
    assert_eq!(observation.source().as_str(), "nmi_query_api");
    assert_eq!(
        code(observation.gateway_response_code()).as_deref(),
        Some("253")
    );
    assert_eq!(
        code(observation.gateway_response_text()).as_deref(),
        Some("Fraud Suspected")
    );
    assert_eq!(
        code(observation.processor_response_code()).as_deref(),
        Some("59")
    );
    assert_eq!(
        code(observation.processor_response_text()).as_deref(),
        Some("Suspected fraud")
    );
    assert_eq!(code(observation.avs_response()).as_deref(), Some("0"));
    assert_eq!(observation.csc_response(), None);
    assert_eq!(
        observation.completeness(),
        GatewayDiagnosticsCompleteness::Partial,
        "a missing CSC result is partial, never a match or mismatch"
    );
}

#[tokio::test]
async fn validate_selects_the_zero_amount_verification() {
    let body = transaction(&format!(
        "<avs_response>Y</avs_response><csc_response>M</csc_response>{}{}",
        action("validate", "0.00", "100"),
        action("sale", "0.00", "300"),
    ));
    let GatewayTransactionDiagnostics::Observed(observation) = diagnose(
        body,
        request(GatewayDiagnosticOperation::Validate, 0, Some("ck_order_1")),
    )
    .await
    .unwrap() else {
        panic!("the validation should be observed");
    };
    assert_eq!(
        observation.operation(),
        GatewayDiagnosticOperation::Validate
    );
    assert_eq!(
        observation
            .gateway_response_code()
            .map(GatewayDiagnostic::expose),
        Some("100")
    );
    assert_eq!(
        observation.csc_response().map(GatewayDiagnostic::expose),
        Some("M")
    );
}

#[tokio::test]
async fn ambiguous_or_unbound_responses_are_unavailable() {
    let one_sale = action("sale", "10.00", "200");
    let cases = [
        (
            "wrong transaction",
            transaction(&one_sale).replace("txn_1", "txn_other"),
            Reason::TransactionMismatch,
        ),
        (
            "missing transaction ID",
            transaction(&one_sale).replace("<transaction_id>txn_1</transaction_id>", ""),
            Reason::TransactionMismatch,
        ),
        (
            "two transactions",
            format!(
                "<nm_response><transaction><transaction_id>txn_1</transaction_id>{one_sale}\
                 </transaction><transaction><transaction_id>txn_1</transaction_id>{one_sale}\
                 </transaction></nm_response>"
            ),
            Reason::MultipleTransactions,
        ),
        (
            "order mismatch",
            transaction(&one_sale).replace("ck_order_1", "ck_order_2"),
            Reason::OrderMismatch,
        ),
        (
            "currency mismatch",
            transaction(&one_sale).replace("<currency>USD</currency>", "<currency>EUR</currency>"),
            Reason::CurrencyMismatch,
        ),
        (
            "two matching sales",
            transaction(&format!("{one_sale}{one_sale}")),
            Reason::AmbiguousAction,
        ),
        (
            "settlement and refund only",
            transaction(&format!(
                "{}{}",
                action("settle", "10.00", "100"),
                action("refund", "10.00", "100")
            )),
            Reason::NoMatchingAction,
        ),
        (
            "different sale amount",
            transaction(&action("sale", "12.00", "100")),
            Reason::NoMatchingAction,
        ),
        (
            "conflicting action type",
            transaction(
                "<action><amount>10.00</amount><action_type>sale</action_type>\
                 <action_type>refund</action_type></action>",
            ),
            Reason::MalformedResponse,
        ),
    ];
    for (case, body, reason) in cases {
        assert_eq!(
            diagnose(body, sale(1_000)).await.unwrap(),
            GatewayTransactionDiagnostics::Unavailable(reason),
            "{case}"
        );
    }

    // A response without an order ID is still bound by its transaction ID.
    let without_order = transaction(&one_sale).replace("<order_id>ck_order_1</order_id>", "");
    assert!(matches!(
        diagnose(without_order, sale(1_000)).await.unwrap(),
        GatewayTransactionDiagnostics::Observed(_)
    ));
    assert_eq!(
        diagnose("<nm_response></nm_response>".to_owned(), sale(1_000))
            .await
            .unwrap(),
        GatewayTransactionDiagnostics::NotFound
    );
    assert!(matches!(
        diagnose(
            "<nm_response><error_response>Invalid query</error_response></nm_response>".to_owned(),
            sale(1_000)
        )
        .await,
        Err(GatewayError::RequestRejected(_))
    ));
}

#[tokio::test]
async fn oversized_codes_are_omitted_and_hostile_text_is_sanitized() {
    let body = transaction(&format!(
        "<avs_response>{}</avs_response><csc_response>N</csc_response>\
         <action><amount>10.00</amount><action_type>sale</action_type>\
         <response_code>200</response_code>\
         <response_text>card 4111 1111 1111 1111 declined</response_text>\
         <processor_response_code>05</processor_response_code>\
         <processor_response_text>{}</processor_response_text></action>",
        "A".repeat(65),
        "x".repeat(2_000),
    ));
    let GatewayTransactionDiagnostics::Observed(observation) =
        diagnose(body, sale(1_000)).await.unwrap()
    else {
        panic!("the sale should be observed");
    };
    assert_eq!(observation.avs_response(), None);
    assert_eq!(
        observation.completeness(),
        GatewayDiagnosticsCompleteness::Partial
    );
    let text = observation.gateway_response_text().unwrap().expose();
    assert!(!text.contains("4111"), "{text}");
    assert!(
        observation
            .processor_response_text()
            .unwrap()
            .expose()
            .len()
            <= syrup_rail::MAX_GATEWAY_TEXT_BYTES
    );
}

#[tokio::test]
async fn padded_codes_are_never_shortened_into_a_verification_result() {
    let body = transaction(&format!(
        "<avs_response>Y{}N</avs_response><csc_response>M</csc_response>\
         <action><amount>10.00</amount><action_type>sale</action_type>\
         <response_code>100</response_code><response_text>Approved</response_text>\
         <processor_response_code>00</processor_response_code>\
         <processor_response_text>Approved</processor_response_text></action>",
        " ".repeat(600)
    ));
    let GatewayTransactionDiagnostics::Observed(observation) =
        diagnose(body, sale(1_000)).await.unwrap()
    else {
        panic!("the sale should be observed");
    };
    assert_eq!(observation.avs_response(), None);
    assert_eq!(
        observation.completeness(),
        GatewayDiagnosticsCompleteness::Partial
    );
}

#[tokio::test]
async fn separator_padded_action_types_are_not_selected_as_sales() {
    let body = transaction(&action(&format!("s{}ale", "-".repeat(64)), "10.00", "100"));
    assert_eq!(
        diagnose(body, sale(1_000)).await.unwrap(),
        GatewayTransactionDiagnostics::Unavailable(Reason::MalformedResponse)
    );
}
