use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use syrup_rail::{CardholderIpAddress, PaymentGateway};

use super::billing_address::{
    CLASSIC_APPROVAL, V5_APPROVAL, capturing_gateway, charge, finish, form_fields, json_body,
    order, recurring_intent,
};
use super::*;

const V4: Ipv4Addr = Ipv4Addr::new(203, 0, 113, 7);

fn v4() -> Option<CardholderIpAddress> {
    Some(CardholderIpAddress::new(IpAddr::V4(V4)))
}

fn initial_intent() -> GatewaySaleIntent {
    GatewaySaleIntent::InitialStoredCredential {
        payment_token: PaymentToken::new("tok_initial").unwrap(),
    }
}

#[test]
fn cardholder_ip_maps_only_onto_customer_initiated_sales() {
    let initial = map_sale_request(
        GatewaySaleRequest::new(charge(), order(), initial_intent(), None).with_cardholder_ip(v4()),
    )
    .expect("USD enrollment maps");
    assert_eq!(initial.cardholder_ip, Some(IpAddr::V4(V4)));

    let renewal = map_sale_request(
        GatewaySaleRequest::new(charge(), order(), recurring_intent(), None)
            .with_cardholder_ip(v4()),
    )
    .expect("USD renewal maps");
    assert_eq!(
        renewal.cardholder_ip, None,
        "a merchant-initiated charge never carries a cardholder address"
    );

    let without = map_sale_request(GatewaySaleRequest::new(
        charge(),
        order(),
        initial_intent(),
        None,
    ))
    .expect("USD enrollment maps");
    assert_eq!(without.cardholder_ip, None);
}

#[tokio::test]
async fn cardholder_ip_reaches_the_classic_wire_only_for_the_initial_sale() {
    // The customer-initiated enrollment sale carries the address.
    let (gateway, captured, server) = capturing_gateway(CLASSIC_APPROVAL).await;
    let outcome = gateway
        .sale(
            GatewaySaleRequest::new(charge(), order(), initial_intent(), None)
                .with_cardholder_ip(v4()),
        )
        .await
        .expect("initial sale should be approved");
    assert_eq!(outcome.status(), GatewayPaymentStatus::Approved);
    let fields = form_fields(&captured.await.unwrap());
    assert_eq!(fields["ipaddress"], "203.0.113.7");
    assert_eq!(fields["initiated_by"], "customer");
    finish(server).await;

    // A merchant-initiated renewal never does, even if the request holds one.
    let (gateway, captured, server) = capturing_gateway(CLASSIC_APPROVAL).await;
    let outcome = gateway
        .sale(
            GatewaySaleRequest::new(charge(), order(), recurring_intent(), None)
                .with_cardholder_ip(v4()),
        )
        .await
        .expect("renewal should be approved");
    assert_eq!(outcome.status(), GatewayPaymentStatus::Approved);
    let fields = form_fields(&captured.await.unwrap());
    assert_eq!(fields["initiated_by"], "merchant");
    assert!(!fields.contains_key("ipaddress"), "{:?}", fields.keys());
    finish(server).await;

    // Without an address the wire is exactly as before.
    let (gateway, captured, server) = capturing_gateway(CLASSIC_APPROVAL).await;
    let outcome = gateway
        .sale(GatewaySaleRequest::new(
            charge(),
            order(),
            initial_intent(),
            None,
        ))
        .await
        .expect("initial sale should be approved");
    assert_eq!(outcome.status(), GatewayPaymentStatus::Approved);
    let fields = form_fields(&captured.await.unwrap());
    assert!(!fields.contains_key("ipaddress"), "{:?}", fields.keys());
    finish(server).await;
}

#[tokio::test]
async fn ipv6_cardholder_address_reaches_the_classic_wire() {
    let address = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0x17));
    let (gateway, captured, server) = capturing_gateway(CLASSIC_APPROVAL).await;
    let outcome = gateway
        .sale(
            GatewaySaleRequest::new(charge(), order(), initial_intent(), None)
                .with_cardholder_ip(Some(CardholderIpAddress::new(address))),
        )
        .await
        .expect("initial sale should be approved");
    assert_eq!(outcome.status(), GatewayPaymentStatus::Approved);
    // The colons survive form encoding.
    let fields = form_fields(&captured.await.unwrap());
    assert_eq!(fields["ipaddress"], "2001:db8::17");
    assert_eq!(fields["initiated_by"], "customer");
    finish(server).await;
}

#[tokio::test]
async fn one_time_v5_sale_sends_no_cardholder_address() {
    let (gateway, captured, server) = capturing_gateway(V5_APPROVAL).await;
    let outcome = gateway
        .sale(
            GatewaySaleRequest::new(
                charge(),
                order(),
                GatewaySaleIntent::OneTime {
                    payment_token: PaymentToken::new("tok_charge").unwrap(),
                },
                None,
            )
            .with_cardholder_ip(v4()),
        )
        .await
        .expect("host charge should be approved");
    assert_eq!(outcome.status(), GatewayPaymentStatus::Approved);
    let body = json_body(&captured.await.unwrap()).to_string();
    assert!(!body.contains("203.0.113.7"), "{body}");
    assert!(!body.contains("ip_address"), "{body}");
    finish(server).await;
}
