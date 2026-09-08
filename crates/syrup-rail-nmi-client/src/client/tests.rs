use std::{
    borrow::Cow,
    ops::Deref,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use reqwest::StatusCode;
use serde_json::{Value, json};
use url::form_urlencoded;
use zeroize::Zeroizing;

use crate::{
    BillingContact, PaymentOutcomeDiagnostic, PaymentSource, SaleIntent, SensitiveText,
    StoredCredential, TransactionReportDiagnostic, VaultAction, lossless_json::LosslessJsonValue,
};

use super::form::{
    NmiFormParams, NmiFormValue, amount_string, classic_sale_params,
    classic_store_payment_method_params, query_account_mode_params, query_transaction_params,
    query_transaction_report_params,
};
use super::response::common::{
    ResolvedScalar, ScalarOccurrence, ScalarOccurrenceCollector, payment_status_from_response_code,
};
use super::response::form::classic_payment_outcome_from_form;
use super::response::json::payment_outcome_from_json as payment_outcome_from_lossless_json;
use super::response::xml::{
    query_account_mode_from_xml, query_outcome_for_request_from_xml,
    query_transaction_reports_from_xml,
};
use super::text::{last4, parse_expiry};
use super::transport::{gateway_error_for_http_status, gateway_error_from_http_response};
use super::v5::{amount_value, order_details_json, sale_body_json};
use super::validation::{
    validate_report_query, validate_sale_request, validate_store_payment_method_request,
    validate_transaction_query,
};
use super::*;

fn query_outcome_from_xml(text: &str) -> Result<Option<PaymentOutcome>, WireError> {
    query_outcome_for_request_from_xml(
        text,
        &TransactionQuery {
            transaction_id: None,
            order_id: None,
        },
    )
}

mod card_metadata;
mod classic_response;
mod concurrency;
mod form_requests;
mod identity_contract;
mod json_decision;
mod json_identity;
mod query_response;
mod report_response;
mod report_transport;
mod request_bounds;
mod transport;

struct DropObserved<T> {
    value: T,
    drops: Arc<AtomicUsize>,
}

impl<T> DropObserved<T> {
    fn new(value: T, drops: Arc<AtomicUsize>) -> Self {
        Self { value, drops }
    }
}

impl<T> Deref for DropObserved<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

impl<T> Drop for DropObserved<T> {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

fn encoded_form(params: &NmiFormParams<'_>) -> Vec<(String, String)> {
    let gateway = Client::new("http://127.0.0.1", "unused_private_key", "unused_query_key")
        .expect("local form-test gateway should construct");
    let request = gateway
        .form_request("/form-test", params)
        .expect("form endpoint should resolve")
        .build()
        .expect("string-only NMI form should build");
    let body = request
        .body()
        .and_then(reqwest::Body::as_bytes)
        .expect("NMI form body should be reusable bytes");
    form_urlencoded::parse(body).into_owned().collect()
}

fn payment_outcome_from_json(value: &Value) -> Result<PaymentOutcome, WireError> {
    let value = serde_json::from_value::<LosslessJsonValue>(value.clone())
        .expect("NMI JSON fixture should parse");
    payment_outcome_from_lossless_json(&value)
}

fn payment_outcome_from_json_text(text: &str) -> Result<PaymentOutcome, WireError> {
    let value = serde_json::from_str::<LosslessJsonValue>(text)
        .expect("lossless NMI JSON fixture should parse");
    payment_outcome_from_lossless_json(&value)
}

fn decimal_pan(zero: u32) -> String {
    "4111111111111111"
        .bytes()
        .map(|byte| {
            char::from_u32(zero + u32::from(byte - b'0'))
                .expect("decimal digit must be a Unicode scalar")
        })
        .collect()
}

#[derive(Clone, Copy)]
enum TestClientConstruction {
    Explicit(DuplicateCheck),
    Legacy,
}

async fn spawn_capturing_server(
    status_line: &'static str,
    content_type: &'static str,
    response_body: Vec<u8>,
) -> (
    Client,
    tokio::sync::oneshot::Receiver<String>,
    tokio::task::JoinHandle<()>,
) {
    spawn_capturing_server_with_duplicate_check(
        status_line,
        content_type,
        response_body,
        DuplicateCheck::ProcessorConfigured,
    )
    .await
}

async fn spawn_capturing_server_with_duplicate_check(
    status_line: &'static str,
    content_type: &'static str,
    response_body: Vec<u8>,
    duplicate_check: DuplicateCheck,
) -> (
    Client,
    tokio::sync::oneshot::Receiver<String>,
    tokio::task::JoinHandle<()>,
) {
    spawn_capturing_server_with_client_construction(
        status_line,
        content_type,
        response_body,
        TestClientConstruction::Explicit(duplicate_check),
    )
    .await
}

async fn spawn_capturing_server_with_legacy_client(
    status_line: &'static str,
    content_type: &'static str,
    response_body: Vec<u8>,
) -> (
    Client,
    tokio::sync::oneshot::Receiver<String>,
    tokio::task::JoinHandle<()>,
) {
    spawn_capturing_server_with_client_construction(
        status_line,
        content_type,
        response_body,
        TestClientConstruction::Legacy,
    )
    .await
}

async fn spawn_capturing_server_with_client_construction(
    status_line: &'static str,
    content_type: &'static str,
    response_body: Vec<u8>,
    construction: TestClientConstruction,
) -> (
    Client,
    tokio::sync::oneshot::Receiver<String>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
    let (request_sender, request_receiver) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("request should connect");
        let mut buffer = Vec::new();
        let mut chunk = [0_u8; 1024];
        let header_end = loop {
            let read = tokio::io::AsyncReadExt::read(&mut stream, &mut chunk)
                .await
                .expect("request should read");
            assert!(read > 0, "request closed before headers");
            buffer.extend_from_slice(&chunk[..read]);
            if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                break end + 4;
            }
        };
        let content_length = {
            let headers = String::from_utf8_lossy(&buffer[..header_end]);
            headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0)
        };
        while buffer.len() < header_end + content_length {
            let read = tokio::io::AsyncReadExt::read(&mut stream, &mut chunk)
                .await
                .expect("request body should read");
            assert!(read > 0, "request closed before body");
            buffer.extend_from_slice(&chunk[..read]);
        }
        request_sender
            .send(String::from_utf8_lossy(&buffer).into_owned())
            .expect("request should be captured");
        let response_headers = format!(
            "{status_line}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            response_body.len()
        );
        tokio::io::AsyncWriteExt::write_all(&mut stream, response_headers.as_bytes())
            .await
            .expect("response headers should write");
        let _ = tokio::io::AsyncWriteExt::write_all(&mut stream, &response_body).await;
    });
    let endpoint = Endpoint::parse_loopback_http(base_url).expect("test endpoint should validate");
    let credentials = Credentials::new("private_key".to_owned(), "query_key".to_owned())
        .expect("test credentials should validate");
    let factory = ClientFactory::new_with_loopback_http().expect("test factory should build");
    #[allow(deprecated)]
    let client = match construction {
        TestClientConstruction::Explicit(duplicate_check) => {
            factory.client_with_duplicate_check(endpoint, credentials, duplicate_check)
        }
        TestClientConstruction::Legacy => factory.client(endpoint, credentials),
    }
    .expect("test client should build");
    (client, request_receiver, server)
}

#[test]
#[allow(deprecated)]
fn legacy_factory_client_no_longer_selects_the_invalid_zero_override() {
    let endpoint =
        Endpoint::parse_loopback_http("http://127.0.0.1").expect("test endpoint should validate");
    let credentials = Credentials::new("private_key".to_owned(), "query_key".to_owned())
        .expect("test credentials should validate");
    let client = ClientFactory::new_with_loopback_http()
        .expect("test factory should build")
        .client(endpoint, credentials)
        .expect("legacy test client should build");

    assert_eq!(client.duplicate_check, DuplicateCheck::ProcessorConfigured);
}

#[test]
fn duplicate_check_policy_is_owned_by_each_factory_client() {
    let factory =
        ClientFactory::new_with_loopback_http().expect("test factory should construct once");
    let client = |policy| {
        factory
            .client_with_duplicate_check(
                Endpoint::parse_loopback_http("http://127.0.0.1")
                    .expect("test endpoint should validate"),
                Credentials::new("private_key".to_owned(), "query_key".to_owned())
                    .expect("test credentials should validate"),
                policy,
            )
            .expect("test client should build")
    };
    let processor_configured = client(DuplicateCheck::ProcessorConfigured);
    let explicit_window = client(DuplicateCheck::Window(
        crate::DuplicateCheckWindow::new(120).expect("window should validate"),
    ));

    assert_eq!(
        processor_configured.duplicate_check,
        DuplicateCheck::ProcessorConfigured
    );
    assert_eq!(
        explicit_window.duplicate_check,
        DuplicateCheck::Window(crate::DuplicateCheckWindow::new(120).unwrap())
    );
}

fn test_sale_request(source: PaymentSource) -> SaleRequest {
    SaleRequest {
        amount_cents: 100,
        order_id: "ck_order".to_owned(),
        intent: SaleIntent::from_legacy_parts(source, None, None)
            .expect("a direct source is a valid sale intent"),
        billing_contact: None,
    }
}

fn test_store_payment_method_request() -> StorePaymentMethodRequest {
    StorePaymentMethodRequest {
        payment_token: "tok_test".to_owned(),
        order_id: "ck_order".to_owned(),
        billing_contact: None,
    }
}
