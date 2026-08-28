use super::*;

fn assert_transaction_local_malformed(report: &TransactionReport) {
    assert_eq!(
        report.diagnostics,
        vec![TransactionReportDiagnostic::MalformedStructure]
    );
    assert_eq!(report.condition, None);
    assert!(report.actions.is_empty());
}

#[test]
fn query_transaction_reports_reads_multiple_transactions_and_actions() {
    let reports = query_transaction_reports_from_xml(
        r#"
        <nm_response>
          <transaction>
            <transaction_id>txn_settled</transaction_id>
            <condition>complete</condition>
            <order_id>ck_order_1</order_id>
            <action>
              <amount>49.00</amount>
              <action_type>settle</action_type>
              <date>20260530120000</date>
              <success>1</success>
              <response_text>ACCEPTED</response_text>
              <response_code>100</response_code>
            </action>
          </transaction>
          <transaction>
            <transaction_id>txn_refunded</transaction_id>
            <condition>complete</condition>
            <order_id>ck_order_2</order_id>
            <action>
              <amount>49.00</amount>
              <action_type>refund</action_type>
              <date>20260530120500</date>
              <success>1</success>
              <response_text>Approved</response_text>
              <response_code>100</response_code>
            </action>
          </transaction>
        </nm_response>
        "#,
    )
    .expect("valid transaction report response should parse");

    assert_eq!(reports.len(), 2);
    assert_eq!(
        reports[0]
            .transaction_id
            .as_ref()
            .map(SensitiveText::expose),
        Some("txn_settled")
    );
    assert_eq!(
        reports[0].order_id.as_ref().map(SensitiveText::expose),
        Some("ck_order_1")
    );
    assert_eq!(
        reports[0].condition.as_ref().map(SensitiveText::expose),
        Some("complete")
    );
    assert_eq!(reports[0].actions.len(), 1);
    assert_eq!(
        reports[0].actions[0]
            .action_type
            .as_ref()
            .map(SensitiveText::expose),
        Some("settle")
    );
    assert_eq!(
        reports[1].actions[0]
            .action_type
            .as_ref()
            .map(SensitiveText::expose),
        Some("refund")
    );
}

#[test]
fn query_transaction_reports_preserve_documented_reversal_success_evidence() {
    // Mirrors the reversal action shape in NMI's published transaction-event
    // samples. The Query parser must preserve explicit success and must never
    // manufacture it when the provider omits the field.
    // https://docs.nmi.com/reference/transaction-events
    let reports = query_transaction_reports_from_xml(
        r#"
        <nm_response>
          <transaction>
            <transaction_id>8597047042</transaction_id>
            <condition>pendingsettlement</condition>
            <action>
              <amount>-1.50</amount>
              <action_type>refund</action_type>
              <date>20230804155935</date>
              <success>1</success>
              <response_text>SUCCESS</response_text>
              <response_code>100</response_code>
            </action>
          </transaction>
          <transaction>
            <transaction_id>8584341675</transaction_id>
            <condition>canceled</condition>
            <action>
              <amount>10.00</amount>
              <action_type>void</action_type>
              <date>20230801115329</date>
              <response_text>SUCCESS</response_text>
              <response_code>100</response_code>
            </action>
          </transaction>
        </nm_response>
        "#,
    )
    .expect("documented reversal action shapes should parse");

    assert_eq!(
        reports[0].actions[0]
            .success
            .as_ref()
            .map(SensitiveText::expose),
        Some("1")
    );
    assert_eq!(reports[1].actions[0].success, None);
}

#[test]
fn query_transaction_reports_diagnose_invalid_identity_fields_independently() {
    let oversized = format!("{}x", "a".repeat(MAX_NMI_FIELD_CHARS));
    for (transaction, expected_transaction_id, expected_order_id) in [
        (
            "<transaction_id>txn_first</transaction_id><transaction_id>txn_second</transaction_id><order_id>ck_order_valid</order_id>".to_owned(),
            None,
            Some("ck_order_valid"),
        ),
        (
            "<transaction_id>txn_valid</transaction_id><order_id>ck_first</order_id><order_id>ck_second</order_id>".to_owned(),
            Some("txn_valid"),
            None,
        ),
        (
            format!("<transaction_id>{oversized}</transaction_id><order_id>ck_order_valid</order_id>"),
            None,
            Some("ck_order_valid"),
        ),
    ] {
        let reports = query_transaction_reports_from_xml(&format!(
            "<nm_response><transaction>{transaction}</transaction></nm_response>"
        ))
        .expect("transaction-local identity defects must not reject the complete page");
        assert_eq!(reports.len(), 1);
        assert_eq!(
            reports[0]
                .transaction_id
                .as_ref()
                .map(SensitiveText::expose),
            expected_transaction_id
        );
        assert_eq!(
            reports[0].order_id.as_ref().map(SensitiveText::expose),
            expected_order_id
        );
        assert_transaction_local_malformed(&reports[0]);
    }
}

#[test]
fn query_transaction_reports_preserve_bounded_provider_identifier_syntax() {
    let reports = query_transaction_reports_from_xml(
        "<nm_response><transaction><transaction_id>txn_4111(1111)1111_1111</transaction_id><order_id>ck/order</order_id></transaction></nm_response>",
    )
    .expect("bounded provider identifiers should be returned for caller validation");

    assert_eq!(reports.len(), 1);
    assert_eq!(
        reports[0]
            .transaction_id
            .as_ref()
            .map(SensitiveText::expose),
        Some("txn_4111(1111)1111_1111")
    );
    assert_eq!(
        reports[0].order_id.as_ref().map(SensitiveText::expose),
        Some("ck/order")
    );
}

#[test]
fn query_transaction_reports_accept_consistent_duplicates_and_missing_transaction_fallback() {
    let reports = query_transaction_reports_from_xml(
        r#"
        <nm_response>
          <transaction>
            <transaction_id>txn_same</transaction_id>
            <transaction_id> txn_same </transaction_id>
            <order_id>ck_same</order_id>
            <order_id>ck_same</order_id>
          </transaction>
          <transaction>
            <order_id>ck_order_only</order_id>
          </transaction>
        </nm_response>
        "#,
    )
    .expect("consistent and genuinely missing report identities should parse");

    assert_eq!(reports.len(), 2);
    assert_eq!(
        reports[0]
            .transaction_id
            .as_ref()
            .map(SensitiveText::expose),
        Some("txn_same")
    );
    assert_eq!(
        reports[0].order_id.as_ref().map(SensitiveText::expose),
        Some("ck_same")
    );
    assert_eq!(reports[1].transaction_id, None);
    assert_eq!(
        reports[1].order_id.as_ref().map(SensitiveText::expose),
        Some("ck_order_only")
    );
}

#[test]
fn query_transaction_reports_diagnose_conflicting_authority_fields_in_both_orders() {
    for condition in [
        "<condition>complete</condition><condition>declined</condition>",
        "<condition>declined</condition><condition>complete</condition>",
    ] {
        let reports = query_transaction_reports_from_xml(&format!(
            "<nm_response><transaction><transaction_id>txn_condition</transaction_id>{condition}</transaction></nm_response>"
        ))
        .expect("transaction-local condition conflict must not reject the page");
        assert_eq!(
            reports[0]
                .transaction_id
                .as_ref()
                .map(SensitiveText::expose),
            Some("txn_condition")
        );
        assert_transaction_local_malformed(&reports[0]);
    }

    for (field, first, second) in [
        ("action_type", "settle", "refund"),
        ("date", "20260530120000", "20260530120500"),
        ("amount", "49.00", "50.00"),
        ("success", "1", "0"),
    ] {
        for values in [
            format!("<{field}>{first}</{field}><{field}>{second}</{field}>"),
            format!("<{field}>{second}</{field}><{field}>{first}</{field}>"),
        ] {
            let reports = query_transaction_reports_from_xml(&format!(
                "<nm_response><transaction><transaction_id>txn_action</transaction_id><action>{values}</action></transaction></nm_response>"
            ))
            .expect("transaction-local action conflict must not reject the page");
            assert_eq!(
                reports[0]
                    .transaction_id
                    .as_ref()
                    .map(SensitiveText::expose),
                Some("txn_action")
            );
            assert_transaction_local_malformed(&reports[0]);
        }
    }
}

#[test]
fn query_transaction_reports_accept_semantically_equivalent_authority_duplicates() {
    for (conditions, action_types, amounts, successes) in [
        (
            "<condition>pending settlement</condition><condition>pending_settlement</condition>",
            "<action_type>REFUND</action_type><action_type>refund</action_type>",
            "<amount>$049.0</amount><amount>49.00</amount>",
            "<success>TRUE</success><success>1</success>",
        ),
        (
            "<condition>pending_settlement</condition><condition>pending settlement</condition>",
            "<action_type>refund</action_type><action_type>REFUND</action_type>",
            "<amount>49.00</amount><amount>$049.0</amount>",
            "<success>1</success><success>TRUE</success>",
        ),
    ] {
        let reports = query_transaction_reports_from_xml(&format!(
            "<nm_response><transaction><transaction_id>txn_equivalent</transaction_id>{conditions}<action>{action_types}<date> 20260530120500 </date><date>20260530120500</date>{amounts}{successes}</action></transaction></nm_response>"
        ))
        .expect("semantically equivalent report authority fields should parse");

        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].actions.len(), 1);
        assert_eq!(
            reports[0].condition.as_ref().map(SensitiveText::expose),
            Some("pending settlement")
        );
        let action = &reports[0].actions[0];
        assert_eq!(
            action.action_type.as_ref().map(SensitiveText::expose),
            Some("refund")
        );
        assert_eq!(
            action.date.as_ref().map(SensitiveText::expose),
            Some("20260530120500")
        );
        assert_eq!(
            action.amount.as_ref().map(SensitiveText::expose),
            Some("49.00")
        );
        assert_eq!(
            action.success.as_ref().map(SensitiveText::expose),
            Some("1")
        );
    }
}

#[test]
fn query_transaction_reports_clear_conflicting_or_invalid_action_response_fields() {
    for response_fields in [
        "<response_code>100</response_code><response_code>430</response_code><response_text>Approved</response_text><response_text>Duplicate</response_text>",
        "<response_code>430</response_code><response_code>100</response_code><response_text>Duplicate</response_text><response_text>Approved</response_text>",
        "<response_code><nested>100</nested></response_code><response_code>100</response_code><response_text><nested>Approved</nested></response_text><response_text>Approved</response_text>",
    ] {
        let reports = query_transaction_reports_from_xml(&format!(
            "<nm_response><transaction><transaction_id>txn_diagnostic</transaction_id><action><action_type>sale</action_type>{response_fields}</action></transaction></nm_response>"
        ))
        .expect("invalid diagnostic fields should be cleared without rejecting the report");
        let action = &reports[0].actions[0];
        assert_eq!(action.response_code, None);
        assert_eq!(action.response_text, None);
    }
}

#[test]
fn query_transaction_reports_isolate_invalid_authority_shapes_to_one_transaction() {
    let oversized = "x".repeat(MAX_NMI_FIELD_CHARS + 1);
    for fields in [
        "<condition><nested>complete</nested></condition><condition>complete</condition>"
            .to_owned(),
        "<action><action_type></action_type><action_type>settle</action_type></action>".to_owned(),
        format!("<action><amount>{oversized}</amount></action>"),
    ] {
        let xml = format!(
            "<nm_response><transaction><transaction_id>txn_valid_first</transaction_id></transaction><transaction><transaction_id>txn_invalid_second</transaction_id>{fields}</transaction></nm_response>"
        );
        let reports = query_transaction_reports_from_xml(&xml)
            .expect("transaction-local authority defect must not reject the page");
        assert_eq!(reports.len(), 2);
        assert!(reports[0].diagnostics.is_empty());
        assert_eq!(
            reports[0]
                .transaction_id
                .as_ref()
                .map(SensitiveText::expose),
            Some("txn_valid_first")
        );
        assert_eq!(
            reports[1]
                .transaction_id
                .as_ref()
                .map(SensitiveText::expose),
            Some("txn_invalid_second")
        );
        assert_transaction_local_malformed(&reports[1]);
    }
}

#[test]
fn query_transaction_reports_require_exactly_one_envelope() {
    for xml in [
        "<html>ok</html>",
        "<root><transaction><transaction_id>txn_ignored</transaction_id></transaction><nm_response><transaction><transaction_id>txn_selected</transaction_id></transaction></nm_response></root>",
        "<root><nm_response><transaction><transaction_id>txn_first</transaction_id></transaction></nm_response><nm_response><transaction><transaction_id>txn_second</transaction_id></transaction></nm_response></root>",
        "<root><nm_response></nm_response><nm_response></nm_response></root>",
    ] {
        assert!(matches!(
            query_transaction_reports_from_xml(xml),
            Err(WireError::MalformedResponse(_))
        ));
    }
}

#[test]
fn invalid_later_report_preserves_the_complete_page() {
    let reports = query_transaction_reports_from_xml(
        r#"
        <nm_response>
          <transaction>
            <transaction_id>txn_valid_first</transaction_id>
            <order_id>ck_valid_first</order_id>
          </transaction>
          <transaction>
            <transaction_id>txn_second_a</transaction_id>
            <transaction_id>txn_second_b</transaction_id>
            <order_id>ck_valid_second</order_id>
          </transaction>
        </nm_response>
        "#,
    )
    .expect("a later transaction-local defect must not discard earlier reports");

    assert_eq!(reports.len(), 2);
    assert!(reports[0].diagnostics.is_empty());
    assert_eq!(
        reports[0]
            .transaction_id
            .as_ref()
            .map(SensitiveText::expose),
        Some("txn_valid_first")
    );
    assert_eq!(reports[1].transaction_id, None);
    assert_eq!(
        reports[1].order_id.as_ref().map(SensitiveText::expose),
        Some("ck_valid_second")
    );
    assert_transaction_local_malformed(&reports[1]);
}

#[test]
fn payment_outcome_truncates_oversized_gateway_fields() {
    let long_value = "a".repeat(MAX_NMI_FIELD_CHARS + 25);
    let outcome = payment_outcome_from_json(&json!({
        "id": "txn_oversized_field",
        "status": "approved",
        "response_text": long_value,
    }))
    .expect("payment outcome should parse");

    assert_eq!(outcome.status, PaymentStatus::Approved);
    assert_eq!(
        outcome
            .response_text
            .as_ref()
            .map(SensitiveText::expose)
            .map(str::len),
        Some(MAX_NMI_FIELD_CHARS)
    );
}

#[test]
fn query_transaction_reports_truncates_oversized_fields() {
    let long_value = "a".repeat(MAX_NMI_FIELD_CHARS + 25);
    let reports = query_transaction_reports_from_xml(&format!(
        r#"
        <nm_response>
          <transaction>
            <transaction_id>txn_long_report</transaction_id>
            <condition>complete</condition>
            <action>
              <action_type>settle</action_type>
              <response_text>{long_value}</response_text>
            </action>
          </transaction>
        </nm_response>
        "#
    ))
    .expect("oversized field should parse with truncation");

    assert_eq!(reports.len(), 1);
    assert_eq!(
        reports[0].actions[0]
            .response_text
            .as_ref()
            .map(SensitiveText::expose)
            .map(str::len),
        Some(MAX_NMI_FIELD_CHARS)
    );
}

#[test]
fn query_transaction_reports_rejects_too_many_transactions() {
    let transaction = "<transaction><transaction_id>txn_many</transaction_id></transaction>";
    let xml = format!(
        "<nm_response>{}</nm_response>",
        transaction.repeat(MAX_NMI_TRANSACTION_REPORTS + 1)
    );

    assert!(matches!(
        query_transaction_reports_from_xml(&xml),
        Err(WireError::MalformedResponse(message)) if message.contains("exceeded 100 transactions")
    ));
}

#[test]
fn query_transaction_reports_do_not_reuse_the_transaction_page_limit_for_actions() {
    let action = "<action><action_type>settle</action_type></action>";
    let xml = format!(
        "<nm_response><transaction><transaction_id>txn_many_actions</transaction_id>{}</transaction></nm_response>",
        action.repeat(MAX_NMI_TRANSACTION_REPORTS + 1)
    );

    let reports = query_transaction_reports_from_xml(&xml)
        .expect("a transaction may accumulate more actions than one report page has transactions");
    assert_eq!(reports.len(), 1);
    assert_eq!(
        reports[0]
            .transaction_id
            .as_ref()
            .map(SensitiveText::expose),
        Some("txn_many_actions")
    );
    assert_eq!(reports[0].actions.len(), MAX_NMI_TRANSACTION_REPORTS + 1);
    assert!(reports[0].diagnostics.is_empty());
}

#[test]
fn malformed_transaction_does_not_consume_later_action_capacity() {
    let later_actions = "<action/>".repeat(MAX_NMI_REPORT_ACTIONS);
    let xml = format!(
        "<nm_response><transaction><transaction_id>txn_malformed_first</transaction_id><order_id>ck_malformed_first</order_id><action><action_type>settle</action_type></action><action><amount>1.00</amount><amount>2.00</amount></action></transaction><transaction><transaction_id>txn_budget_consumer</transaction_id>{later_actions}</transaction></nm_response>"
    );
    assert!(xml.len() < MAX_NMI_REPORT_RESPONSE_BYTES);

    let reports = query_transaction_reports_from_xml(&xml)
        .expect("malformed action evidence must not consume later page capacity");

    assert_eq!(reports.len(), 2);
    assert_eq!(
        reports[0]
            .transaction_id
            .as_ref()
            .map(SensitiveText::expose),
        Some("txn_malformed_first")
    );
    assert_eq!(
        reports[0].order_id.as_ref().map(SensitiveText::expose),
        Some("ck_malformed_first")
    );
    assert_transaction_local_malformed(&reports[0]);
    assert_eq!(reports[1].actions.len(), MAX_NMI_REPORT_ACTIONS);
    assert!(reports[1].diagnostics.is_empty());
}

#[test]
fn query_transaction_reports_accept_exact_aggregate_action_capacity() {
    let first_action_count = MAX_NMI_REPORT_ACTIONS / 2;
    let second_action_count = MAX_NMI_REPORT_ACTIONS - first_action_count;
    let first_actions = "<action/>".repeat(first_action_count);
    let second_actions = "<action/>".repeat(second_action_count);
    let xml = format!(
        "<nm_response><transaction><transaction_id>txn_exact_cap_first</transaction_id>{first_actions}</transaction><transaction><transaction_id>txn_exact_cap_second</transaction_id>{second_actions}</transaction></nm_response>"
    );
    assert!(xml.len() < MAX_NMI_REPORT_RESPONSE_BYTES);

    let reports = query_transaction_reports_from_xml(&xml)
        .expect("the exact aggregate action capacity should be accepted");

    assert_eq!(reports.len(), 2);
    assert_eq!(reports[0].actions.len(), first_action_count);
    assert_eq!(reports[1].actions.len(), second_action_count);
    assert!(reports.iter().all(|report| report.diagnostics.is_empty()));
}

#[test]
fn query_transaction_reports_quarantine_the_transaction_exceeding_aggregate_action_capacity() {
    let first_action_count = MAX_NMI_REPORT_ACTIONS / 2;
    let second_action_count = MAX_NMI_REPORT_ACTIONS - first_action_count + 1;
    let first_actions = "<action/>".repeat(first_action_count);
    let second_actions = "<action/>".repeat(second_action_count);
    let xml = format!(
        "<nm_response><transaction><transaction_id>txn_cap_plus_one_first</transaction_id>{first_actions}</transaction><transaction><transaction_id>txn_cap_plus_one_second</transaction_id>{second_actions}</transaction></nm_response>"
    );
    assert!(xml.len() < MAX_NMI_REPORT_RESPONSE_BYTES);

    let reports = query_transaction_reports_from_xml(&xml)
        .expect("aggregate action overflow should remain transaction-local");

    assert_eq!(reports.len(), 2);
    assert_eq!(reports[0].actions.len(), first_action_count);
    assert!(reports[0].diagnostics.is_empty());
    assert_eq!(
        reports[1]
            .transaction_id
            .as_ref()
            .map(SensitiveText::expose),
        Some("txn_cap_plus_one_second")
    );
    assert_transaction_local_malformed(&reports[1]);
}

#[test]
fn query_transaction_reports_bound_expanded_action_record_storage() {
    let xml = format!(
        "<nm_response><transaction><transaction_id>txn_action_storage_bound</transaction_id>{}</transaction></nm_response>",
        "<action/>".repeat(MAX_NMI_REPORT_ACTIONS + 1)
    );
    assert!(xml.len() < MAX_NMI_REPORT_RESPONSE_BYTES);

    let reports = query_transaction_reports_from_xml(&xml)
        .expect("an oversized transaction-local action list should not reject the page");

    assert_eq!(reports.len(), 1);
    assert_transaction_local_malformed(&reports[0]);
}

#[test]
fn query_transaction_reports_decode_entities_and_require_exact_block_tags() {
    let reports = query_transaction_reports_from_xml(
        r#"
        <nm_response>
          <transaction_id>not_a_transaction_block</transaction_id>
          <transaction status="complete">
            <transaction_id>txn_entity</transaction_id>
            <condition>complete</condition>
            <order_id>ck&#95;order&#95;entity</order_id>
            <action sequence="1">
              <amount>49.00</amount>
              <action_type>settle &amp; capture</action_type>
              <date>20260530120000</date>
              <success>1</success>
              <response_text>Approved &amp; captured</response_text>
              <response_code>100</response_code>
            </action>
          </transaction>
        </nm_response>
        "#,
    )
    .expect("valid transaction report response should parse");

    assert_eq!(reports.len(), 1);
    assert_eq!(
        reports[0]
            .transaction_id
            .as_ref()
            .map(SensitiveText::expose),
        Some("txn_entity")
    );
    assert_eq!(
        reports[0].order_id.as_ref().map(SensitiveText::expose),
        Some("ck_order_entity")
    );
    assert_eq!(
        reports[0].actions[0]
            .action_type
            .as_ref()
            .map(SensitiveText::expose),
        Some("settle & capture")
    );
    assert_eq!(
        reports[0].actions[0]
            .response_text
            .as_ref()
            .map(SensitiveText::expose),
        Some("Approved & captured")
    );
}

#[test]
fn query_transaction_reports_reject_nested_or_wrapped_transaction_elements() {
    for xml in [
        r#"
        <nm_response>
          <transactions>
            <transaction>
              <transaction_id>txn_wrapped</transaction_id>
            </transaction>
          </transactions>
        </nm_response>
        "#,
        r#"
        <nm_response>
          <transaction>
            <transaction_id>txn_outer</transaction_id>
            <action>
              <action_type>settle</action_type>
              <transaction>
                <transaction_id>txn_nested</transaction_id>
              </transaction>
            </action>
          </transaction>
        </nm_response>
        "#,
    ] {
        assert!(matches!(
            query_transaction_reports_from_xml(xml),
            Err(WireError::MalformedResponse(message))
                if message.contains("nested or wrapped transaction")
        ));
    }
}

#[test]
fn query_transaction_reports_diagnose_nested_or_wrapped_action_elements_locally() {
    for actions in [
        "<actions><action><action_type>refund</action_type></action></actions>",
        "<action><action_type>settle</action_type><action><action_type>refund</action_type></action></action>",
    ] {
        let reports = query_transaction_reports_from_xml(&format!(
            "<nm_response><transaction><transaction_id>txn_action_shape</transaction_id>{actions}</transaction></nm_response>"
        ))
        .expect("transaction-local action shape must not reject the page");
        assert_eq!(
            reports[0]
                .transaction_id
                .as_ref()
                .map(SensitiveText::expose),
            Some("txn_action_shape")
        );
        assert_transaction_local_malformed(&reports[0]);
    }
}

#[test]
fn query_transaction_reports_rejects_non_report_success_body() {
    assert!(matches!(
        query_transaction_reports_from_xml("<html>ok</html>"),
        Err(WireError::MalformedResponse(message))
            if message.contains("nm_response envelope")
    ));
}

#[test]
fn query_account_mode_accepts_documented_boolean_and_status_shapes() {
    for (xml, expected) in [
        (
            "<nm_response><test_mode_enabled>false</test_mode_enabled></nm_response>",
            AccountMode::Live,
        ),
        (
            "<nm_response><account_details><test_mode_enabled>true</test_mode_enabled></account_details></nm_response>",
            AccountMode::Test,
        ),
        (
            "<nm_response><test_mode_status>inactive</test_mode_status></nm_response>",
            AccountMode::Live,
        ),
        (
            "<nm_response><test_mode_status>active</test_mode_status></nm_response>",
            AccountMode::Test,
        ),
    ] {
        assert_eq!(
            query_account_mode_from_xml(xml).expect("account mode should parse"),
            expected
        );
    }
}

#[test]
fn query_account_mode_rejects_missing_invalid_and_conflicting_status() {
    for xml in [
        "<nm_response></nm_response>",
        "<nm_response><test_mode_enabled>maybe</test_mode_enabled></nm_response>",
        "<nm_response><test_mode_enabled></test_mode_enabled></nm_response>",
        "<nm_response><test_mode_enabled><nested>true</nested></test_mode_enabled></nm_response>",
        "<nm_response><transaction><test_mode_enabled>false</test_mode_enabled></transaction></nm_response>",
        "<nm_response><test_mode_enabled>true</test_mode_enabled><test_mode_status>inactive</test_mode_status></nm_response>",
        "<root><test_mode_enabled>true</test_mode_enabled><nm_response><test_mode_enabled>false</test_mode_enabled></nm_response></root>",
        "<root><nm_response></nm_response><nm_response><test_mode_enabled>false</test_mode_enabled></nm_response></root>",
    ] {
        assert!(matches!(
            query_account_mode_from_xml(xml),
            Err(WireError::MalformedResponse(_))
        ));
    }

    let oversized = "x".repeat(MAX_NMI_FIELD_CHARS + 1);
    assert!(matches!(
        query_account_mode_from_xml(&format!(
            "<nm_response><test_mode_status>{oversized}</test_mode_status></nm_response>"
        )),
        Err(WireError::MalformedResponse(_))
    ));
}

#[test]
fn query_transaction_reports_accepts_empty_report_envelope() {
    let reports = query_transaction_reports_from_xml("<nm_response></nm_response>")
        .expect("empty report envelope should parse");

    assert!(reports.is_empty());
}

#[test]
fn query_transaction_reports_rejects_unclosed_transaction_block() {
    assert!(matches!(
        query_transaction_reports_from_xml(
            r#"
            <nm_response>
              <transaction>
                <transaction_id>txn_missing_close</transaction_id>
            </nm_response>
            "#
        ),
        Err(WireError::MalformedResponse(message))
            if message.contains("unclosed <transaction>")
    ));
}

#[test]
fn query_transaction_reports_rejects_unclosed_action_block() {
    assert!(matches!(
        query_transaction_reports_from_xml(
            r#"
            <nm_response>
              <transaction>
                <transaction_id>txn_missing_action_close</transaction_id>
                <action>
                  <action_type>settle</action_type>
              </transaction>
            </nm_response>
            "#
        ),
        Err(WireError::MalformedResponse(message))
            if message.contains("unclosed <action>")
    ));
}
