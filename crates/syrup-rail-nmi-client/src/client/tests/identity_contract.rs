use super::*;

#[tokio::test]
async fn public_sale_downgrades_approval_without_transaction_identity() {
    let (client, request_receiver, server) = spawn_capturing_server(
        "HTTP/1.1 200 OK",
        "application/json",
        br#"{"status":"approved"}"#.to_vec(),
    )
    .await;

    let outcome = client
        .sale(test_sale_request(PaymentSource::PaymentToken(
            "tok_missing_transaction".to_owned(),
        )))
        .await
        .expect("missing identity is an anomalous outcome, not a transport failure");

    assert_eq!(outcome.status(), PaymentStatus::Unknown);
    assert_eq!(
        outcome.diagnostics(),
        &[PaymentOutcomeDiagnostic::MissingTransactionIdentifier]
    );
    assert!(outcome.into_parts().transaction_id.is_none());
    let request = request_receiver.await.expect("request should be captured");
    assert!(request.starts_with("POST /api/v5/payments/sale HTTP/1.1"));
    server.await.expect("server task should finish");
}

#[tokio::test]
async fn public_sale_treats_an_empty_approved_transaction_identity_as_missing() {
    let (client, request_receiver, server) = spawn_capturing_server(
        "HTTP/1.1 200 OK",
        "application/json",
        br#"{"status":"approved","id":""}"#.to_vec(),
    )
    .await;

    let outcome = client
        .sale(test_sale_request(PaymentSource::PaymentToken(
            "tok_empty_transaction".to_owned(),
        )))
        .await
        .expect("an empty approved identity is an anomalous outcome");

    assert_eq!(outcome.status(), PaymentStatus::Unknown);
    assert_eq!(
        outcome.diagnostics(),
        &[PaymentOutcomeDiagnostic::MissingTransactionIdentifier]
    );
    assert!(outcome.into_parts().transaction_id.is_none());
    let request = request_receiver.await.expect("request should be captured");
    assert!(request.starts_with("POST /api/v5/payments/sale HTTP/1.1"));
    server.await.expect("server task should finish");
}

#[tokio::test]
async fn public_vault_creation_sale_requires_transaction_and_vault_identities() {
    let (client, request_receiver, server) = spawn_capturing_server(
        "HTTP/1.1 200 OK",
        "text/plain",
        b"response=1&response_code=100&transactionid=txn_without_vault".to_vec(),
    )
    .await;
    let mut request = test_sale_request(PaymentSource::PaymentToken("tok_create_vault".to_owned()));
    request.vault_action = Some(VaultAction::AddCustomer);

    let outcome = client
        .sale(request)
        .await
        .expect("missing vault identity is an anomalous outcome");

    assert_eq!(outcome.status(), PaymentStatus::Unknown);
    assert_eq!(
        outcome.diagnostics(),
        &[PaymentOutcomeDiagnostic::MissingCustomerVaultIdentifier]
    );
    let parts = outcome.into_parts();
    assert_eq!(
        parts.transaction_id.as_ref().map(SensitiveText::expose),
        Some("txn_without_vault")
    );
    assert!(parts.customer_vault_id.is_none());
    let request = request_receiver.await.expect("request should be captured");
    assert!(request.starts_with("POST /api/transact.php HTTP/1.1"));
    server.await.expect("server task should finish");
}

#[tokio::test]
async fn public_store_payment_method_requires_both_durable_identities() {
    let (client, request_receiver, server) = spawn_capturing_server(
        "HTTP/1.1 200 OK",
        "text/plain",
        b"response=1&response_code=100".to_vec(),
    )
    .await;

    let outcome = client
        .store_payment_method(test_store_payment_method_request())
        .await
        .expect("missing durable identities are an anomalous outcome");

    assert_eq!(outcome.status(), PaymentStatus::Unknown);
    assert_eq!(
        outcome.diagnostics(),
        &[
            PaymentOutcomeDiagnostic::MissingTransactionIdentifier,
            PaymentOutcomeDiagnostic::MissingCustomerVaultIdentifier,
        ]
    );
    let parts = outcome.into_parts();
    assert!(parts.transaction_id.is_none());
    assert!(parts.customer_vault_id.is_none());
    let request = request_receiver.await.expect("request should be captured");
    assert!(request.starts_with("POST /api/transact.php HTTP/1.1"));
    server.await.expect("server task should finish");
}

#[tokio::test]
async fn public_store_payment_method_rejects_blank_transaction_with_valid_vault_identity() {
    let (client, request_receiver, server) = spawn_capturing_server(
        "HTTP/1.1 200 OK",
        "text/plain",
        b"response=1&response_code=100&transactionid=&customer_vault_id=vault_valid".to_vec(),
    )
    .await;

    let outcome = client
        .store_payment_method(test_store_payment_method_request())
        .await
        .expect("missing transaction identity is an anomalous outcome");

    assert_eq!(outcome.status(), PaymentStatus::Unknown);
    assert_eq!(
        outcome.diagnostics(),
        &[PaymentOutcomeDiagnostic::MissingTransactionIdentifier]
    );
    let parts = outcome.into_parts();
    assert!(parts.transaction_id.is_none());
    assert_eq!(
        parts.customer_vault_id.as_ref().map(SensitiveText::expose),
        Some("vault_valid")
    );
    let request = request_receiver.await.expect("request should be captured");
    assert!(request.starts_with("POST /api/transact.php HTTP/1.1"));
    server.await.expect("server task should finish");
}

#[tokio::test]
async fn public_transaction_query_downgrades_approval_without_transaction_identity() {
    let (client, request_receiver, server) = spawn_capturing_server(
        "HTTP/1.1 200 OK",
        "text/xml",
        b"<nm_response><transaction><condition>complete</condition><order_id>ck_missing_query_transaction</order_id></transaction></nm_response>".to_vec(),
    )
    .await;

    let outcome = client
        .query_transaction(TransactionQuery {
            transaction_id: None,
            order_id: Some("ck_missing_query_transaction".to_owned()),
        })
        .await
        .expect("query response should parse")
        .expect("query response should include a transaction");

    assert_eq!(outcome.status(), PaymentStatus::Unknown);
    assert_eq!(
        outcome.diagnostics(),
        &[PaymentOutcomeDiagnostic::MissingTransactionIdentifier]
    );
    assert!(outcome.into_parts().transaction_id.is_none());
    let request = request_receiver.await.expect("request should be captured");
    assert!(request.starts_with("POST /api/query.php HTTP/1.1"));
    server.await.expect("server task should finish");
}

#[tokio::test]
async fn order_only_query_retains_a_bound_decline_with_unusable_transaction_identity() {
    let (client, request_receiver, server) = spawn_capturing_server(
        "HTTP/1.1 200 OK",
        "text/xml",
        br#"<nm_response>
            <transaction>
                <response>2</response>
                <transaction_id>txn_first</transaction_id>
                <transaction_id>txn_second</transaction_id>
                <order_id>order_requested</order_id>
            </transaction>
        </nm_response>"#
            .to_vec(),
    )
    .await;

    let outcome = client
        .query_transaction(TransactionQuery {
            transaction_id: None,
            order_id: Some("order_requested".to_owned()),
        })
        .await
        .expect("the matching order query should parse")
        .expect("the matching transaction should be returned");

    assert_eq!(outcome.status(), PaymentStatus::Declined);
    assert_eq!(
        outcome.diagnostics(),
        &[PaymentOutcomeDiagnostic::InvalidOrConflictingTransactionIdentifier]
    );
    assert!(outcome.into_parts().transaction_id.is_none());
    let request = request_receiver.await.expect("request should be captured");
    assert!(request.contains("order_id=order_requested"));
    server.await.expect("server task should finish");
}

#[tokio::test]
async fn order_only_query_diagnoses_a_bound_decline_with_missing_transaction_identity() {
    let (client, request_receiver, server) = spawn_capturing_server(
        "HTTP/1.1 200 OK",
        "text/xml",
        br#"<nm_response>
            <transaction>
                <response>2</response>
                <order_id>order_missing_transaction</order_id>
            </transaction>
        </nm_response>"#
            .to_vec(),
    )
    .await;

    let outcome = client
        .query_transaction(TransactionQuery {
            transaction_id: None,
            order_id: Some("order_missing_transaction".to_owned()),
        })
        .await
        .expect("the matching order query should parse")
        .expect("the matching transaction should be returned");

    assert_eq!(outcome.status(), PaymentStatus::Declined);
    assert_eq!(
        outcome.diagnostics(),
        &[PaymentOutcomeDiagnostic::MissingTransactionIdentifier]
    );
    assert!(outcome.into_parts().transaction_id.is_none());
    let request = request_receiver.await.expect("request should be captured");
    assert!(request.contains("order_id=order_missing_transaction"));
    server.await.expect("server task should finish");
}

#[tokio::test]
async fn public_transaction_query_rejects_a_mismatched_transaction_identifier() {
    let (client, request_receiver, server) = spawn_capturing_server(
        "HTTP/1.1 200 OK",
        "text/xml",
        b"<nm_response><transaction><condition>complete</condition><transaction_id>txn_other</transaction_id></transaction></nm_response>".to_vec(),
    )
    .await;

    let error = client
        .query_transaction(TransactionQuery {
            transaction_id: Some("txn_requested".to_owned()),
            order_id: None,
        })
        .await
        .expect_err("a different transaction must not satisfy an exact query");

    assert!(matches!(error, QueryError::MalformedResponse(_)));
    let request = request_receiver.await.expect("request should be captured");
    assert!(request.contains("transaction_id=txn_requested"));
    server.await.expect("server task should finish");
}

#[tokio::test]
async fn public_transaction_query_rejects_an_unbound_order_identifier() {
    for (response, expected_detail) in [
        (
            "<nm_response><transaction><condition>complete</condition><transaction_id>txn_returned</transaction_id></transaction></nm_response>",
            "NMI exact query response did not include order identifier.",
        ),
        (
            "<nm_response><transaction><condition>complete</condition><transaction_id>txn_returned</transaction_id><order_id>order_other</order_id></transaction></nm_response>",
            "NMI exact query response did not match the requested order identifier.",
        ),
        (
            "<nm_response><transaction><condition>complete</condition><transaction_id>txn_returned</transaction_id><order_id>order_first</order_id><order_id>order_second</order_id></transaction></nm_response>",
            "NMI exact query response contained an invalid or conflicting order identifier.",
        ),
    ] {
        let (client, request_receiver, server) =
            spawn_capturing_server("HTTP/1.1 200 OK", "text/xml", response.as_bytes().to_vec())
                .await;

        let error = client
            .query_transaction(TransactionQuery {
                transaction_id: None,
                order_id: Some("order_requested".to_owned()),
            })
            .await
            .expect_err("an unbound transaction must not satisfy an order query");

        assert!(matches!(error, QueryError::MalformedResponse(_)));
        assert_eq!(error.detail().expose(), expected_detail);
        let request = request_receiver.await.expect("request should be captured");
        assert!(request.contains("order_id=order_requested"));
        server.await.expect("server task should finish");
    }
}

#[tokio::test]
async fn public_transaction_query_binds_every_requested_identifier() {
    let (client, request_receiver, server) = spawn_capturing_server(
        "HTTP/1.1 200 OK",
        "text/xml",
        br#"<nm_response>
            <transaction>
                <condition>complete</condition>
                <transaction_id>
                    txn_requested
                </transaction_id>
                <order_id>
                    order_requested
                </order_id>
            </transaction>
        </nm_response>"#
            .to_vec(),
    )
    .await;

    let outcome = client
        .query_transaction(TransactionQuery {
            transaction_id: Some(" txn_requested ".to_owned()),
            order_id: Some(" order_requested ".to_owned()),
        })
        .await
        .expect("matching exact-query identifiers should parse")
        .expect("the matching transaction should be returned");

    assert_eq!(outcome.status(), PaymentStatus::Approved);
    let request = request_receiver.await.expect("request should be captured");
    assert!(request.contains("transaction_id=txn_requested"));
    assert!(request.contains("order_id=order_requested"));
    server.await.expect("server task should finish");
}

#[tokio::test]
async fn public_transaction_query_does_not_misclassify_an_unrelated_vault_identity_error() {
    let (client, request_receiver, server) = spawn_capturing_server(
        "HTTP/1.1 200 OK",
        "text/xml",
        b"<nm_response><transaction><condition>complete</condition><transaction_id>txn_requested</transaction_id><order_id>order_first</order_id><order_id>order_second</order_id><customer_vault_id>vault_first</customer_vault_id><customer_vault_id>vault_second</customer_vault_id></transaction></nm_response>".to_vec(),
    )
    .await;

    let outcome = client
        .query_transaction(TransactionQuery {
            transaction_id: Some("txn_requested".to_owned()),
            order_id: None,
        })
        .await
        .expect("the exact transaction identity should still bind")
        .expect("the matching transaction should be returned");

    assert_eq!(outcome.status(), PaymentStatus::Unknown);
    assert_eq!(outcome.transaction_id, None);
    assert_eq!(outcome.customer_vault_id, None);
    assert_eq!(
        outcome.diagnostics(),
        &[PaymentOutcomeDiagnostic::InvalidOrConflictingCustomerVaultIdentifier]
    );
    let request = request_receiver.await.expect("request should be captured");
    assert!(request.contains("transaction_id=txn_requested"));
    server.await.expect("server task should finish");
}

#[test]
fn scalar_occurrence_collector_resolves_missing_consistent_and_invalid() {
    assert_eq!(
        ScalarOccurrenceCollector::default().finish(),
        ResolvedScalar::Missing
    );

    let mut consistent = ScalarOccurrenceCollector::default();
    consistent.record(ScalarOccurrence::Scalar(Cow::Borrowed(" txn_same ")));
    consistent.record(ScalarOccurrence::Scalar(Cow::Borrowed("txn_same")));
    assert_eq!(
        consistent.finish(),
        ResolvedScalar::OneConsistent("txn_same".to_owned())
    );

    let mut conflicting = ScalarOccurrenceCollector::default();
    conflicting.record(ScalarOccurrence::Scalar(Cow::Borrowed("txn_first")));
    conflicting.record(ScalarOccurrence::Scalar(Cow::Borrowed("txn_second")));
    assert_eq!(conflicting.finish(), ResolvedScalar::InvalidOrConflicting);

    for occurrence in [
        ScalarOccurrence::Scalar(Cow::Borrowed("")),
        ScalarOccurrence::InvalidShape,
    ] {
        let mut invalid = ScalarOccurrenceCollector::default();
        invalid.record(occurrence);
        assert_eq!(invalid.finish(), ResolvedScalar::InvalidOrConflicting);
    }

    let mut provider_specific = ScalarOccurrenceCollector::default();
    provider_specific.record(ScalarOccurrence::Scalar(Cow::Borrowed(
        "txn=provider/value",
    )));
    assert_eq!(
        provider_specific.finish(),
        ResolvedScalar::OneConsistent("txn=provider/value".to_owned())
    );
}

#[test]
fn present_non_object_json_identity_containers_are_invalid_not_missing() {
    for container in ["transaction", "payment", "customer_vault", "customer"] {
        for invalid_shape in ["null", "true", "42", r#""not_an_object""#, "[]"] {
            let text = format!(
                r#"{{
                    "response":"1",
                    "transaction_id":"txn_valid_alternate",
                    "customer_vault_id":"vault_valid_alternate",
                    "{container}":{invalid_shape}
                }}"#
            );
            let outcome = payment_outcome_from_json_text(&text)
                .expect("malformed identity container should produce an outcome");
            assert_eq!(
                outcome.status,
                PaymentStatus::Unknown,
                "{container}={invalid_shape} must poison its collector"
            );
            assert_eq!(outcome.transaction_id, None);
            assert_eq!(outcome.customer_vault_id, None);
        }
    }
}

#[test]
fn duplicate_valid_and_non_object_json_identity_containers_are_invalid_in_both_orders() {
    for (container, valid_object, other_identity) in [
        (
            "transaction",
            r#"{"id":"txn_valid_nested"}"#,
            r#""customer_vault_id":"vault_valid""#,
        ),
        (
            "payment",
            r#"{"id":"txn_valid_nested"}"#,
            r#""customer_vault_id":"vault_valid""#,
        ),
        (
            "customer_vault",
            r#"{"id":"vault_valid_nested"}"#,
            r#""transaction_id":"txn_valid""#,
        ),
        (
            "customer",
            r#"{"customer_vault_id":"vault_valid_nested"}"#,
            r#""transaction_id":"txn_valid""#,
        ),
    ] {
        for duplicate_members in [
            format!(r#""{container}":{valid_object},"{container}":null"#),
            format!(r#""{container}":null,"{container}":{valid_object}"#),
        ] {
            let outcome = payment_outcome_from_json_text(&format!(
                r#"{{"response":"1",{other_identity},{duplicate_members}}}"#
            ))
            .expect("duplicate malformed identity container should produce an outcome");
            assert_eq!(outcome.status, PaymentStatus::Unknown);
            assert_eq!(outcome.transaction_id, None);
            assert_eq!(outcome.customer_vault_id, None);
        }
    }
}

#[test]
fn present_identity_objects_without_supported_leaves_remain_missing() {
    for container in ["transaction", "payment", "customer_vault", "customer"] {
        let outcome = payment_outcome_from_json_text(&format!(
            r#"{{
                "response":"1",
                "transaction_id":"txn_valid_alternate",
                "customer_vault_id":"vault_valid_alternate",
                "{container}":{{"unrelated":"metadata"}}
            }}"#
        ))
        .expect("identity object without a supported leaf should remain missing");
        assert_eq!(outcome.status, PaymentStatus::Approved);
        assert_eq!(
            outcome.transaction_id.as_ref().map(SensitiveText::expose),
            Some("txn_valid_alternate")
        );
        assert_eq!(
            outcome
                .customer_vault_id
                .as_ref()
                .map(SensitiveText::expose),
            Some("vault_valid_alternate")
        );
    }
}

#[test]
fn provider_identifiers_remain_raw_for_caller_policy_across_formats() {
    for zero in [0x0966, 0x09e6, 0x1d7ce] {
        let transaction_id = format!("txn_{}", decimal_pan(zero));

        let json = payment_outcome_from_json(&json!({
            "response": "1",
            "transaction_id": transaction_id,
            "customer_vault_id": "vault_valid"
        }))
        .expect("JSON response should parse");

        let classic_text = form_urlencoded::Serializer::new(String::new())
            .append_pair("response", "1")
            .append_pair("transactionid", &transaction_id)
            .append_pair("customer_vault_id", "vault_valid")
            .finish();
        let classic = classic_payment_outcome_from_form(&classic_text)
            .expect("classic response should parse");

        let xml = query_outcome_from_xml(&format!(
            "<nm_response><transaction><response>1</response><transaction_id>{transaction_id}</transaction_id><customer_vault_id>vault_valid</customer_vault_id></transaction></nm_response>"
        ))
        .expect("XML response should parse")
        .expect("XML transaction should be present");

        for outcome in [json, classic, xml] {
            assert_eq!(outcome.status, PaymentStatus::Approved);
            assert_eq!(
                outcome.transaction_id.as_ref().map(SensitiveText::expose),
                Some(transaction_id.as_str())
            );
            assert_eq!(
                outcome
                    .customer_vault_id
                    .as_ref()
                    .map(SensitiveText::expose),
                Some("vault_valid")
            );
            let debug = format!("{outcome:?}");
            assert!(!debug.contains(&transaction_id));
            assert!(!debug.contains("vault_valid"));
        }

        let reports = query_transaction_reports_from_xml(&format!(
                "<nm_response><transaction><transaction_id>{transaction_id}</transaction_id><order_id>ck_order_valid</order_id></transaction></nm_response>"
            ))
            .expect("provider identifiers are returned for caller validation");
        assert_eq!(reports.len(), 1);
        assert_eq!(
            reports[0]
                .transaction_id
                .as_ref()
                .map(SensitiveText::expose),
            Some(transaction_id.as_str())
        );
    }
}
