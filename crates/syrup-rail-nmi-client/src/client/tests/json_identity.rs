use super::*;

#[test]
fn documented_v5_sale_accepts_blank_optional_customer_vault_id() {
    let outcome = payment_outcome_from_json_text(
        r#"{
          "object": "transaction",
          "id": "55667788",
          "type": "cc",
          "amount": "10.00",
          "currency": "USD",
          "auth_code": "183927",
          "avs_response": "Y",
          "cvv_response": "M",
          "customer_vault_id": "",
          "status": "pendingsettlement",
          "response": "1",
          "response_text": "",
          "response_code": "100",
          "processor_id": "example-proc",
          "created_date": "2025-03-26T15:04:12.000Z",
          "updated_date": "2025-03-26T15:04:12.000Z",
          "payment_details": {
            "card_number": "4***********1111",
            "card_exp": "1226",
            "card_type": "Visa",
            "card_bin": "411111"
          },
          "billing_address": {
            "first_name": "Jane",
            "last_name": "Doe",
            "address1": "100 Main St",
            "city": "Chicago",
            "state": "IL",
            "zip": "60601",
            "country": "US"
          },
          "actions": [
            {
              "id": "44332211",
              "type": "sale",
              "amount": "10.00",
              "success": true,
              "response": "1",
              "response_text": "",
              "response_code": "100",
              "auth_code": "183927"
            }
          ]
        }"#,
    )
    .expect("NMI's documented successful v5 sale response should parse");

    assert_eq!(outcome.status, PaymentStatus::Approved);
    assert_eq!(
        outcome.transaction_id.as_ref().map(SensitiveText::expose),
        Some("55667788")
    );
    assert_eq!(outcome.customer_vault_id, None);
    assert_eq!(
        outcome.response.as_ref().map(SensitiveText::expose),
        Some("1")
    );
    assert_eq!(
        outcome.response_code.as_ref().map(SensitiveText::expose),
        Some("100")
    );
    assert!(outcome.diagnostics.is_empty());
}

#[test]
fn optional_json_vault_identifier_accepts_absent_values_and_valid_aliases() {
    for vault_value in [json!(""), json!(" \t "), Value::Null] {
        let outcome = payment_outcome_from_json(&json!({
            "id": "txn_optional_vault",
            "customer_vault_id": vault_value,
            "status": "approved"
        }))
        .expect("an absent optional vault identifier should parse");

        assert_eq!(outcome.status, PaymentStatus::Approved);
        assert_eq!(
            outcome.transaction_id.as_ref().map(SensitiveText::expose),
            Some("txn_optional_vault")
        );
        assert_eq!(outcome.customer_vault_id, None);
        assert!(outcome.diagnostics.is_empty());
    }

    for absent_alias in [r#""""#, "null"] {
        let outcome = payment_outcome_from_json_text(&format!(
            r#"{{
                "id":"txn_optional_vault_alias",
                "customer_vault_id":{absent_alias},
                "customer_vault":{{"id":"vault_present"}},
                "status":"approved"
            }}"#
        ))
        .expect("an absent optional alias must not hide a valid vault identifier");

        assert_eq!(outcome.status, PaymentStatus::Approved);
        assert_eq!(
            outcome
                .customer_vault_id
                .as_ref()
                .map(SensitiveText::expose),
            Some("vault_present")
        );
        assert!(outcome.diagnostics.is_empty());
    }
}

#[test]
fn required_json_transaction_identifier_rejects_blank_or_null_aliases() {
    for invalid_alias in [r#""""#, "null"] {
        let outcome = payment_outcome_from_json_text(&format!(
            r#"{{
                "transaction_id":{invalid_alias},
                "transaction":{{"id":"txn_alternate"}},
                "customer_vault_id":"vault_valid",
                "status":"approved"
            }}"#
        ))
        .expect("an invalid required transaction identifier should parse conservatively");

        assert_eq!(outcome.status, PaymentStatus::Unknown);
        assert_eq!(outcome.transaction_id, None);
        assert_eq!(outcome.customer_vault_id, None);
        assert_eq!(
            outcome.diagnostics,
            vec![PaymentOutcomeDiagnostic::InvalidOrConflictingTransactionIdentifier]
        );
    }
}

#[test]
fn payment_outcome_keeps_nested_card_kind_out_of_brand_fallback() {
    let outcome = payment_outcome_from_json(&json!({
        "id": "txn_v5_nested_card",
        "status": "approved",
        "payment_details": {
            "type": "card",
            "card": {
                "type": "visa",
                "last4": "1111",
                "exp": "1029"
            }
        }
    }))
    .expect("nested card descriptor should parse");

    assert_eq!(
        outcome
            .descriptor
            .payment_type
            .as_ref()
            .map(SensitiveText::expose),
        Some("card")
    );
    assert_eq!(
        outcome
            .descriptor
            .card_brand
            .as_ref()
            .map(SensitiveText::expose),
        Some("visa")
    );
    assert_eq!(
        outcome
            .descriptor
            .card_last4
            .as_ref()
            .map(SensitiveText::expose),
        Some("1111")
    );
    assert_eq!(outcome.descriptor.card_exp_month, Some(10));
    assert_eq!(outcome.descriptor.card_exp_year, Some(2029));

    let method_kind_only = payment_outcome_from_json(&json!({
        "id": "txn_v5_method_kind_only",
        "status": "approved",
        "payment_details": {
            "type": "card",
            "card_number": "411111******1111"
        }
    }))
    .expect("payment method kind should parse");
    assert_eq!(method_kind_only.descriptor.card_brand, None);
}

#[test]
fn lossless_json_accepts_every_consistent_identity_alias_and_duplicate() {
    let outcome = payment_outcome_from_json_text(
        r#"{
            "response":"1",
            "transaction_id":"txn_same",
            "transaction_id":"txn_same",
            "transaction":{"id":"txn_same","id":"txn_same"},
            "payment":{"id":"txn_same"},
            "id":"txn_same",
            "customer_vault_id":"vault_same",
            "customer_vault_id":"vault_same",
            "customer_vault":{
                "customer_vault_id":"vault_same",
                "customer_id":"vault_same",
                "id":"vault_same",
                "id":"vault_same"
            },
            "customer":{"customer_vault_id":"vault_same"}
        }"#,
    )
    .expect("consistent aliases should parse");

    assert_eq!(outcome.status, PaymentStatus::Approved);
    assert_eq!(
        outcome.transaction_id.as_ref().map(SensitiveText::expose),
        Some("txn_same")
    );
    assert_eq!(
        outcome
            .customer_vault_id
            .as_ref()
            .map(SensitiveText::expose),
        Some("vault_same")
    );
}

#[test]
fn lossless_json_rejects_duplicate_alias_conflicts_and_clears_both_identities() {
    for (text, diagnostic) in [
        (
            r#"{
                "response":"1",
                "transaction_id":"txn_first",
                "transaction_id":"txn_second",
                "customer_vault_id":"vault_valid"
            }"#,
            PaymentOutcomeDiagnostic::InvalidOrConflictingTransactionIdentifier,
        ),
        (
            r#"{
                "response":"1",
                "transaction_id":"txn_valid",
                "customer_vault_id":"vault_first",
                "customer_vault":{"id":"vault_second"}
            }"#,
            PaymentOutcomeDiagnostic::InvalidOrConflictingCustomerVaultIdentifier,
        ),
        (
            r#"{
                "response":"1",
                "transaction_id":{},
                "transaction":{"id":"txn_valid"},
                "customer_vault_id":"vault_valid"
            }"#,
            PaymentOutcomeDiagnostic::InvalidOrConflictingTransactionIdentifier,
        ),
    ] {
        let outcome = payment_outcome_from_json_text(text)
            .expect("identity conflict should produce an unknown outcome");
        assert_eq!(outcome.status, PaymentStatus::Unknown);
        assert_eq!(outcome.transaction_id, None);
        assert_eq!(outcome.customer_vault_id, None);
        assert_eq!(outcome.diagnostics, vec![diagnostic]);
    }
}

#[test]
fn lossless_json_preserves_bounded_provider_specific_identifier_syntax() {
    let outcome = payment_outcome_from_json_text(
        r#"{
            "response":"2",
            "transaction_id":"txn=provider/value",
            "customer_vault_id":"vault:provider"
        }"#,
    )
    .expect("provider-specific identifiers should parse");

    assert_eq!(outcome.status, PaymentStatus::Declined);
    assert_eq!(
        outcome.transaction_id.as_ref().map(SensitiveText::expose),
        Some("txn=provider/value")
    );
    assert_eq!(
        outcome
            .customer_vault_id
            .as_ref()
            .map(SensitiveText::expose),
        Some("vault:provider")
    );
}

#[test]
fn lossless_json_treats_numeric_and_string_duplicates_as_consistent() {
    let outcome = payment_outcome_from_json_text(
        r#"{
            "response":"1",
            "transaction_id":1234567890123,
            "transaction_id":"1234567890123",
            "customer_vault_id":9876543210123,
            "customer_vault_id":"9876543210123"
        }"#,
    )
    .expect("equivalent numeric and string identifiers should parse");

    assert_eq!(outcome.status, PaymentStatus::Approved);
    assert_eq!(
        outcome.transaction_id.as_ref().map(SensitiveText::expose),
        Some("1234567890123")
    );
    assert_eq!(
        outcome
            .customer_vault_id
            .as_ref()
            .map(SensitiveText::expose),
        Some("9876543210123")
    );
}
#[test]
fn payment_outcome_reads_nested_customer_vault_id() {
    let outcome = payment_outcome_from_json(&json!({
        "transaction_id": "txn_nested_vault",
        "status": "approved",
        "customer_vault": {
            "id": "vault_nested"
        }
    }))
    .expect("payment outcome should parse");

    assert_eq!(outcome.status, PaymentStatus::Approved);
    assert_eq!(
        outcome
            .customer_vault_id
            .as_ref()
            .map(SensitiveText::expose),
        Some("vault_nested")
    );
}

#[test]
fn payment_outcome_does_not_use_merchant_customer_id_as_vault_id() {
    let outcome = payment_outcome_from_json(&json!({
        "transaction_id": "txn_customer_reference",
        "status": "approved",
        "customer_id": "merchant_customer_reference",
        "customer": {
            "id": "nested_merchant_customer_reference"
        }
    }))
    .expect("payment outcome should parse");

    assert_eq!(outcome.status, PaymentStatus::Approved);
    assert_eq!(outcome.customer_vault_id, None);
}

#[test]
fn payment_outcome_treats_conflicting_transaction_ids_as_unknown() {
    let outcome = payment_outcome_from_json(&json!({
        "transaction_id": "txn_authoritative",
        "transaction": { "id": "txn_nested" },
        "payment": { "id": "txn_payment" },
        "id": "txn_generic",
        "status": "approved"
    }))
    .expect("payment outcome should parse");

    assert_eq!(outcome.status, PaymentStatus::Unknown);
    assert_eq!(outcome.transaction_id, None);
    assert_eq!(outcome.customer_vault_id, None);
}

#[test]
fn payment_outcome_wraps_raw_processor_fields_without_formatting_disclosure() {
    let outcome = payment_outcome_from_json(&json!({
        "transaction_id": "card_number=4111111111111111",
        "customer_vault_id": "payment_token=tok_secret",
        "response": "1",
        "response_code": "cvv=123",
        "response_text": "Approved card_number=4111111111111111",
        "condition": "complete payment_token=tok_secret"
    }))
    .expect("payment outcome should parse");

    assert_eq!(outcome.status, PaymentStatus::Unknown);
    assert_eq!(
        outcome.transaction_id.as_ref().map(SensitiveText::expose),
        Some("card_number=4111111111111111")
    );
    assert_eq!(
        outcome
            .customer_vault_id
            .as_ref()
            .map(SensitiveText::expose),
        Some("payment_token=tok_secret")
    );
    assert_eq!(
        outcome.response.as_ref().map(SensitiveText::expose),
        Some("1")
    );
    assert_eq!(
        outcome.response_code.as_ref().map(SensitiveText::expose),
        Some("cvv=123")
    );
    assert_eq!(
        outcome.response_text.as_ref().map(SensitiveText::expose),
        Some("Approved card_number=4111111111111111")
    );
    assert_eq!(
        outcome.condition.as_ref().map(SensitiveText::expose),
        Some("complete payment_token=tok_secret")
    );
    let debug = format!("{outcome:?}");
    assert!(!debug.contains("4111111111111111"));
    assert!(!debug.contains("tok_secret"));
    assert!(!debug.contains("cvv=123"));
}

#[test]
fn payment_outcome_preserves_numeric_gateway_identifiers() {
    let outcome = payment_outcome_from_json(&json!({
        "transaction_id": "1234567890123",
        "customer_vault_id": "9876543210123",
        "response": "1",
        "condition": "complete"
    }))
    .expect("payment outcome should parse");

    assert_eq!(outcome.status, PaymentStatus::Approved);
    assert_eq!(
        outcome.transaction_id.as_ref().map(SensitiveText::expose),
        Some("1234567890123")
    );
    assert_eq!(
        outcome
            .customer_vault_id
            .as_ref()
            .map(SensitiveText::expose),
        Some("9876543210123")
    );
}

#[test]
fn payment_outcome_preserves_pan_shaped_vault_id_for_caller_validation() {
    let outcome = payment_outcome_from_json(&json!({
        "transaction_id": "txn_pan_shaped_vault",
        "customer_vault_id": "4111111111111111",
        "response": "1",
        "condition": "complete"
    }))
    .expect("payment outcome should parse");

    assert_eq!(outcome.status, PaymentStatus::Approved);
    assert_eq!(
        outcome.transaction_id.as_ref().map(SensitiveText::expose),
        Some("txn_pan_shaped_vault")
    );
    assert_eq!(
        outcome
            .customer_vault_id
            .as_ref()
            .map(SensitiveText::expose),
        Some("4111111111111111")
    );
}

#[test]
fn payment_outcome_rejects_oversized_identifiers_before_bounded_copy() {
    let shared_prefix = "a".repeat(MAX_NMI_FIELD_CHARS);
    for transaction_id in [format!("{shared_prefix}x"), format!("{shared_prefix}y")] {
        let outcome = payment_outcome_from_json(&json!({
            "transaction_id": transaction_id,
            "status": "approved"
        }))
        .expect("invalid typed identifier should not break response parsing");
        assert_eq!(outcome.status, PaymentStatus::Unknown);
        assert_eq!(outcome.transaction_id, None);
    }
}

#[test]
fn payment_outcome_preserves_bounded_identifiers_for_caller_validation() {
    for transaction_id in [
        "txn_4111(1111)1111_1111",
        "txn_4111\u{200b}1111\u{fe0f}1111_1111",
    ] {
        let outcome = payment_outcome_from_json(&json!({
            "transaction_id": transaction_id,
            "status": "approved"
        }))
        .expect("bounded provider identifier should parse");
        assert_eq!(outcome.status, PaymentStatus::Approved);
        assert_eq!(
            outcome.transaction_id.as_ref().map(SensitiveText::expose),
            Some(transaction_id)
        );
    }
}

#[test]
fn invalid_authoritative_json_identifier_cannot_hide_behind_valid_alternate() {
    let outcome = payment_outcome_from_json(&json!({
        "transaction_id": "txn=malformed",
        "transaction": { "id": "txn_nested_valid" },
        "status": "approved"
    }))
    .expect("response should parse");

    assert_eq!(outcome.status, PaymentStatus::Unknown);
    assert_eq!(outcome.transaction_id, None);
    assert_eq!(outcome.customer_vault_id, None);
}

#[test]
fn payment_outcome_drops_malformed_json_card_descriptor_fields() {
    let outcome = payment_outcome_from_json(&json!({
        "id": "txn_v5_bad_card",
        "status": "approved",
        "payment_details": {
            "type": "card",
            "card_type": "visa",
            "last4": "abc1",
            "card_exp": "992999"
        }
    }))
    .expect("payment outcome should parse");

    assert_eq!(outcome.status, PaymentStatus::Approved);
    assert_eq!(outcome.descriptor.card_last4, None);
    assert_eq!(outcome.descriptor.card_exp_month, None);
    assert_eq!(outcome.descriptor.card_exp_year, None);
}
