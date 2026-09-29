use super::*;
use crate::BillingAddress;

fn padded_address() -> BillingAddress {
    BillingAddress {
        address1: " 1 Main St ".to_owned(),
        address2: Some(" Suite 2 ".to_owned()),
        city: Some(" Boston ".to_owned()),
        state: Some("MA".to_owned()),
        zip: Some(" 02110-1234 ".to_owned()),
        country: "US".to_owned(),
    }
}

fn expected_address_fields() -> [(&'static str, &'static str); 6] {
    [
        ("address1", "1 Main St"),
        ("address2", "Suite 2"),
        ("city", "Boston"),
        ("state", "MA"),
        ("zip", "02110-1234"),
        ("country", "US"),
    ]
}

fn named_contact(address: Option<BillingAddress>) -> BillingContact {
    BillingContact {
        first_name: Some(" Ada ".to_owned()),
        last_name: Some(" Lovelace ".to_owned()),
        email: Some(" ada@example.test ".to_owned()),
        address,
    }
}

fn address_only_contact(address: BillingAddress) -> BillingContact {
    BillingContact {
        first_name: None,
        last_name: None,
        email: None,
        address: Some(address),
    }
}

fn initial_sale(contact: Option<BillingContact>) -> SaleRequest {
    SaleRequest {
        amount_cents: 4_900,
        currency: "USD".to_owned(),
        order_id: "ck_sub_123".to_owned(),
        source: PaymentSource::PaymentToken("tok_test".to_owned()),
        vault_action: Some(VaultAction::AddCustomer),
        stored_credential: Some(StoredCredential::InitialCustomer),
        billing_contact: contact,
    }
}

fn merchant_renewal(contact: Option<BillingContact>) -> SaleRequest {
    SaleRequest {
        amount_cents: 4_900,
        currency: "USD".to_owned(),
        order_id: "ck_renewal_123".to_owned(),
        source: PaymentSource::CustomerVault("vault_123".to_owned()),
        vault_action: None,
        stored_credential: Some(StoredCredential::RecurringMerchant {
            initial_transaction_id: "txn_initial_123".to_owned(),
        }),
        billing_contact: contact,
    }
}

fn one_time_sale(contact: Option<BillingContact>) -> SaleRequest {
    SaleRequest {
        amount_cents: 4_900,
        currency: "USD".to_owned(),
        order_id: "ck_charge_123".to_owned(),
        source: PaymentSource::PaymentToken("tok_test".to_owned()),
        vault_action: None,
        stored_credential: None,
        billing_contact: contact,
    }
}

fn store(contact: Option<BillingContact>) -> StorePaymentMethodRequest {
    StorePaymentMethodRequest {
        payment_token: "tok_update".to_owned(),
        order_id: "ck_payment_method_123".to_owned(),
        billing_contact: contact,
    }
}

fn owned_pairs(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

fn assert_address_is_borrowed_and_redacted(params: &NmiFormParams<'_>) {
    let debug = format!("{params:?}");
    for (key, value) in expected_address_fields() {
        assert!(
            matches!(params.field(key), Some(NmiFormValue::Borrowed(borrowed)) if *borrowed == value),
            "{key} must borrow its trimmed source value"
        );
        assert!(
            debug.contains(&format!("{key:?}: \"[redacted]\"")),
            "{key} must stay off the form Debug allowlist: {debug}"
        );
    }
    for value in ["1 Main St", "Suite 2", "Boston", "02110-1234"] {
        assert!(!debug.contains(value), "{value} leaked into {debug}");
    }
}

#[test]
fn classic_sale_and_validate_forms_carry_every_trimmed_address_field() {
    let request = initial_sale(Some(named_contact(Some(padded_address()))));
    let params = classic_sale_params(
        "private_key",
        &request,
        "49.00".to_owned(),
        DuplicateCheck::ProcessorConfigured,
    );
    assert_eq!(
        encoded_form(&params),
        owned_pairs(&[
            ("security_key", "private_key"),
            ("type", "sale"),
            ("amount", "49.00"),
            ("currency", "USD"),
            ("orderid", "ck_sub_123"),
            ("payment_token", "tok_test"),
            ("customer_vault", "add_customer"),
            ("billing_method", "recurring"),
            ("stored_credential_indicator", "stored"),
            ("initiated_by", "customer"),
            ("first_name", "Ada"),
            ("last_name", "Lovelace"),
            ("email", "ada@example.test"),
            ("address1", "1 Main St"),
            ("address2", "Suite 2"),
            ("city", "Boston"),
            ("state", "MA"),
            ("zip", "02110-1234"),
            ("country", "US"),
        ])
    );
    assert_address_is_borrowed_and_redacted(&params);

    // Renewals send an address-only contact; names and email stay absent.
    let request = merchant_renewal(Some(address_only_contact(padded_address())));
    let params = classic_sale_params(
        "private_key",
        &request,
        "49.00".to_owned(),
        DuplicateCheck::ProcessorConfigured,
    );
    assert_eq!(
        encoded_form(&params),
        owned_pairs(&[
            ("security_key", "private_key"),
            ("type", "sale"),
            ("amount", "49.00"),
            ("currency", "USD"),
            ("orderid", "ck_renewal_123"),
            ("customer_vault_id", "vault_123"),
            ("billing_method", "recurring"),
            ("stored_credential_indicator", "used"),
            ("initiated_by", "merchant"),
            ("initial_transaction_id", "txn_initial_123"),
            ("address1", "1 Main St"),
            ("address2", "Suite 2"),
            ("city", "Boston"),
            ("state", "MA"),
            ("zip", "02110-1234"),
            ("country", "US"),
        ])
    );
    assert_address_is_borrowed_and_redacted(&params);

    let request = store(Some(named_contact(Some(padded_address()))));
    let params = classic_store_payment_method_params("private_key", &request);
    assert_eq!(
        encoded_form(&params),
        owned_pairs(&[
            ("security_key", "private_key"),
            ("customer_vault", "add_customer"),
            ("payment_token", "tok_update"),
            ("orderid", "ck_payment_method_123"),
            ("type", "validate"),
            ("billing_method", "recurring"),
            ("initiated_by", "customer"),
            ("stored_credential_indicator", "stored"),
            ("first_name", "Ada"),
            ("last_name", "Lovelace"),
            ("email", "ada@example.test"),
            ("address1", "1 Main St"),
            ("address2", "Suite 2"),
            ("city", "Boston"),
            ("state", "MA"),
            ("zip", "02110-1234"),
            ("country", "US"),
        ])
    );
    assert_address_is_borrowed_and_redacted(&params);
}

#[test]
fn blank_optional_address_fields_are_omitted_from_every_wire() {
    let sparse = BillingAddress {
        address1: "1 Main St".to_owned(),
        address2: Some("  ".to_owned()),
        city: None,
        state: Some(" ".to_owned()),
        zip: Some(String::new()),
        country: "US".to_owned(),
    };
    let request = merchant_renewal(Some(address_only_contact(sparse)));
    let params = classic_sale_params(
        "private_key",
        &request,
        "49.00".to_owned(),
        DuplicateCheck::ProcessorConfigured,
    );
    let form: std::collections::HashMap<_, _> = encoded_form(&params).into_iter().collect();
    assert_eq!(form.get("address1").map(String::as_str), Some("1 Main St"));
    assert_eq!(form.get("country").map(String::as_str), Some("US"));
    for key in ["address2", "city", "state", "zip"] {
        assert!(!form.contains_key(key), "{key} must be omitted");
    }
    assert!(validate_sale_request(&request, "private_key").is_ok());

    let request = one_time_sale(request.billing_contact);
    let body = sale_body_json(
        &request,
        amount_value(request.amount_cents).expect("positive amount"),
        DuplicateCheck::ProcessorConfigured,
    );
    assert_eq!(
        body["billing_address"],
        json!({ "address1": "1 Main St", "country": "US" })
    );
}

#[test]
fn v5_sale_billing_address_carries_every_address_field() {
    let request = one_time_sale(Some(named_contact(Some(padded_address()))));
    let body = sale_body_json(
        &request,
        amount_value(request.amount_cents).expect("positive amount"),
        DuplicateCheck::ProcessorConfigured,
    );
    assert_eq!(
        body["billing_address"],
        json!({
            "first_name": "Ada",
            "last_name": "Lovelace",
            "email": "ada@example.test",
            "address1": "1 Main St",
            "address2": "Suite 2",
            "city": "Boston",
            "state": "MA",
            "zip": "02110-1234",
            "country": "US",
        })
    );
    assert!(body.get("cit_mit").is_none());
    assert!(body.get("customer_vault").is_none());

    let request = one_time_sale(Some(address_only_contact(padded_address())));
    let body = sale_body_json(
        &request,
        amount_value(request.amount_cents).expect("positive amount"),
        DuplicateCheck::ProcessorConfigured,
    );
    let mut expected = serde_json::Map::new();
    for (key, value) in expected_address_fields() {
        expected.insert(key.to_owned(), json!(value));
    }
    assert_eq!(body["billing_address"], Value::Object(expected));
}

#[test]
fn addressless_requests_keep_their_exact_wire_bytes() {
    let names_only = || named_contact(None);

    let request = initial_sale(Some(names_only()));
    let params = classic_sale_params(
        "private_key",
        &request,
        "49.00".to_owned(),
        DuplicateCheck::ProcessorConfigured,
    );
    assert_eq!(
        encoded_form(&params),
        owned_pairs(&[
            ("security_key", "private_key"),
            ("type", "sale"),
            ("amount", "49.00"),
            ("currency", "USD"),
            ("orderid", "ck_sub_123"),
            ("payment_token", "tok_test"),
            ("customer_vault", "add_customer"),
            ("billing_method", "recurring"),
            ("stored_credential_indicator", "stored"),
            ("initiated_by", "customer"),
            ("first_name", "Ada"),
            ("last_name", "Lovelace"),
            ("email", "ada@example.test"),
        ])
    );

    let request = merchant_renewal(None);
    let params = classic_sale_params(
        "private_key",
        &request,
        "49.00".to_owned(),
        DuplicateCheck::ProcessorConfigured,
    );
    assert_eq!(
        encoded_form(&params),
        owned_pairs(&[
            ("security_key", "private_key"),
            ("type", "sale"),
            ("amount", "49.00"),
            ("currency", "USD"),
            ("orderid", "ck_renewal_123"),
            ("customer_vault_id", "vault_123"),
            ("billing_method", "recurring"),
            ("stored_credential_indicator", "used"),
            ("initiated_by", "merchant"),
            ("initial_transaction_id", "txn_initial_123"),
        ])
    );

    let request = store(Some(names_only()));
    let params = classic_store_payment_method_params("private_key", &request);
    assert_eq!(
        encoded_form(&params),
        owned_pairs(&[
            ("security_key", "private_key"),
            ("customer_vault", "add_customer"),
            ("payment_token", "tok_update"),
            ("orderid", "ck_payment_method_123"),
            ("type", "validate"),
            ("billing_method", "recurring"),
            ("initiated_by", "customer"),
            ("stored_credential_indicator", "stored"),
            ("first_name", "Ada"),
            ("last_name", "Lovelace"),
            ("email", "ada@example.test"),
        ])
    );

    let request = one_time_sale(Some(names_only()));
    let body = sale_body_json(
        &request,
        amount_value(request.amount_cents).expect("positive amount"),
        DuplicateCheck::ProcessorConfigured,
    );
    assert_eq!(
        serde_json::to_string(&body).expect("v5 body should serialize"),
        concat!(
            r#"{"amount":"49.00","#,
            r#""billing_address":{"email":"ada@example.test","first_name":"Ada","last_name":"Lovelace"},"#,
            r#""currency":"USD","order_details":{"id":"ck_charge_123"},"#,
            r#""payment_details":{"payment_token":"tok_test"}}"#,
        )
    );
}

type RouteValidation = fn(Option<BillingContact>) -> Result<(), MutationError>;

fn wire_routes() -> [(&'static str, RouteValidation); 4] {
    [
        ("classic initial sale", |contact| {
            validate_sale_request(&initial_sale(contact), "private_key")
        }),
        ("classic merchant renewal", |contact| {
            validate_sale_request(&merchant_renewal(contact), "private_key")
        }),
        ("classic validate", |contact| {
            validate_store_payment_method_request(&store(contact), "private_key")
        }),
        ("v5 sale", |contact| {
            validate_sale_request(&one_time_sale(contact), "private_key")
        }),
    ]
}

fn with_address(change: impl FnOnce(&mut BillingAddress)) -> Option<BillingContact> {
    let mut address = padded_address();
    change(&mut address);
    Some(address_only_contact(address))
}

type AddressChange = Box<dyn Fn(&mut BillingAddress)>;

#[test]
fn address_bounds_apply_identically_on_every_wire_route() {
    // Each multibyte value has no more characters than its byte cap.
    let multibyte_line = "é".repeat(51);
    let multibyte_city = "é".repeat(26);
    assert!(multibyte_line.chars().count() <= 100 && multibyte_line.len() > 100);
    assert!(multibyte_city.chars().count() <= 50 && multibyte_city.len() > 50);

    let accepted: Vec<(&str, AddressChange)> = vec![
        ("full address", Box::new(|_| {})),
        (
            "100-byte lines",
            Box::new(|address| {
                address.address1 = "a".repeat(100);
                address.address2 = Some("b".repeat(100));
            }),
        ),
        (
            "50-byte city",
            Box::new(|address| address.city = Some("c".repeat(50))),
        ),
        (
            "multibyte fields within the byte cap",
            Box::new(|address| {
                address.address1 = "é".repeat(50);
                address.address2 = Some("ü".repeat(50));
                address.city = Some("ö".repeat(25));
            }),
        ),
        (
            "20-byte ZIP",
            Box::new(|address| address.zip = Some("1".repeat(20))),
        ),
        (
            "alphanumeric postcode with space",
            Box::new(|address| address.zip = Some("SW1A 1AA".to_owned())),
        ),
        (
            "numeric subdivision",
            Box::new(|address| address.state = Some("01".to_owned())),
        ),
        (
            "absent optional fields",
            Box::new(|address| {
                address.address2 = None;
                address.city = None;
                address.state = None;
                address.zip = None;
            }),
        ),
    ];
    let line_error = "billing address line 1 exceeds the supported size";
    let line2_error = "billing address line 2 exceeds the supported size";
    let city_error = "billing city exceeds the supported size";
    let zip_size_error = "billing ZIP code exceeds the supported size";
    let zip_format_error = "billing ZIP code contains unsupported characters";
    let state_error = "billing state must be two ASCII letters or digits";
    let country_error = "billing country must be two uppercase ASCII letters";
    let rejected: Vec<(&str, AddressChange, &str)> = vec![
        (
            "blank line 1",
            Box::new(|address| address.address1 = "  ".to_owned()),
            "billing address line 1 is required",
        ),
        (
            "101-byte line 1",
            Box::new(|address| address.address1 = "a".repeat(101)),
            line_error,
        ),
        (
            "multibyte line 1 over the byte cap",
            Box::new({
                let value = multibyte_line.clone();
                move |address| address.address1 = value.clone()
            }),
            line_error,
        ),
        (
            "101-byte line 2",
            Box::new(|address| address.address2 = Some("b".repeat(101))),
            line2_error,
        ),
        (
            "multibyte line 2 over the byte cap",
            Box::new({
                let value = multibyte_line.clone();
                move |address| address.address2 = Some(value.clone())
            }),
            line2_error,
        ),
        (
            "51-byte city",
            Box::new(|address| address.city = Some("c".repeat(51))),
            city_error,
        ),
        (
            "multibyte city over the byte cap",
            Box::new({
                let value = multibyte_city.clone();
                move |address| address.city = Some(value.clone())
            }),
            city_error,
        ),
        (
            "21-byte ZIP",
            Box::new(|address| address.zip = Some("1".repeat(21))),
            zip_size_error,
        ),
        (
            "multibyte ZIP over the byte cap",
            Box::new(|address| address.zip = Some("é".repeat(11))),
            zip_size_error,
        ),
        (
            "ZIP punctuation",
            Box::new(|address| address.zip = Some("02110_1234".to_owned())),
            zip_format_error,
        ),
        (
            "multibyte ZIP within the byte cap",
            Box::new(|address| address.zip = Some("0211é".to_owned())),
            zip_format_error,
        ),
        (
            "one-character state",
            Box::new(|address| address.state = Some("M".to_owned())),
            state_error,
        ),
        (
            "three-character state",
            Box::new(|address| address.state = Some("MAS".to_owned())),
            state_error,
        ),
        (
            "punctuated state",
            Box::new(|address| address.state = Some("M-".to_owned())),
            state_error,
        ),
        (
            "non-ASCII two-character state",
            Box::new(|address| address.state = Some("ÄB".to_owned())),
            state_error,
        ),
        (
            "padded state",
            Box::new(|address| address.state = Some(" MA".to_owned())),
            state_error,
        ),
        (
            "padded country",
            Box::new(|address| address.country = "US ".to_owned()),
            country_error,
        ),
        (
            "lowercase country",
            Box::new(|address| address.country = "us".to_owned()),
            country_error,
        ),
        (
            "three-letter country",
            Box::new(|address| address.country = "USA".to_owned()),
            country_error,
        ),
        (
            "numeric country",
            Box::new(|address| address.country = "U1".to_owned()),
            country_error,
        ),
        (
            "blank country",
            Box::new(|address| address.country = " ".to_owned()),
            country_error,
        ),
    ];

    for (route, validate) in wire_routes() {
        for (case, change) in &accepted {
            assert!(
                validate(with_address(change)).is_ok(),
                "{route} must accept {case}"
            );
        }
        for (case, change, detail) in &rejected {
            let error =
                validate(with_address(change)).expect_err(&format!("{route} must reject {case}"));
            assert!(
                matches!(error, MutationError::InvalidRequest(_)),
                "{route}: {case}"
            );
            assert_eq!(error.detail().expose(), *detail, "{route}: {case}");
            assert_eq!(
                error.certainty(),
                crate::MutationCertainty::NotSubmitted,
                "{route}: {case}"
            );
        }
    }
}

#[tokio::test]
async fn malformed_addresses_fail_before_network_io_on_every_route() {
    let (client, mut request_receiver, server) =
        spawn_capturing_server("HTTP/1.1 200 OK", "text/plain", b"unexpected".to_vec()).await;
    let invalid = || with_address(|address| address.address1 = "a".repeat(101));

    let errors = [
        client.sale(initial_sale(invalid())).await,
        client.sale(merchant_renewal(invalid())).await,
        client.sale(one_time_sale(invalid())).await,
        client.store_payment_method(store(invalid())).await,
    ];
    for error in errors {
        let error = error.expect_err("an invalid address must fail locally");
        assert!(matches!(error, MutationError::InvalidRequest(_)));
        assert_eq!(error.certainty(), crate::MutationCertainty::NotSubmitted);
        assert!(!format!("{error:?}").contains("aaaa"));
    }
    assert!(
        request_receiver.try_recv().is_err(),
        "locally rejected address requests must not reach the listener"
    );
    server.abort();
}

#[tokio::test]
async fn merchant_renewal_size_is_measured_as_the_classic_form_it_is_sent_as() {
    let private_key = "%".repeat(crate::MAX_CREDENTIAL_BYTES);
    let vault_heavy = |stored_credential| SaleRequest {
        amount_cents: 4_900,
        currency: "USD".to_owned(),
        order_id: "ck_renewal_123".to_owned(),
        source: PaymentSource::CustomerVault("%".repeat(MAX_NMI_IDENTIFIER_BYTES)),
        vault_action: None,
        stored_credential,
        billing_contact: with_address(|address| address.address1 = "%".repeat(100)),
    };
    let renewal = vault_heavy(Some(StoredCredential::RecurringMerchant {
        initial_transaction_id: "%".repeat(MAX_NMI_IDENTIFIER_BYTES),
    }));
    // The same values fit when JSON-encoded, where `%` costs one byte instead
    // of three; a vault sale without stored credentials is sent as v5 JSON.
    assert!(validate_sale_request(&vault_heavy(None), &private_key).is_ok());
    let error = validate_sale_request(&renewal, &private_key)
        .expect_err("the renewal's Classic form exceeds the encoded request budget");
    assert_eq!(
        error.detail().expose(),
        "NMI outbound request exceeds the supported size"
    );

    let (mut client, mut request_receiver, server) =
        spawn_capturing_server("HTTP/1.1 200 OK", "text/plain", b"unexpected".to_vec()).await;
    client.credentials = Credentials::new(private_key, "query_key".to_owned())
        .expect("maximum-size private key should construct");
    let error = client
        .sale(renewal)
        .await
        .expect_err("oversized renewal form must fail locally");
    assert!(matches!(error, MutationError::InvalidRequest(_)));
    assert_eq!(error.certainty(), crate::MutationCertainty::NotSubmitted);
    assert!(request_receiver.try_recv().is_err());
    server.abort();
}

#[test]
fn raw_address_formatting_is_value_free() {
    let contact = named_contact(Some(padded_address()));
    let debug = format!("{contact:?}");
    assert!(debug.contains("has_address: true"));
    let address_debug = format!("{:?}", padded_address());
    assert!(address_debug.contains("has_zip: true"));
    let sale_debug = format!("{:?}", initial_sale(Some(contact)));
    for value in ["Main", "Suite", "Boston", "02110", "Ada", "example.test"] {
        assert!(!debug.contains(value), "{value} leaked into {debug}");
        assert!(!address_debug.contains(value));
        assert!(!sale_debug.contains(value));
    }
}
