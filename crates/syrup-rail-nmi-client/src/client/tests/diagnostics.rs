use super::super::response::xml::query_diagnostics_from_xml;
use super::*;
use crate::{
    TransactionDiagnosticsLookup, TransactionDiagnosticsParts, TransactionDiagnosticsQuery,
};

const DECLINED_RENEWAL: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<nm_response>
  <transaction>
    <transaction_id>txn_renewal_1</transaction_id>
    <order_id>ck_renewal_order</order_id>
    <currency>USD</currency>
    <first_name>Jordan</first_name>
    <address_1>1 Secret St</address_1>
    <email>secret@example.test</email>
    <cc_number>4xxxxxxxxxxx1111</cc_number>
    <avs_response>0</avs_response>
    <csc_response></csc_response>
    <action>
      <amount>10.00</amount>
      <action_type>sale</action_type>
      <success>0</success>
      <response_code>253</response_code>
      <response_text>Fraud Suspected</response_text>
      <processor_response_code>59</processor_response_code>
      <processor_response_text>Suspected fraud</processor_response_text>
    </action>
    <action>
      <amount>10.00</amount>
      <action_type>settle</action_type>
      <success>1</success>
      <response_code>100</response_code>
    </action>
  </transaction>
</nm_response>"#;

fn found(text: &str) -> TransactionDiagnosticsParts {
    match query_diagnostics_from_xml(text).expect("diagnostic fixture should parse") {
        TransactionDiagnosticsLookup::Found(transaction) => transaction.into_parts(),
        other => panic!("expected one transaction, got {other:?}"),
    }
}

fn exposed(value: &Option<SensitiveText>) -> Option<&str> {
    value.as_ref().map(SensitiveText::expose)
}

#[test]
fn exact_diagnostics_keep_gateway_processor_and_verification_fields_separate() {
    let parts = found(DECLINED_RENEWAL);
    assert!(!parts.malformed);
    assert!(!parts.incomplete);
    assert_eq!(exposed(&parts.transaction_id), Some("txn_renewal_1"));
    assert_eq!(exposed(&parts.order_id), Some("ck_renewal_order"));
    assert_eq!(exposed(&parts.currency), Some("USD"));
    assert_eq!(exposed(&parts.avs_response), Some("0"));
    assert_eq!(exposed(&parts.csc_response), None, "an empty CSC is absent");
    assert_eq!(parts.actions.len(), 2);
    let sale = &parts.actions[0];
    assert_eq!(exposed(&sale.action_type), Some("sale"));
    assert_eq!(exposed(&sale.amount), Some("10.00"));
    assert_eq!(exposed(&sale.response_code), Some("253"));
    assert_eq!(exposed(&sale.response_text), Some("Fraud Suspected"));
    assert_eq!(exposed(&sale.processor_response_code), Some("59"));
    assert_eq!(
        exposed(&sale.processor_response_text),
        Some("Suspected fraud")
    );
    assert!(!sale.incomplete);
    let settle = &parts.actions[1];
    assert_eq!(exposed(&settle.action_type), Some("settle"));
    assert_eq!(exposed(&settle.processor_response_code), None);

    // Names, addresses, email, and card fields in the response never surface,
    // even through formatting.
    let debug = format!("{parts:?}");
    for value in ["Jordan", "Secret", "secret@example.test", "1111", "Fraud"] {
        assert!(!debug.contains(value), "{value} leaked into {debug}");
    }
}

#[test]
fn transaction_count_is_reported_without_selecting_by_recency() {
    assert!(matches!(
        query_diagnostics_from_xml("<nm_response></nm_response>").unwrap(),
        TransactionDiagnosticsLookup::NotFound
    ));
    let two = "<nm_response><transaction><transaction_id>a</transaction_id></transaction>\
               <transaction><transaction_id>b</transaction_id></transaction></nm_response>";
    assert!(matches!(
        query_diagnostics_from_xml(two).unwrap(),
        TransactionDiagnosticsLookup::MultipleTransactions
    ));
    assert!(matches!(
        query_diagnostics_from_xml(
            "<nm_response><error_response>Invalid query</error_response></nm_response>"
        ),
        Err(WireError::RequestRejected(_))
    ));
    assert!(matches!(
        query_diagnostics_from_xml("<nm_response><transaction>"),
        Err(WireError::MalformedResponse(_))
    ));
}

#[test]
fn conflicting_selection_fields_make_the_transaction_malformed() {
    for body in [
        // Conflicting transaction identifiers.
        "<transaction_id>a</transaction_id><transaction_id>b</transaction_id>\
         <action><action_type>sale</action_type><amount>1.00</amount></action>",
        // Conflicting action types.
        "<transaction_id>a</transaction_id>\
         <action><action_type>sale</action_type><action_type>refund</action_type>\
         <amount>1.00</amount></action>",
        // Conflicting amounts.
        "<transaction_id>a</transaction_id>\
         <action><action_type>sale</action_type><amount>1.00</amount><amount>2.00</amount></action>",
        // Nested action structure.
        "<transaction_id>a</transaction_id>\
         <action><action><action_type>sale</action_type></action></action>",
        // Conflicting currencies.
        "<transaction_id>a</transaction_id><currency>USD</currency><currency>EUR</currency>\
         <action><action_type>sale</action_type><amount>1.00</amount></action>",
    ] {
        let parts = found(&format!(
            "<nm_response><transaction>{body}</transaction></nm_response>"
        ));
        assert!(parts.malformed, "{body}");
        assert!(parts.actions.is_empty(), "no action may escape: {body}");
    }
}

#[test]
fn unusable_optional_fields_are_omitted_and_bounded() {
    let long_text = "x".repeat(2_000);
    let parts = found(&format!(
        "<nm_response><transaction><transaction_id>a</transaction_id>\
         <avs_response>Y</avs_response><avs_response>N</avs_response>\
         <csc_response><nested>M</nested></csc_response>\
         <action><action_type>sale</action_type><amount>$001.5</amount>\
         <response_code>200</response_code><response_code>300</response_code>\
         <response_text>{long_text}</response_text></action>\
         <action><action_type>refund</action_type><amount>1.50</amount></action>\
         </transaction></nm_response>"
    ));
    assert!(!parts.malformed);
    assert!(
        parts.incomplete,
        "conflicting AVS and nested CSC are unusable"
    );
    assert_eq!(parts.avs_response.as_ref().map(SensitiveText::expose), None);
    assert_eq!(parts.csc_response.as_ref().map(SensitiveText::expose), None);
    let sale = &parts.actions[0];
    assert_eq!(
        sale.amount.as_ref().map(SensitiveText::expose),
        Some("1.50")
    );
    assert!(sale.incomplete);
    assert_eq!(sale.response_code.as_ref().map(SensitiveText::expose), None);
    let text = sale.response_text.as_ref().unwrap().expose();
    assert!(text.chars().count() <= MAX_NMI_FIELD_CHARS);
    assert!(!parts.actions[1].incomplete, "incompleteness is per action");
}

#[tokio::test]
async fn diagnostic_queries_select_only_by_transaction_id() {
    let (client, captured, server) = spawn_capturing_server(
        "HTTP/1.1 200 OK",
        "text/xml",
        DECLINED_RENEWAL.as_bytes().to_vec(),
    )
    .await;
    let lookup = client
        .query_transaction_diagnostics(TransactionDiagnosticsQuery {
            transaction_id: " txn_renewal_1 ".to_owned(),
        })
        .await
        .expect("diagnostic query should parse");
    assert!(matches!(lookup, TransactionDiagnosticsLookup::Found(_)));
    let request = captured.await.expect("request captured");
    let (head, body) = request.split_once("\r\n\r\n").expect("request body");
    assert!(head.starts_with("POST /api/query.php "));
    let fields: std::collections::HashMap<_, _> = form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect();
    assert_eq!(
        fields.get("transaction_id").map(String::as_str),
        Some("txn_renewal_1")
    );
    assert_eq!(
        fields.get("security_key").map(String::as_str),
        Some("query_key")
    );
    assert!(!fields.contains_key("order_id"));
    assert_eq!(fields.len(), 2);
    server.await.expect("server finished");

    let (client, mut request_receiver, server) =
        spawn_capturing_server("HTTP/1.1 200 OK", "text/xml", b"unexpected".to_vec()).await;
    for transaction_id in ["  ".to_owned(), "t".repeat(MAX_NMI_IDENTIFIER_BYTES + 1)] {
        assert!(matches!(
            client
                .query_transaction_diagnostics(TransactionDiagnosticsQuery { transaction_id })
                .await,
            Err(QueryError::InvalidRequest(_))
        ));
    }
    assert!(request_receiver.try_recv().is_err());
    server.abort();
    assert!(
        !format!(
            "{:?}",
            TransactionDiagnosticsQuery {
                transaction_id: "txn-secret".to_owned()
            }
        )
        .contains("txn-secret")
    );
}

#[test]
fn codes_are_bounded_before_any_truncation() {
    let padded = format!("Y{}N", " ".repeat(600));
    let at_limit = "7".repeat(MAX_NMI_DIAGNOSTIC_CODE_BYTES);
    let over_limit = "7".repeat(MAX_NMI_DIAGNOSTIC_CODE_BYTES + 1);
    let parts = found(&format!(
        "<nm_response><transaction><transaction_id>a</transaction_id>\
         <avs_response>{padded}</avs_response><csc_response>{at_limit}</csc_response>\
         <action><action_type>sale</action_type><amount>1.00</amount>\
         <response_code>{at_limit}</response_code>\
         <processor_response_code>{over_limit}</processor_response_code></action>\
         </transaction></nm_response>"
    ));
    assert_eq!(
        exposed(&parts.avs_response),
        None,
        "a padded code is not shortened to Y"
    );
    assert!(parts.incomplete);
    assert_eq!(exposed(&parts.csc_response), Some(at_limit.as_str()));
    let sale = &parts.actions[0];
    assert_eq!(exposed(&sale.response_code), Some(at_limit.as_str()));
    assert_eq!(exposed(&sale.processor_response_code), None);
    assert!(sale.incomplete);
}

#[test]
fn oversized_action_types_cannot_normalize_into_a_sale() {
    for action_type in [
        format!("s{}ale", "-".repeat(MAX_NMI_DIAGNOSTIC_CODE_BYTES)),
        "sale".repeat(MAX_NMI_DIAGNOSTIC_CODE_BYTES),
    ] {
        let parts = found(&format!(
            "<nm_response><transaction><transaction_id>a</transaction_id>\
             <action><action_type>{action_type}</action_type><amount>1.00</amount></action>\
             </transaction></nm_response>"
        ));
        assert!(parts.malformed, "{action_type}");
        assert!(parts.actions.is_empty());
    }
    let parts = found(
        "<nm_response><transaction><transaction_id>a</transaction_id>\
         <action><action_type> Sale </action_type><amount>1.00</amount></action>\
         </transaction></nm_response>",
    );
    assert!(!parts.malformed);
    assert_eq!(exposed(&parts.actions[0].action_type), Some("sale"));
}
