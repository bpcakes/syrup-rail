use super::*;

#[test]
fn xml_card_metadata_equivalent_duplicates_preserve_brand() {
    for fields in [
        "<cc_type>Visa</cc_type><cc_type>VISA</cc_type>",
        "<cc_type>VISA</cc_type><cc_type>Visa</cc_type>",
    ] {
        let outcome = query_outcome_from_xml(&format!(
            "<nm_response><transaction><transaction_id>txn_metadata</transaction_id><condition>complete</condition>{fields}</transaction></nm_response>"
        )).unwrap().unwrap();
        assert_eq!(outcome.status, PaymentStatus::Approved);
        assert!(outcome.diagnostics.is_empty());
        assert_eq!(
            outcome
                .descriptor
                .card_brand
                .as_ref()
                .map(SensitiveText::expose),
            Some("VISA")
        );
    }
}

#[test]
fn xml_card_metadata_survives_recognized_lifecycle_conditions_without_payment_authority() {
    for condition in ["refunded", "voided", "chargeback", "canceled"] {
        let outcome = query_outcome_from_xml(&format!(
            "<nm_response><transaction><transaction_id>txn_metadata</transaction_id><condition>{condition}</condition><cc_type>Visa</cc_type><cc_number>4xxxxxxxxxxx1111</cc_number><cc_exp>1029</cc_exp></transaction></nm_response>"
        )).unwrap().unwrap();
        assert_eq!(outcome.status, PaymentStatus::Unknown);
        assert!(outcome.diagnostics.is_empty());
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
            (Some(10), Some(2029))
        );
    }
}

#[test]
fn classic_card_metadata_equivalent_aliases_preserve_brand_independent_of_order() {
    for fields in [
        "cctype=Visa&cc_type=VISA&card_type=visa",
        "card_type=visa&cc_type=VISA&cctype=Visa",
        "cc_type=VISA&cctype=Visa&card_type=visa",
    ] {
        let outcome = classic_payment_outcome_from_form(&format!(
            "response=1&transactionid=txn_metadata&{fields}"
        ))
        .unwrap();
        assert_eq!(outcome.status, PaymentStatus::Approved);
        assert!(outcome.diagnostics.is_empty());
        assert_eq!(
            outcome
                .descriptor
                .card_brand
                .as_ref()
                .map(SensitiveText::expose),
            Some("visa")
        );
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
fn classic_card_metadata_aliases_and_expiry_preserve_payment_decisions() {
    for alias in ["cc_type", "cctype", "card_type"] {
        for (decision, expected) in [
            ("response=1&response_code=100", PaymentStatus::Approved),
            ("response=2&response_code=200", PaymentStatus::Declined),
            ("response=3&response_code=300", PaymentStatus::Failed),
        ] {
            let outcome = classic_payment_outcome_from_form(&format!(
                "{decision}&transactionid=txn_metadata&{alias}=Visa&cc_number=4xxxxxxxxxxx1111&cc_exp=1029"
            )).unwrap();
            assert_eq!(outcome.status, expected);
            assert_eq!(
                outcome
                    .descriptor
                    .card_brand
                    .as_ref()
                    .map(SensitiveText::expose),
                Some("Visa")
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
                (Some(10), Some(2029))
            );
        }
    }
}

#[test]
fn classic_and_xml_card_metadata_handle_duplicate_invalid_and_missing_expiry() {
    for (form, xml, expected) in [
        ("", "", (None, None)),
        ("cc_exp=", "<cc_exp/>", (None, None)),
        ("cc_exp=1329", "<cc_exp>1329</cc_exp>", (None, None)),
        ("cc_exp=garbage", "<cc_exp>garbage</cc_exp>", (None, None)),
        (
            "cc_exp=1029&cc_exp=1129",
            "<cc_exp>1029</cc_exp><cc_exp>1129</cc_exp>",
            (None, None),
        ),
        (
            "cc_exp=1029&cc_exp=garbage",
            "<cc_exp>1029</cc_exp><cc_exp>garbage</cc_exp>",
            (None, None),
        ),
        (
            "cc_exp=1029&cc_exp=1029",
            "<cc_exp>1029</cc_exp><cc_exp>1029</cc_exp>",
            (Some(10), Some(2029)),
        ),
        (
            "cc_exp=102029",
            "<cc_exp>102029</cc_exp>",
            (Some(10), Some(2029)),
        ),
    ] {
        let classic = classic_payment_outcome_from_form(&format!(
            "response=1&transactionid=txn_metadata&{form}"
        ))
        .unwrap();
        let query = query_outcome_from_xml(&format!(
            "<nm_response><transaction><transaction_id>txn_metadata</transaction_id><condition>pendingsettlement</condition>{xml}</transaction></nm_response>"
        )).unwrap().unwrap();
        for outcome in [classic, query] {
            assert_eq!(outcome.status, PaymentStatus::Approved);
            assert_eq!(
                (
                    outcome.descriptor.card_exp_month,
                    outcome.descriptor.card_exp_year
                ),
                expected
            );
        }
    }
}

#[test]
fn card_metadata_conflicts_and_full_pan_do_not_gain_authority() {
    let classic = classic_payment_outcome_from_form(
        "response=1&transactionid=txn_metadata&cc_type=Visa&cctype=Mastercard&cc_number=4111111111111111&cc_exp=1029&cc_exp=1129"
    ).unwrap();
    let query = query_outcome_from_xml(
        "<nm_response><transaction><transaction_id>txn_metadata</transaction_id><condition>complete</condition><cc_type>Visa</cc_type><cc_type>Mastercard</cc_type><cc_number>4111111111111111</cc_number><cc_exp><value>1029</value></cc_exp></transaction></nm_response>"
    ).unwrap().unwrap();
    for outcome in [classic, query] {
        assert_eq!(outcome.status, PaymentStatus::Approved);
        assert_eq!(outcome.descriptor.card_brand, None);
        assert_eq!(outcome.descriptor.card_last4, None);
        assert_eq!(
            (
                outcome.descriptor.card_exp_month,
                outcome.descriptor.card_exp_year
            ),
            (None, None)
        );
        let debug = format!("{outcome:?}");
        assert!(!debug.contains("4111111111111111"));
        assert!(!debug.contains("txn_metadata"));
    }
}
