use super::*;
use crate::{TransactionReceiptParts, TransactionReceiptQuery};

mod evidence;
mod evidence_transport;

fn transaction() -> String {
    "<transaction><transaction_id>txn_receipt</transaction_id><order_id>order_receipt</order_id><currency>USD</currency><condition>complete</condition><cc_number>411111******1234</cc_number><action><action_type>sale</action_type><amount>1.00</amount><success>1</success><response_code>100</response_code><date>20260919100200</date></action></transaction>".to_owned()
}

fn parse(body: &str) -> Result<Option<crate::TransactionReceipt>, WireError> {
    super::super::response::xml::receipt::parse(
        body,
        &TransactionQuery {
            transaction_id: Some("txn_receipt".to_owned()),
            order_id: Some("order_receipt".to_owned()),
        },
    )
}

fn found(transaction: &str) -> TransactionReceiptParts {
    parse(&format!("<nm_response>{transaction}</nm_response>"))
        .unwrap()
        .unwrap()
        .into_parts()
}

#[test]
fn original_action_and_card_survive_later_lifecycle_states_without_exposing_values() {
    for condition in [
        "complete",
        "refunded",
        "canceled",
        "voided",
        "pendingsettlement",
    ] {
        let text = transaction().replace("complete", condition).replace("</transaction>",
            "<action><action_type>refund</action_type><date>20261001100000</date><amount>1.00</amount><success>1</success></action></transaction>");
        let parts = found(&text);
        assert_eq!(parts.currency.expose(), "USD");
        assert_eq!(parts.card_last4.as_ref().unwrap().expose(), "1234");
        assert_eq!(parts.actions.len(), 2);
        let debug = format!("{parts:?}");
        for secret in ["txn_receipt", "order_receipt", "1234", "20260919100200"] {
            assert!(!debug.contains(secret));
        }
        let action = parts.actions.into_iter().next().unwrap().into_parts();
        assert_eq!(action.date.unwrap().expose(), "20260919100200");
        assert_eq!(action.response_code.unwrap().expose(), "100");
    }
}

#[test]
fn conflicting_fields_are_never_downgraded_to_missing() {
    for (field, original, conflicting) in [
        ("transaction_id", "txn_receipt", "other"),
        ("order_id", "order_receipt", "other"),
        ("currency", "USD", "EUR"),
        ("cc_number", "411111******1234", "411111******5678"),
        ("action_type", "sale", "refund"),
        ("amount", "1.00", "2.00"),
        ("success", "1", "0"),
        ("response_code", "100", "200"),
        ("date", "20260919100200", "20260919100300"),
    ] {
        for (first, second) in [(original, conflicting), (conflicting, original)] {
            let body = transaction().replace(
                &format!("<{field}>{original}</{field}>"),
                &format!("<{field}>{first}</{field}><{field}>{second}</{field}>"),
            );
            assert!(
                parse(&format!("<nm_response>{body}</nm_response>")).is_err(),
                "{field}"
            );
        }
        for (first, second) in [(original, ""), ("", original)] {
            let body = transaction().replace(
                &format!("<{field}>{original}</{field}>"),
                &format!("<{field}>{first}</{field}><{field}>{second}</{field}>"),
            );
            assert!(parse(&format!("<nm_response>{body}</nm_response>")).is_err());
        }
        // Identical echoes remain usable.
        found(&transaction().replace(
            &format!("</{field}>"),
            &format!("</{field}><{field}>{original}</{field}>"),
        ));
    }
}

#[test]
fn card_requires_a_valid_mask_and_four_visible_trailing_digits() {
    for valid in ["1234", "4xxxxxxxxxxx1234", "411111******1234", "####1234"] {
        assert_eq!(
            found(&transaction().replace("411111******1234", valid))
                .card_last4
                .unwrap()
                .expose(),
            "1234"
        );
    }
    for invalid in [
        "411111******",
        "1234****",
        "garbage 1234",
        "411111******1234x",
        "4111111111111234",
        "é*1234",
        "***123",
    ] {
        let body = transaction().replace("411111******1234", invalid);
        assert!(
            parse(&format!("<nm_response>{body}</nm_response>")).is_err(),
            "{invalid}"
        );
    }
    assert!(
        found(&transaction().replace("<cc_number>411111******1234</cc_number>", ""))
            .card_last4
            .is_none()
    );
    assert!(
        found(&transaction().replace("411111******1234", ""))
            .card_last4
            .is_none()
    );
    // Truncation at 512 characters would invent the wrong visible suffix.
    let oversized = format!("{}1234****5678", "*".repeat(MAX_NMI_FIELD_CHARS - 4));
    let body = transaction().replace("411111******1234", &oversized);
    assert!(parse(&format!("<nm_response>{body}</nm_response>")).is_err());
}

#[test]
fn exact_identity_structure_currency_and_work_bounds_are_enforced() {
    let base = transaction();
    for invalid in [
        base.replace("txn_receipt", "wrong"),
        base.replace("order_receipt", "wrong"),
        base.replace("<order_id>order_receipt</order_id>", ""),
        base.replace("<currency>USD</currency>", ""),
        base.replace("USD", "US"),
        base.replace(
            "<amount>1.00</amount>",
            "<amount><nested>1.00</nested></amount>",
        ),
        base.replace("<action>", "<wrapper><action>")
            .replace("</action>", "</action></wrapper>"),
        format!("{base}{base}"),
        format!("<wrapper>{base}</wrapper>"),
        base.replace(
            "</transaction>",
            &format!("{}</transaction>", "<action/>".repeat(100)),
        ),
        base.replace("USD", &"x".repeat(MAX_NMI_FIELD_CHARS + 1)),
    ] {
        assert!(parse(&format!("<nm_response>{invalid}</nm_response>")).is_err());
    }
    assert!(parse("<nm_response/>").unwrap().is_none());
    assert!(parse(&"x".repeat(MAX_NMI_STANDARD_RESPONSE_BYTES + 1)).is_err());
    assert_eq!(
        found(&base.replace("USD", "EUR")).currency.expose(),
        "EUR",
        "host compares the returned currency"
    );
}

#[tokio::test]
async fn receipt_transport_binds_both_selectors_without_report_filters() {
    let (client, requests, server) = spawn_capturing_server(
        "HTTP/1.1 200 OK",
        "text/xml",
        format!("<nm_response>{}</nm_response>", transaction()).into_bytes(),
    )
    .await;
    client
        .query_transaction_receipt(TransactionReceiptQuery {
            transaction_id: "txn_receipt".to_owned(),
            order_id: "order_receipt".to_owned(),
        })
        .await
        .unwrap()
        .unwrap();
    let request = requests.await.unwrap();
    assert!(request.starts_with("POST /api/query.php"));
    let (_, body) = request.split_once("\r\n\r\n").unwrap();
    let fields: std::collections::HashMap<_, _> = form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect();
    assert_eq!(fields.len(), 3);
    assert_eq!(fields["transaction_id"], "txn_receipt");
    assert_eq!(fields["order_id"], "order_receipt");
    server.await.unwrap();
}

#[tokio::test]
async fn invalid_selector_is_rejected_before_network_io() {
    let client = Client::new("http://127.0.0.1:1", "private", "query").unwrap();
    for (transaction_id, order_id) in [("", "order"), ("txn", " "), ("txn", &"x".repeat(513))] {
        assert!(matches!(
            client
                .query_transaction_receipt(TransactionReceiptQuery {
                    transaction_id: transaction_id.to_owned(),
                    order_id: order_id.to_owned(),
                })
                .await,
            Err(QueryError::InvalidRequest(_))
        ));
    }
}
