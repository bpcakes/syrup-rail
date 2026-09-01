use super::*;

#[test]
fn caller_controlled_request_fields_enforce_inclusive_byte_limits() {
    let mut sale = test_sale_request(PaymentSource::PaymentToken(
        "t".repeat(MAX_NMI_PAYMENT_TOKEN_BYTES),
    ));
    assert!(validate_sale_request(&sale, "private_key").is_ok());
    sale.source = PaymentSource::PaymentToken("t".repeat(MAX_NMI_PAYMENT_TOKEN_BYTES + 1));
    assert!(matches!(
        validate_sale_request(&sale, "private_key"),
        Err(MutationError::InvalidRequest(_))
    ));

    let mut sale = test_sale_request(PaymentSource::CustomerVault(
        "v".repeat(MAX_NMI_IDENTIFIER_BYTES),
    ));
    assert!(validate_sale_request(&sale, "private_key").is_ok());
    sale.source = PaymentSource::CustomerVault("v".repeat(MAX_NMI_IDENTIFIER_BYTES + 1));
    assert!(matches!(
        validate_sale_request(&sale, "private_key"),
        Err(MutationError::InvalidRequest(_))
    ));

    let mut sale = test_sale_request(PaymentSource::PaymentToken("tok_test".to_owned()));
    sale.order_id = "o".repeat(MAX_NMI_ORDER_ID_BYTES);
    assert!(validate_sale_request(&sale, "private_key").is_ok());
    sale.order_id.push('o');
    assert!(matches!(
        validate_sale_request(&sale, "private_key"),
        Err(MutationError::InvalidRequest(_))
    ));

    let mut sale = test_sale_request(PaymentSource::CustomerVault("vault_test".to_owned()));
    sale.stored_credential = Some(StoredCredential::RecurringMerchant {
        initial_transaction_id: "i".repeat(MAX_NMI_IDENTIFIER_BYTES),
    });
    assert!(validate_sale_request(&sale, "private_key").is_ok());
    sale.stored_credential = Some(StoredCredential::RecurringMerchant {
        initial_transaction_id: "i".repeat(MAX_NMI_IDENTIFIER_BYTES + 1),
    });
    assert!(matches!(
        validate_sale_request(&sale, "private_key"),
        Err(MutationError::InvalidRequest(_))
    ));

    let query = TransactionQuery {
        transaction_id: Some("q".repeat(MAX_NMI_IDENTIFIER_BYTES)),
        order_id: Some("o".repeat(MAX_NMI_ORDER_ID_BYTES)),
    };
    assert!(validate_transaction_query(&query, "query_key").is_ok());
    let query = TransactionQuery {
        transaction_id: Some("q".repeat(MAX_NMI_IDENTIFIER_BYTES + 1)),
        order_id: None,
    };
    assert!(matches!(
        validate_transaction_query(&query, "query_key"),
        Err(QueryError::InvalidRequest(_))
    ));
    let query = TransactionQuery {
        transaction_id: None,
        order_id: Some("o".repeat(MAX_NMI_ORDER_ID_BYTES + 1)),
    };
    assert!(matches!(
        validate_transaction_query(&query, "query_key"),
        Err(QueryError::InvalidRequest(_))
    ));

    let report = ReportQuery {
        start_date: "s".repeat(MAX_NMI_REPORT_DATE_BYTES),
        end_date: "e".repeat(MAX_NMI_REPORT_DATE_BYTES),
        result_limit: 100,
        page_number: 0,
    };
    assert!(validate_report_query(&report, "query_key").is_ok());
    let report = ReportQuery {
        start_date: "s".repeat(MAX_NMI_REPORT_DATE_BYTES + 1),
        ..report
    };
    assert!(matches!(
        validate_report_query(&report, "query_key"),
        Err(QueryError::InvalidRequest(_))
    ));
    let report = ReportQuery {
        start_date: "20260701000000".to_owned(),
        end_date: "e".repeat(MAX_NMI_REPORT_DATE_BYTES + 1),
        result_limit: 100,
        page_number: 0,
    };
    assert!(matches!(
        validate_report_query(&report, "query_key"),
        Err(QueryError::InvalidRequest(_))
    ));
}

#[test]
fn billing_contact_fields_enforce_inclusive_byte_limits() {
    let mut sale = test_sale_request(PaymentSource::PaymentToken("tok_test".to_owned()));
    sale.billing_contact = Some(BillingContact {
        first_name: Some("f".repeat(MAX_NMI_CONTACT_NAME_BYTES)),
        last_name: Some("l".repeat(MAX_NMI_CONTACT_NAME_BYTES)),
        email: Some("e".repeat(MAX_NMI_EMAIL_BYTES)),
    });
    assert!(validate_sale_request(&sale, "private_key").is_ok());

    for contact in [
        BillingContact {
            first_name: Some("f".repeat(MAX_NMI_CONTACT_NAME_BYTES + 1)),
            last_name: None,
            email: None,
        },
        BillingContact {
            first_name: None,
            last_name: Some("l".repeat(MAX_NMI_CONTACT_NAME_BYTES + 1)),
            email: None,
        },
        BillingContact {
            first_name: None,
            last_name: None,
            email: Some("e".repeat(MAX_NMI_EMAIL_BYTES + 1)),
        },
    ] {
        let mut request = test_store_payment_method_request();
        request.billing_contact = Some(contact);
        assert!(matches!(
            validate_store_payment_method_request(&request, "private_key"),
            Err(MutationError::InvalidRequest(_))
        ));
    }
}

#[test]
fn encoded_request_budget_rejects_individually_bounded_aggregate_data() {
    let request = test_sale_request(PaymentSource::PaymentToken(
        "\0".repeat(MAX_NMI_PAYMENT_TOKEN_BYTES),
    ));
    let error = validate_sale_request(&request, "private_key")
        .expect_err("JSON escaping must remain inside the aggregate request bound");

    assert!(matches!(error, MutationError::InvalidRequest(_)));
    assert_eq!(
        error.detail().expose(),
        "NMI outbound request exceeds the supported size"
    );
    assert!(!format!("{error:?}").contains('\0'));
    assert!(!format!("{error}").contains('\0'));

    let mut request = test_sale_request(PaymentSource::PaymentToken(
        "%".repeat(MAX_NMI_PAYMENT_TOKEN_BYTES),
    ));
    request.order_id = "%".repeat(MAX_NMI_ORDER_ID_BYTES);
    request.vault_action = Some(VaultAction::AddCustomer);
    request.billing_contact = Some(BillingContact {
        first_name: Some("%".repeat(MAX_NMI_CONTACT_NAME_BYTES)),
        last_name: Some("%".repeat(MAX_NMI_CONTACT_NAME_BYTES)),
        email: None,
    });
    assert!(matches!(
        validate_sale_request(&request, "private_key"),
        Err(MutationError::InvalidRequest(_))
    ));
}

#[tokio::test]
async fn oversized_public_requests_are_rejected_without_network_io() {
    let (client, mut request_receiver, server) =
        spawn_capturing_server("HTTP/1.1 200 OK", "text/plain", b"unexpected".to_vec()).await;

    let sale_error = client
        .sale(test_sale_request(PaymentSource::PaymentToken(
            "token-sentinel".repeat(MAX_NMI_PAYMENT_TOKEN_BYTES),
        )))
        .await
        .expect_err("oversized sale token must fail locally");
    assert!(matches!(sale_error, MutationError::InvalidRequest(_)));
    assert_eq!(
        sale_error.certainty(),
        crate::MutationCertainty::NotSubmitted
    );

    let aggregate_error = client
        .sale(test_sale_request(PaymentSource::PaymentToken(
            "\0".repeat(MAX_NMI_PAYMENT_TOKEN_BYTES),
        )))
        .await
        .expect_err("aggregate-oversized JSON sale must fail locally");
    assert!(matches!(aggregate_error, MutationError::InvalidRequest(_)));

    let mut aggregate_form_request = test_sale_request(PaymentSource::PaymentToken(
        "%".repeat(MAX_NMI_PAYMENT_TOKEN_BYTES),
    ));
    aggregate_form_request.order_id = "%".repeat(MAX_NMI_ORDER_ID_BYTES);
    aggregate_form_request.vault_action = Some(VaultAction::AddCustomer);
    aggregate_form_request.billing_contact = Some(BillingContact {
        first_name: Some("%".repeat(MAX_NMI_CONTACT_NAME_BYTES)),
        last_name: Some("%".repeat(MAX_NMI_CONTACT_NAME_BYTES)),
        email: None,
    });
    let aggregate_form_error = client
        .sale(aggregate_form_request)
        .await
        .expect_err("aggregate-oversized Classic sale must fail locally");
    assert!(matches!(
        aggregate_form_error,
        MutationError::InvalidRequest(_)
    ));

    let mut store = test_store_payment_method_request();
    store.order_id = "order-sentinel".repeat(MAX_NMI_ORDER_ID_BYTES);
    let store_error = client
        .store_payment_method(store)
        .await
        .expect_err("oversized store order ID must fail locally");
    assert!(matches!(store_error, MutationError::InvalidRequest(_)));

    let query_error = client
        .query_transaction(TransactionQuery {
            transaction_id: Some("transaction-sentinel".repeat(MAX_NMI_IDENTIFIER_BYTES)),
            order_id: None,
        })
        .await
        .expect_err("oversized transaction ID must fail locally");
    assert!(matches!(query_error, QueryError::InvalidRequest(_)));

    let report_error = client
        .query_transaction_reports(ReportQuery {
            start_date: "date-sentinel".repeat(MAX_NMI_REPORT_DATE_BYTES),
            end_date: "20260702000000".to_owned(),
            result_limit: 100,
            page_number: 0,
        })
        .await
        .expect_err("oversized report date must fail locally");
    assert!(matches!(report_error, QueryError::InvalidRequest(_)));

    let mut whitespace_sale =
        test_sale_request(PaymentSource::PaymentToken("tok_whitespace".to_owned()));
    whitespace_sale.order_id = " ".repeat(MAX_NMI_ORDER_ID_BYTES + 1);
    let whitespace_sale_error = client
        .sale(whitespace_sale)
        .await
        .expect_err("oversized whitespace order ID must fail before trimming");
    assert!(matches!(
        whitespace_sale_error,
        MutationError::InvalidRequest(_)
    ));
    assert_eq!(
        whitespace_sale_error.detail().expose(),
        "payment order ID exceeds the supported size"
    );

    let mut whitespace_store = test_store_payment_method_request();
    whitespace_store.payment_token = " ".repeat(MAX_NMI_PAYMENT_TOKEN_BYTES + 1);
    let whitespace_store_error = client
        .store_payment_method(whitespace_store)
        .await
        .expect_err("oversized whitespace token must fail before trimming");
    assert!(matches!(
        whitespace_store_error,
        MutationError::InvalidRequest(_)
    ));
    assert_eq!(
        whitespace_store_error.detail().expose(),
        "payment token exceeds the supported size"
    );

    let whitespace_query_error = client
        .query_transaction(TransactionQuery {
            transaction_id: Some(" ".repeat(MAX_NMI_IDENTIFIER_BYTES + 1)),
            order_id: None,
        })
        .await
        .expect_err("oversized whitespace transaction ID must fail before trimming");
    assert!(matches!(
        whitespace_query_error,
        QueryError::InvalidRequest(_)
    ));
    assert_eq!(
        whitespace_query_error.detail().expose(),
        "transaction ID exceeds the supported size"
    );

    let whitespace_report_error = client
        .query_transaction_reports(ReportQuery {
            start_date: " ".repeat(MAX_NMI_REPORT_DATE_BYTES + 1),
            end_date: "20260702000000".to_owned(),
            result_limit: 100,
            page_number: 0,
        })
        .await
        .expect_err("oversized whitespace report date must fail before trimming");
    assert!(matches!(
        whitespace_report_error,
        QueryError::InvalidRequest(_)
    ));
    assert_eq!(
        whitespace_report_error.detail().expose(),
        "report start date exceeds the supported size"
    );

    assert!(
        request_receiver.try_recv().is_err(),
        "locally rejected requests must not reach the listener"
    );
    for sentinel in [
        "token-sentinel",
        "order-sentinel",
        "transaction-sentinel",
        "date-sentinel",
    ] {
        assert!(!format!("{sale_error:?}").contains(sentinel));
        assert!(!format!("{store_error:?}").contains(sentinel));
        assert!(!format!("{query_error:?}").contains(sentinel));
        assert!(!format!("{report_error:?}").contains(sentinel));
    }
    server.abort();
}

#[tokio::test]
async fn final_form_budget_is_a_local_invalid_query_without_network_io() {
    let (mut client, mut request_receiver, server) =
        spawn_capturing_server("HTTP/1.1 200 OK", "text/plain", b"unexpected".to_vec()).await;
    client.credentials = Credentials::new(
        "private_key".to_owned(),
        "%".repeat(crate::MAX_CREDENTIAL_BYTES),
    )
    .expect("maximum-size query credential should construct");

    let error = client
        .query_transaction(TransactionQuery {
            transaction_id: Some("%".repeat(MAX_NMI_IDENTIFIER_BYTES)),
            order_id: Some("%".repeat(MAX_NMI_ORDER_ID_BYTES)),
        })
        .await
        .expect_err("key-aware final form bound must reject the aggregate request");

    assert!(matches!(error, QueryError::InvalidRequest(_)));
    assert_eq!(
        error.detail().expose(),
        "NMI outbound request exceeds the supported size"
    );
    assert!(
        request_receiver.try_recv().is_err(),
        "final local form rejection must occur before network I/O"
    );
    assert!(!format!("{error:?}").contains('%'));
    assert!(!format!("{error}").contains('%'));
    server.abort();
}
