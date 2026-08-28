use syrup_rail_nmi_client::{
    BillingContact, ClientFactory, ConfigurationError, Credentials, DuplicateCheck,
    DuplicateCheckWindow, Endpoint, MutationCertainty, MutationError, SaleIntent, SaleRequest,
    SensitiveText,
};

#[test]
fn duplicate_check_windows_are_positive_and_bounded() {
    assert!(matches!(
        DuplicateCheckWindow::new(0),
        Err(ConfigurationError::DuplicateCheckWindowOutOfRange)
    ));
    assert_eq!(
        DuplicateCheckWindow::new(DuplicateCheckWindow::MIN_SECONDS)
            .expect("minimum window should validate")
            .seconds(),
        DuplicateCheckWindow::MIN_SECONDS
    );
    assert_eq!(
        DuplicateCheckWindow::new(DuplicateCheckWindow::MAX_SECONDS)
            .expect("maximum window should validate")
            .seconds(),
        DuplicateCheckWindow::MAX_SECONDS
    );
    assert!(matches!(
        DuplicateCheckWindow::new(DuplicateCheckWindow::MAX_SECONDS + 1),
        Err(ConfigurationError::DuplicateCheckWindowOutOfRange)
    ));
}

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
            .client_with_duplicate_check(
                loopback_endpoint,
                credentials,
                DuplicateCheck::ProcessorConfigured,
            ),
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
        .client_with_duplicate_check(
            Endpoint::parse_https("https://merchant.example.test")
                .expect("endpoint should validate"),
            credentials,
            DuplicateCheck::ProcessorConfigured,
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
        order_id: "order-debug-sentinel".to_owned(),
        intent: SaleIntent::InitialStoredCredential {
            payment_token: "token-debug-sentinel".to_owned(),
        },
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
