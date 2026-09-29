use super::*;

#[tokio::test]
async fn disabled_customer_receipts_reach_every_payment_wire() {
    for disabled in [false, true] {
        for operation in ["v5_sale", "initial_sale", "renewal", "store"] {
            let is_json = operation == "v5_sale";
            let response = if is_json {
                br#"{"id":"txn_receipt","status":"approved"}"#.to_vec()
            } else {
                b"response=1&response_code=100&transactionid=txn_receipt&customer_vault_id=vault_receipt&responsetext=Approved".to_vec()
            };
            let (client, captured, server) = spawn_capturing_server(
                "HTTP/1.1 200 OK",
                if is_json {
                    "application/json"
                } else {
                    "application/x-www-form-urlencoded"
                },
                response,
            )
            .await;
            let client = if disabled {
                client.with_customer_receipts_disabled()
            } else {
                client
            };
            let contact = Some(BillingContact {
                first_name: Some("Test".to_owned()),
                last_name: Some("Customer".to_owned()),
                email: Some("receipt@example.test".to_owned()),
                address: None,
            });
            let outcome = if operation == "store" {
                client
                    .store_payment_method(StorePaymentMethodRequest {
                        payment_token: "tok_receipt".to_owned(),
                        order_id: "receipt_store".to_owned(),
                        billing_contact: contact,
                    })
                    .await
            } else {
                let mut request =
                    test_sale_request(PaymentSource::PaymentToken("tok_receipt".to_owned()));
                request.billing_contact = contact;
                if operation == "initial_sale" {
                    request.vault_action = Some(VaultAction::AddCustomer);
                    request.stored_credential = Some(StoredCredential::InitialCustomer);
                } else if operation == "renewal" {
                    request.source = PaymentSource::CustomerVault("vault_receipt".to_owned());
                    request.stored_credential = Some(StoredCredential::RecurringMerchant {
                        initial_transaction_id: "txn_initial".to_owned(),
                    });
                }
                client.sale(request).await
            }
            .expect("payment should parse");
            assert_eq!(outcome.status, PaymentStatus::Approved);
            let request = captured.await.expect("request captured");
            let (_, body) = request.split_once("\r\n\r\n").expect("request body");
            if is_json {
                assert!(request.starts_with("POST /api/v5/payments/sale "));
                let body: Value = serde_json::from_str(body).unwrap();
                assert_eq!(
                    body.get("customer_receipt"),
                    disabled.then_some(&Value::Bool(false))
                );
                assert_eq!(body["billing_address"]["email"], "receipt@example.test");
            } else {
                assert!(request.starts_with("POST /api/transact.php "));
                let fields: std::collections::HashMap<_, _> =
                    form_urlencoded::parse(body.as_bytes())
                        .into_owned()
                        .collect();
                assert_eq!(
                    fields.get("customer_receipt").map(String::as_str),
                    disabled.then_some("false")
                );
                assert_eq!(fields["email"], "receipt@example.test");
                assert_eq!(
                    fields["type"],
                    if operation == "store" {
                        "validate"
                    } else {
                        "sale"
                    }
                );
            }
            server.await.expect("server finished");
        }
    }
}

#[tokio::test]
async fn addresses_reach_every_payment_wire_without_changing_cit_mit_or_receipts() {
    // Recovery submits the same vault-creating initial sale as enrollment, so
    // `initial_sale` covers both Classic routes.
    let expected_address = [
        ("address1", "1 Main St"),
        ("address2", "Suite 2"),
        ("city", "Boston"),
        ("state", "MA"),
        ("zip", "02110"),
        ("country", "US"),
    ];
    let address = || crate::BillingAddress {
        address1: "1 Main St".to_owned(),
        address2: Some("Suite 2".to_owned()),
        city: Some("Boston".to_owned()),
        state: Some("MA".to_owned()),
        zip: Some("02110".to_owned()),
        country: "US".to_owned(),
    };
    for disabled in [false, true] {
        for operation in ["v5_sale", "initial_sale", "renewal", "store"] {
            let is_json = operation == "v5_sale";
            let response = if is_json {
                br#"{"id":"txn_receipt","status":"approved"}"#.to_vec()
            } else {
                b"response=1&response_code=100&transactionid=txn_receipt&customer_vault_id=vault_receipt&responsetext=Approved".to_vec()
            };
            let (client, captured, server) = spawn_capturing_server(
                "HTTP/1.1 200 OK",
                if is_json {
                    "application/json"
                } else {
                    "application/x-www-form-urlencoded"
                },
                response,
            )
            .await;
            let client = if disabled {
                client.with_customer_receipts_disabled()
            } else {
                client
            };
            let named = || BillingContact {
                first_name: Some("Test".to_owned()),
                last_name: Some("Customer".to_owned()),
                email: Some("receipt@example.test".to_owned()),
                address: Some(address()),
            };
            let outcome = if operation == "store" {
                client
                    .store_payment_method(StorePaymentMethodRequest {
                        payment_token: "tok_receipt".to_owned(),
                        order_id: "receipt_store".to_owned(),
                        billing_contact: Some(named()),
                    })
                    .await
            } else {
                let mut request =
                    test_sale_request(PaymentSource::PaymentToken("tok_receipt".to_owned()));
                request.billing_contact = Some(named());
                if operation == "initial_sale" {
                    request.vault_action = Some(VaultAction::AddCustomer);
                    request.stored_credential = Some(StoredCredential::InitialCustomer);
                } else if operation == "renewal" {
                    request.source = PaymentSource::CustomerVault("vault_receipt".to_owned());
                    request.stored_credential = Some(StoredCredential::RecurringMerchant {
                        initial_transaction_id: "txn_initial".to_owned(),
                    });
                    request.billing_contact = Some(BillingContact {
                        first_name: None,
                        last_name: None,
                        email: None,
                        address: Some(address()),
                    });
                }
                client.sale(request).await
            }
            .expect("payment should parse");
            assert_eq!(outcome.status, PaymentStatus::Approved);
            let request = captured.await.expect("request captured");
            let (_, body) = request.split_once("\r\n\r\n").expect("request body");
            if is_json {
                assert!(request.starts_with("POST /api/v5/payments/sale "));
                let body: Value = serde_json::from_str(body).unwrap();
                assert_eq!(
                    body.get("customer_receipt"),
                    disabled.then_some(&Value::Bool(false))
                );
                for (key, value) in expected_address {
                    assert_eq!(body["billing_address"][key], value, "{key}");
                }
                assert_eq!(body["billing_address"]["email"], "receipt@example.test");
                assert!(body.get("cit_mit").is_none());
                assert!(body.get("customer_vault").is_none());
                server.await.expect("server finished");
                continue;
            }
            assert!(request.starts_with("POST /api/transact.php "));
            let fields: std::collections::HashMap<_, _> = form_urlencoded::parse(body.as_bytes())
                .into_owned()
                .collect();
            let field = |key: &str| fields.get(key).map(String::as_str);
            assert_eq!(field("customer_receipt"), disabled.then_some("false"));
            for (key, value) in expected_address {
                assert_eq!(field(key), Some(value), "{operation} {key}");
            }
            assert_eq!(field("billing_method"), Some("recurring"));
            match operation {
                "initial_sale" | "store" => {
                    assert_eq!(field("stored_credential_indicator"), Some("stored"));
                    assert_eq!(field("initiated_by"), Some("customer"));
                    assert_eq!(field("initial_transaction_id"), None);
                    assert_eq!(field("customer_vault"), Some("add_customer"));
                    assert_eq!(field("email"), Some("receipt@example.test"));
                    assert_eq!(
                        field("type"),
                        Some(if operation == "store" {
                            "validate"
                        } else {
                            "sale"
                        })
                    );
                }
                "renewal" => {
                    assert_eq!(field("type"), Some("sale"));
                    assert_eq!(field("stored_credential_indicator"), Some("used"));
                    assert_eq!(field("initiated_by"), Some("merchant"));
                    assert_eq!(field("initial_transaction_id"), Some("txn_initial"));
                    assert_eq!(field("customer_vault_id"), Some("vault_receipt"));
                    assert_eq!(field("customer_vault"), None);
                    for key in ["first_name", "last_name", "email"] {
                        assert_eq!(field(key), None, "renewal must not send {key}");
                    }
                }
                _ => unreachable!("covered above"),
            }
            server.await.expect("server finished");
        }
    }
}
