use super::*;

#[test]
fn query_outcome_reads_transaction_summary_not_nested_actions() {
    let outcome = query_outcome_from_xml(
        r#"
        <nm_response>
          <transaction>
            <transaction_id>txn_voided</transaction_id>
            <condition>voided</condition>
            <customer_vault_id>vault_123</customer_vault_id>
            <cc_number>411111******1111</cc_number>
            <action>
              <response>1</response>
              <response_text>Approved</response_text>
            </action>
          </transaction>
        </nm_response>
        "#,
    )
    .expect("transaction response should parse")
    .expect("transaction should be present");

    assert_eq!(outcome.status, PaymentStatus::Unknown);
    assert_eq!(
        outcome.transaction_id.as_ref().map(SensitiveText::expose),
        Some("txn_voided")
    );
    assert_eq!(outcome.response, None);
    assert_eq!(
        outcome
            .customer_vault_id
            .as_ref()
            .map(SensitiveText::expose),
        Some("vault_123")
    );
    assert_eq!(
        outcome
            .descriptor
            .card_last4
            .as_ref()
            .map(SensitiveText::expose),
        Some("1111")
    );
}

#[test]
fn query_outcome_clears_conflicting_descriptor_elements() {
    let outcome = query_outcome_from_xml(
        r#"
        <nm_response>
          <transaction>
            <transaction_id>txn_descriptor_conflict</transaction_id>
            <condition>complete</condition>
            <transaction_type>sale</transaction_type>
            <transaction_type>auth</transaction_type>
            <cc_type>Visa</cc_type>
            <cc_type>Mastercard</cc_type>
            <cc_number>411111******1111</cc_number>
            <cc_number>555555******4444</cc_number>
          </transaction>
        </nm_response>
        "#,
    )
    .expect("descriptor conflicts should not affect payment authority")
    .expect("transaction should be present");

    assert_eq!(outcome.status, PaymentStatus::Approved);
    assert_eq!(outcome.descriptor.payment_type, None);
    assert_eq!(outcome.descriptor.card_brand, None);
    assert_eq!(outcome.descriptor.card_last4, None);
}

#[test]
fn query_outcome_wraps_raw_processor_identifiers() {
    let outcome = query_outcome_from_xml(
        r#"
        <nm_response>
          <transaction>
            <transaction_id>card_number=4111111111111111</transaction_id>
            <condition>complete</condition>
            <customer_vault_id>payment_token=tok_secret</customer_vault_id>
          </transaction>
        </nm_response>
        "#,
    )
    .expect("transaction response should parse")
    .expect("transaction should be present");

    assert_eq!(outcome.status, PaymentStatus::Approved);
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
}

#[test]
fn query_outcome_preserves_numeric_gateway_identifiers() {
    let outcome = query_outcome_from_xml(
        r#"
        <nm_response>
          <transaction>
            <transaction_id>1234567890123</transaction_id>
            <condition>complete</condition>
            <customer_vault_id>9876543210123</customer_vault_id>
          </transaction>
        </nm_response>
        "#,
    )
    .expect("transaction response should parse")
    .expect("transaction should be present");

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
fn query_outcome_accepts_consistent_repeated_identity_elements() {
    let outcome = query_outcome_from_xml(
        r#"
        <nm_response>
          <transaction>
            <transaction_id>txn_same</transaction_id>
            <transaction_id> txn_same </transaction_id>
            <customer_vault_id>vault_same</customer_vault_id>
            <customer_vault_id>vault_same</customer_vault_id>
            <condition>complete</condition>
          </transaction>
        </nm_response>
        "#,
    )
    .expect("consistent XML identities should parse")
    .expect("transaction should be present");

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
fn query_outcome_accepts_empty_optional_customer_vault_id() {
    let outcome = query_outcome_from_xml(
        r#"
        <nm_response>
          <transaction>
            <transaction_id>txn_query_without_vault</transaction_id>
            <customer_vault_id/>
            <condition>complete</condition>
          </transaction>
        </nm_response>
        "#,
    )
    .expect("an empty optional Query API vault identifier should parse")
    .expect("the transaction should be present");

    assert_eq!(outcome.status, PaymentStatus::Approved);
    assert_eq!(
        outcome.transaction_id.as_ref().map(SensitiveText::expose),
        Some("txn_query_without_vault")
    );
    assert_eq!(outcome.customer_vault_id, None);
    assert!(outcome.diagnostics.is_empty());
}

#[test]
fn query_outcome_rejects_invalid_or_conflicting_identity_elements() {
    for transaction in [
        "<response>1</response><transaction_id>txn_first</transaction_id><transaction_id>txn_second</transaction_id><customer_vault_id>vault_valid</customer_vault_id>",
        "<response>1</response><transaction_id>txn_valid</transaction_id><customer_vault_id>vault_first</customer_vault_id><customer_vault_id>vault_second</customer_vault_id>",
        "<response>1</response><transaction_id><nested>txn_invalid</nested></transaction_id><customer_vault_id>vault_valid</customer_vault_id>",
        "<response>2</response><transaction_id></transaction_id><customer_vault_id>vault_valid</customer_vault_id>",
    ] {
        let outcome = query_outcome_from_xml(&format!(
            "<nm_response><transaction>{transaction}</transaction></nm_response>"
        ))
        .expect("invalid XML identity should produce an unknown outcome")
        .expect("transaction should be present");
        assert_eq!(outcome.status, PaymentStatus::Unknown);
        assert_eq!(outcome.transaction_id, None);
        assert_eq!(outcome.customer_vault_id, None);
    }
}

#[test]
fn query_outcome_preserves_pan_shaped_vault_id_for_caller_validation() {
    let outcome = query_outcome_from_xml(
        r#"
        <nm_response>
          <transaction>
            <transaction_id>txn_xml_pan_shaped_vault</transaction_id>
            <condition>complete</condition>
            <customer_vault_id>4111111111111111</customer_vault_id>
          </transaction>
        </nm_response>
        "#,
    )
    .expect("transaction response should parse")
    .expect("transaction should be present");

    assert_eq!(outcome.status, PaymentStatus::Approved);
    assert_eq!(
        outcome.transaction_id.as_ref().map(SensitiveText::expose),
        Some("txn_xml_pan_shaped_vault")
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
fn query_outcome_preserves_bounded_unicode_and_rejects_oversized_identifiers() {
    for (transaction_id, accepted) in [
        ("txn_4111\u{200b}1111\u{fe0f}1111_1111".to_owned(), true),
        (format!("{}x", "a".repeat(MAX_NMI_FIELD_CHARS)), false),
    ] {
        let outcome = query_outcome_from_xml(&format!(
            "<nm_response><transaction><transaction_id>{transaction_id}</transaction_id><condition>complete</condition></transaction></nm_response>"
        ))
        .expect("query response should parse")
        .expect("transaction should exist");
        if accepted {
            assert_eq!(outcome.status, PaymentStatus::Approved);
            assert_eq!(
                outcome.transaction_id.as_ref().map(SensitiveText::expose),
                Some(transaction_id.as_str())
            );
        } else {
            assert_eq!(outcome.status, PaymentStatus::Unknown);
            assert_eq!(outcome.transaction_id, None);
        }
    }
}

#[test]
fn query_outcome_maps_settlement_and_canceled_conditions() {
    let pending_settlement = query_outcome_from_xml(
        r#"
        <nm_response>
          <transaction>
            <transaction_id>txn_pending_settlement</transaction_id>
            <condition>pendingsettlement</condition>
          </transaction>
        </nm_response>
        "#,
    )
    .expect("pending settlement response should parse")
    .expect("pending settlement transaction should be present");
    let canceled = query_outcome_from_xml(
        r#"
        <nm_response>
          <transaction>
            <transaction_id>txn_canceled</transaction_id>
            <condition>canceled</condition>
          </transaction>
        </nm_response>
        "#,
    )
    .expect("canceled response should parse")
    .expect("canceled transaction should be present");

    assert_eq!(pending_settlement.status, PaymentStatus::Approved);
    assert_eq!(canceled.status, PaymentStatus::Unknown);
}

#[test]
fn query_outcome_treats_conflicting_status_and_condition_as_unknown() {
    let outcome = query_outcome_from_xml(
        r#"
        <nm_response>
          <transaction>
            <transaction_id>txn_conflict</transaction_id>
            <status>approved</status>
            <condition>declined</condition>
          </transaction>
        </nm_response>
        "#,
    )
    .expect("conflicting transaction response should parse")
    .expect("conflicting transaction should be present");

    assert_eq!(outcome.status, PaymentStatus::Unknown);
    assert_eq!(
        outcome.diagnostics,
        vec![PaymentOutcomeDiagnostic::ConflictingDecisionEvidence]
    );
}

#[test]
fn query_duplicate_response_code_remains_reconcilable_and_composes_diagnostics() {
    let outcome = query_outcome_from_xml(
        r#"
        <nm_response>
          <transaction>
            <transaction_id>txn_query_duplicate</transaction_id>
            <response>3</response>
            <response_code>0430</response_code>
            <status><unexpected>shape</unexpected></status>
          </transaction>
        </nm_response>
        "#,
    )
    .expect("duplicate exact-query response should parse")
    .expect("duplicate transaction should be present");

    assert_eq!(outcome.status, PaymentStatus::Unknown);
    assert_eq!(
        outcome.transaction_id.as_ref().map(SensitiveText::expose),
        Some("txn_query_duplicate")
    );
    assert_eq!(
        outcome.diagnostics,
        vec![
            PaymentOutcomeDiagnostic::DuplicateTransactionAtProcessor,
            PaymentOutcomeDiagnostic::InvalidOrConflictingDecisionField,
        ]
    );
}

#[test]
fn query_outcome_decision_duplicates_are_order_independent() {
    for fields in [
        "<response>1</response><response>2</response>",
        "<response>2</response><response>1</response>",
        "<status>approved</status><status>declined</status>",
        "<status>declined</status><status>approved</status>",
        "<condition>complete</condition><condition>voided</condition>",
        "<condition>voided</condition><condition>complete</condition>",
        "<response_code>100</response_code><response_code>200</response_code>",
        "<response_code>200</response_code><response_code>100</response_code>",
        "<response>1</response><response><nested>1</nested></response>",
        "<response><nested>1</nested></response><response>1</response>",
    ] {
        let outcome = query_outcome_from_xml(&format!(
            "<nm_response><transaction><transaction_id>txn_duplicate</transaction_id>{fields}</transaction></nm_response>"
        ))
        .expect("conflicting exact-query fields should parse conservatively")
        .expect("transaction should be present");
        assert_eq!(outcome.status, PaymentStatus::Unknown, "{fields}");
        assert_eq!(
            outcome.diagnostics,
            vec![PaymentOutcomeDiagnostic::InvalidOrConflictingDecisionField],
            "{fields}"
        );
    }
}

#[test]
fn query_outcome_semantic_duplicates_retain_only_unambiguous_raw_values() {
    let equivalent = query_outcome_from_xml(
        r#"
        <nm_response>
          <transaction>
            <transaction_id>txn_equivalent</transaction_id>
            <condition>pending settlement</condition>
            <condition>pending_settlement</condition>
          </transaction>
        </nm_response>
        "#,
    )
    .expect("semantically equivalent exact-query fields should parse")
    .expect("transaction should be present");
    assert_eq!(equivalent.status, PaymentStatus::Approved);
    assert_eq!(
        equivalent.condition.as_ref().map(SensitiveText::expose),
        Some("pending settlement")
    );

    let lifecycle_ambiguity = query_outcome_from_xml(
        r#"
        <nm_response>
          <transaction>
            <transaction_id>txn_lifecycle_ambiguity</transaction_id>
            <condition>complete</condition>
            <condition>pending_settlement</condition>
          </transaction>
        </nm_response>
        "#,
    )
    .expect("same-status lifecycle ambiguity should parse")
    .expect("transaction should be present");
    assert_eq!(lifecycle_ambiguity.status, PaymentStatus::Approved);
    assert_eq!(lifecycle_ambiguity.condition, None);
}

#[test]
fn query_outcome_auxiliary_text_conflicts_are_cleared() {
    for fields in [
        "<response_text>Approved</response_text><response_text>Declined</response_text>",
        "<response_text>Declined</response_text><response_text>Approved</response_text>",
    ] {
        let outcome = query_outcome_from_xml(&format!(
            "<nm_response><transaction><transaction_id>txn_text</transaction_id><response>1</response><response_code>100</response_code><condition>complete</condition>{fields}</transaction></nm_response>"
        ))
        .expect("auxiliary exact-query text conflict should parse")
        .expect("transaction should be present");
        assert_eq!(outcome.status, PaymentStatus::Approved);
        assert_eq!(outcome.response_text, None);
    }
}

#[test]
fn exact_query_requires_one_envelope_and_at_most_one_direct_transaction() {
    for xml in [
        "<nm_response><transaction><transaction_id>txn_first</transaction_id><condition>complete</condition></transaction><transaction><transaction_id>txn_second</transaction_id><condition>declined</condition></transaction></nm_response>",
        "<nm_response><transaction><transaction_id>txn_second</transaction_id><condition>declined</condition></transaction><transaction><transaction_id>txn_first</transaction_id><condition>complete</condition></transaction></nm_response>",
        "<root><nm_response><transaction><condition>complete</condition></transaction></nm_response><nm_response><transaction><condition>declined</condition></transaction></nm_response></root>",
        "<root><transaction><condition>declined</condition></transaction><nm_response><transaction><condition>complete</condition></transaction></nm_response></root>",
        "<nm_response><wrapper><transaction><condition>complete</condition></transaction></wrapper></nm_response>",
        "<nm_response><transaction><condition>complete</condition><action><transaction><condition>declined</condition></transaction></action></transaction></nm_response>",
    ] {
        assert!(matches!(
            query_outcome_from_xml(xml),
            Err(WireError::MalformedResponse(_))
        ));
    }

    assert!(
        query_outcome_from_xml("<nm_response></nm_response>")
            .expect("empty exact-query envelope should parse")
            .is_none()
    );
}

#[test]
fn query_error_envelopes_fail_closed_across_all_query_parsers() {
    const PROVIDER_SENTINEL: &str = "provider-error-ref-sentinel";
    let xml = format!(
        r#"
        <nm_response>
          <error_response>Specified API key not found REFID:{PROVIDER_SENTINEL}</error_response>
          <test_mode_enabled>false</test_mode_enabled>
          <transaction>
            <transaction_id>txn_error_envelope</transaction_id>
            <condition>complete</condition>
          </transaction>
        </nm_response>
        "#
    );

    for result in [
        query_outcome_from_xml(&xml).map(|_| ()),
        query_transaction_reports_from_xml(&xml).map(|_| ()),
        query_account_mode_from_xml(&xml).map(|_| ()),
    ] {
        let error = result.expect_err("Query API error envelopes must fail closed");
        let error = error.into_query();
        assert!(matches!(&error, QueryError::Configuration(_)));
        assert!(!error.detail().expose().contains(PROVIDER_SENTINEL));
        assert!(!format!("{error:?} {error}").contains(PROVIDER_SENTINEL));
    }
}

#[test]
fn noncredential_query_error_envelope_is_a_redacted_invalid_request() {
    const PROVIDER_SENTINEL: &str = "provider-query-error-sentinel";
    let error = query_outcome_from_xml(&format!(
        "<nm_response><error_response>Invalid query filter {PROVIDER_SENTINEL}</error_response></nm_response>"
    ))
    .expect_err("provider query rejection must fail closed")
    .into_query();

    assert!(matches!(&error, QueryError::InvalidRequest(_)));
    assert!(!error.detail().expose().contains(PROVIDER_SENTINEL));
    assert!(!format!("{error:?} {error}").contains(PROVIDER_SENTINEL));
}
