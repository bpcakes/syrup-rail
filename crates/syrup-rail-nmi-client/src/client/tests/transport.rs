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
        order_id: order_id.to_owned(),
        intent: SaleIntent::AddCustomer {
            payment_token: "tok_round_trip".to_owned(),
        },
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
            order_id: "ck_order_auth_header".to_owned(),
            intent: SaleIntent::PaymentToken("tok_auth_header".to_owned()),
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
async fn v5_recurring_merchant_sale_sends_scheduled_billing_metadata() {
    let (client, request_receiver, server) = spawn_capturing_server(
        "HTTP/1.1 200 OK",
        "application/json",
        br#"{"id":"txn_recurring","status":"approved"}"#.to_vec(),
    )
    .await;

    let outcome = client
        .sale(SaleRequest {
            amount_cents: 4_900,
            order_id: "ck_recurring_order".to_owned(),
            intent: SaleIntent::RecurringStoredCredential {
                customer_vault_id: "vault_recurring".to_owned(),
                initial_transaction_id: "txn_initial".to_owned(),
            },
            billing_contact: None,
        })
        .await
        .expect("recurring v5 sale should parse");
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
    assert!(request.starts_with("POST /api/v5/payments/sale HTTP/1.1"));
    let (_, body) = request
        .split_once("\r\n\r\n")
        .expect("captured request should contain a body");
    let body: Value = serde_json::from_str(body).expect("sale body should be JSON");
    assert_eq!(
        body,
        json!({
            "amount": "49.00",
            "currency": "USD",
            "payment_details": {
                "customer_vault_id": "vault_recurring"
            },
            "order_details": {
                "id": "ck_recurring_order"
            },
            "customer_vault": {
                "billing_method": "recurring"
            },
            "cit_mit": {
                "stored_credential_indicator": "used",
                "initiated_by": "merchant",
                "initial_transaction_id": "txn_initial"
            }
        })
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
            order_id: "ck_invalid_json".to_owned(),
            intent: SaleIntent::PaymentToken("tok_invalid_json".to_owned()),
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
            order_id: "ck_connect_failure".to_owned(),
            intent: SaleIntent::PaymentToken("tok_connect_failure".to_owned()),
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
            order_id: "ck_established_reset".to_owned(),
            intent: SaleIntent::PaymentToken("tok_established_reset".to_owned()),
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
    .client_with_duplicate_check(
        Endpoint::parse_loopback_http(endpoint_url).expect("test endpoint should validate"),
        Credentials::new("private_key".to_owned(), "query_key".to_owned())
            .expect("test credentials should validate"),
        DuplicateCheck::ProcessorConfigured,
    )
    .expect("explicit loopback client should construct");

    let error = client
        .sale(SaleRequest {
            amount_cents: 100,
            order_id: "ck_refused_stream".to_owned(),
            intent: SaleIntent::PaymentToken("tok_refused_stream".to_owned()),
            billing_contact: None,
        })
        .await
        .expect_err("protocol NACK should make the sale indeterminate");
    assert!(matches!(error, MutationError::Indeterminate(_)));
    assert_eq!(error.certainty(), crate::MutationCertainty::Indeterminate);
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        1,
        "NMI mutation transport must never replay a protocol NACK"
    );
    server.abort();
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
            order_id: "ck_oversized_response".to_owned(),
            intent: SaleIntent::PaymentToken("tok_oversized_response".to_owned()),
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
        order_id: "ck_order_123".to_owned(),
        intent: SaleIntent::PaymentToken("tok_test".to_owned()),
        billing_contact: None,
    };
    let body = sale_body_json(
        &request,
        amount_value(request.amount_cents).expect("positive cents should format"),
        DuplicateCheck::ProcessorConfigured,
    );
    assert_eq!(body.get("currency"), Some(&json!("USD")));
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

        let request = SaleRequest {
            amount_cents: 100,
            order_id: "ck_classic_duplicate_policy".to_owned(),
            intent: SaleIntent::InitialStoredCredential {
                payment_token: "tok_classic_duplicate_policy".to_owned(),
            },
            billing_contact: None,
        };
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

    let request = SaleRequest {
        amount_cents: 100,
        order_id: "ck_legacy_duplicate_policy".to_owned(),
        intent: SaleIntent::InitialStoredCredential {
            payment_token: "tok_legacy_duplicate_policy".to_owned(),
        },
        billing_contact: None,
    };
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
fn sale_currency_is_fixed_by_the_amount_cents_contract() {
    let request = test_sale_request(PaymentSource::PaymentToken("token".to_owned()));
    let body = sale_body_json(
        &request,
        amount_value(100).expect("valid amount"),
        DuplicateCheck::ProcessorConfigured,
    );
    assert_eq!(body.get("currency"), Some(&json!("USD")));
}

include!("transport/http_semantics.rs");
