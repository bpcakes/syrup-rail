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
            order_id: "ck_http_rate_limited".to_owned(),
            intent: SaleIntent::PaymentToken("tok_http_rate_limited".to_owned()),
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
                order_id: order_id.to_owned(),
                intent: SaleIntent::PaymentToken("tok_http_duplicate".to_owned()),
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
            order_id: "ck_http_validation".to_owned(),
            intent: SaleIntent::PaymentToken("tok_http_validation".to_owned()),
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
                order_id: format!("ck_unproven_validation_{suffix}"),
                intent: SaleIntent::PaymentToken("tok_unproven_validation".to_owned()),
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
                order_id: format!("ck_unreadable_validation_{suffix}"),
                intent: SaleIntent::PaymentToken("tok_unreadable_validation".to_owned()),
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
            order_id: "ck_http_422".to_owned(),
            intent: SaleIntent::PaymentToken("tok_http_422".to_owned()),
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
