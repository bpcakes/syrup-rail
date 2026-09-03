use super::*;

#[test]
fn payment_outcome_accepts_v5_status_without_legacy_response() {
    let outcome = payment_outcome_from_json(&json!({
        "id": "txn_v5_1",
        "status": "approved",
        "condition": "pending settlement",
        "payment_details": {
            "type": "card",
            "card_type": "visa",
            "card_number": "411111******1111",
            "card_exp": "1029"
        }
    }))
    .expect("payment outcome should parse");

    assert_eq!(outcome.status, PaymentStatus::Approved);
    assert_eq!(
        outcome.transaction_id.as_ref().map(SensitiveText::expose),
        Some("txn_v5_1")
    );
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
    assert!(outcome.diagnostics.is_empty());
}
#[test]
fn payment_outcome_accepts_gateway_success_status_aliases() {
    for status in [
        "success",
        "successful",
        "pending settlement",
        "pending_settlement",
        "pending-settlement",
    ] {
        let outcome = payment_outcome_from_json(&json!({
            "id": "txn_status_alias",
            "status": status
        }))
        .expect("payment outcome should parse");

        assert_eq!(
            outcome.status,
            PaymentStatus::Approved,
            "{status} should be treated as approved"
        );
    }
}

#[test]
fn lossless_json_decision_duplicates_are_order_independent() {
    for members in [
        r#""response":"1","response":"2""#,
        r#""response":"2","response":"1""#,
        r#""status":"approved","status":"declined""#,
        r#""status":"declined","status":"approved""#,
        r#""condition":"complete","condition":"voided""#,
        r#""condition":"voided","condition":"complete""#,
        r#""response_code":"100","response_code":"200""#,
        r#""response_code":"200","response_code":"100""#,
        r#""response":"1","response":{}"#,
        r#""response":{},"response":"1""#,
    ] {
        let outcome = payment_outcome_from_json_text(&format!(
            r#"{{"transaction_id":"txn_duplicate_decision",{members}}}"#
        ))
        .expect("conflicting decision fields should parse conservatively");

        assert_eq!(outcome.status, PaymentStatus::Unknown, "{members}");
        assert_eq!(
            outcome.diagnostics,
            vec![PaymentOutcomeDiagnostic::InvalidOrConflictingDecisionField],
            "{members}"
        );
    }
}

#[test]
fn lossless_json_decision_duplicates_use_semantic_equivalence() {
    let equivalent = payment_outcome_from_json_text(
        r#"{
            "transaction_id":"txn_equivalent",
            "response":1,
            "response":"1",
            "status":"pending settlement",
            "status":"pending_settlement",
            "condition":"pending settlement",
            "condition":"pending_settlement",
            "response_code":100,
            "response_code":"100"
        }"#,
    )
    .expect("semantically equivalent decision fields should parse");
    assert_eq!(equivalent.status, PaymentStatus::Approved);
    assert_eq!(
        equivalent.condition.as_ref().map(SensitiveText::expose),
        Some("pending settlement")
    );

    let same_outcome_different_lifecycle = payment_outcome_from_json_text(
        r#"{
            "transaction_id":"txn_lifecycle_ambiguity",
            "condition":"complete",
            "condition":"pending_settlement"
        }"#,
    )
    .expect("approved lifecycle ambiguity should parse conservatively");
    assert_eq!(
        same_outcome_different_lifecycle.status,
        PaymentStatus::Approved
    );
    assert_eq!(same_outcome_different_lifecycle.condition, None);
}

#[test]
fn payment_decision_evidence_is_reduced_symmetrically() {
    for members in [
        r#""response":"1","status":"in progress""#,
        r#""status":"in progress","response":"1""#,
        r#""response":"2","status":"approved""#,
        r#""status":"approved","response":"2""#,
        r#""response":"3","condition":"declined""#,
        r#""condition":"declined","response":"3""#,
        r#""status":"approved","condition":"processor surprise""#,
        r#""condition":"processor surprise","status":"approved""#,
        r#""response":"1","response_code":"200""#,
        r#""response_code":"200","response":"1""#,
        r#""response":"1","response_code":"not-a-code""#,
        r#""response_code":"not-a-code","response":"1""#,
    ] {
        let outcome = payment_outcome_from_json_text(&format!(
            r#"{{"transaction_id":"txn_cross_field",{members}}}"#
        ))
        .expect("conflicting evidence should parse conservatively");
        assert_eq!(outcome.status, PaymentStatus::Unknown, "{members}");
    }

    let agreed = payment_outcome_from_json_text(
        r#"{
            "transaction_id":"txn_agreed",
            "response":"2",
            "response_code":"200",
            "status":"declined",
            "condition":"declined"
        }"#,
    )
    .expect("consistent decline evidence should parse");
    assert_eq!(agreed.status, PaymentStatus::Declined);
}

#[test]
fn decision_diagnostic_precedence_covers_each_closed_state() {
    for (members, expected_status, expected_diagnostic) in [
        (r#""response":"1""#, PaymentStatus::Approved, None),
        (r#""status":"pending""#, PaymentStatus::Unknown, None),
        (
            r#""response":"1","response_code":"200""#,
            PaymentStatus::Unknown,
            Some(PaymentOutcomeDiagnostic::ConflictingDecisionEvidence),
        ),
        (
            r#""response":"1","status":"processor_surprise""#,
            PaymentStatus::Unknown,
            Some(PaymentOutcomeDiagnostic::UnrecognizedDecisionEvidence),
        ),
        (
            r#""response":"1","response":"2","status":"processor_surprise""#,
            PaymentStatus::Unknown,
            Some(PaymentOutcomeDiagnostic::InvalidOrConflictingDecisionField),
        ),
        (
            "",
            PaymentStatus::Unknown,
            Some(PaymentOutcomeDiagnostic::MissingDecisionEvidence),
        ),
    ] {
        let comma = if members.is_empty() { "" } else { "," };
        let outcome = payment_outcome_from_json_text(&format!(
            r#"{{"transaction_id":"txn_decision_precedence"{comma}{members}}}"#
        ))
        .expect("closed decision state should parse conservatively");

        assert_eq!(outcome.status, expected_status, "{members}");
        assert_eq!(
            outcome.diagnostics,
            expected_diagnostic.into_iter().collect::<Vec<_>>(),
            "{members}"
        );
    }
}

#[test]
fn unrecognized_decision_evidence_remains_redacted_from_outcome_debug() {
    let raw = "decision-debug-sentinel";
    let outcome = payment_outcome_from_json(&json!({
        "transaction_id": "txn_redacted_decision",
        "status": raw,
    }))
    .expect("unrecognized decision evidence should parse conservatively");

    assert_eq!(outcome.status, PaymentStatus::Unknown);
    assert_eq!(
        outcome.diagnostics,
        vec![PaymentOutcomeDiagnostic::UnrecognizedDecisionEvidence]
    );
    assert!(!format!("{outcome:?}").contains(raw));
}

#[test]
fn processor_communication_and_duplicate_codes_require_reconciliation() {
    for response_code in ["400", "420", "421", "430", "440", "441"] {
        assert_eq!(
            payment_status_from_response_code(response_code),
            PaymentStatus::Unknown
        );
        let outcome = payment_outcome_from_json_text(&format!(
            r#"{{"transaction_id":"txn_ambiguous_code","response_code":"{response_code}"}}"#
        ))
        .expect("ambiguous response code should parse conservatively");
        assert_eq!(
            outcome.status,
            PaymentStatus::Unknown,
            "response code {response_code} must remain reconcilable"
        );
        let expected = if response_code == "430" {
            PaymentOutcomeDiagnostic::DuplicateTransactionAtProcessor
        } else {
            PaymentOutcomeDiagnostic::IndeterminatePaymentOutcome
        };
        assert_eq!(outcome.diagnostics, vec![expected], "{response_code}");

        let approved_outcome = payment_outcome_from_json_text(&format!(
            r#"{{"transaction_id":"txn_ambiguous_approval","response":"1","response_code":"{response_code}"}}"#
        ))
        .expect("indeterminate response code must veto coarse approval");
        assert_eq!(
            approved_outcome.status,
            PaymentStatus::Unknown,
            "response code {response_code} must veto approval"
        );
        assert_eq!(
            approved_outcome.diagnostics,
            vec![
                expected,
                PaymentOutcomeDiagnostic::ConflictingDecisionEvidence,
            ],
            "{response_code}"
        );
    }

    for response_code in ["300", "410", "411", "460", "461"] {
        assert_eq!(
            payment_status_from_response_code(response_code),
            PaymentStatus::Failed
        );
    }
}

#[test]
fn documented_v5_duplicate_rejection_remains_reconcilable() {
    let outcome = payment_outcome_from_json_text(
        r#"{
            "response":"3",
            "response_code":"430",
            "response_text":"Duplicate transaction"
        }"#,
    )
    .expect("duplicate v5 response should parse");

    assert_eq!(outcome.status, PaymentStatus::Unknown);
    assert_eq!(outcome.transaction_id, None);
    assert_eq!(
        outcome.diagnostics,
        vec![PaymentOutcomeDiagnostic::DuplicateTransactionAtProcessor]
    );
}

#[test]
fn foreground_json_absence_preserves_non_approved_decisions() {
    for id in ["null", r#""""#, r#""   ""#] {
        let failed = payment_outcome_from_json_text(&format!(
            r#"{{"response":"3","response_code":"300","id":{id}}}"#
        ))
        .expect("blank terminal identity should be treated as absent");
        assert_eq!(failed.status, PaymentStatus::Failed, "id={id}");
        assert!(failed.diagnostics.is_empty(), "id={id}");

        let declined = payment_outcome_from_json_text(&format!(
            r#"{{"response":"2","response_code":"200","id":{id}}}"#
        ))
        .expect("blank declined identity should be treated as absent");
        assert_eq!(declined.status, PaymentStatus::Declined, "id={id}");
        assert!(declined.diagnostics.is_empty(), "id={id}");
    }
}

#[test]
fn duplicate_response_code_composes_with_other_decision_diagnostics() {
    for (extra_field, expected) in [
        (
            r#""status":{}"#,
            PaymentOutcomeDiagnostic::InvalidOrConflictingDecisionField,
        ),
        (
            r#""status":"processor_surprise""#,
            PaymentOutcomeDiagnostic::UnrecognizedDecisionEvidence,
        ),
    ] {
        let outcome =
            payment_outcome_from_json_text(&format!(r#"{{"response_code":"430",{extra_field}}}"#))
                .expect("duplicate response with anomalous evidence should parse conservatively");

        assert_eq!(outcome.status, PaymentStatus::Unknown);
        let expected = match expected {
            PaymentOutcomeDiagnostic::InvalidOrConflictingDecisionField => vec![
                PaymentOutcomeDiagnostic::InvalidOrConflictingDecisionField,
                PaymentOutcomeDiagnostic::DuplicateTransactionAtProcessor,
            ],
            PaymentOutcomeDiagnostic::UnrecognizedDecisionEvidence => vec![
                PaymentOutcomeDiagnostic::DuplicateTransactionAtProcessor,
                PaymentOutcomeDiagnostic::UnrecognizedDecisionEvidence,
            ],
            _ => unreachable!("test table contains only the two named diagnostics"),
        };
        assert_eq!(outcome.diagnostics, expected);
    }
}

#[test]
fn numerically_equivalent_duplicate_response_codes_are_recognized() {
    for response_code in ["0430", "+430"] {
        let outcome =
            payment_outcome_from_json_text(&format!(r#"{{"response_code":"{response_code}"}}"#))
                .expect("numeric duplicate response code should parse");

        assert_eq!(outcome.status, PaymentStatus::Unknown);
        assert_eq!(
            outcome.diagnostics,
            vec![PaymentOutcomeDiagnostic::DuplicateTransactionAtProcessor],
            "{response_code}"
        );
    }
}

#[test]
fn noncanonical_301_evidence_never_proves_preprocessing_rate_limiting() {
    for json in [
        r#"{"response":"3","response_code":"301","response_code":"+0301"}"#,
        r#"{"response":"3","response_code":"+0301","response_code":"301"}"#,
        r#"{"response":"3","response_code":"+0301"}"#,
        r#"{"response":"3","response_code":"0301"}"#,
        r#"{"response":" 3 ","response_code":"301"}"#,
        r#"{"response":"3","response_code":" 301 "}"#,
        r#"{"response":"3","response_code":"301","response_code":" 301 "}"#,
        r#"{"response":"3","response_code":" 301 ","response_code":"301"}"#,
        r#"{"response":3,"response_code":301}"#,
    ] {
        let outcome = payment_outcome_from_json_text(json)
            .expect("noncanonical 301 evidence must remain a reconcilable outcome");
        assert_eq!(outcome.status, PaymentStatus::Unknown, "{json}");
    }
}

#[test]
fn exact_json_301_envelope_is_known_not_submitted() {
    assert!(matches!(
        payment_outcome_from_json_text(
            r#"{"response":"3","response_code":"301","response_text":"Rate limit exceeded"}"#,
        ),
        Err(WireError::RateLimited(_))
    ));
}

#[test]
fn equivalent_gateway_state_spellings_are_exposed_order_independently() {
    for json in [
        r#"{"condition":"Pending Settlement","condition":"pending_settlement"}"#,
        r#"{"condition":"pending_settlement","condition":"Pending Settlement"}"#,
    ] {
        let outcome = payment_outcome_from_json_text(json)
            .expect("equivalent gateway states should resolve deterministically");
        assert_eq!(
            outcome.condition.as_ref().map(SensitiveText::expose),
            Some("Pending Settlement"),
            "{json}"
        );
    }
}

#[test]
fn conflicting_unknown_response_codes_preserve_each_diagnostic() {
    for json in [
        r#"{"response_code":"420","response_code":"430"}"#,
        r#"{"response_code":"430","response_code":"420"}"#,
    ] {
        let outcome = payment_outcome_from_json_text(json)
            .expect("conflicting response codes must remain a reconcilable outcome");
        assert_eq!(outcome.status, PaymentStatus::Unknown, "{json}");
        assert_eq!(outcome.response_code, None, "{json}");
        assert_eq!(
            outcome.diagnostics,
            vec![
                PaymentOutcomeDiagnostic::InvalidOrConflictingDecisionField,
                PaymentOutcomeDiagnostic::IndeterminatePaymentOutcome,
                PaymentOutcomeDiagnostic::DuplicateTransactionAtProcessor,
            ],
            "{json}"
        );
    }
}

#[test]
fn equivalent_response_code_spellings_are_exposed_order_independently() {
    for json in [
        r#"{"response_code":"430","response_code":"+0430"}"#,
        r#"{"response_code":"+0430","response_code":"430"}"#,
    ] {
        let outcome = payment_outcome_from_json_text(json)
            .expect("equivalent response codes should resolve deterministically");
        assert_eq!(
            outcome.response_code.as_ref().map(SensitiveText::expose),
            Some("430"),
            "{json}"
        );
    }

    let outcome = payment_outcome_from_json_text(r#"{"response_code":"0430"}"#)
        .expect("one exact provider spelling should be preserved");
    assert_eq!(
        outcome.response_code.as_ref().map(SensitiveText::expose),
        Some("0430")
    );
}

#[test]
fn json_301_with_processing_evidence_is_never_known_not_submitted() {
    for evidence in [
        r#""customer_vault_id":"vault_assigned""#,
        r#""customer_vault_id":"vault_first","customer_vault_id":"vault_second""#,
        r#""customer_vault_id":"vault_second","customer_vault_id":"vault_first""#,
        r#""customer_vault_id":{}"#,
        r#""authcode":"auth_assigned""#,
        r#""auth_code":"auth_assigned""#,
        r#""authorization_code":"auth_assigned""#,
        r#""auth_code":"auth_first","authorization_code":"auth_second""#,
        r#""authorization_code":"auth_second","auth_code":"auth_first""#,
        r#""auth_code":{}"#,
        r#""authorization":{"code":"auth_assigned"}"#,
        r#""payment":{"auth_code":"auth_assigned"}"#,
        r#""payment":{"authorization_code":"auth_assigned"}"#,
        r#""payment_details":{"auth_code":"auth_assigned"}"#,
        r#""payment_details":{"authorization_code":"auth_assigned"}"#,
        r#""avsresponse":"Y""#,
        r#""avs_response":"Y""#,
        r#""payment_details":{"avs_response":"Y"}"#,
        r#""payment_details":{"card":{"avs_response":"Y"}}"#,
        r#""card":{"avs_response":"Y"}"#,
        r#""cvvresponse":"M""#,
        r#""cvv_response":"M""#,
        r#""payment_details":{"cvv_response":"M"}"#,
        r#""payment_details":{"card":{"cvv_response":"M"}}"#,
        r#""card":{"cvv_response":"M"}"#,
        r#""action":{"action_type":"sale"}"#,
        r#""action":{}"#,
        r#""actions":[{"action_type":"sale"}]"#,
        r#""actions":true"#,
        r#""transaction":{"action":{"action_type":"sale"}}"#,
        r#""transaction":{"actions":[{"action_type":"sale"}]}"#,
        r#""payment":{"action":{"action_type":"sale"}}"#,
        r#""payment":{"actions":[{"action_type":"sale"}]}"#,
        r#""transaction":{}"#,
        r#""payment":{}"#,
        r#""payment_details":{"card_number":"4***********4242"}"#,
        r#""card":{"last4":"4242"}"#,
        r#""future_provider_field":null"#,
    ] {
        for json in [
            format!(r#"{{"response":"3","response_code":"301",{evidence}}}"#),
            format!(r#"{{{evidence},"response_code":"301","response":"3"}}"#),
        ] {
            let outcome = payment_outcome_from_json_text(&json)
                .expect("JSON 301 processing evidence must remain indeterminate");
            assert_eq!(outcome.status, PaymentStatus::Unknown, "{json}");
        }
    }

    let outcome = payment_outcome_from_json_text(
        r#"{
                "response":"3",
                "response_code":"301",
                "customer_vault_id":null,
                "auth_code":"",
                "avs_response":" ",
                "cvv_response":null,
                "actions":[],
                "authorization":{"code":null},
                "transaction":{"actions":[]},
                "payment":{"authorization_code":"","actions":[]},
                "payment_details":{
                    "auth_code":null,
                    "avs_response":"",
                    "card":{"cvv_response":null}
                }
            }"#,
    )
    .expect("an extended 301 envelope cannot prove pre-processing");
    assert_eq!(outcome.status, PaymentStatus::Unknown);
}

#[test]
fn conflicting_json_response_text_is_cleared_without_changing_structured_status() {
    for response_text in [
        r#""response_text":"Approved","response_text":"Declined""#,
        r#""response_text":"Declined","response_text":"Approved""#,
    ] {
        let outcome = payment_outcome_from_json_text(&format!(
            r#"{{"transaction_id":"txn_text_conflict","response":"1","response_code":"100","condition":"complete",{response_text}}}"#
        ))
        .expect("auxiliary text conflict should parse");
        assert_eq!(outcome.status, PaymentStatus::Approved);
        assert_eq!(outcome.response_text, None);
    }
}

#[test]
fn payment_outcome_treats_conflicting_v5_status_and_condition_as_unknown() {
    let outcome = payment_outcome_from_json(&json!({
        "id": "txn_v5_conflict",
        "status": "approved",
        "condition": "declined"
    }))
    .expect("payment outcome should parse");

    assert_eq!(outcome.status, PaymentStatus::Unknown);
    assert_eq!(
        outcome.diagnostics,
        vec![PaymentOutcomeDiagnostic::ConflictingDecisionEvidence]
    );
}

#[test]
fn payment_outcome_treats_response_approved_conflict_as_unknown() {
    let outcome = payment_outcome_from_json(&json!({
        "id": "txn_response_approved_conflict",
        "response": "1",
        "status": "approved",
        "condition": "voided"
    }))
    .expect("payment outcome should parse");

    assert_eq!(outcome.status, PaymentStatus::Unknown);
}

#[test]
fn payment_outcome_trims_legacy_response_code() {
    let outcome = payment_outcome_from_json(&json!({
        "id": "txn_trimmed_response",
        "response": " 1 ",
        "condition": " complete "
    }))
    .expect("trimmed response outcome should parse");

    assert_eq!(outcome.status, PaymentStatus::Approved);
    assert_eq!(
        outcome.response.as_ref().map(SensitiveText::expose),
        Some("1")
    );
    assert_eq!(
        outcome.condition.as_ref().map(SensitiveText::expose),
        Some("complete")
    );
}

#[test]
fn payment_outcome_treats_unrecognized_gateway_status_as_unknown() {
    let outcome = payment_outcome_from_json(&json!({
        "id": "txn_unrecognized_status",
        "status": "processor_surprise"
    }))
    .expect("unrecognized processor status should parse as unknown");

    assert_eq!(outcome.status, PaymentStatus::Unknown);
    assert_eq!(
        outcome.transaction_id.as_ref().map(SensitiveText::expose),
        Some("txn_unrecognized_status")
    );
    assert_eq!(
        outcome.diagnostics,
        vec![PaymentOutcomeDiagnostic::UnrecognizedDecisionEvidence]
    );
}

#[test]
fn payment_outcome_treats_unrecognized_numeric_response_as_unknown() {
    let outcome = payment_outcome_from_json(&json!({
        "id": "txn_unrecognized_response",
        "response": "4"
    }))
    .expect("unrecognized numeric response should parse as unknown");

    assert_eq!(outcome.status, PaymentStatus::Unknown);
    assert_eq!(
        outcome.transaction_id.as_ref().map(SensitiveText::expose),
        Some("txn_unrecognized_response")
    );
    assert_eq!(
        outcome.diagnostics,
        vec![PaymentOutcomeDiagnostic::UnrecognizedDecisionEvidence]
    );
}

#[test]
fn known_unknown_gateway_states_do_not_emit_diagnostics() {
    for status in [
        "unknown",
        "pending",
        "queued",
        "in progress",
        "processing",
        "review",
        "under review",
        "refunded",
    ] {
        let outcome = payment_outcome_from_json(&json!({
            "id": "txn_known_unknown",
            "status": status
        }))
        .expect("known unresolved status should parse");

        assert_eq!(outcome.status, PaymentStatus::Unknown, "{status}");
        assert!(outcome.diagnostics.is_empty(), "{status}");
    }
}

#[test]
fn generic_gateway_errors_have_indeterminate_provenance() {
    for (json, expected) in [
        (
            r#"{"id":"txn_generic_error","status":"error"}"#,
            vec![PaymentOutcomeDiagnostic::IndeterminatePaymentOutcome],
        ),
        (
            r#"{"id":"txn_generic_error","condition":"error"}"#,
            vec![PaymentOutcomeDiagnostic::IndeterminatePaymentOutcome],
        ),
        (
            r#"{"id":"txn_generic_error","response":"3","status":"pending"}"#,
            vec![PaymentOutcomeDiagnostic::IndeterminatePaymentOutcome],
        ),
        (
            r#"{"id":"txn_generic_error","response":"3","status":"provider_surprise"}"#,
            vec![
                PaymentOutcomeDiagnostic::IndeterminatePaymentOutcome,
                PaymentOutcomeDiagnostic::UnrecognizedDecisionEvidence,
            ],
        ),
        (
            r#"{"id":"txn_generic_error","status":"error","status":"provider_surprise"}"#,
            vec![
                PaymentOutcomeDiagnostic::InvalidOrConflictingDecisionField,
                PaymentOutcomeDiagnostic::IndeterminatePaymentOutcome,
            ],
        ),
        (
            r#"{"id":"txn_generic_error","response":"3","status":"failed","condition":"pending"}"#,
            vec![
                PaymentOutcomeDiagnostic::IndeterminatePaymentOutcome,
                PaymentOutcomeDiagnostic::ConflictingDecisionEvidence,
            ],
        ),
    ] {
        let outcome = payment_outcome_from_json_text(json)
            .expect("a recognized generic gateway error should parse");

        assert_eq!(outcome.status, PaymentStatus::Unknown, "{json}");
        assert_eq!(outcome.diagnostics, expected, "{json}");
    }
}

#[test]
fn json_determinate_failure_supersedes_generic_error_provenance() {
    for json in [
        r#"{"id":"txn_failed","response":"3","status":"failed"}"#,
        r#"{"id":"txn_failed","response":"3","condition":"failed"}"#,
        r#"{"id":"txn_failed","response":"3","response_code":"300"}"#,
    ] {
        let outcome = payment_outcome_from_json_text(json)
            .expect("determinate failure evidence should resolve the generic error");

        assert_eq!(outcome.status, PaymentStatus::Failed, "{json}");
        assert!(outcome.diagnostics.is_empty(), "{json}");
    }
}

#[test]
fn missing_decision_evidence_emits_a_payload_free_diagnostic() {
    let outcome = payment_outcome_from_json(&json!({
        "id": "txn_missing_decision"
    }))
    .expect("missing decision evidence should parse conservatively");

    assert_eq!(outcome.status, PaymentStatus::Unknown);
    assert_eq!(
        outcome.diagnostics,
        vec![PaymentOutcomeDiagnostic::MissingDecisionEvidence]
    );
}
