use super::*;

#[test]
fn transaction_query_identifiers_are_normalized_for_validation_and_transport() {
    let query = TransactionQuery {
        transaction_id: Some(" \t txn_query \n".to_owned()),
        order_id: Some("\n ck_query \t".to_owned()),
    };
    assert!(validate_transaction_query(&query, "query_key").is_ok());

    let params = query_transaction_params("query_key", &query);
    assert_eq!(
        params.field("transaction_id").map(NmiFormValue::as_str),
        Some("txn_query")
    );
    assert_eq!(
        params.field("order_id").map(NmiFormValue::as_str),
        Some("ck_query")
    );

    let order_only = TransactionQuery {
        transaction_id: Some(" \t\n".to_owned()),
        order_id: Some(" ck_order_only ".to_owned()),
    };
    assert!(validate_transaction_query(&order_only, "query_key").is_ok());
    let params = query_transaction_params("query_key", &order_only);
    assert!(params.field("transaction_id").is_none());
    assert_eq!(
        params.field("order_id").map(NmiFormValue::as_str),
        Some("ck_order_only")
    );

    let empty = TransactionQuery {
        transaction_id: Some(" \t".to_owned()),
        order_id: Some("\n ".to_owned()),
    };
    assert!(matches!(
        validate_transaction_query(&empty, "query_key"),
        Err(QueryError::InvalidRequest(_))
    ));
}

#[test]
fn order_ids_are_normalized_consistently_across_mutation_and_query_transports() {
    let order_id = " \t ck_round_trip \n";
    let sale = SaleRequest {
        amount_cents: 4_900,
        currency: "USD".to_owned(),
        order_id: order_id.to_owned(),
        source: PaymentSource::PaymentToken("tok_round_trip".to_owned()),
        vault_action: Some(VaultAction::AddCustomer),
        stored_credential: None,
        billing_contact: None,
    };
    assert!(validate_sale_request(&sale, "private_key").is_ok());
    let sale_params = classic_sale_params(
        "private_key",
        &sale,
        "49.00".to_owned(),
        DuplicateCheck::ProcessorConfigured,
    );
    assert_eq!(
        sale_params.field("orderid").map(NmiFormValue::as_str),
        Some("ck_round_trip")
    );
    assert_eq!(
        order_details_json(&sale.order_id)
            .get("id")
            .and_then(Value::as_str),
        Some("ck_round_trip")
    );

    let store = StorePaymentMethodRequest {
        payment_token: "tok_round_trip".to_owned(),
        order_id: order_id.to_owned(),
        billing_contact: None,
    };
    assert!(validate_store_payment_method_request(&store, "private_key").is_ok());
    let store_params = classic_store_payment_method_params("private_key", &store);
    assert_eq!(
        store_params.field("orderid").map(NmiFormValue::as_str),
        Some("ck_round_trip")
    );

    let query = TransactionQuery {
        transaction_id: None,
        order_id: Some(order_id.to_owned()),
    };
    assert!(validate_transaction_query(&query, "query_key").is_ok());
    let query_params = query_transaction_params("query_key", &query);
    assert_eq!(
        query_params.field("order_id").map(NmiFormValue::as_str),
        Some("ck_round_trip")
    );
}

#[tokio::test]
async fn json_sale_sends_raw_private_key_authorization_header() {
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
        let body = br#"{"id":"txn_auth_header","status":"approved"}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
            body.len()
        );
        tokio::io::AsyncWriteExt::write_all(&mut stream, response.as_bytes())
            .await
            .expect("response headers should write");
        tokio::io::AsyncWriteExt::write_all(&mut stream, body)
            .await
            .expect("response body should write");
    });
    let gateway = Client::new(base_url, "private_key", "query_key").expect("gateway should build");

    let outcome = gateway
        .sale(SaleRequest {
            amount_cents: 4_900,
            currency: "USD".to_owned(),
            order_id: "ck_order_auth_header".to_owned(),
            source: PaymentSource::PaymentToken("tok_auth_header".to_owned()),
            vault_action: None,
            stored_credential: None,
            billing_contact: None,
        })
        .await
        .expect("sale should parse test response");

    assert_eq!(outcome.status, PaymentStatus::Approved);
    let request = request_receiver.await.expect("request should be captured");
    let authorization = request
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("authorization")
                .then(|| value.trim().to_owned())
        })
        .expect("authorization header should be present");
    assert_eq!(authorization, "private_key");
    assert_ne!(authorization, "Bearer private_key");
    assert!(request.starts_with("POST /api/v5/payments/sale HTTP/1.1"));
    server.await.expect("server task should finish");
}

#[tokio::test]
async fn classic_recurring_merchant_sale_sends_scheduled_billing_metadata() {
    let (client, request_receiver, server) = spawn_capturing_server(
        "HTTP/1.1 200 OK",
        "application/x-www-form-urlencoded",
        b"response=1&response_code=100&transactionid=txn_recurring&responsetext=Approved".to_vec(),
    )
    .await;

    let outcome = client
        .sale(SaleRequest {
            amount_cents: 4_900,
            currency: "USD".to_owned(),
            order_id: "ck_recurring_order".to_owned(),
            source: PaymentSource::CustomerVault("vault_recurring".to_owned()),
            vault_action: None,
            stored_credential: Some(StoredCredential::RecurringMerchant {
                initial_transaction_id: "txn_initial".to_owned(),
            }),
            billing_contact: None,
        })
        .await
        .expect("recurring Classic sale should parse");
    let outcome = outcome.into_parts();
    assert_eq!(outcome.status, PaymentStatus::Approved);
    assert_eq!(
        outcome
            .customer_vault_id
            .as_ref()
            .map(SensitiveText::expose),
        Some("vault_recurring"),
        "the effective vault identity should retain the validated request source when NMI omits it"
    );

    let request = request_receiver.await.expect("request should be captured");
    assert!(request.starts_with("POST /api/transact.php HTTP/1.1"));
    let (_, body) = request
        .split_once("\r\n\r\n")
        .expect("captured request should contain a body");
    let fields: std::collections::BTreeMap<_, _> = form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect();
    for (key, value) in [
        ("security_key", "private_key"),
        ("type", "sale"),
        ("amount", "49.00"),
        ("currency", "USD"),
        ("orderid", "ck_recurring_order"),
        ("customer_vault_id", "vault_recurring"),
        ("billing_method", "recurring"),
        ("initiated_by", "merchant"),
        ("stored_credential_indicator", "used"),
        ("initial_transaction_id", "txn_initial"),
    ] {
        assert_eq!(fields.get(key).map(String::as_str), Some(value), "{key}");
    }
    assert_eq!(
        fields.len(),
        10,
        "no vault creation, token, or duplicate-window override"
    );
    server.await.expect("server task should finish");
}

#[tokio::test]
async fn http_200_invalid_v5_json_is_an_indeterminate_mutation() {
    let (client, _request_receiver, server) = spawn_capturing_server(
        "HTTP/1.1 200 OK",
        "application/json",
        b"{not valid json".to_vec(),
    )
    .await;

    let error = client
        .sale(SaleRequest {
            amount_cents: 100,
            currency: "USD".to_owned(),
            order_id: "ck_invalid_json".to_owned(),
            source: PaymentSource::PaymentToken("tok_invalid_json".to_owned()),
            vault_action: None,
            stored_credential: None,
            billing_contact: None,
        })
        .await
        .expect_err("accepted mutation with invalid JSON must be indeterminate");

    assert!(matches!(error, MutationError::Indeterminate(_)));
    assert_eq!(error.certainty(), crate::MutationCertainty::Indeterminate);
    server.await.expect("server task should finish");
}

#[tokio::test]
async fn http_200_semantically_malformed_classic_body_is_indeterminate() {
    let (client, _request_receiver, server) =
        spawn_capturing_server("HTTP/1.1 200 OK", "text/plain", Vec::new()).await;

    let error = client
        .store_payment_method(StorePaymentMethodRequest {
            payment_token: "tok_empty_classic".to_owned(),
            order_id: "ck_empty_classic".to_owned(),
            billing_contact: None,
        })
        .await
        .expect_err("accepted mutation with empty Classic body must be indeterminate");

    assert!(matches!(error, MutationError::Indeterminate(_)));
    assert_eq!(error.certainty(), crate::MutationCertainty::Indeterminate);
    server.await.expect("server task should finish");
}

#[tokio::test]
async fn connection_failure_is_known_not_submitted() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
    drop(listener);
    let client = Client::new(base_url, "private", "query").expect("client should build");

    let error = client
        .sale(SaleRequest {
            amount_cents: 100,
            currency: "USD".to_owned(),
            order_id: "ck_connect_failure".to_owned(),
            source: PaymentSource::PaymentToken("tok_connect_failure".to_owned()),
            vault_action: None,
            stored_credential: None,
            billing_contact: None,
        })
        .await
        .expect_err("a refused connection must fail before submission");

    assert!(matches!(error, MutationError::Unavailable(_)));
    assert_eq!(error.certainty(), crate::MutationCertainty::NotSubmitted);
}

#[tokio::test]
async fn established_http1_reset_is_indeterminate_and_never_retried() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
    let attempts = Arc::new(AtomicUsize::new(0));
    let server_attempts = attempts.clone();
    let (client_finished, mut client_finished_receiver) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("request should connect");
        server_attempts.fetch_add(1, Ordering::SeqCst);

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

        assert!(buffer.starts_with(b"POST /api/v5/payments/sale HTTP/1.1"));
        stream
            .set_zero_linger()
            .expect("test server should configure an abortive close");
        drop(stream);

        loop {
            tokio::select! {
                biased;
                accepted = listener.accept() => {
                    let (stream, _) = accepted.expect("retry connection should accept");
                    server_attempts.fetch_add(1, Ordering::SeqCst);
                    stream
                        .set_zero_linger()
                        .expect("retry connection should configure an abortive close");
                    drop(stream);
                }
                _ = &mut client_finished_receiver => break,
            }
        }
    });
    let client = Client::new(base_url, "private", "query").expect("client should build");

    let error = client
        .sale(SaleRequest {
            amount_cents: 100,
            currency: "USD".to_owned(),
            order_id: "ck_established_reset".to_owned(),
            source: PaymentSource::PaymentToken("tok_established_reset".to_owned()),
            vault_action: None,
            stored_credential: None,
            billing_contact: None,
        })
        .await
        .expect_err("a reset after submission must preserve mutation uncertainty");

    assert!(matches!(error, MutationError::Indeterminate(_)));
    assert_eq!(error.certainty(), crate::MutationCertainty::Indeterminate);
    client_finished
        .send(())
        .expect("server should wait for the client result");
    server.await.expect("server task should finish");
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        1,
        "NMI mutation transport must never replay an accepted HTTP/1 request"
    );
}

#[tokio::test]
async fn successful_oversized_response_is_an_indeterminate_mutation() {
    let (client, _request_receiver, server) = spawn_capturing_server(
        "HTTP/1.1 200 OK",
        "application/json",
        vec![b'a'; MAX_NMI_STANDARD_RESPONSE_BYTES + 1],
    )
    .await;

    let error = client
        .sale(SaleRequest {
            amount_cents: 100,
            currency: "USD".to_owned(),
            order_id: "ck_oversized_response".to_owned(),
            source: PaymentSource::PaymentToken("tok_oversized_response".to_owned()),
            vault_action: None,
            stored_credential: None,
            billing_contact: None,
        })
        .await
        .expect_err("oversized successful response must be indeterminate");

    assert!(matches!(error, MutationError::Indeterminate(_)));
    server.await.expect("server task should finish");
}

#[test]
fn sale_order_details_use_payment_order_id_field() {
    let body = order_details_json("ck_recovery_018f8442c1327cc98000000000000001");

    assert_eq!(
        body.get("id").and_then(Value::as_str),
        Some("ck_recovery_018f8442c1327cc98000000000000001")
    );
    assert!(body.get("order_id").is_none());
}

#[test]
fn sale_amount_json_is_fixed_precision_decimal_string() {
    let amount = amount_value(4_900).expect("positive cents should format");
    let fractional_amount = amount_value(1_234).expect("positive cents should format");

    assert_eq!(amount, json!("49.00"));
    assert_eq!(fractional_amount, json!("12.34"));
}

#[test]
fn sale_amount_json_rejects_non_positive_cents() {
    assert!(matches!(
        amount_value(0),
        Err(WireError::LocalInvalidRequest(message)) if message.contains("positive")
    ));
    assert!(matches!(
        amount_value(-1),
        Err(WireError::LocalInvalidRequest(message)) if message.contains("positive")
    ));
}

#[test]
fn sale_body_json_applies_the_configured_duplicate_check_policy() {
    let request = SaleRequest {
        amount_cents: 4_900,
        currency: "USD".to_owned(),
        order_id: "ck_order_123".to_owned(),
        source: PaymentSource::PaymentToken("tok_test".to_owned()),
        vault_action: None,
        stored_credential: None,
        billing_contact: None,
    };
    let body = sale_body_json(
        &request,
        amount_value(request.amount_cents).expect("positive cents should format"),
        DuplicateCheck::ProcessorConfigured,
    );
    assert!(body.get("dup_seconds").is_none());
    assert!(body.get("duplicate_check_seconds").is_none());

    let body = sale_body_json(
        &request,
        amount_value(request.amount_cents).expect("positive cents should format"),
        DuplicateCheck::Window(
            crate::DuplicateCheckWindow::new(120).expect("window should be valid"),
        ),
    );
    assert_eq!(body.get("dup_seconds"), Some(&json!(120)));
    assert!(body.get("duplicate_check_seconds").is_none());
}

#[tokio::test]
async fn explicit_duplicate_check_policy_reaches_the_v5_wire() {
    for (duplicate_check, expected_wire_seconds) in [
        (DuplicateCheck::ProcessorConfigured, None),
        (
            DuplicateCheck::Window(
                crate::DuplicateCheckWindow::new(120).expect("window should be valid"),
            ),
            Some(120),
        ),
        (
            DuplicateCheck::Window(
                crate::DuplicateCheckWindow::new(crate::DuplicateCheckWindow::MAX_SECONDS)
                    .expect("maximum window should be valid"),
            ),
            Some(7_862_400),
        ),
    ] {
        let (client, request_receiver, server) = spawn_capturing_server_with_duplicate_check(
            "HTTP/1.1 200 OK",
            "application/json",
            br#"{"id":"txn_duplicate_policy","status":"approved"}"#.to_vec(),
            duplicate_check,
        )
        .await;

        let outcome = client
            .sale(test_sale_request(PaymentSource::PaymentToken(
                "tok_duplicate_policy".to_owned(),
            )))
            .await
            .expect("sale should parse");
        assert_eq!(outcome.status, PaymentStatus::Approved);

        let request = request_receiver.await.expect("request should be captured");
        let (_, body) = request
            .split_once("\r\n\r\n")
            .expect("captured request should contain a body");
        let body: Value = serde_json::from_str(body).expect("sale body should be JSON");
        assert_eq!(
            body.get("dup_seconds").and_then(Value::as_u64),
            expected_wire_seconds
        );
        assert!(body.get("duplicate_check_seconds").is_none());
        server.await.expect("server task should finish");
    }
}

#[tokio::test]
async fn explicit_duplicate_check_policy_reaches_the_classic_wire() {
    for (duplicate_check, expected_wire_seconds) in [
        (DuplicateCheck::ProcessorConfigured, None),
        (
            DuplicateCheck::Window(
                crate::DuplicateCheckWindow::new(120).expect("window should be valid"),
            ),
            Some("120"),
        ),
        (
            DuplicateCheck::Window(
                crate::DuplicateCheckWindow::new(crate::DuplicateCheckWindow::MAX_SECONDS)
                    .expect("maximum window should be valid"),
            ),
            Some("7862400"),
        ),
    ] {
        let response = b"response=1&responsetext=Approved&authcode=AUTH&transactionid=txn_classic_duplicate_policy&customer_vault_id=vault_classic_duplicate_policy&avsresponse=&cvvresponse=&orderid=ck_order&type=sale&response_code=100".to_vec();
        let (client, request_receiver, server) = spawn_capturing_server_with_duplicate_check(
            "HTTP/1.1 200 OK",
            "text/plain",
            response,
            duplicate_check,
        )
        .await;

        let mut request = test_sale_request(PaymentSource::PaymentToken(
            "tok_classic_duplicate_policy".to_owned(),
        ));
        request.vault_action = Some(VaultAction::AddCustomer);
        request.stored_credential = Some(StoredCredential::InitialCustomer);
        let outcome = client
            .sale(request)
            .await
            .expect("Classic sale should parse");
        assert_eq!(outcome.status, PaymentStatus::Approved);

        let request = request_receiver.await.expect("request should be captured");
        assert!(request.starts_with("POST /api/transact.php HTTP/1.1"));
        let (_, body) = request
            .split_once("\r\n\r\n")
            .expect("captured request should contain a body");
        let form: std::collections::HashMap<_, _> = form_urlencoded::parse(body.as_bytes())
            .into_owned()
            .collect();
        assert_eq!(
            form.get("dup_seconds").map(String::as_str),
            expected_wire_seconds
        );
        assert!(!form.contains_key("duplicate_check_seconds"));
        server.await.expect("server task should finish");
    }
}

#[tokio::test]
async fn legacy_factory_client_omits_the_invalid_zero_override_on_classic_wire() {
    let response = b"response=1&responsetext=Approved&authcode=AUTH&transactionid=txn_legacy_duplicate_policy&customer_vault_id=vault_legacy_duplicate_policy&avsresponse=&cvvresponse=&orderid=ck_order&type=sale&response_code=100".to_vec();
    let (client, request_receiver, server) =
        spawn_capturing_server_with_legacy_client("HTTP/1.1 200 OK", "text/plain", response).await;

    let mut request = test_sale_request(PaymentSource::PaymentToken(
        "tok_legacy_duplicate_policy".to_owned(),
    ));
    request.vault_action = Some(VaultAction::AddCustomer);
    request.stored_credential = Some(StoredCredential::InitialCustomer);
    let outcome = client
        .sale(request)
        .await
        .expect("legacy Classic sale should parse");
    assert_eq!(outcome.status, PaymentStatus::Approved);

    let request = request_receiver.await.expect("request should be captured");
    let (_, body) = request
        .split_once("\r\n\r\n")
        .expect("captured request should contain a body");
    let form: std::collections::HashMap<_, _> = form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect();
    assert!(!form.contains_key("dup_seconds"));
    assert!(!form.contains_key("duplicate_check_seconds"));
    server.await.expect("server task should finish");
}

#[tokio::test]
async fn legacy_factory_client_omits_the_invalid_zero_override_on_v5_wire() {
    let (client, request_receiver, server) = spawn_capturing_server_with_legacy_client(
        "HTTP/1.1 200 OK",
        "application/json",
        br#"{"id":"txn_legacy_v5_duplicate_policy","status":"approved"}"#.to_vec(),
    )
    .await;

    let outcome = client
        .sale(test_sale_request(PaymentSource::PaymentToken(
            "tok_legacy_v5_duplicate_policy".to_owned(),
        )))
        .await
        .expect("legacy v5 sale should parse");
    assert_eq!(outcome.status, PaymentStatus::Approved);

    let request = request_receiver.await.expect("request should be captured");
    let (_, body) = request
        .split_once("\r\n\r\n")
        .expect("captured request should contain a body");
    let body: Value = serde_json::from_str(body).expect("sale body should be JSON");
    assert!(body.get("dup_seconds").is_none());
    assert!(body.get("duplicate_check_seconds").is_none());
    server.await.expect("server task should finish");
}

#[tokio::test]
async fn duplicate_check_policy_does_not_reach_store_payment_method_wire() {
    let response = b"response=1&responsetext=Approved&authcode=AUTH&transactionid=txn_store_policy_isolation&customer_vault_id=vault_store_policy_isolation&avsresponse=&cvvresponse=&orderid=ck_store_policy_isolation&type=validate&response_code=100".to_vec();
    let (client, request_receiver, server) = spawn_capturing_server_with_duplicate_check(
        "HTTP/1.1 200 OK",
        "text/plain",
        response,
        DuplicateCheck::Window(
            crate::DuplicateCheckWindow::new(120).expect("window should be valid"),
        ),
    )
    .await;

    let outcome = client
        .store_payment_method(StorePaymentMethodRequest {
            payment_token: "tok_store_policy_isolation".to_owned(),
            order_id: "ck_store_policy_isolation".to_owned(),
            billing_contact: None,
        })
        .await
        .expect("store-payment-method response should parse");
    assert_eq!(outcome.status, PaymentStatus::Approved);

    let request = request_receiver.await.expect("request should be captured");
    assert!(request.starts_with("POST /api/transact.php HTTP/1.1"));
    let (_, body) = request
        .split_once("\r\n\r\n")
        .expect("captured request should contain a body");
    let form: std::collections::HashMap<_, _> = form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect();
    assert_eq!(form.get("type").map(String::as_str), Some("validate"));
    assert!(!form.contains_key("dup_seconds"));
    assert!(!form.contains_key("duplicate_check_seconds"));
    server.await.expect("server task should finish");
}

#[test]
fn sale_currency_is_usd_only_for_amount_cents_contract() {
    assert!(ensure_supported_sale_currency("USD").is_ok());
    assert!(matches!(
        ensure_supported_sale_currency("JPY"),
        Err(WireError::LocalInvalidRequest(message)) if message.contains("unsupported")
    ));
}

#[test]
fn endpoint_requires_https_except_explicit_loopback_http() {
    assert!(Client::new("https://payarc.transactiongateway.com", "private", "query").is_ok());
    assert!(Client::new("https://payments.example.test", "private", "query").is_ok());
    assert!(Client::new("http://localhost:8080", "private", "query").is_ok());
    assert!(Client::new("http://127.0.0.1:8080", "private", "query").is_ok());
    assert!(Client::new("http://[::1]:8080", "private", "query").is_ok());
    for non_loopback in [
        "http://nmi.provider.localhost:8080",
        "http://payments.example.test",
    ] {
        assert!(matches!(
            Client::new(non_loopback, "private", "query"),
            Err(ConfigurationError::EndpointMustUseLoopbackHttp)
        ));
    }
    assert!(matches!(
        Client::new("https://user:pass@secure.example.test", "private", "query"),
        Err(ConfigurationError::EndpointInvalid)
    ));
    assert!(matches!(
        Client::new("http://user:pass@localhost:8080", "private", "query"),
        Err(ConfigurationError::EndpointInvalid)
    ));
    assert!(matches!(
        Client::new("https://secure.example.test/api", "private", "query"),
        Err(ConfigurationError::EndpointInvalid)
    ));
}

#[test]
fn http_status_mapping_keeps_gateway_errors_from_becoming_card_declines() {
    assert!(matches!(
        gateway_error_for_http_status(StatusCode::BAD_REQUEST, "bad token".to_owned()),
        WireError::RequestRejected(_)
    ));
    assert!(matches!(
        gateway_error_for_http_status(StatusCode::UNAUTHORIZED, "bad key".to_owned()),
        WireError::Configuration(_)
    ));
    assert!(matches!(
        gateway_error_for_http_status(StatusCode::TOO_MANY_REQUESTS, "slow down".to_owned()),
        WireError::TransportRateLimited(_)
    ));
    for status in [StatusCode::NOT_FOUND, StatusCode::METHOD_NOT_ALLOWED] {
        assert!(matches!(
            gateway_error_for_http_status(status, "endpoint rejected".to_owned()),
            WireError::Configuration(_)
        ));
    }
    for status in [
        StatusCode::INTERNAL_SERVER_ERROR,
        StatusCode::BAD_GATEWAY,
        StatusCode::SERVICE_UNAVAILABLE,
        StatusCode::GATEWAY_TIMEOUT,
    ] {
        assert!(matches!(
            gateway_error_for_http_status(status, "server failed".to_owned()),
            WireError::Indeterminate(_)
        ));
    }
    assert!(matches!(
        gateway_error_for_http_status(StatusCode::FOUND, "redirect".to_owned()),
        WireError::Indeterminate(_)
    ));
    assert!(matches!(
        gateway_error_for_http_status(
            StatusCode::SWITCHING_PROTOCOLS,
            "unexpected protocol switch".to_owned()
        ),
        WireError::Indeterminate(_)
    ));
}

#[test]
fn query_errors_preserve_rate_limits_and_reclassify_indeterminate_failures() {
    assert!(matches!(
        WireError::Indeterminate("transport".to_owned()).into_query(),
        QueryError::Unavailable(_)
    ));
    assert!(matches!(
        WireError::RateLimited("slow down".to_owned()).into_query(),
        QueryError::RateLimited(_)
    ));
    assert!(matches!(
        WireError::TransportRateLimited("slow down".to_owned()).into_query(),
        QueryError::RateLimited(_)
    ));
    assert!(matches!(
        WireError::TransportRateLimited("slow down".to_owned()).into_mutation(),
        MutationError::RateLimitedIndeterminate(_)
    ));
    assert!(matches!(
        WireError::MalformedResponse("xml".to_owned()).into_query(),
        QueryError::MalformedResponse(_)
    ));
    assert!(matches!(
        WireError::MalformedResponse("partial payment response".to_owned()).into_mutation(),
        MutationError::Indeterminate(_)
    ));
}

#[test]
fn non_json_http_errors_are_mapped_by_status() {
    assert!(matches!(
        gateway_error_from_http_response(StatusCode::UNAUTHORIZED, "<html>bad key</html>"),
        WireError::Configuration(_)
    ));
    assert!(matches!(
        gateway_error_from_http_response(StatusCode::TOO_MANY_REQUESTS, ""),
        WireError::TransportRateLimited(_)
    ));
}

#[tokio::test]
async fn classic_301_is_a_known_not_submitted_rate_limit() {
    let response = b"response=3&responsetext=Rate+limit+exceeded&authcode=&transactionid=&avsresponse=&cvvresponse=&orderid=&type=&response_code=301".to_vec();
    let (client, request_receiver, server) =
        spawn_capturing_server("HTTP/1.1 200 OK", "text/plain", response).await;

    let error = client
        .store_payment_method(StorePaymentMethodRequest {
            payment_token: "tok_rate_limited".to_owned(),
            order_id: "ck_rate_limited".to_owned(),
            billing_contact: None,
        })
        .await
        .expect_err("Classic response code 301 must be surfaced as a rate limit");

    assert!(matches!(error, MutationError::RateLimited(_)));
    assert_eq!(error.certainty(), crate::MutationCertainty::NotSubmitted);
    let request = request_receiver.await.expect("request should be captured");
    assert!(request.starts_with("POST /api/transact.php HTTP/1.1"));
    server.await.expect("server task should finish");
}

#[tokio::test]
async fn http_429_sale_remains_indeterminate() {
    let (client, request_receiver, server) = spawn_capturing_server(
        "HTTP/1.1 429 Too Many Requests",
        "application/json",
        br#"{"response_text":"Rate limit exceeded"}"#.to_vec(),
    )
    .await;

    let error = client
        .sale(SaleRequest {
            amount_cents: 100,
            currency: "USD".to_owned(),
            order_id: "ck_http_rate_limited".to_owned(),
            source: PaymentSource::PaymentToken("tok_http_rate_limited".to_owned()),
            vault_action: None,
            stored_credential: None,
            billing_contact: None,
        })
        .await
        .expect_err("HTTP 429 must preserve mutation uncertainty");

    assert!(matches!(error, MutationError::RateLimitedIndeterminate(_)));
    assert_eq!(error.certainty(), crate::MutationCertainty::Indeterminate);
    let request = request_receiver.await.expect("request should be captured");
    assert!(request.starts_with("POST /api/v5/payments/sale HTTP/1.1"));
    server.await.expect("server task should finish");
}

#[tokio::test]
async fn non_success_sale_with_payment_decision_evidence_is_indeterminate() {
    for (status, order_id) in [
        ("HTTP/1.1 400 Bad Request", "ck_http_400_duplicate"),
        ("HTTP/1.1 422 Unprocessable Entity", "ck_http_422_duplicate"),
    ] {
        let (client, request_receiver, server) = spawn_capturing_server(
            status,
            "application/json",
            br#"{"response":"3","response_code":"430","response_text":"Duplicate transaction"}"#
                .to_vec(),
        )
        .await;

        let error = client
            .sale(SaleRequest {
                amount_cents: 100,
                currency: "USD".to_owned(),
                order_id: order_id.to_owned(),
                source: PaymentSource::PaymentToken("tok_http_duplicate".to_owned()),
                vault_action: None,
                stored_credential: None,
                billing_contact: None,
            })
            .await
            .expect_err("non-success response with payment evidence must be indeterminate");

        assert!(matches!(error, MutationError::Indeterminate(_)), "{status}");
        assert_eq!(
            error.certainty(),
            crate::MutationCertainty::Indeterminate,
            "{status}"
        );
        request_receiver.await.expect("request should be captured");
        server.await.expect("server task should finish");
    }
}

#[tokio::test]
async fn documented_v5_validation_error_is_known_not_submitted() {
    let (client, request_receiver, server) = spawn_capturing_server(
        "HTTP/1.1 400 Bad Request",
        "application/json",
        br#"{"type":"validationError","error_code":"E_INVALID_SUBMISSION","message":"The provided data is invalid.","ref_id":null,"details":[{"fieldName":"amount","message":"This field is required."}]}"#.to_vec(),
    )
    .await;

    let error = client
        .sale(SaleRequest {
            amount_cents: 100,
            currency: "USD".to_owned(),
            order_id: "ck_http_validation".to_owned(),
            source: PaymentSource::PaymentToken("tok_http_validation".to_owned()),
            vault_action: None,
            stored_credential: None,
            billing_contact: None,
        })
        .await
        .expect_err("documented validation failure must be surfaced");

    assert!(matches!(error, MutationError::RequestRejected(_)));
    assert_eq!(error.certainty(), crate::MutationCertainty::NotSubmitted);
    request_receiver.await.expect("request should be captured");
    server.await.expect("server task should finish");
}

#[tokio::test]
async fn only_the_documented_v5_validation_envelope_proves_http_400_non_submission() {
    for (body, suffix) in [
        (br#"{}"#.as_slice(), "empty_object"),
        (br#"{not-json"#.as_slice(), "malformed"),
        (br#"{"id":"txn_exists"}"#.as_slice(), "transaction_id"),
        (
            br#"{"transaction":{"response_code":"430"}}"#.as_slice(),
            "nested_decision",
        ),
        (
            br#"{"actions":[{"id":"action_exists"}]}"#.as_slice(),
            "action",
        ),
        (br#"{"status":400}"#.as_slice(), "generic_status"),
        (
            br#"{"type":"validationError","error_code":"E_INVALID_SUBMISSION","message":"invalid","details":[]}"#.as_slice(),
            "empty_validation_details",
        ),
        (
            br#"{"type":"validationError","error_code":"E_INVALID_SUBMISSION","message":"invalid"}"#.as_slice(),
            "missing_validation_details",
        ),
        (
            br#"{"type":"validationError","error_code":"E_INVALID_SUBMISSION","message":"invalid","details":[{"fieldName":"amount","message":"required"}],"timestamp":"future-drift"}"#.as_slice(),
            "extra_validation_field",
        ),
        (
            br#"{"type":"validationError","error_code":"E_INVALID_SUBMISSION","message":"invalid","details":[{"fieldName":"amount","message":"required","code":"future-drift"}]}"#.as_slice(),
            "extra_validation_detail_field",
        ),
        (
            br#"{"type":"validationError","error_code":"E_INVALID_SUBMISSION","message":"invalid","ref_id":123,"details":[{"fieldName":"amount","message":"required"}]}"#.as_slice(),
            "invalid_ref_id",
        ),
        (
            br#"{"type":"validationError","error_code":"E_INVALID_SUBMISSION","message":"invalid","details":[],"id":"txn_exists"}"#.as_slice(),
            "validation_with_transaction",
        ),
        (
            br#"{"type":"validationError","type":"validationError","error_code":"E_INVALID_SUBMISSION","message":"invalid","details":[]}"#.as_slice(),
            "duplicate_validation_field",
        ),
    ] {
        let (client, request_receiver, server) = spawn_capturing_server(
            "HTTP/1.1 400 Bad Request",
            "application/json",
            body.to_vec(),
        )
        .await;

        let error = client
            .sale(SaleRequest {
                amount_cents: 100,
                currency: "USD".to_owned(),
                order_id: format!("ck_unproven_validation_{suffix}"),
                source: PaymentSource::PaymentToken("tok_unproven_validation".to_owned()),
                vault_action: None,
                stored_credential: None,
                billing_contact: None,
            })
            .await
            .expect_err("an unproven HTTP 400 envelope must remain reconcilable");

        assert!(matches!(error, MutationError::Indeterminate(_)), "{suffix}");
        assert_eq!(
            error.certainty(),
            crate::MutationCertainty::Indeterminate,
            "{suffix}"
        );
        request_receiver.await.expect("request should be captured");
        server.await.expect("server task should finish");
    }
}

#[tokio::test]
async fn unreadable_http_400_payment_bodies_never_prove_non_submission() {
    for (body, suffix) in [
        (vec![0xff, 0xfe], "non_utf8"),
        (vec![b'a'; MAX_NMI_STANDARD_RESPONSE_BYTES + 1], "oversized"),
    ] {
        let (client, request_receiver, server) =
            spawn_capturing_server("HTTP/1.1 400 Bad Request", "application/octet-stream", body)
                .await;

        let error = client
            .sale(SaleRequest {
                amount_cents: 100,
                currency: "USD".to_owned(),
                order_id: format!("ck_unreadable_validation_{suffix}"),
                source: PaymentSource::PaymentToken("tok_unreadable_validation".to_owned()),
                vault_action: None,
                stored_credential: None,
                billing_contact: None,
            })
            .await
            .expect_err("an unreadable HTTP 400 body must remain reconcilable");

        assert!(matches!(error, MutationError::Indeterminate(_)), "{suffix}");
        assert_eq!(error.certainty(), crate::MutationCertainty::Indeterminate);
        request_receiver.await.expect("request should be captured");
        server.await.expect("server task should finish");
    }
}

#[tokio::test]
async fn interrupted_http_400_payment_body_never_proves_non_submission() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let base_url = format!("http://{}", listener.local_addr().expect("local address"));
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("request should connect");
        let mut request = Vec::new();
        let mut chunk = [0_u8; 1024];
        let header_end = loop {
            let read = tokio::io::AsyncReadExt::read(&mut stream, &mut chunk)
                .await
                .expect("request headers should read");
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
            let read = tokio::io::AsyncReadExt::read(&mut stream, &mut chunk)
                .await
                .expect("request body should read");
            assert!(read > 0, "request closed before body");
            request.extend_from_slice(&chunk[..read]);
        }
        tokio::io::AsyncWriteExt::write_all(
            &mut stream,
            b"HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: 128\r\nconnection: close\r\n\r\n{",
        )
        .await
        .expect("partial response should write");
    });
    let client =
        Client::new(base_url, "private_key", "query_key").expect("loopback client should build");

    let error = client
        .sale(test_sale_request(PaymentSource::PaymentToken(
            "tok_interrupted_400".to_owned(),
        )))
        .await
        .expect_err("an interrupted HTTP 400 body must remain reconcilable");

    assert!(matches!(error, MutationError::Indeterminate(_)));
    assert_eq!(error.certainty(), crate::MutationCertainty::Indeterminate);
    server.await.expect("server task should finish");
}

#[tokio::test]
async fn classic_non_success_payment_responses_never_hide_processing_evidence() {
    for (status, body, suffix) in [
        (
            "HTTP/1.1 400 Bad Request",
            b"invalid request".as_slice(),
            "undocumented_400",
        ),
        (
            "HTTP/1.1 404 Not Found",
            b"response=3&response_code=430&transactionid=txn_exists".as_slice(),
            "decision_on_404",
        ),
        (
            "HTTP/1.1 404 Not Found",
            br#"{"response":"3","response_code":"430","id":"txn_exists"}"#.as_slice(),
            "json_decision_on_404",
        ),
    ] {
        let (client, request_receiver, server) =
            spawn_capturing_server(status, "text/plain", body.to_vec()).await;

        let error = client
            .store_payment_method(StorePaymentMethodRequest {
                payment_token: "tok_classic_non_success".to_owned(),
                order_id: format!("ck_classic_non_success_{suffix}"),
                billing_contact: None,
            })
            .await
            .expect_err("Classic non-success mutation response must remain reconcilable");

        assert!(matches!(error, MutationError::Indeterminate(_)), "{suffix}");
        assert_eq!(error.certainty(), crate::MutationCertainty::Indeterminate);
        request_receiver.await.expect("request should be captured");
        server.await.expect("server task should finish");
    }
}

#[tokio::test]
async fn classic_mutation_certainty_is_derived_from_the_endpoint_path() {
    let (client, request_receiver, server) = spawn_capturing_server(
        "HTTP/1.1 400 Bad Request",
        "text/plain",
        b"invalid request".to_vec(),
    )
    .await;
    let request = StorePaymentMethodRequest {
        payment_token: "tok_endpoint_purpose".to_owned(),
        order_id: "ck_endpoint_purpose".to_owned(),
        billing_contact: None,
    };
    let params = classic_store_payment_method_params("private_key", &request);

    let error = client
        .post_form_text("/api/transact.php", &params)
        .await
        .expect_err("the mutation endpoint must override the query-shaped helper name");

    assert!(matches!(error, WireError::Indeterminate(_)));
    request_receiver.await.expect("request should be captured");
    server.await.expect("server task should finish");
}

#[tokio::test]
async fn v5_non_success_payment_responses_never_hide_form_processing_evidence() {
    for (status, suffix) in [
        ("HTTP/1.1 401 Unauthorized", "unauthorized"),
        ("HTTP/1.1 403 Forbidden", "forbidden"),
        ("HTTP/1.1 404 Not Found", "not_found"),
        ("HTTP/1.1 405 Method Not Allowed", "method_not_allowed"),
    ] {
        let (client, request_receiver, server) = spawn_capturing_server(
            status,
            "text/plain",
            b"response=3&response_code=430&transactionid=txn_exists".to_vec(),
        )
        .await;

        let error = client
            .sale(test_sale_request(PaymentSource::PaymentToken(format!(
                "tok_v5_form_evidence_{suffix}"
            ))))
            .await
            .expect_err("v5 form-encoded payment evidence must remain reconcilable");

        assert!(matches!(error, MutationError::Indeterminate(_)), "{suffix}");
        assert_eq!(
            error.certainty(),
            crate::MutationCertainty::Indeterminate,
            "{suffix}"
        );
        request_receiver.await.expect("request should be captured");
        server.await.expect("server task should finish");
    }
}

#[tokio::test]
async fn generic_http_status_does_not_masquerade_as_payment_evidence() {
    for (body, suffix) in [
        (
            br#"{"type":"notFound","status":404,"message":"missing endpoint"}"#.as_slice(),
            "numeric_json_status",
        ),
        (
            br#"{"type":"notFound","status":"404","message":"missing endpoint"}"#.as_slice(),
            "text_json_status",
        ),
        (br#"[]"#.as_slice(), "non_object_json"),
        (
            b"<html><a href='?response=1'>missing endpoint</a></html>".as_slice(),
            "html_with_form_like_fragment",
        ),
    ] {
        let (client, request_receiver, server) =
            spawn_capturing_server("HTTP/1.1 404 Not Found", "application/json", body.to_vec())
                .await;

        let error = client
            .sale(test_sale_request(PaymentSource::PaymentToken(format!(
                "tok_generic_status_{suffix}"
            ))))
            .await
            .expect_err("a missing endpoint must remain a configuration error");

        assert!(matches!(error, MutationError::Configuration(_)), "{suffix}");
        assert_eq!(
            error.certainty(),
            crate::MutationCertainty::NotSubmitted,
            "{suffix}"
        );
        request_receiver.await.expect("request should be captured");
        server.await.expect("server task should finish");
    }
}

#[tokio::test]
async fn unrecognized_json_object_on_non_success_sale_fails_closed() {
    for (body, suffix) in [
        (
            br#"{"result":{"transaction_id":"txn_exists"}}"#.as_slice(),
            "unknown_container",
        ),
        (br#"{}"#.as_slice(), "empty_object"),
        (
            br#"{"type":"notFound","message":"missing","future":null}"#.as_slice(),
            "extended_error",
        ),
        (
            br#"{"type":"notFound","message":"missing","ref_id":123}"#.as_slice(),
            "invalid_ref_id",
        ),
        (
            br#"{"type":"transaction","status":404}"#.as_slice(),
            "type_is_not_an_error_proof",
        ),
    ] {
        let (client, request_receiver, server) =
            spawn_capturing_server("HTTP/1.1 404 Not Found", "application/json", body.to_vec())
                .await;

        let error = client
            .sale(test_sale_request(PaymentSource::PaymentToken(format!(
                "tok_unknown_error_object_{suffix}"
            ))))
            .await
            .expect_err("an unrecognized structured error cannot prove non-submission");

        assert!(matches!(error, MutationError::Indeterminate(_)), "{suffix}");
        assert_eq!(
            error.certainty(),
            crate::MutationCertainty::Indeterminate,
            "{suffix}"
        );
        request_receiver.await.expect("request should be captured");
        server.await.expect("server task should finish");
    }
}

#[tokio::test]
async fn nonempty_top_level_json_array_on_non_success_sale_fails_closed() {
    let (client, request_receiver, server) = spawn_capturing_server(
        "HTTP/1.1 404 Not Found",
        "application/json",
        br#"[{"id":"txn_exists"}]"#.to_vec(),
    )
    .await;

    let error = client
        .sale(test_sale_request(PaymentSource::PaymentToken(
            "tok_array_evidence".to_owned(),
        )))
        .await
        .expect_err("an unrecognized non-empty payment envelope must remain reconcilable");

    assert!(matches!(error, MutationError::Indeterminate(_)));
    assert_eq!(error.certainty(), crate::MutationCertainty::Indeterminate);
    request_receiver.await.expect("request should be captured");
    server.await.expect("server task should finish");
}

#[tokio::test]
async fn anomalous_status_fields_are_payment_evidence_on_non_success_responses() {
    for (body, suffix) in [
        (br#"{"status":"processor_surprise"}"#.as_slice(), "unknown"),
        (br#"{"status":{}}"#.as_slice(), "invalid_shape"),
        (
            br#"{"status":"404","status":"processor_surprise"}"#.as_slice(),
            "conflicting",
        ),
        (br#"{"status":" 404 "}"#.as_slice(), "noncanonical_http"),
        (br#"{"status":401}"#.as_slice(), "mismatched_http"),
    ] {
        let (client, request_receiver, server) =
            spawn_capturing_server("HTTP/1.1 404 Not Found", "application/json", body.to_vec())
                .await;

        let error = client
            .sale(test_sale_request(PaymentSource::PaymentToken(format!(
                "tok_anomalous_status_{suffix}"
            ))))
            .await
            .expect_err("an anomalous status field must remain reconcilable");

        assert!(matches!(error, MutationError::Indeterminate(_)), "{suffix}");
        assert_eq!(
            error.certainty(),
            crate::MutationCertainty::Indeterminate,
            "{suffix}"
        );
        request_receiver.await.expect("request should be captured");
        server.await.expect("server task should finish");
    }
}

#[tokio::test]
async fn classic_status_fields_distinguish_http_metadata_from_payment_evidence() {
    for (body, expected_indeterminate, suffix) in [
        (b"status=404".as_slice(), false, "matching_http"),
        (b"status=processor_surprise".as_slice(), true, "unknown"),
        (
            b"status=404&status=processor_surprise".as_slice(),
            true,
            "conflicting",
        ),
        (b"status=%20404%20".as_slice(), true, "noncanonical_http"),
        (b"status=401".as_slice(), true, "mismatched_http"),
    ] {
        let (client, request_receiver, server) =
            spawn_capturing_server("HTTP/1.1 404 Not Found", "text/plain", body.to_vec()).await;

        let error = client
            .store_payment_method(StorePaymentMethodRequest {
                payment_token: "tok_classic_status".to_owned(),
                order_id: format!("ck_classic_status_{suffix}"),
                billing_contact: None,
            })
            .await
            .expect_err("a non-success Classic payment must fail");

        if expected_indeterminate {
            assert!(matches!(error, MutationError::Indeterminate(_)), "{suffix}");
            assert_eq!(
                error.certainty(),
                crate::MutationCertainty::Indeterminate,
                "{suffix}"
            );
        } else {
            assert!(matches!(error, MutationError::Configuration(_)), "{suffix}");
            assert_eq!(
                error.certainty(),
                crate::MutationCertainty::NotSubmitted,
                "{suffix}"
            );
        }
        request_receiver.await.expect("request should be captured");
        server.await.expect("server task should finish");
    }
}

#[tokio::test]
async fn undocumented_http_422_sale_without_payment_evidence_is_indeterminate() {
    let (client, request_receiver, server) = spawn_capturing_server(
        "HTTP/1.1 422 Unprocessable Entity",
        "application/json",
        br#"{"message":"unprocessable"}"#.to_vec(),
    )
    .await;

    let error = client
        .sale(SaleRequest {
            amount_cents: 100,
            currency: "USD".to_owned(),
            order_id: "ck_http_422".to_owned(),
            source: PaymentSource::PaymentToken("tok_http_422".to_owned()),
            vault_action: None,
            stored_credential: None,
            billing_contact: None,
        })
        .await
        .expect_err("undocumented HTTP 422 must preserve mutation uncertainty");

    assert!(matches!(error, MutationError::Indeterminate(_)));
    assert_eq!(error.certainty(), crate::MutationCertainty::Indeterminate);
    request_receiver.await.expect("request should be captured");
    server.await.expect("server task should finish");
}

#[tokio::test]
async fn http_429_query_preserves_the_rate_limit_signal() {
    let (client, request_receiver, server) = spawn_capturing_server(
        "HTTP/1.1 429 Too Many Requests",
        "text/plain",
        b"Rate limit exceeded".to_vec(),
    )
    .await;

    let error = client
        .query_transaction(TransactionQuery {
            transaction_id: Some("txn_rate_limited_query".to_owned()),
            order_id: None,
        })
        .await
        .expect_err("HTTP 429 must remain retryable at the query policy boundary");

    assert!(matches!(error, QueryError::RateLimited(_)));
    let request = request_receiver.await.expect("request should be captured");
    assert!(request.starts_with("POST /api/query.php HTTP/1.1"));
    server.await.expect("server task should finish");
}

#[tokio::test]
async fn http_422_query_remains_a_permanent_invalid_request() {
    let (client, request_receiver, server) = spawn_capturing_server(
        "HTTP/1.1 422 Unprocessable Entity",
        "application/json",
        br#"{"message":"invalid query"}"#.to_vec(),
    )
    .await;

    let error = client
        .query_transaction(TransactionQuery {
            transaction_id: Some("txn_invalid_query".to_owned()),
            order_id: None,
        })
        .await
        .expect_err("query HTTP 422 must not become a transient outage");

    assert!(matches!(error, QueryError::InvalidRequest(_)));
    request_receiver.await.expect("request should be captured");
    server.await.expect("server task should finish");
}

#[test]
fn response_code_301_requires_absent_transaction_and_lifecycle_evidence() {
    for classic in [
        "response=3&response_code=301&transactionid=txn_already_assigned",
        "response=3&response_code=301&transactionid=&status=approved",
    ] {
        let outcome = classic_payment_outcome_from_form(classic)
            .expect("ambiguous Classic 301 response must remain an outcome");
        assert_eq!(outcome.status, PaymentStatus::Unknown);
    }

    for json in [
        r#"{"response":"3","response_code":"301","id":"txn_already_assigned"}"#,
        r#"{"response":"3","response_code":"301","id":null,"status":"approved"}"#,
    ] {
        let outcome = payment_outcome_from_json_text(json)
            .expect("ambiguous JSON 301 response must remain an outcome");
        assert_eq!(outcome.status, PaymentStatus::Unknown);
    }
}

#[test]
fn http_error_body_detail_crosses_boundary_in_redacted_wrapper() {
    let error = gateway_error_from_http_response(
        StatusCode::BAD_REQUEST,
        r#"{"response_text":"processor card_number=4111111111111111"}"#,
    )
    .into_mutation();

    let MutationError::RequestRejected(detail) = error else {
        panic!("bad request should map to request-rejected gateway error");
    };
    assert_eq!(detail.expose(), "processor card_number=4111111111111111");
    assert!(!format!("{detail:?}").contains("4111111111111111"));
    assert!(!format!("{detail}").contains("4111111111111111"));
}
