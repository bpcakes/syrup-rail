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
    let form: std::collections::HashMap<&str, &str> = params.iter().collect();
    form.get("ipaddress").map(|value| (*value).to_owned())
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
        crate::client::validation::validate_sale_request(&request, "private_key")
            .expect("an IPv6 address never invalidates the sale");

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
    let address = "2001:db8:1111:2222:3333:4444:5555:6666";
    let request = initial_sale_with_cardholder_ip(Some(address.parse().unwrap()));
    assert_eq!(classic_ipaddress(&request).as_deref(), Some(address));
}

#[test]
fn v5_sale_body_sends_no_cardholder_address() {
    let request = SaleRequest {
        amount_cents: 4_900,
        currency: "USD".to_owned(),
        order_id: "ck_v5_cardholder_ip".to_owned(),
        source: PaymentSource::PaymentToken("tok_v5".to_owned()),
        vault_action: None,
        stored_credential: None,
        billing_contact: None,
        descriptor: None,
        cardholder_ip: Some(std::net::IpAddr::V4(std::net::Ipv4Addr::new(
            203, 0, 113, 7,
        ))),
    };
    let body = sale_body_json(
        &request,
        amount_value(request.amount_cents).unwrap(),
        DuplicateCheck::ProcessorConfigured,
    )
    .to_string();
    assert!(!body.contains("203.0.113.7"), "{body}");
    assert!(!body.contains("ip_address"), "{body}");
}
