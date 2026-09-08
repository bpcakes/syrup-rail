use super::*;

#[tokio::test]
async fn financial_descriptors_stay_compatible_and_metadata_query_enriches_display() {
    let (gateway, server) = gateway_with_response(
        "response=1&response_code=100&transactionid=txn_metadata&customer_vault_id=vault_metadata&cc_type=MasterCard&cc_number=5xxxxxxxxxxx1111&cc_exp=1029",
    ).await;
    let attempt_id: PaymentAttemptId = "00000000-0000-0000-0000-000000000053".parse().unwrap();
    let sale = gateway
        .sale(GatewaySaleRequest::new(
            ChargeAmount::new(100, CurrencyCode::new("USD").unwrap()).unwrap(),
            GatewayOrderId::from_generated_attempt(
                "ck_order_00000000000000000000000000000053",
                attempt_id,
            )
            .unwrap(),
            GatewaySaleIntent::InitialStoredCredential {
                payment_token: PaymentToken::new("tok_metadata").unwrap(),
            },
            None,
        ))
        .await
        .unwrap();
    server.await.unwrap();
    assert_eq!(sale.status(), GatewayPaymentStatus::Approved);
    assert!(
        sale.descriptor().card_brand().is_none(),
        "v0.5.2 ignores cc_type in Classic financial evidence"
    );
    assert_eq!(sale.descriptor().card_exp_month(), None);
    assert_eq!(sale.descriptor().card_exp_year(), None);
    assert_eq!(sale.descriptor().card_last_four().unwrap().expose(), "1111");
    let (gateway, server) = gateway_with_response(
        "<nm_response><transaction><transaction_id>txn_metadata</transaction_id><condition>pendingsettlement</condition><cc_type>MasterCard</cc_type><cc_number>5xxxxxxxxxxx1111</cc_number><cc_exp>1029</cc_exp></transaction></nm_response>",
    ).await;
    let query = gateway
        .query_transaction(
            GatewayQueryRequest::new(
                Some(GatewayTransactionId::new("txn_metadata").unwrap()),
                None,
            )
            .unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
    server.await.unwrap();
    assert_eq!(query.status(), GatewayPaymentStatus::Approved);
    assert_eq!(
        query.descriptor().canonical_card_brand(),
        Some(PaymentCardBrand::Mastercard)
    );
    assert_eq!(query.descriptor().card_exp_month(), None);
    assert_eq!(query.descriptor().card_exp_year(), None);
    let (gateway, server) = gateway_with_response(
        "<nm_response><transaction><transaction_id>txn_metadata</transaction_id><condition>pendingsettlement</condition><cc_type>MasterCard</cc_type><cc_number>5xxxxxxxxxxx1111</cc_number><cc_exp>1029</cc_exp></transaction></nm_response>",
    ).await;
    let outcome = gateway
        .query_payment_method_metadata(
            GatewayQueryRequest::new(
                Some(GatewayTransactionId::new("txn_metadata").unwrap()),
                None,
            )
            .unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
    server.await.unwrap();
    assert_eq!(outcome.status(), GatewayPaymentStatus::Approved);
    assert_eq!(
        outcome.transaction_id().map(GatewayTransactionId::expose),
        Some("txn_metadata")
    );
    assert_eq!(
        outcome.descriptor().canonical_card_brand(),
        Some(PaymentCardBrand::Mastercard)
    );
    assert_eq!(
        outcome
            .descriptor()
            .card_last_four()
            .map(|last4| last4.expose()),
        Some("1111")
    );
    assert_eq!(outcome.descriptor().card_exp_month(), Some(10));
    assert_eq!(outcome.descriptor().card_exp_year(), Some(2029));
    assert!(!format!("{outcome:?}").contains("txn_metadata"));
}
