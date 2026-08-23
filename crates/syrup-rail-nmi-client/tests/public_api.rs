use syrup_rail_nmi_client::{
    BillingContact, ClientFactory, ConfigurationError, Credentials, Endpoint, MutationCertainty,
    MutationError, PaymentSource, SaleRequest, SensitiveText, StoredCredential, VaultAction,
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
