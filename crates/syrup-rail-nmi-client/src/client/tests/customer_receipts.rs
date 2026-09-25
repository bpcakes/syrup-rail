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
