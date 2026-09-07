use super::*;

#[tokio::test]
async fn classic_and_exact_xml_card_metadata_reaches_canonical_display() {
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
    for outcome in [sale, query] {
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
}
