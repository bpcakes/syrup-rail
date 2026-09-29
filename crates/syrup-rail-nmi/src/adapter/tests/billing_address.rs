use std::collections::HashMap;

use syrup_rail::{
    BillingAddress, GatewayPaymentMethodReference, GatewayTransactionId, PaymentGateway,
};

use super::*;

const CLASSIC_APPROVAL: &str = "response=1&response_code=100&transactionid=txn_address&customer_vault_id=vault_address&responsetext=Approved";
const V5_APPROVAL: &str = r#"{"id":"txn_address","status":"approved"}"#;

async fn capturing_gateway(
    response_body: &'static str,
) -> (
    NmiPaymentGateway,
    tokio::sync::oneshot::Receiver<String>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let endpoint = Endpoint::parse_loopback_http(format!(
        "http://{}",
        listener.local_addr().expect("test listener address")
    ))
    .expect("loopback endpoint should validate");
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("request should connect");
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
        sender
            .send(String::from_utf8_lossy(&request).into_owned())
            .expect("request should be captured");
        let headers = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
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
        .expect("test client should construct")
        .with_customer_receipts_disabled();
    (NmiPaymentGateway::new(client), receiver, server)
}

fn charge() -> ChargeAmount {
    ChargeAmount::new(4_900, CurrencyCode::new("USD").unwrap()).unwrap()
}

fn order() -> GatewayOrderId {
    GatewayOrderId::from_correlation("ck_address_order").unwrap()
}

/// A core-normalized address: the lowercase country is stored uppercase and
/// the postal code keeps its leading zero.
fn core_address() -> BillingAddress {
    BillingAddress::new(" 1 Main St ".to_owned(), "us".to_owned())
        .unwrap()
        .with_line2(Some("Suite 2".to_owned()))
        .unwrap()
        .with_city(Some("Boston".to_owned()))
        .unwrap()
        .with_region(Some("MA".to_owned()))
        .unwrap()
        .with_postal_code(Some("02110".to_owned()))
        .unwrap()
}

const EXPECTED_ADDRESS: [(&str, &str); 6] = [
    ("address1", "1 Main St"),
    ("address2", "Suite 2"),
    ("city", "Boston"),
    ("state", "MA"),
    ("zip", "02110"),
    ("country", "US"),
];

fn named_contact() -> BillingContact {
    BillingContact::new(
        Some("Ada".to_owned()),
        Some("Lovelace".to_owned()),
        Some("ada@example.test".to_owned()),
    )
    .unwrap()
    .with_address(core_address())
}

fn recurring_intent() -> GatewaySaleIntent {
    GatewaySaleIntent::RecurringStoredCredential {
        payment_method_reference: GatewayPaymentMethodReference::new("vault_existing").unwrap(),
        initial_transaction_id: GatewayTransactionId::new("txn_initial").unwrap(),
    }
}

fn form_fields(request: &str) -> HashMap<String, String> {
    let (head, body) = request.split_once("\r\n\r\n").expect("request body");
    assert!(head.starts_with("POST /api/transact.php "), "{head}");
    url::form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect()
}

fn json_body(request: &str) -> serde_json::Value {
    let (head, body) = request.split_once("\r\n\r\n").expect("request body");
    assert!(head.starts_with("POST /api/v5/payments/sale "), "{head}");
    serde_json::from_str(body).expect("v5 body should be JSON")
}

async fn finish(server: tokio::task::JoinHandle<()>) {
    tokio::time::timeout(std::time::Duration::from_secs(5), server)
        .await
        .expect("test server should not hang")
        .expect("test server assertions should pass");
}

#[tokio::test]
async fn core_addresses_reach_the_nmi_wire_for_every_payment_intent() {
    // Initial enrollment and recovery both submit InitialStoredCredential.
    let (gateway, captured, server) = capturing_gateway(CLASSIC_APPROVAL).await;
    let outcome = gateway
        .sale(GatewaySaleRequest::new(
            charge(),
            order(),
            GatewaySaleIntent::InitialStoredCredential {
                payment_token: PaymentToken::new("tok_initial").unwrap(),
            },
            Some(named_contact()),
        ))
        .await
        .expect("initial sale should be approved");
    assert_eq!(outcome.status(), GatewayPaymentStatus::Approved);
    let fields = form_fields(&captured.await.unwrap());
    for (key, value) in EXPECTED_ADDRESS {
        assert_eq!(fields.get(key).map(String::as_str), Some(value), "{key}");
    }
    assert_eq!(fields["type"], "sale");
    assert_eq!(fields["customer_vault"], "add_customer");
    assert_eq!(fields["stored_credential_indicator"], "stored");
    assert_eq!(fields["initiated_by"], "customer");
    assert_eq!(fields["first_name"], "Ada");
    assert_eq!(fields["email"], "ada@example.test");
    assert_eq!(fields["customer_receipt"], "false");
    finish(server).await;

    // Merchant renewals carry an address-only contact.
    let (gateway, captured, server) = capturing_gateway(CLASSIC_APPROVAL).await;
    let outcome = gateway
        .sale(GatewaySaleRequest::new(
            charge(),
            order(),
            recurring_intent(),
            Some(BillingContact::from_address(core_address())),
        ))
        .await
        .expect("renewal should be approved");
    assert_eq!(outcome.status(), GatewayPaymentStatus::Approved);
    let fields = form_fields(&captured.await.unwrap());
    for (key, value) in EXPECTED_ADDRESS {
        assert_eq!(fields.get(key).map(String::as_str), Some(value), "{key}");
    }
    assert_eq!(fields["customer_vault_id"], "vault_existing");
    assert_eq!(fields["stored_credential_indicator"], "used");
    assert_eq!(fields["initiated_by"], "merchant");
    assert_eq!(fields["initial_transaction_id"], "txn_initial");
    assert_eq!(fields["customer_receipt"], "false");
    for key in ["first_name", "last_name", "email", "customer_vault"] {
        assert!(!fields.contains_key(key), "renewal must not send {key}");
    }
    finish(server).await;

    // Payment-method replacement uses Classic validate.
    let (gateway, captured, server) = capturing_gateway(CLASSIC_APPROVAL).await;
    let outcome = gateway
        .store_payment_method(GatewayStorePaymentMethodRequest::new(
            PaymentToken::new("tok_replacement").unwrap(),
            order(),
            Some(named_contact()),
        ))
        .await
        .expect("replacement should be approved");
    assert_eq!(outcome.status(), GatewayPaymentStatus::Approved);
    let fields = form_fields(&captured.await.unwrap());
    for (key, value) in EXPECTED_ADDRESS {
        assert_eq!(fields.get(key).map(String::as_str), Some(value), "{key}");
    }
    assert_eq!(fields["type"], "validate");
    assert_eq!(fields["stored_credential_indicator"], "stored");
    assert_eq!(fields["last_name"], "Lovelace");
    finish(server).await;

    // Host charges use the v5 one-time sale.
    let (gateway, captured, server) = capturing_gateway(V5_APPROVAL).await;
    let outcome = gateway
        .sale(GatewaySaleRequest::new(
            charge(),
            order(),
            GatewaySaleIntent::OneTime {
                payment_token: PaymentToken::new("tok_charge").unwrap(),
            },
            Some(named_contact()),
        ))
        .await
        .expect("host charge should be approved");
    assert_eq!(outcome.status(), GatewayPaymentStatus::Approved);
    let body = json_body(&captured.await.unwrap());
    for (key, value) in EXPECTED_ADDRESS {
        assert_eq!(body["billing_address"][key], value, "{key}");
    }
    assert_eq!(body["billing_address"]["first_name"], "Ada");
    assert_eq!(body["customer_receipt"], false);
    assert!(body.get("cit_mit").is_none());
    finish(server).await;
}

#[tokio::test]
async fn addressless_renewal_sends_no_contact_fields() {
    let (gateway, captured, server) = capturing_gateway(CLASSIC_APPROVAL).await;
    let outcome = gateway
        .sale(GatewaySaleRequest::new(
            charge(),
            order(),
            recurring_intent(),
            None,
        ))
        .await
        .expect("renewal should be approved");
    assert_eq!(outcome.status(), GatewayPaymentStatus::Approved);
    let fields = form_fields(&captured.await.unwrap());
    let mut keys = fields.keys().map(String::as_str).collect::<Vec<_>>();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "amount",
            "billing_method",
            "currency",
            "customer_receipt",
            "customer_vault_id",
            "initial_transaction_id",
            "initiated_by",
            "orderid",
            "security_key",
            "stored_credential_indicator",
            "type",
        ]
    );
    finish(server).await;
}

#[tokio::test]
async fn core_valid_address_rejected_by_nmi_rules_is_not_submitted() {
    let long_line = "a".repeat(150);
    let cases = [
        BillingAddress::new(long_line, "US".to_owned()).unwrap(),
        BillingAddress::new("1 Main St".to_owned(), "US".to_owned())
            .unwrap()
            .with_region(Some("Massachusetts".to_owned()))
            .unwrap(),
        BillingAddress::new("1 Main St".to_owned(), "US".to_owned())
            .unwrap()
            .with_postal_code(Some("02110#1".to_owned()))
            .unwrap(),
    ];
    // A closed loopback port: any submission attempt would surface as a
    // transport failure rather than a local malformed-request rejection.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint =
        Endpoint::parse_loopback_http(format!("http://{}", listener.local_addr().unwrap()))
            .unwrap();
    drop(listener);
    let client = ClientFactory::new_with_loopback_http()
        .unwrap()
        .client_with_duplicate_check(
            endpoint,
            Credentials::new("private_key".to_owned(), "query_key".to_owned()).unwrap(),
            DuplicateCheck::ProcessorConfigured,
        )
        .unwrap();
    let gateway = NmiPaymentGateway::new(client);
    for address in cases {
        let sale = gateway
            .sale(GatewaySaleRequest::new(
                charge(),
                order(),
                recurring_intent(),
                Some(BillingContact::from_address(address.clone())),
            ))
            .await;
        assert!(matches!(
            sale,
            Err(GatewayMutationError::NotSubmitted(
                GatewayNotSubmittedError::Malformed(_)
            ))
        ));
        let store = gateway
            .store_payment_method(GatewayStorePaymentMethodRequest::new(
                PaymentToken::new("tok_replacement").unwrap(),
                order(),
                Some(BillingContact::from_address(address)),
            ))
            .await;
        assert!(matches!(
            store,
            Err(GatewayMutationError::NotSubmitted(
                GatewayNotSubmittedError::Malformed(_)
            ))
        ));
    }
}
