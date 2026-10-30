use syrup_rail::{PaymentGateway, StatementDescriptor};

use super::billing_address::{
    CLASSIC_APPROVAL, V5_APPROVAL, capturing_gateway, charge, finish, form_fields, json_body,
    order, recurring_intent,
};
use super::*;

fn descriptor() -> StatementDescriptor {
    StatementDescriptor::new("IDENTITYPRO TRIAL")
        .unwrap()
        .with_phone(Some("800-555-0199".to_owned()))
        .unwrap()
        .with_url(Some("identitypro.com".to_owned()))
        .unwrap()
}

#[test]
fn core_descriptor_maps_onto_the_classic_descriptor_fields() {
    let mapped = map_sale_request(
        GatewaySaleRequest::new(charge(), order(), recurring_intent(), None)
            .with_descriptor(Some(descriptor())),
    )
    .expect("USD renewal maps");
    assert_eq!(
        mapped.descriptor,
        Some(syrup_rail_nmi_client::StatementDescriptor {
            name: "IDENTITYPRO TRIAL".to_owned(),
            phone: Some("800-555-0199".to_owned()),
            url: Some("identitypro.com".to_owned()),
        })
    );
    let without = map_sale_request(GatewaySaleRequest::new(
        charge(),
        order(),
        recurring_intent(),
        None,
    ))
    .expect("USD renewal maps");
    assert_eq!(without.descriptor, None);
}

#[tokio::test]
async fn descriptor_reaches_the_classic_wire_for_stored_credential_sales() {
    // Merchant-initiated renewal, flagged as the first charge after a trial.
    let (gateway, captured, server) = capturing_gateway(CLASSIC_APPROVAL).await;
    let GatewaySaleIntent::RecurringStoredCredential {
        payment_method_reference,
        initial_transaction_id,
        ..
    } = recurring_intent()
    else {
        unreachable!()
    };
    let outcome = gateway
        .sale(
            GatewaySaleRequest::new(
                charge(),
                order(),
                GatewaySaleIntent::RecurringStoredCredential {
                    payment_method_reference,
                    initial_transaction_id,
                    first_charge_after_paid_trial: true,
                },
                None,
            )
            .with_descriptor(Some(descriptor())),
        )
        .await
        .expect("renewal should be approved");
    assert_eq!(outcome.status(), GatewayPaymentStatus::Approved);
    let fields = form_fields(&captured.await.unwrap());
    assert_eq!(fields["descriptor"], "IDENTITYPRO TRIAL");
    assert_eq!(fields["descriptor_phone"], "800-555-0199");
    assert_eq!(fields["descriptor_url"], "identitypro.com");
    assert_eq!(fields["stored_credential_indicator"], "used");
    assert_eq!(fields["initiated_by"], "merchant");
    assert_eq!(fields["initial_transaction_id"], "txn_initial");
    // The flag itself has no NMI field.
    assert!(
        !fields.keys().any(|key| key.contains("trial")),
        "{:?}",
        fields.keys().collect::<Vec<_>>()
    );
    finish(server).await;

    // Initial enrollment sale.
    let (gateway, captured, server) = capturing_gateway(CLASSIC_APPROVAL).await;
    let outcome = gateway
        .sale(
            GatewaySaleRequest::new(
                charge(),
                order(),
                GatewaySaleIntent::InitialStoredCredential {
                    payment_token: PaymentToken::new("tok_initial").unwrap(),
                },
                None,
            )
            .with_descriptor(Some(StatementDescriptor::new("IDENTITYPRO.COM").unwrap())),
        )
        .await
        .expect("initial sale should be approved");
    assert_eq!(outcome.status(), GatewayPaymentStatus::Approved);
    let fields = form_fields(&captured.await.unwrap());
    assert_eq!(fields["descriptor"], "IDENTITYPRO.COM");
    assert!(!fields.contains_key("descriptor_phone"));
    assert!(!fields.contains_key("descriptor_url"));
    finish(server).await;

    // No descriptor: the wire is exactly as before.
    let (gateway, captured, server) = capturing_gateway(CLASSIC_APPROVAL).await;
    let outcome = gateway
        .sale(GatewaySaleRequest::new(
            charge(),
            order(),
            recurring_intent(),
            None,
        ))
        .await
        .expect("renewal should be approved");
    assert_eq!(outcome.status(), GatewayPaymentStatus::Approved);
    let fields = form_fields(&captured.await.unwrap());
    for key in ["descriptor", "descriptor_phone", "descriptor_url"] {
        assert!(!fields.contains_key(key), "{key}");
    }
    finish(server).await;
}

#[tokio::test]
async fn one_time_v5_sale_ignores_the_descriptor() {
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
            .with_descriptor(Some(descriptor())),
        )
        .await
        .expect("host charge should be approved");
    assert_eq!(outcome.status(), GatewayPaymentStatus::Approved);
    let body = json_body(&captured.await.unwrap());
    assert!(!body.to_string().contains("descriptor"), "{body}");
    finish(server).await;
}
