use super::*;
use crate::{ReceiptConflictField, TransactionReceiptLookup};

fn request() -> TransactionReceiptQuery {
    TransactionReceiptQuery {
        transaction_id: "txn_receipt".to_owned(),
        order_id: "order_receipt".to_owned(),
    }
}

#[tokio::test]
async fn public_evidence_method_exposes_bound_conflict_using_exact_read_only_transport() {
    let body = transaction()
        .replace("<currency>USD</currency>", "")
        .replace(
            "</cc_number>",
            "</cc_number><cc_number>****5678</cc_number>",
        );
    let (client, requests, server) = spawn_capturing_server(
        "HTTP/1.1 200 OK",
        "text/xml",
        format!("<nm_response>{body}</nm_response>").into_bytes(),
    )
    .await;
    let result = client
        .query_transaction_receipt_evidence(request())
        .await
        .unwrap();
    let TransactionReceiptLookup::BoundConflict { fields } = result else {
        panic!("expected an attributed card conflict despite missing currency");
    };
    assert_eq!(fields, [ReceiptConflictField::CardLast4]);
    let request = requests.await.unwrap();
    assert!(request.starts_with("POST /api/query.php"));
    let (_, body) = request.split_once("\r\n\r\n").unwrap();
    let fields: std::collections::HashMap<_, _> = form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect();
    assert_eq!(fields.len(), 3);
    assert_eq!(fields["transaction_id"], "txn_receipt");
    assert_eq!(fields["order_id"], "order_receipt");
    assert!(fields.contains_key("security_key"));
    server.await.unwrap();
}

#[tokio::test]
async fn public_evidence_method_distinguishes_normalized_found_missing_and_invalid() {
    let equivalent = transaction().replace("</currency>", "</currency><currency>usd</currency>");
    for (body, expected) in [
        (equivalent, "found"),
        (String::new(), "missing"),
        (
            transaction().replace("411111******1234", "not-a-mask"),
            "incomplete",
        ),
    ] {
        let (client, requests, server) = spawn_capturing_server(
            "HTTP/1.1 200 OK",
            "text/xml",
            format!("<nm_response>{body}</nm_response>").into_bytes(),
        )
        .await;
        match (
            client.query_transaction_receipt_evidence(request()).await,
            expected,
        ) {
            (Ok(TransactionReceiptLookup::Found(receipt)), "found") => {
                assert_eq!(receipt.into_parts().currency.expose(), "USD")
            }
            (Ok(TransactionReceiptLookup::NotFound), "missing") => {}
            (Ok(TransactionReceiptLookup::Incomplete { card_last4, .. }), "incomplete") => {
                assert!(card_last4.is_none());
            }
            _ => panic!("unexpected public receipt outcome"),
        }
        requests.await.unwrap();
        server.await.unwrap();
    }
}

#[tokio::test]
async fn public_evidence_method_never_attributes_unbound_or_ambiguous_contradictions() {
    let conflicting = transaction().replace(
        "</response_code>",
        "</response_code><response_code>200</response_code>",
    );
    for body in [
        conflicting.replace("txn_receipt", "other"),
        conflicting.replace("order_receipt", "other"),
        conflicting.replace("<transaction_id>txn_receipt</transaction_id>", ""),
        conflicting.replace("<order_id>order_receipt</order_id>", ""),
        conflicting.replace("</order_id>", "</order_id><order_id>other</order_id>"),
        format!("{conflicting}{conflicting}"),
    ] {
        let (client, requests, server) = spawn_capturing_server(
            "HTTP/1.1 200 OK",
            "text/xml",
            format!("<nm_response>{body}</nm_response>").into_bytes(),
        )
        .await;
        let error = client
            .query_transaction_receipt_evidence(request())
            .await
            .unwrap_err();
        assert!(matches!(error, QueryError::MalformedResponse(_)));
        for secret in ["txn_receipt", "order_receipt", "other", "100", "200"] {
            assert!(!format!("{error:?} {error}").contains(secret));
        }
        requests.await.unwrap();
        server.await.unwrap();
    }
}

#[tokio::test]
async fn legacy_public_method_keeps_strict_duplicate_and_error_behavior() {
    for extra in ["usd", "EUR"] {
        let body = transaction().replace(
            "</currency>",
            &format!("</currency><currency>{extra}</currency>"),
        );
        let (client, requests, server) = spawn_capturing_server(
            "HTTP/1.1 200 OK",
            "text/xml",
            format!("<nm_response>{body}</nm_response>").into_bytes(),
        )
        .await;
        assert!(matches!(
            client.query_transaction_receipt(request()).await,
            Err(QueryError::MalformedResponse(_))
        ));
        requests.await.unwrap();
        server.await.unwrap();
    }
}

#[tokio::test]
async fn evidence_selectors_are_validated_before_network_io() {
    let client = Client::new("http://127.0.0.1:1", "private", "query").unwrap();
    for (transaction_id, order_id) in [("", "order"), ("txn", " "), ("txn", &"x".repeat(513))] {
        assert!(matches!(
            client
                .query_transaction_receipt_evidence(TransactionReceiptQuery {
                    transaction_id: transaction_id.to_owned(),
                    order_id: order_id.to_owned(),
                })
                .await,
            Err(QueryError::InvalidRequest(_))
        ));
    }
}

#[tokio::test]
async fn public_incomplete_evidence_preserves_comparisons_without_selecting_invalid_fields() {
    let base = transaction()
        .replace("1.00", "2.00")
        .replace("1234", "5678");
    for (body, field) in [
        (base.replace("20260919100200", "bad-date"), "date"),
        (base.replace("<currency>USD</currency>", ""), "currency"),
        (
            base.replace(
                "</cc_number>",
                "</cc_number><cc_number>invalid-mask</cc_number>",
            ),
            "card",
        ),
        (
            base.replace("</amount>", "</amount><amount>bad-amount</amount>"),
            "amount",
        ),
        (
            base.replace("</success>", "</success><success>unknown</success>"),
            "success",
        ),
        (
            base.replace(
                "</response_code>",
                "</response_code><response_code>unknown</response_code>",
            ),
            "code",
        ),
    ] {
        let (client, requests, server) = spawn_capturing_server(
            "HTTP/1.1 200 OK",
            "text/xml",
            format!("<nm_response>{body}</nm_response>").into_bytes(),
        )
        .await;
        let outcome = client
            .query_transaction_receipt_evidence(request())
            .await
            .unwrap();
        let debug = format!("{outcome:?}");
        let TransactionReceiptLookup::Incomplete {
            currency,
            card_last4,
            actions,
        } = outcome
        else {
            panic!("invalid {field} cannot yield a complete receipt or an invented conflict");
        };
        let action = actions.into_iter().next().unwrap().into_parts();
        assert_eq!(
            currency.as_ref().map(SensitiveText::expose),
            (field != "currency").then_some("USD")
        );
        assert_eq!(
            card_last4.as_ref().map(SensitiveText::expose),
            (field != "card").then_some("5678")
        );
        assert_eq!(
            action.amount.as_ref().map(SensitiveText::expose),
            (field != "amount").then_some("2.00")
        );
        assert_eq!(
            action.date.as_ref().map(SensitiveText::expose),
            (field != "date").then_some("20260919100200")
        );
        assert_eq!(
            action.success.as_ref().map(SensitiveText::expose),
            (field != "success").then_some("1")
        );
        assert_eq!(
            action.response_code.as_ref().map(SensitiveText::expose),
            (field != "code").then_some("100")
        );
        for secret in [
            "txn_receipt",
            "order_receipt",
            "USD",
            "5678",
            "2.00",
            "20260919100200",
        ] {
            assert!(!debug.contains(secret));
        }
        requests.await.unwrap();
        server.await.unwrap();
    }
}
