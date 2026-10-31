use super::*;

#[test]
fn classic_sale_applies_the_configured_duplicate_check_policy() {
    let request = SaleRequest {
        amount_cents: 4_900,
        currency: "USD".to_owned(),
        order_id: "ck_order_123".to_owned(),
        source: PaymentSource::PaymentToken("tok_test".to_owned()),
        vault_action: None,
        stored_credential: None,
        billing_contact: None,
        descriptor: None,
    };
    let params = classic_sale_params(
        "private_key",
        &request,
        "49.00".to_owned(),
        DuplicateCheck::ProcessorConfigured,
    );
    let form: std::collections::HashMap<&str, &str> = params.iter().collect();
    assert!(!form.contains_key("dup_seconds"));
    assert!(!form.contains_key("duplicate_check_seconds"));

    let params = classic_sale_params(
        "private_key",
        &request,
        "49.00".to_owned(),
        DuplicateCheck::Window(
            crate::DuplicateCheckWindow::new(120).expect("window should be valid"),
        ),
    );
    let form: std::collections::HashMap<&str, &str> = params.iter().collect();
    assert_eq!(form.get("dup_seconds").copied(), Some("120"));
    assert!(!form.contains_key("duplicate_check_seconds"));

    let params = classic_sale_params(
        "private_key",
        &request,
        "49.00".to_owned(),
        DuplicateCheck::Window(
            crate::DuplicateCheckWindow::new(crate::DuplicateCheckWindow::MAX_SECONDS)
                .expect("maximum window should be valid"),
        ),
    );
    let form: std::collections::HashMap<&str, &str> = params.iter().collect();
    assert_eq!(form.get("dup_seconds").copied(), Some("7862400"));
    assert!(!form.contains_key("duplicate_check_seconds"));
}

#[test]
fn sale_customer_vault_form_adds_payment_method_to_vault() {
    let request = SaleRequest {
        amount_cents: 4_900,
        currency: "USD".to_owned(),
        order_id: "ck_sub_123".to_owned(),
        source: PaymentSource::PaymentToken("tok_test".to_owned()),
        vault_action: Some(VaultAction::AddCustomer),
        stored_credential: Some(StoredCredential::InitialCustomer),
        billing_contact: Some(BillingContact {
            first_name: Some(" Ada ".to_owned()),
            last_name: Some(" Lovelace ".to_owned()),
            email: Some(" ada@example.test ".to_owned()),
            address: None,
        }),
        descriptor: None,
    };
    let params = classic_sale_params(
        "private_key",
        &request,
        "49.00".to_owned(),
        DuplicateCheck::ProcessorConfigured,
    );
    let form: std::collections::HashMap<&str, &str> = params.iter().collect();

    assert_eq!(form.get("security_key").copied(), Some("private_key"));
    assert_eq!(form.get("type").copied(), Some("sale"));
    assert_eq!(form.get("amount").copied(), Some("49.00"));
    assert_eq!(form.get("currency").copied(), Some("USD"));
    assert_eq!(form.get("orderid").copied(), Some("ck_sub_123"));
    assert_eq!(form.get("payment_token").copied(), Some("tok_test"));
    assert_eq!(form.get("customer_vault").copied(), Some("add_customer"));
    assert_eq!(form.get("first_name").copied(), Some("Ada"));
    assert_eq!(form.get("last_name").copied(), Some("Lovelace"));
    assert_eq!(form.get("email").copied(), Some("ada@example.test"));
    assert_eq!(form.get("billing_method").copied(), Some("recurring"));
    assert_eq!(
        form.get("stored_credential_indicator").copied(),
        Some("stored")
    );
    assert_eq!(form.get("initiated_by").copied(), Some("customer"));
    assert!(!form.contains_key("initial_transaction_id"));
    let debug = format!("{params:?}");
    assert!(debug.contains("[redacted]"));
    assert!(!debug.contains("private_key"));
    assert!(!debug.contains("tok_test"));
    assert!(!debug.contains("ck_sub_123"));
    assert!(!debug.contains("ada@example.test"));
}

#[test]
fn sale_customer_vault_form_includes_recurring_merchant_flags() {
    let request = SaleRequest {
        amount_cents: 4_900,
        currency: "USD".to_owned(),
        order_id: "ck_renewal_123".to_owned(),
        source: PaymentSource::CustomerVault("vault_123".to_owned()),
        vault_action: None,
        stored_credential: Some(StoredCredential::RecurringMerchant {
            initial_transaction_id: "txn_initial_123".to_owned(),
        }),
        billing_contact: None,
        descriptor: None,
    };
    let params = classic_sale_params(
        "private_key",
        &request,
        "49.00".to_owned(),
        DuplicateCheck::ProcessorConfigured,
    );
    let form: std::collections::HashMap<&str, &str> = params.iter().collect();

    assert_eq!(form.get("customer_vault_id").copied(), Some("vault_123"));
    assert_eq!(form.get("billing_method").copied(), Some("recurring"));
    assert_eq!(
        form.get("stored_credential_indicator").copied(),
        Some("used")
    );
    assert_eq!(form.get("initiated_by").copied(), Some("merchant"));
    assert_eq!(
        form.get("initial_transaction_id").copied(),
        Some("txn_initial_123")
    );
}

#[test]
fn store_payment_method_uses_validate_without_amount() {
    let request = StorePaymentMethodRequest {
        payment_token: "tok_update".to_owned(),
        order_id: "ck_payment_method_123".to_owned(),
        billing_contact: Some(BillingContact {
            first_name: Some(" Ada ".to_owned()),
            last_name: Some(" Lovelace ".to_owned()),
            email: Some(" ada@example.test ".to_owned()),
            address: None,
        }),
    };
    let params = classic_store_payment_method_params("private_key", &request);
    let form: std::collections::HashMap<&str, &str> = params.iter().collect();

    assert_eq!(form.get("security_key").copied(), Some("private_key"));
    assert_eq!(form.get("customer_vault").copied(), Some("add_customer"));
    assert_eq!(form.get("payment_token").copied(), Some("tok_update"));
    assert!(!form.contains_key("dup_seconds"));
    assert_eq!(form.get("orderid").copied(), Some("ck_payment_method_123"));
    assert_eq!(form.get("type").copied(), Some("validate"));
    assert!(!form.contains_key("duplicate_check_seconds"));
    assert_eq!(form.get("billing_method").copied(), Some("recurring"));
    assert_eq!(form.get("initiated_by").copied(), Some("customer"));
    assert_eq!(
        form.get("stored_credential_indicator").copied(),
        Some("stored")
    );
    assert_eq!(form.get("first_name").copied(), Some("Ada"));
    assert_eq!(form.get("last_name").copied(), Some("Lovelace"));
    assert_eq!(form.get("email").copied(), Some("ada@example.test"));
    assert!(!form.contains_key("amount"));
    let debug = format!("{params:?}");
    assert!(debug.contains("[redacted]"));
    assert!(!debug.contains("private_key"));
    assert!(!debug.contains("tok_update"));
    assert!(!debug.contains("ck_payment_method_123"));
    assert!(!debug.contains("ada@example.test"));
}

#[test]
fn form_params_borrow_sensitive_values_and_own_only_public_scalars() {
    let private_key = Zeroizing::new("private_key_sentinel".to_owned());
    let request = SaleRequest {
        amount_cents: 4_900,
        currency: "USD".to_owned(),
        order_id: "ck_borrowed_order".to_owned(),
        source: PaymentSource::CustomerVault("vault_borrowed".to_owned()),
        vault_action: None,
        stored_credential: Some(StoredCredential::RecurringMerchant {
            initial_transaction_id: "txn_borrowed".to_owned(),
        }),
        billing_contact: Some(BillingContact {
            first_name: Some(" Ada ".to_owned()),
            last_name: Some(" Lovelace ".to_owned()),
            email: Some(" ada@example.test ".to_owned()),
            address: None,
        }),
        descriptor: None,
    };
    let params = classic_sale_params(
        private_key.as_str(),
        &request,
        "49.00".to_owned(),
        DuplicateCheck::ProcessorConfigured,
    );

    let assert_borrowed = |key: &str| {
        assert!(
            matches!(params.field(key), Some(NmiFormValue::Borrowed(_))),
            "{key} must borrow its source"
        );
    };
    for key in [
        "security_key",
        "currency",
        "orderid",
        "customer_vault_id",
        "initial_transaction_id",
        "first_name",
        "last_name",
        "email",
    ] {
        assert_borrowed(key);
    }
    assert!(matches!(
        params.field("amount"),
        Some(NmiFormValue::PublicOwned(value)) if value == "49.00"
    ));
    let Some(NmiFormValue::Borrowed(borrowed_key)) = params.field("security_key") else {
        panic!("security key should be borrowed");
    };
    assert_eq!(borrowed_key.as_ptr(), private_key.as_ptr());

    let wire: std::collections::HashMap<_, _> = encoded_form(&params).into_iter().collect();
    assert_eq!(
        wire.get("security_key").map(String::as_str),
        Some("private_key_sentinel")
    );
    assert_eq!(
        wire.get("customer_vault_id").map(String::as_str),
        Some("vault_borrowed")
    );
    assert_eq!(
        wire.get("initial_transaction_id").map(String::as_str),
        Some("txn_borrowed")
    );
}

#[test]
fn query_forms_share_borrowing_serializer_and_wire_contract() {
    let query_key = Zeroizing::new("query_key_sentinel".to_owned());
    let params = query_account_mode_params(query_key.as_str());
    for key in ["security_key", "report_type"] {
        assert!(matches!(params.field(key), Some(NmiFormValue::Borrowed(_))));
    }
    let wire: std::collections::HashMap<_, _> = encoded_form(&params).into_iter().collect();
    assert_eq!(
        wire.get("security_key").map(String::as_str),
        Some("query_key_sentinel")
    );
    assert_eq!(
        wire.get("report_type").map(String::as_str),
        Some("test_mode_status")
    );

    let query = TransactionQuery {
        transaction_id: Some("txn_query".to_owned()),
        order_id: Some("ck_query".to_owned()),
    };
    let params = query_transaction_params(query_key.as_str(), &query);
    for key in ["security_key", "transaction_id", "order_id"] {
        assert!(matches!(params.field(key), Some(NmiFormValue::Borrowed(_))));
    }
    let wire: std::collections::HashMap<_, _> = encoded_form(&params).into_iter().collect();
    assert_eq!(
        wire.get("transaction_id").map(String::as_str),
        Some("txn_query")
    );
    assert_eq!(wire.get("order_id").map(String::as_str), Some("ck_query"));
    assert!(!wire.contains_key("dup_seconds"));

    let report = ReportQuery {
        start_date: "20260701000000".to_owned(),
        end_date: "20260702000000".to_owned(),
        result_limit: 100,
        page_number: 3,
    };
    let params = query_transaction_report_params(query_key.as_str(), &report);
    for key in ["security_key", "start_date", "end_date"] {
        assert!(matches!(params.field(key), Some(NmiFormValue::Borrowed(_))));
    }
    for key in ["result_limit", "page_number"] {
        assert!(matches!(
            params.field(key),
            Some(NmiFormValue::PublicOwned(_))
        ));
    }
    let wire: std::collections::HashMap<_, _> = encoded_form(&params).into_iter().collect();
    assert_eq!(
        wire.get("security_key").map(String::as_str),
        Some("query_key_sentinel")
    );
    assert_eq!(wire.get("result_limit").map(String::as_str), Some("100"));
    assert_eq!(wire.get("page_number").map(String::as_str), Some("3"));
    assert!(!wire.contains_key("dup_seconds"));
    assert_eq!(
        wire.get("result_order").map(String::as_str),
        Some("standard")
    );
}

#[test]
fn account_mode_request_is_bounded_by_the_credential_contract() {
    let query_key = "%".repeat(crate::MAX_CREDENTIAL_BYTES);
    let client = Client::new("http://127.0.0.1", "private_key", query_key.clone())
        .expect("maximum-size credential should construct");
    let params = query_account_mode_params(client.credentials.query_security_key.as_str());
    client
        .form_request("/api/query.php", &params)
        .expect("maximum-size encoded account-mode request should remain bounded")
        .build()
        .expect("bounded account-mode form should serialize");

    assert!(matches!(
        Credentials::new(
            "private_key".to_owned(),
            "%".repeat(crate::MAX_CREDENTIAL_BYTES + 1)
        ),
        Err(ConfigurationError::CredentialTooLong)
    ));
}

#[tokio::test]
async fn borrowed_form_owners_drop_on_construction_form_endpoint_and_send_paths() {
    let construction_drops = Arc::new(AtomicUsize::new(0));
    {
        let _key = DropObserved::new(
            Zeroizing::new("construction_key".to_owned()),
            construction_drops.clone(),
        );
        assert!(amount_string(0).is_err());
    }
    assert_eq!(construction_drops.load(Ordering::SeqCst), 1);

    let form_drops = Arc::new(AtomicUsize::new(0));
    {
        let key = DropObserved::new(Zeroizing::new("form_key".to_owned()), form_drops.clone());
        let request = DropObserved::new(
            StorePaymentMethodRequest {
                payment_token: "tok_form".to_owned(),
                order_id: "ck_form".to_owned(),
                billing_contact: None,
            },
            form_drops.clone(),
        );
        let params = classic_store_payment_method_params(key.as_str(), &request);
        let wire = encoded_form(&params);
        assert!(
            wire.iter()
                .any(|(key, value)| { key == "security_key" && value == "form_key" })
        );
    }
    assert_eq!(form_drops.load(Ordering::SeqCst), 2);

    let endpoint_drops = Arc::new(AtomicUsize::new(0));
    {
        let key = DropObserved::new(
            Zeroizing::new("endpoint_key".to_owned()),
            endpoint_drops.clone(),
        );
        let request = DropObserved::new(
            StorePaymentMethodRequest {
                payment_token: "tok_endpoint".to_owned(),
                order_id: "ck_endpoint".to_owned(),
                billing_contact: None,
            },
            endpoint_drops.clone(),
        );
        let params = classic_store_payment_method_params(key.as_str(), &request);
        let gateway = Client::new(
            "http://127.0.0.1:1",
            "unused_private_key",
            "unused_query_key",
        )
        .expect("local gateway should construct");
        let error = gateway
            .post_form_text("//example.test/escaped", &params)
            .await
            .expect_err("escaped endpoint must fail before form submission");
        assert!(matches!(error, WireError::LocalInvalidRequest(_)));
    }
    assert_eq!(endpoint_drops.load(Ordering::SeqCst), 2);

    let send_drops = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("temporary listener should bind");
    let base_url = format!(
        "http://{}",
        listener.local_addr().expect("listener address")
    );
    drop(listener);
    {
        let key = DropObserved::new(Zeroizing::new("send_key".to_owned()), send_drops.clone());
        let request = DropObserved::new(
            StorePaymentMethodRequest {
                payment_token: "tok_send".to_owned(),
                order_id: "ck_send".to_owned(),
                billing_contact: None,
            },
            send_drops.clone(),
        );
        let params = classic_store_payment_method_params(key.as_str(), &request);
        let gateway = Client::new(base_url, "unused_private_key", "unused_query_key")
            .expect("local gateway should construct");
        let error = gateway
            .post_form_text("/api/transact.php", &params)
            .await
            .expect_err("closed local port should fail submission");
        assert!(matches!(error, WireError::Unavailable(_)));
    }
    assert_eq!(send_drops.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn invalid_mutation_shape_is_rejected_before_transport() {
    let client = Client::new(
        "http://127.0.0.1:1",
        "unused_private_key",
        "unused_query_key",
    )
    .expect("closed-loopback client should construct");
    let error = client
        .sale(SaleRequest {
            amount_cents: 100,
            currency: "USD".to_owned(),
            order_id: "ck_invalid_shape".to_owned(),
            source: PaymentSource::PaymentToken("tok_invalid_shape".to_owned()),
            vault_action: None,
            stored_credential: Some(StoredCredential::RecurringMerchant {
                initial_transaction_id: "txn_initial".to_owned(),
            }),
            billing_contact: None,
            descriptor: None,
        })
        .await
        .expect_err("invalid source/stored-credential combination must fail locally");

    assert!(matches!(error, MutationError::InvalidRequest(_)));
    assert_eq!(error.certainty(), crate::MutationCertainty::NotSubmitted);
}

#[tokio::test]
async fn initial_stored_credential_requires_vault_creation_before_transport() {
    let client = Client::new(
        "http://127.0.0.1:1",
        "unused_private_key",
        "unused_query_key",
    )
    .expect("closed-loopback client should construct");
    let error = client
        .sale(SaleRequest {
            amount_cents: 100,
            currency: "USD".to_owned(),
            order_id: "ck_initial_without_vault".to_owned(),
            source: PaymentSource::PaymentToken("tok_initial_without_vault".to_owned()),
            vault_action: None,
            stored_credential: Some(StoredCredential::InitialCustomer),
            billing_contact: None,
            descriptor: None,
        })
        .await
        .expect_err("initial stored credential without vault creation must fail locally");

    assert!(matches!(error, MutationError::InvalidRequest(_)));
    assert_eq!(error.certainty(), crate::MutationCertainty::NotSubmitted);
}

fn descriptor() -> crate::StatementDescriptor {
    crate::StatementDescriptor {
        name: " IDENTITYPRO TRIAL ".to_owned(),
        phone: Some(" 800-555-0199 ".to_owned()),
        url: Some(" identitypro.com ".to_owned()),
    }
}

fn renewal_with_descriptor(descriptor: Option<crate::StatementDescriptor>) -> SaleRequest {
    SaleRequest {
        amount_cents: 2_699,
        currency: "USD".to_owned(),
        order_id: "ck_descriptor_renewal".to_owned(),
        source: PaymentSource::CustomerVault("vault_descriptor".to_owned()),
        vault_action: None,
        stored_credential: Some(StoredCredential::RecurringMerchant {
            initial_transaction_id: "txn_descriptor_initial".to_owned(),
        }),
        billing_contact: None,
        descriptor,
    }
}

#[test]
fn classic_sale_sends_trimmed_descriptor_fields_and_keeps_them_debug_visible() {
    let request = renewal_with_descriptor(Some(descriptor()));
    let params = classic_sale_params(
        "private_key",
        &request,
        "26.99".to_owned(),
        DuplicateCheck::ProcessorConfigured,
    );
    let form: std::collections::HashMap<&str, &str> = params.iter().collect();
    assert_eq!(form.get("descriptor").copied(), Some("IDENTITYPRO TRIAL"));
    assert_eq!(form.get("descriptor_phone").copied(), Some("800-555-0199"));
    assert_eq!(form.get("descriptor_url").copied(), Some("identitypro.com"));
    for key in ["descriptor", "descriptor_phone", "descriptor_url"] {
        assert!(
            matches!(params.field(key), Some(NmiFormValue::Borrowed(_))),
            "{key} must borrow its source"
        );
    }
    // Merchant statement text is configuration, not customer data.
    let debug = format!("{params:?}");
    assert!(debug.contains("IDENTITYPRO TRIAL"));
    assert!(debug.contains("800-555-0199"));
    assert!(debug.contains("identitypro.com"));
    assert!(!debug.contains("private_key"));
    assert!(!debug.contains("vault_descriptor"));
    let request_debug = format!("{request:?}");
    assert!(request_debug.contains("IDENTITYPRO TRIAL"));
    assert!(!request_debug.contains("vault_descriptor"));

    // Blank optional fields are omitted; the sale is otherwise unchanged.
    let request = renewal_with_descriptor(Some(crate::StatementDescriptor {
        name: "IDENTITYPRO.COM".to_owned(),
        phone: Some("  ".to_owned()),
        url: None,
    }));
    let params = classic_sale_params(
        "private_key",
        &request,
        "26.99".to_owned(),
        DuplicateCheck::ProcessorConfigured,
    );
    let form: std::collections::HashMap<&str, &str> = params.iter().collect();
    assert_eq!(form.get("descriptor").copied(), Some("IDENTITYPRO.COM"));
    assert!(!form.contains_key("descriptor_phone"));
    assert!(!form.contains_key("descriptor_url"));
    assert_eq!(
        form.get("customer_vault_id").copied(),
        Some("vault_descriptor")
    );
    assert_eq!(form.get("initiated_by").copied(), Some("merchant"));

    // Without a descriptor the wire keeps its previous field set.
    let request = renewal_with_descriptor(None);
    let params = classic_sale_params(
        "private_key",
        &request,
        "26.99".to_owned(),
        DuplicateCheck::ProcessorConfigured,
    );
    let form: std::collections::HashMap<&str, &str> = params.iter().collect();
    for key in ["descriptor", "descriptor_phone", "descriptor_url"] {
        assert!(
            !form.contains_key(key),
            "{key} must be absent without a descriptor"
        );
    }
}

#[test]
fn v5_one_time_sale_never_sends_descriptor_fields() {
    let request = SaleRequest {
        amount_cents: 4_900,
        currency: "USD".to_owned(),
        order_id: "ck_v5_soft_text".to_owned(),
        source: PaymentSource::PaymentToken("tok_v5".to_owned()),
        vault_action: None,
        stored_credential: None,
        billing_contact: None,
        descriptor: Some(descriptor()),
    };
    assert!(!sale_uses_classic_form(&request));
    let body = sale_body_json(
        &request,
        amount_value(request.amount_cents).unwrap(),
        DuplicateCheck::ProcessorConfigured,
    );
    let rendered = body.to_string();
    assert!(!rendered.contains("descriptor"), "{rendered}");
    assert!(!rendered.contains("IDENTITYPRO"), "{rendered}");
}

#[tokio::test]
async fn descriptor_outside_local_rules_is_rejected_before_transport() {
    let client = Client::new(
        "http://127.0.0.1:1",
        "unused_private_key",
        "unused_query_key",
    )
    .expect("closed-loopback client should construct");
    let valid = descriptor();
    let cases = [
        crate::StatementDescriptor {
            name: "   ".to_owned(),
            ..valid.clone()
        },
        crate::StatementDescriptor {
            name: "A".repeat(26),
            ..valid.clone()
        },
        crate::StatementDescriptor {
            name: "IDENTITÉ PRO".to_owned(),
            ..valid.clone()
        },
        crate::StatementDescriptor {
            name: "TRIAL\tEND".to_owned(),
            ..valid.clone()
        },
        crate::StatementDescriptor {
            phone: Some("800-555-HELP".to_owned()),
            ..valid.clone()
        },
        crate::StatementDescriptor {
            phone: Some("1".repeat(21)),
            ..valid.clone()
        },
        crate::StatementDescriptor {
            url: Some("identity pro.com".to_owned()),
            ..valid.clone()
        },
        crate::StatementDescriptor {
            url: Some("a".repeat(41)),
            ..valid.clone()
        },
        crate::StatementDescriptor {
            url: Some("identité.com".to_owned()),
            ..valid.clone()
        },
        crate::StatementDescriptor {
            url: Some("identity\u{7f}pro.com".to_owned()),
            ..valid.clone()
        },
        // Trimming never shortens a value that is still too long.
        crate::StatementDescriptor {
            name: format!(" {} ", "A".repeat(26)),
            ..valid.clone()
        },
        crate::StatementDescriptor {
            phone: Some(format!(" {} ", "1".repeat(21))),
            ..valid.clone()
        },
        crate::StatementDescriptor {
            url: Some(format!(" {} ", "a".repeat(41))),
            ..valid.clone()
        },
    ];
    for descriptor in cases {
        let error = client
            .sale(renewal_with_descriptor(Some(descriptor.clone())))
            .await
            .expect_err("descriptor outside local rules must fail locally");
        assert!(
            matches!(error, MutationError::InvalidRequest(_)),
            "{descriptor:?}: {error:?}"
        );
        assert_eq!(error.certainty(), crate::MutationCertainty::NotSubmitted);
    }
    // A conforming descriptor reaches transport and fails on the closed port.
    let error = client
        .sale(renewal_with_descriptor(Some(valid)))
        .await
        .expect_err("closed local port should fail submission");
    assert!(
        !matches!(error, MutationError::InvalidRequest(_)),
        "valid descriptor must pass local validation: {error:?}"
    );
}

#[tokio::test]
async fn descriptor_bounds_apply_to_the_trimmed_values_sent() {
    let client = Client::new(
        "http://127.0.0.1:1",
        "unused_private_key",
        "unused_query_key",
    )
    .expect("closed-loopback client should construct");
    // Like the core `StatementDescriptor`, surrounding whitespace never counts
    // toward a bound, and a whitespace-only optional field is omitted.
    let cases = [
        crate::StatementDescriptor {
            name: format!("  {}  ", "A".repeat(25)),
            phone: Some(format!(" {} ", "1".repeat(20))),
            url: Some(format!(" {} ", "a".repeat(40))),
        },
        crate::StatementDescriptor {
            name: "IDENTITYPRO".to_owned(),
            phone: Some(" ".repeat(30)),
            url: Some(" ".repeat(50)),
        },
    ];
    for descriptor in cases {
        let request = renewal_with_descriptor(Some(descriptor.clone()));
        {
            let params = classic_sale_params(
                "private_key",
                &request,
                "26.99".to_owned(),
                DuplicateCheck::ProcessorConfigured,
            );
            let form: std::collections::HashMap<&str, &str> = params.iter().collect();
            fn sent(value: &Option<String>) -> Option<&str> {
                value
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
            }
            assert_eq!(
                form.get("descriptor").copied(),
                Some(descriptor.name.trim())
            );
            assert_eq!(
                form.get("descriptor_phone").copied(),
                sent(&descriptor.phone)
            );
            assert_eq!(form.get("descriptor_url").copied(), sent(&descriptor.url));
        }
        // The descriptor passes local validation and fails only on the closed
        // local port.
        let error = client
            .sale(request)
            .await
            .expect_err("closed local port should fail submission");
        assert!(
            !matches!(error, MutationError::InvalidRequest(_)),
            "{descriptor:?}: {error:?}"
        );
    }
}
