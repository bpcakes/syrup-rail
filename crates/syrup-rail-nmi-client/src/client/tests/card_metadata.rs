use super::*;

fn metadata_query(text: &str) -> crate::PaymentMethodMetadataParts {
    query_metadata_for_request_from_xml(
        text,
        &TransactionQuery {
            transaction_id: Some("txn_metadata".into()),
            order_id: None,
        },
    )
    .unwrap()
    .unwrap()
    .into_parts()
}

#[test]
fn xml_card_metadata_equivalent_duplicates_preserve_brand() {
    for fields in [
        "<cc_type>Visa</cc_type><cc_type>VISA</cc_type>",
        "<cc_type>VISA</cc_type><cc_type>Visa</cc_type>",
    ] {
        let xml = format!(
            "<nm_response><transaction><transaction_id>txn_metadata</transaction_id><condition>complete</condition>{fields}</transaction></nm_response>"
        );
        let metadata = metadata_query(&xml);
        assert_eq!(metadata.status, PaymentStatus::Approved);
        assert!(metadata.diagnostics.is_empty());
        assert_eq!(
            metadata
                .descriptor
                .card_brand
                .as_ref()
                .map(SensitiveText::expose),
            Some("VISA")
        );
        // v0.5.2 treated differently cased repeated evidence as conflicting.
        let financial = query_outcome_from_xml(&xml).unwrap().unwrap();
        assert!(financial.descriptor.card_brand.is_none());
    }
}

#[test]
fn xml_card_metadata_survives_recognized_lifecycle_conditions_without_payment_authority() {
    for condition in ["refunded", "voided", "chargeback", "canceled"] {
        let metadata = metadata_query(&format!(
            "<nm_response><transaction><transaction_id>txn_metadata</transaction_id><condition>{condition}</condition><cc_type>Visa</cc_type><cc_number>4xxxxxxxxxxx1111</cc_number><cc_exp>1029</cc_exp></transaction></nm_response>"
        ));
        assert_eq!(metadata.status, PaymentStatus::Unknown);
        assert!(metadata.diagnostics.is_empty());
        assert_eq!(
            metadata
                .descriptor
                .card_last4
                .as_ref()
                .map(SensitiveText::expose),
            Some("1111")
        );
        assert_eq!(
            (
                metadata.descriptor.card_exp_month,
                metadata.descriptor.card_exp_year
            ),
            (Some(10), Some(2029))
        );
    }
}

#[test]
fn classic_financial_descriptors_keep_v052_alias_and_expiry_semantics() {
    // Characterization of tag v0.5.2 (9f30107), including aliases whose new
    // interpretation would change already retained immutable evidence.
    for (fields, expected_brand) in [
        ("cc_type=Visa", None),
        ("cctype=Visa", Some("Visa")),
        ("card_type=Visa", Some("Visa")),
        ("cctype=Visa&cc_type=Mastercard", Some("Visa")),
        ("cctype=Visa&card_type=visa", None),
        ("card_type=visa&cctype=Visa", None),
    ] {
        for (decision, expected_status) in [
            ("response=1&response_code=100", PaymentStatus::Approved),
            ("response=2&response_code=200", PaymentStatus::Declined),
            ("response=3&response_code=300", PaymentStatus::Failed),
        ] {
            let outcome = classic_payment_outcome_from_form(&format!(
                "{decision}&transactionid=txn_metadata&{fields}&cc_number=4xxxxxxxxxxx1111&cc_exp=1029"
            )).unwrap();
            assert_eq!(outcome.status, expected_status);
            assert_eq!(
                outcome
                    .descriptor
                    .card_brand
                    .as_ref()
                    .map(SensitiveText::expose),
                expected_brand
            );
            assert_eq!(
                outcome
                    .descriptor
                    .card_last4
                    .as_ref()
                    .map(SensitiveText::expose),
                Some("1111")
            );
            assert_eq!(
                (
                    outcome.descriptor.card_exp_month,
                    outcome.descriptor.card_exp_year
                ),
                (None, None)
            );
        }
    }
}

#[test]
fn classic_card_metadata_aliases_remain_processing_evidence_on_http_errors() {
    for body in [
        "cctype=Visa",
        "card_type=Visa",
        "cc_type=Visa",
        "cc_exp=1029",
    ] {
        assert!(
            crate::client::response::form::classic_form_has_payment_processing_evidence(body, 401)
        );
    }
}

#[test]
fn metadata_expiry_validation_does_not_change_financial_xml() {
    for (fields, expected) in [
        ("", (None, None)),
        ("<cc_exp/>", (None, None)),
        ("<cc_exp>1329</cc_exp>", (None, None)),
        ("<cc_exp>garbage</cc_exp>", (None, None)),
        ("<cc_exp>1029</cc_exp><cc_exp>1129</cc_exp>", (None, None)),
        (
            "<cc_exp>1029</cc_exp><cc_exp>garbage</cc_exp>",
            (None, None),
        ),
        (
            "<cc_exp>1029</cc_exp><cc_exp>1029</cc_exp>",
            (Some(10), Some(2029)),
        ),
        ("<cc_exp>102029</cc_exp>", (Some(10), Some(2029))),
    ] {
        let xml = format!(
            "<nm_response><transaction><transaction_id>txn_metadata</transaction_id><condition>pendingsettlement</condition>{fields}</transaction></nm_response>"
        );
        let financial = query_outcome_from_xml(&xml).unwrap().unwrap();
        assert_eq!(financial.status, PaymentStatus::Approved);
        assert_eq!(
            (
                financial.descriptor.card_exp_month,
                financial.descriptor.card_exp_year
            ),
            (None, None)
        );
        let metadata = metadata_query(&xml);
        assert_eq!(metadata.status, PaymentStatus::Approved);
        assert_eq!(
            (
                metadata.descriptor.card_exp_month,
                metadata.descriptor.card_exp_year
            ),
            expected
        );
    }
}

#[test]
fn card_metadata_conflicts_and_full_pan_do_not_gain_authority() {
    let xml = "<nm_response><transaction><transaction_id>txn_metadata</transaction_id><condition>complete</condition><cc_type>Visa</cc_type><cc_type>Mastercard</cc_type><cc_number>4111111111111111</cc_number><cc_exp><value>1029</value></cc_exp></transaction></nm_response>";
    let metadata = query_metadata_for_request_from_xml(
        xml,
        &TransactionQuery {
            transaction_id: Some("txn_metadata".into()),
            order_id: None,
        },
    )
    .unwrap()
    .unwrap();
    let debug = format!("{metadata:?}");
    assert!(!debug.contains("4111111111111111"));
    assert!(!debug.contains("txn_metadata"));
    let parts = metadata.into_parts();
    assert_eq!(parts.status, PaymentStatus::Approved);
    assert!(parts.descriptor.card_brand.is_none());
    assert!(parts.descriptor.card_last4.is_none());
    assert_eq!(
        (
            parts.descriptor.card_exp_month,
            parts.descriptor.card_exp_year
        ),
        (None, None)
    );
}

#[test]
fn metadata_and_financial_queries_enforce_the_same_exact_selectors_and_envelope() {
    let request = TransactionQuery {
        transaction_id: Some("txn_metadata".into()),
        order_id: Some("order_metadata".into()),
    };
    for xml in [
        "<nm_response><transaction><transaction_id>other</transaction_id><order_id>order_metadata</order_id></transaction></nm_response>",
        "<nm_response><transaction><transaction_id>txn_metadata</transaction_id><order_id>other</order_id></transaction></nm_response>",
        "<nm_response><transaction><transaction_id>txn_metadata</transaction_id></transaction></nm_response>",
        "<nm_response><transaction><transaction_id>txn_metadata</transaction_id><transaction_id>other</transaction_id><order_id>order_metadata</order_id></transaction></nm_response>",
        "<nm_response><transaction/><transaction/></nm_response>",
        "<nm_response><wrapper><transaction/></wrapper></nm_response>",
        "<nm_response><error_response>Invalid security key</error_response></nm_response>",
        "<nm_response>",
    ] {
        assert!(
            query_outcome_for_request_from_xml(xml, &request).is_err(),
            "{xml}"
        );
        assert!(
            query_metadata_for_request_from_xml(xml, &request).is_err(),
            "{xml}"
        );
    }
    assert!(
        query_metadata_for_request_from_xml("<nm_response/>", &request)
            .unwrap()
            .is_none()
    );
    let matching = "<nm_response><transaction><transaction_id>txn_metadata</transaction_id><order_id>order_metadata</order_id><condition>complete</condition><cc_exp>1029</cc_exp></transaction></nm_response>";
    let metadata = query_metadata_for_request_from_xml(matching, &request)
        .unwrap()
        .unwrap()
        .into_parts();
    assert_eq!(metadata.descriptor.card_exp_month, Some(10));
    assert_eq!(metadata.descriptor.card_exp_year, Some(2029));
}

#[test]
fn financial_json_still_retains_preexisting_expiry() {
    let outcome = payment_outcome_from_json(&json!({
        "response": "1", "transactionid": "txn_metadata",
        "payment_details": {"card": {"card_type": "Visa", "exp": "1029", "last4": "1111"}}
    }))
    .unwrap();
    assert_eq!(
        (
            outcome.descriptor.card_exp_month,
            outcome.descriptor.card_exp_year
        ),
        (Some(10), Some(2029))
    );
}
