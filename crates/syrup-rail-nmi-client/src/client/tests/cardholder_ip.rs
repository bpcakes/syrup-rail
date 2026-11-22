//! The cardholder network address on the wire: IPv4 or IPv6 on a Classic sale,
//! nothing on the v5 sale, and never in `Debug` output.

use super::*;

fn initial_sale_with_cardholder_ip(cardholder_ip: Option<std::net::IpAddr>) -> SaleRequest {
    SaleRequest {
        amount_cents: 100,
        currency: "USD".to_owned(),
        order_id: "ck_cardholder_ip".to_owned(),
        source: PaymentSource::PaymentToken("tok_cardholder_ip".to_owned()),
        vault_action: Some(VaultAction::AddCustomer),
        stored_credential: Some(StoredCredential::InitialCustomer),
        billing_contact: None,
        descriptor: None,
        cardholder_ip,
    }
}

fn classic_ipaddress(request: &SaleRequest) -> Option<String> {
    let params = classic_sale_params(
        "private_key",
        request,
        "1.00".to_owned(),
        DuplicateCheck::ProcessorConfigured,
    );
    let mut values = params
        .iter()
        .filter(|(key, _)| *key == "ipaddress")
        .map(|(_, value)| value.to_owned())
        .collect::<Vec<_>>();
    let value = values.pop();
    assert!(values.is_empty(), "the field is sent at most once");
    value
}

#[test]
fn classic_sale_sends_an_ipv4_cardholder_address_and_redacts_it_from_debug() {
    use std::net::{IpAddr, Ipv4Addr};

    let address = Ipv4Addr::new(203, 0, 113, 7);
    let request = initial_sale_with_cardholder_ip(Some(IpAddr::V4(address)));
    assert_eq!(classic_ipaddress(&request).as_deref(), Some("203.0.113.7"));

    // An IPv4-mapped IPv6 address is the same IPv4 cardholder.
    let mapped = initial_sale_with_cardholder_ip(Some(IpAddr::V6(address.to_ipv6_mapped())));
    assert_eq!(classic_ipaddress(&mapped).as_deref(), Some("203.0.113.7"));

    // A network address is customer data: neither the wire parameters nor the
    // request print it.
    let params = classic_sale_params(
        "private_key",
        &request,
        "1.00".to_owned(),
        DuplicateCheck::ProcessorConfigured,
    );
    assert!(matches!(
        params.field("ipaddress"),
        Some(NmiFormValue::CustomerOwned(_))
    ));
    let debug = format!("{params:?} {request:?}");
    assert!(!debug.contains("203.0.113.7"), "{debug}");
    assert!(debug.contains("has_cardholder_ip: true"), "{debug}");

    assert_eq!(
        classic_ipaddress(&initial_sale_with_cardholder_ip(None)),
        None
    );
}

#[test]
fn classic_sale_sends_an_ipv6_cardholder_address_and_redacts_it_from_debug() {
    // NMI records the text exactly as sent, so every spelling of one address
    // must reach it in the same compressed form.
    for spelling in [
        "2001:db8:1234::97",
        "2001:0DB8:1234:0000:0000:0000:0000:0097",
    ] {
        let request = initial_sale_with_cardholder_ip(Some(spelling.parse().unwrap()));
        assert_eq!(
            classic_ipaddress(&request).as_deref(),
            Some("2001:db8:1234::97")
        );
        let params = classic_sale_params(
            "private_key",
            &request,
            "1.00".to_owned(),
            DuplicateCheck::ProcessorConfigured,
        );
        assert!(matches!(
            params.field("ipaddress"),
            Some(NmiFormValue::CustomerOwned(_))
        ));
        let debug = format!("{params:?} {request:?}");
        assert!(!debug.contains("2001"), "{debug}");
        assert!(debug.contains("has_cardholder_ip: true"), "{debug}");
    }
}

#[test]
fn classic_sale_sends_an_uncompressible_ipv6_cardholder_address_whole() {
    // Eight four-digit groups; the documentation prefixes have none this long.
    let address = "fd00:1111:2222:3333:4444:5555:6666:7777";
    assert_eq!(address.len(), crate::client::MAX_NMI_IP_ADDRESS_BYTES);
    let request = initial_sale_with_cardholder_ip(Some(address.parse().unwrap()));
    assert_eq!(classic_ipaddress(&request).as_deref(), Some(address));
    crate::client::validation::validate_sale_request(&request, "private_key")
        .expect("the longest address is exactly at its bound");
}

#[test]
fn classic_sale_owns_only_the_formatted_amount_and_cardholder_address() {
    let mut request = initial_sale_with_cardholder_ip(Some("2001:db8::17".parse().unwrap()));
    request.billing_contact = Some(BillingContact {
        first_name: Some("Ada".to_owned()),
        last_name: Some("Lovelace".to_owned()),
        email: Some("ada@example.test".to_owned()),
        address: None,
    });
    let params = classic_sale_params(
        "private_key",
        &request,
        "1.00".to_owned(),
        DuplicateCheck::ProcessorConfigured,
    );
    // The whole form, in order: exactly one address, after everything else.
    let sent = encoded_form(&params);
    let keys = sent.iter().map(|(key, _)| key.as_str()).collect::<Vec<_>>();
    assert_eq!(
        keys,
        [
            "security_key",
            "type",
            "amount",
            "currency",
            "orderid",
            "payment_token",
            "customer_vault",
            "billing_method",
            "stored_credential_indicator",
            "initiated_by",
            "first_name",
            "last_name",
            "email",
            "ipaddress",
        ]
    );
    assert_eq!(sent.last().unwrap().1, "2001:db8::17");
    for key in keys {
        let value = params.field(key).expect("an encoded field exists");
        match key {
            "amount" => assert!(matches!(value, NmiFormValue::PublicOwned(_))),
            "ipaddress" => assert!(matches!(value, NmiFormValue::CustomerOwned(_))),
            _ => assert!(matches!(value, NmiFormValue::Borrowed(_)), "{key}"),
        }
    }
}

/// The longest private key a sale's size budget leaves room for. Each key
/// character takes one byte in either encoding, so a shorter result is the
/// exact number of bytes something else in the request was charged.
fn largest_key(request: &SaleRequest) -> usize {
    let validate = |length: usize| {
        crate::client::validation::validate_sale_request(request, &"k".repeat(length))
    };
    let lengths = (1..=16 * 1024).collect::<Vec<usize>>();
    let largest = lengths.partition_point(|length| validate(*length).is_ok());
    assert!((1..lengths.len()).contains(&largest));
    let Err(crate::MutationError::InvalidRequest(refused)) = validate(largest + 1) else {
        panic!("one more byte is over the limit");
    };
    assert_eq!(
        refused.expose(),
        "NMI outbound request exceeds the supported size"
    );
    largest
}

#[test]
fn classic_sale_size_validation_counts_the_encoded_cardholder_address() {
    let without = largest_key(&initial_sale_with_cardholder_ip(None));
    for (address, encoded) in [
        ("203.0.113.7", "203.0.113.7"),
        ("2001:db8::17", "2001%3Adb8%3A%3A17"),
    ] {
        let request = initial_sale_with_cardholder_ip(Some(address.parse().unwrap()));
        assert_eq!(without - largest_key(&request), encoded.len(), "{address}");
    }
}

/// The sales that use the v5 JSON API: a one-time token sale and a saved-card
/// sale without a stored credential.
fn v5_sales(cardholder_ip: Option<std::net::IpAddr>) -> [SaleRequest; 2] {
    [
        PaymentSource::PaymentToken("tok_v5".to_owned()),
        PaymentSource::CustomerVault("vault_v5".to_owned()),
    ]
    .map(|source| SaleRequest {
        amount_cents: 4_900,
        currency: "USD".to_owned(),
        order_id: "ck_v5_cardholder_ip".to_owned(),
        source,
        vault_action: None,
        stored_credential: None,
        billing_contact: None,
        descriptor: None,
        cardholder_ip,
    })
}

#[test]
fn v5_sales_send_no_cardholder_address_and_are_not_charged_for_one() {
    let address = "203.0.113.7".parse().unwrap();
    for (with, without) in v5_sales(Some(address)).iter().zip(&v5_sales(None)) {
        assert!(!sale_uses_classic_form(with));
        let body = sale_body_json(
            with,
            amount_value(with.amount_cents).unwrap(),
            DuplicateCheck::ProcessorConfigured,
        )
        .to_string();
        assert!(!body.contains("203.0.113.7"), "{body}");
        assert!(!body.contains("ip_address"), "{body}");
        assert!(!body.contains("ipaddress"), "{body}");
        assert_eq!(largest_key(with), largest_key(without));
    }
}
