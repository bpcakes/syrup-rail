use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use zeroize::Zeroizing;

use crate::configuration::{MAX_NMI_CONCURRENT_REPORTS, configured_http_client};
use crate::{
    BillingContact, ClientFactory, ConfigurationError, Credentials, Endpoint, MAX_CREDENTIAL_BYTES,
    MutationCertainty, MutationError, PaymentSource, SaleRequest, SensitiveText, StoredCredential,
    VaultAction,
};

#[test]
fn endpoint_parsers_keep_https_and_loopback_http_explicit() {
    assert!(Endpoint::parse_https("https://merchant.example.test").is_ok());
    assert!(matches!(
        Endpoint::parse_https("http://merchant.example.test"),
        Err(ConfigurationError::EndpointMustUseHttps)
    ));
    for non_loopback in [
        "http://gateway.localhost:8080",
        "http://merchant.example.test",
    ] {
        assert!(matches!(
            Endpoint::parse_loopback_http(non_loopback),
            Err(ConfigurationError::EndpointMustUseLoopbackHttp)
        ));
    }
    let loopback_endpoint = Endpoint::parse_loopback_http("http://localhost:8080")
        .expect("loopback endpoint should validate");
    let credentials = Credentials::new("private-key".to_owned(), "query-key".to_owned())
        .expect("credentials should validate");
    assert!(matches!(
        ClientFactory::new()
            .expect("HTTPS factory should construct")
            .client(loopback_endpoint, credentials),
        Err(ConfigurationError::LoopbackHttpDisabled)
    ));
    for invalid in [
        "https://user:password@merchant.example.test",
        "https://merchant.example.test/api",
        "https://merchant.example.test/?key=secret",
        "https://merchant.example.test/#fragment",
    ] {
        assert!(matches!(
            Endpoint::parse_https(invalid),
            Err(ConfigurationError::EndpointInvalid)
        ));
    }
}

#[test]
fn credential_client_and_sensitive_text_formatting_is_value_free() {
    let private = "private-debug-sentinel";
    let query = "query-debug-sentinel";
    let credentials =
        Credentials::new(private.to_owned(), query.to_owned()).expect("credentials validate");
    let credential_debug = format!("{credentials:?}");
    assert!(!credential_debug.contains(private));
    assert!(!credential_debug.contains(query));
    let authorization = credentials.private_api_key_header();
    assert!(authorization.is_sensitive());
    assert!(!format!("{authorization:?}").contains(private));

    let client = ClientFactory::new()
        .expect("HTTP client should construct")
        .client(
            Endpoint::parse_https("https://merchant.example.test")
                .expect("endpoint should validate"),
            credentials,
        )
        .expect("HTTPS client should construct");
    let client_debug = format!("{client:?}");
    assert!(!client_debug.contains(private));
    assert!(!client_debug.contains(query));

    let provider_value = "provider-value-debug-sentinel";
    let sensitive = SensitiveText::new(provider_value);
    assert_eq!(sensitive.expose(), provider_value);
    assert!(!format!("{sensitive:?}").contains(provider_value));
    assert!(!format!("{sensitive}").contains(provider_value));
    assert_eq!(sensitive.into_inner(), provider_value);
}

#[test]
fn credentials_accept_existing_zeroizing_owners_without_reallocation() {
    let private = Zeroizing::new("private-key".to_owned());
    let query = Zeroizing::new("query-key".to_owned());
    let private_pointer = private.as_ptr();
    let query_pointer = query.as_ptr();

    let credentials =
        Credentials::new(private, query).expect("zeroizing credentials should validate");

    assert_eq!(credentials.private_api_key.as_ptr(), private_pointer);
    assert_eq!(credentials.query_security_key.as_ptr(), query_pointer);
}

#[test]
fn credentials_reject_oversized_whitespace_before_required_checks() {
    assert!(matches!(
        Credentials::new(" ".repeat(MAX_CREDENTIAL_BYTES + 1), "query-key".to_owned()),
        Err(ConfigurationError::CredentialTooLong)
    ));
    assert!(matches!(
        Credentials::new(
            "private-key".to_owned(),
            " ".repeat(MAX_CREDENTIAL_BYTES + 1)
        ),
        Err(ConfigurationError::CredentialTooLong)
    ));
    assert!(matches!(
        Credentials::new(" ".repeat(MAX_CREDENTIAL_BYTES), "query-key".to_owned()),
        Err(ConfigurationError::PrivateApiKeyRequired)
    ));
    assert!(matches!(
        Credentials::new("private-key".to_owned(), " ".repeat(MAX_CREDENTIAL_BYTES)),
        Err(ConfigurationError::QuerySecurityKeyRequired)
    ));
}

#[test]
fn request_and_error_formatting_redacts_values() {
    let request = SaleRequest {
        amount_cents: 100,
        currency: "USD".to_owned(),
        order_id: "order-debug-sentinel".to_owned(),
        source: PaymentSource::PaymentToken("token-debug-sentinel".to_owned()),
        vault_action: Some(VaultAction::AddCustomer),
        stored_credential: Some(StoredCredential::InitialCustomer),
        billing_contact: Some(BillingContact {
            first_name: Some("name-debug-sentinel".to_owned()),
            last_name: None,
            email: Some("email-debug-sentinel".to_owned()),
        }),
    };
    let debug = format!("{request:?}");
    for value in [
        "order-debug-sentinel",
        "token-debug-sentinel",
        "name-debug-sentinel",
        "email-debug-sentinel",
    ] {
        assert!(!debug.contains(value));
    }

    let detail = "error-detail-debug-sentinel";
    let error = MutationError::Indeterminate(detail.into());
    assert!(!format!("{error:?}").contains(detail));
    assert!(!format!("{error}").contains(detail));
    assert_eq!(error.detail().expose(), detail);
    assert_eq!(error.certainty(), MutationCertainty::Indeterminate);

    let rate_limited = MutationError::RateLimitedIndeterminate(detail.into());
    assert!(!format!("{rate_limited:?}").contains(detail));
    assert!(!format!("{rate_limited}").contains(detail));
    assert_eq!(rate_limited.detail().expose(), detail);
    assert_eq!(rate_limited.certainty(), MutationCertainty::Indeterminate);
}

#[tokio::test]
async fn http2_refused_stream_sale_is_indeterminate_and_never_retried() {
    // Production does not currently enable reqwest's HTTP/2 feature. This
    // dev-only transport case preserves mutation certainty if workspace
    // feature unification or a future production transport enables it.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let endpoint_url = format!(
        "http://{}",
        listener.local_addr().expect("listener address")
    );
    let attempts = Arc::new(AtomicUsize::new(0));
    let server_attempts = attempts.clone();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("client should connect");
        let mut connection = h2::server::handshake(stream)
            .await
            .expect("HTTP/2 handshake should succeed");
        while let Some(request) = connection.accept().await {
            let (_request, mut respond) = request.expect("request should be valid HTTP/2");
            server_attempts.fetch_add(1, Ordering::SeqCst);
            respond.send_reset(h2::Reason::REFUSED_STREAM);
        }
    });
    let http = configured_http_client(reqwest::Client::builder().http2_prior_knowledge())
        .expect("HTTP/2 test client should construct");
    let client = ClientFactory {
        https: http.clone(),
        loopback_http: Some(http),
        report_admission: Arc::new(tokio::sync::Semaphore::new(MAX_NMI_CONCURRENT_REPORTS)),
    }
    .client(
        Endpoint::parse_loopback_http(endpoint_url).expect("test endpoint should validate"),
        Credentials::new("private_key".to_owned(), "query_key".to_owned())
            .expect("test credentials should validate"),
    )
    .expect("explicit loopback client should construct");

    let error = client
        .sale(SaleRequest {
            amount_cents: 100,
            currency: "USD".to_owned(),
            order_id: "ck_refused_stream".to_owned(),
            source: PaymentSource::PaymentToken("tok_refused_stream".to_owned()),
            vault_action: None,
            stored_credential: None,
            billing_contact: None,
        })
        .await
        .expect_err("protocol NACK should make the sale indeterminate");
    assert!(matches!(error, MutationError::Indeterminate(_)));
    assert_eq!(error.certainty(), MutationCertainty::Indeterminate);
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        1,
        "NMI mutation transport must never replay a protocol NACK"
    );
    server.abort();
}
