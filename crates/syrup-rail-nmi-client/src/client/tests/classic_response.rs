use super::*;

#[test]
fn classic_payment_outcome_reads_successful_vault_sale_response() {
    let outcome = classic_payment_outcome_from_form(
        "response=1&responsetext=SUCCESS&transactionid=txn_123&customer_vault_id=vault_123&response_code=100&type=sale&cctype=Visa&cc_number=411111******1111",
    )
    .expect("successful classic form response should parse");

    assert_eq!(outcome.status, PaymentStatus::Approved);
    assert_eq!(
        outcome.transaction_id.as_ref().map(SensitiveText::expose),
        Some("txn_123")
    );
    assert_eq!(
        outcome
            .customer_vault_id
            .as_ref()
            .map(SensitiveText::expose),
        Some("vault_123")
    );
    assert_eq!(
        outcome.response.as_ref().map(SensitiveText::expose),
        Some("1")
    );
    assert_eq!(
        outcome.response_code.as_ref().map(SensitiveText::expose),
        Some("100")
    );
    assert_eq!(
        outcome.response_text.as_ref().map(SensitiveText::expose),
        Some("SUCCESS")
    );
    assert_eq!(
        outcome
            .descriptor
            .payment_type
            .as_ref()
            .map(SensitiveText::expose),
        Some("sale")
    );
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
}

#[test]
fn classic_payment_outcome_accepts_blank_optional_customer_vault_id() {
    let outcome = classic_payment_outcome_from_form(
        "response=1&transactionid=txn_classic_without_vault&customer_vault_id=&response_code=100",
    )
    .expect("a blank optional Classic vault identifier should parse");

    assert_eq!(outcome.status, PaymentStatus::Approved);
    assert_eq!(
        outcome.transaction_id.as_ref().map(SensitiveText::expose),
        Some("txn_classic_without_vault")
    );
    assert_eq!(outcome.customer_vault_id, None);
    assert!(outcome.diagnostics.is_empty());
}

#[test]
fn classic_payment_outcome_wraps_raw_processor_identifiers() {
    let outcome = classic_payment_outcome_from_form(
        "response=1&responsetext=SUCCESS&transactionid=card_number%3D4111111111111111&customer_vault_id=payment_token%3Dtok_secret&response_code=100",
    )
    .expect("classic form response should parse");

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
fn classic_payment_outcome_preserves_numeric_gateway_identifiers() {
    let outcome = classic_payment_outcome_from_form(
        "response=1&responsetext=SUCCESS&transactionid=1234567890123&customer_vault_id=9876543210123&response_code=100",
    )
    .expect("classic form response should parse");

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
fn classic_payment_outcome_accepts_consistent_duplicate_aliases() {
    let outcome = classic_payment_outcome_from_form(
        "response=1&transactionid=txn_same&transactionid=txn_same&transaction_id=txn_same&customer_vault_id=vault_same&customer_vaultid=vault_same",
    )
    .expect("consistent classic aliases should parse");

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
fn classic_payment_outcome_rejects_invalid_identity_bundle() {
    for (response, expected_status, expected_diagnostic) in [
        (
            "response=1&transactionid=txn_first&transactionid=txn_second&customer_vault_id=vault_valid",
            PaymentStatus::Unknown,
            PaymentOutcomeDiagnostic::InvalidOrConflictingTransactionIdentifier,
        ),
        (
            "response=1&transactionid=txn_valid&customer_vault_id=vault_first&customer_vaultid=vault_second",
            PaymentStatus::Unknown,
            PaymentOutcomeDiagnostic::InvalidOrConflictingCustomerVaultIdentifier,
        ),
        (
            "response=1&transactionid=&transaction_id=txn_valid&customer_vault_id=vault_valid",
            PaymentStatus::Unknown,
            PaymentOutcomeDiagnostic::InvalidOrConflictingTransactionIdentifier,
        ),
        (
            "response=2&transactionid=txn_first&transactionid=txn_second&customer_vault_id=vault_valid",
            PaymentStatus::Declined,
            PaymentOutcomeDiagnostic::InvalidOrConflictingTransactionIdentifier,
        ),
        (
            "response=3&response_code=300&transactionid=txn_first&transactionid=txn_second&customer_vault_id=vault_valid",
            PaymentStatus::Failed,
            PaymentOutcomeDiagnostic::InvalidOrConflictingTransactionIdentifier,
        ),
    ] {
        let outcome = classic_payment_outcome_from_form(response)
            .expect("invalid classic identity should quarantine its identity bundle");
        assert_eq!(outcome.status, expected_status);
        assert_eq!(outcome.transaction_id, None);
        assert_eq!(outcome.customer_vault_id, None);
        assert_eq!(outcome.diagnostics, vec![expected_diagnostic]);
    }
}

#[test]
fn classic_payment_outcome_preserves_pan_shaped_vault_id_for_caller_validation() {
    let outcome = classic_payment_outcome_from_form(
        "response=1&responsetext=SUCCESS&transactionid=txn_classic_pan_shaped_vault&customer_vault_id=4111111111111111&response_code=100",
    )
    .expect("classic form response should parse");

    assert_eq!(outcome.status, PaymentStatus::Approved);
    assert_eq!(
        outcome.transaction_id.as_ref().map(SensitiveText::expose),
        Some("txn_classic_pan_shaped_vault")
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
fn classic_payment_outcome_preserves_bounded_syntax_and_rejects_oversized_identifiers() {
    let long_id = format!("{}x", "a".repeat(MAX_NMI_FIELD_CHARS));
    for (transaction_id, expected) in [
        ("txn%3Dprovider".to_owned(), Some("txn=provider")),
        (
            "txn_4111%281111%291111_1111".to_owned(),
            Some("txn_4111(1111)1111_1111"),
        ),
        (long_id, None),
    ] {
        let outcome = classic_payment_outcome_from_form(&format!(
            "response=1&responsetext=SUCCESS&transactionid={transaction_id}"
        ))
        .expect("classic response should parse");
        if let Some(expected) = expected {
            assert_eq!(outcome.status, PaymentStatus::Approved);
            assert_eq!(
                outcome.transaction_id.as_ref().map(SensitiveText::expose),
                Some(expected)
            );
        } else {
            assert_eq!(outcome.status, PaymentStatus::Unknown);
            assert_eq!(outcome.transaction_id, None);
        }
    }
}

#[test]
fn classic_payment_outcome_preserves_vault_disabled_failure() {
    let outcome = classic_payment_outcome_from_form(
        "response=3&responsetext=Your+account+is+not+set+up+to+use+the+Customer+Vault&response_code=300",
    )
    .expect("failed classic form response should parse");

    assert_eq!(outcome.status, PaymentStatus::Failed);
    assert_eq!(
        outcome.response.as_ref().map(SensitiveText::expose),
        Some("3")
    );
    assert_eq!(
        outcome.response_code.as_ref().map(SensitiveText::expose),
        Some("300")
    );
    assert_eq!(
        outcome.response_text.as_ref().map(SensitiveText::expose),
        Some("Your account is not set up to use the Customer Vault")
    );
}

#[test]
fn classic_determinate_failure_codes_accept_an_empty_transaction_identifier() {
    for response_code in ["300", "410", "411", "460", "461"] {
        let response = format!(
            "response=3&responsetext=Terminal+gateway+failure&response_code={response_code}&transactionid="
        );
        let outcome = classic_payment_outcome_from_form(&response)
            .expect("an empty identifier cannot overturn a terminal gateway decision");

        assert_eq!(outcome.status, PaymentStatus::Failed, "{response_code}");
        assert_eq!(outcome.transaction_id, None, "{response_code}");
        assert!(outcome.diagnostics.is_empty(), "{response_code}");
    }
}

#[test]
fn classic_indeterminate_error_codes_remain_reconcilable_without_an_identifier() {
    for response_code in ["400", "420", "421", "440", "441"] {
        let response = format!(
            "response=3&responsetext=Processor+error&response_code={response_code}&transactionid="
        );
        let outcome = classic_payment_outcome_from_form(&response)
            .expect("a processor error should remain an outcome");

        assert_eq!(outcome.status, PaymentStatus::Unknown, "{response_code}");
        assert_eq!(outcome.transaction_id, None, "{response_code}");
        assert_eq!(
            outcome.diagnostics,
            vec![PaymentOutcomeDiagnostic::IndeterminatePaymentOutcome],
            "{response_code}"
        );
    }
}

#[test]
fn classic_generic_provider_error_has_indeterminate_provenance() {
    for (response, expected) in [
        (
            "response=3&responsetext=System+error&transactionid=",
            vec![PaymentOutcomeDiagnostic::IndeterminatePaymentOutcome],
        ),
        (
            "response=3&status=pending&responsetext=System+error&transactionid=",
            vec![PaymentOutcomeDiagnostic::IndeterminatePaymentOutcome],
        ),
        (
            "response=3&status=provider_surprise&responsetext=System+error&transactionid=",
            vec![
                PaymentOutcomeDiagnostic::IndeterminatePaymentOutcome,
                PaymentOutcomeDiagnostic::UnrecognizedDecisionEvidence,
            ],
        ),
        (
            "response=3&status=failed&condition=pending&responsetext=Conflicting+system+error&transactionid=",
            vec![
                PaymentOutcomeDiagnostic::IndeterminatePaymentOutcome,
                PaymentOutcomeDiagnostic::ConflictingDecisionEvidence,
            ],
        ),
    ] {
        let outcome = classic_payment_outcome_from_form(response)
            .expect("a recognized generic provider error should remain an outcome");

        assert_eq!(outcome.status, PaymentStatus::Unknown, "{response}");
        assert_eq!(outcome.transaction_id, None, "{response}");
        assert_eq!(outcome.diagnostics, expected, "{response}");
    }
}

#[test]
fn classic_determinate_failure_supersedes_generic_error_provenance() {
    for response in [
        "response=3&condition=failed&responsetext=Terminal+failure&transactionid=",
        "response=3&status=failed&responsetext=Terminal+failure&transactionid=",
        "response=3&response_code=300&responsetext=Terminal+failure&transactionid=",
    ] {
        let outcome = classic_payment_outcome_from_form(response)
            .expect("determinate failure evidence should resolve the generic error");

        assert_eq!(outcome.status, PaymentStatus::Failed, "{response}");
        assert_eq!(outcome.transaction_id, None, "{response}");
        assert!(outcome.diagnostics.is_empty(), "{response}");
    }
}

#[test]
fn classic_terminal_response_quarantines_identity_without_erasing_failure() {
    let outcome = classic_payment_outcome_from_form(
        "response=3&response_code=300&transactionid=&transaction_id=txn_conflicting_failure",
    )
    .expect("conflicting terminal response identity should remain failed");

    assert_eq!(outcome.status, PaymentStatus::Failed);
    assert_eq!(outcome.transaction_id, None);
    assert_eq!(
        outcome.diagnostics,
        vec![PaymentOutcomeDiagnostic::InvalidOrConflictingTransactionIdentifier]
    );
}

#[test]
fn classic_noncanonical_301_evidence_is_order_independent_and_reconcilable() {
    for response in [
        "response=3&response_code=301&responsecode=%2B0301",
        "response=3&responsecode=%2B0301&response_code=301",
        "response=3&response_code=0301",
        "response=+3+&response_code=301",
        "response=3&response_code=+301+",
        "response=3&response_code=301&responsecode=+301+",
        "response=3&responsecode=+301+&response_code=301",
    ] {
        let outcome = classic_payment_outcome_from_form(response)
            .expect("noncanonical Classic 301 evidence must remain reconcilable");
        assert_eq!(outcome.status, PaymentStatus::Unknown, "{response}");
    }
}

#[test]
fn classic_duplicate_response_code_requires_reconciliation() {
    let outcome = classic_payment_outcome_from_form(
        "response=3&responsetext=Duplicate+transaction&response_code=430&transactionid=txn_duplicate_evidence",
    )
    .expect("duplicate Classic response should parse");

    assert_eq!(outcome.status, PaymentStatus::Unknown);
    assert_eq!(
        outcome.transaction_id.as_ref().map(SensitiveText::expose),
        Some("txn_duplicate_evidence")
    );
    assert_eq!(
        outcome.response_code.as_ref().map(SensitiveText::expose),
        Some("430")
    );
    assert_eq!(
        outcome.diagnostics,
        vec![PaymentOutcomeDiagnostic::DuplicateTransactionAtProcessor]
    );
}

#[test]
fn classic_duplicate_response_treats_an_empty_identity_as_absent() {
    let outcome = classic_payment_outcome_from_form(
        "response=3&responsetext=Duplicate+transaction&response_code=430&transactionid=",
    )
    .expect("duplicate Classic response with an empty identity should parse");

    assert_eq!(outcome.status, PaymentStatus::Unknown);
    assert_eq!(
        outcome.diagnostics,
        vec![PaymentOutcomeDiagnostic::DuplicateTransactionAtProcessor]
    );
}

#[test]
fn classic_duplicate_response_keeps_unrecognized_decision_diagnostic() {
    let outcome = classic_payment_outcome_from_form(
        "response_code=430&status=processor_surprise&transactionid=txn_duplicate_unknown_status",
    )
    .expect("duplicate Classic response with an unknown status should parse");

    assert_eq!(outcome.status, PaymentStatus::Unknown);
    assert_eq!(
        outcome.diagnostics,
        vec![
            PaymentOutcomeDiagnostic::DuplicateTransactionAtProcessor,
            PaymentOutcomeDiagnostic::UnrecognizedDecisionEvidence,
        ]
    );
}

#[test]
fn classic_numeric_duplicate_code_aliases_are_semantically_consistent() {
    let outcome = classic_payment_outcome_from_form(
        "response_code=430&responsecode=%2B0430&transactionid=txn_duplicate_code_aliases",
    )
    .expect("equivalent duplicate response-code aliases should parse");

    assert_eq!(outcome.status, PaymentStatus::Unknown);
    assert_eq!(
        outcome.response_code.as_ref().map(SensitiveText::expose),
        Some("430")
    );
    assert_eq!(
        outcome.diagnostics,
        vec![PaymentOutcomeDiagnostic::DuplicateTransactionAtProcessor]
    );
}

#[test]
fn classic_301_without_lifecycle_evidence_is_known_not_submitted() {
    let error = classic_payment_outcome_from_form(
        "response=3&responsetext=Rate+limit+exceeded&response_code=301&transactionid=&authcode=&avsresponse=&cvvresponse=&orderid=&type=",
    )
    .expect_err("a Classic 301 without lifecycle evidence must be known not submitted");

    assert!(matches!(error, WireError::RateLimited(_)));
}

#[test]
fn classic_rate_limit_requires_the_exact_preprocessing_decision_tuple() {
    for response in [
        "response=03&response_code=301",
        "response=3&response_code=0301",
        "response=3&response_code=3010",
        "response=3&response_code=301.0",
        "response=3&response_code=301&status=pending",
    ] {
        assert!(
            !matches!(
                classic_payment_outcome_from_form(response),
                Err(WireError::RateLimited(_))
            ),
            "{response} must not be treated as the documented preprocessing rate limit"
        );
    }
}

#[test]
fn classic_301_with_lifecycle_evidence_is_never_known_not_submitted() {
    for lifecycle_fields in [
        "customer_vault_id=vault_assigned",
        "authcode=auth_assigned",
        "customer_vault_id=vault_first&customer_vaultid=vault_second",
        "customer_vaultid=vault_second&customer_vault_id=vault_first",
        "authcode=auth_first&auth_code=auth_second",
        "auth_code=auth_second&authcode=auth_first",
        "avsresponse=Y",
        "avsresponse=Y&avs_response=N",
        "avs_response=N&avsresponse=Y",
        "cvvresponse=M",
        "cvvresponse=M&cvv_response=N",
        "cvv_response=N&cvvresponse=M",
        "transactionid=txn_first&transaction_id=txn_second",
        "transaction_id=txn_second&transactionid=txn_first",
        "type=sale",
        "cctype=visa",
        "card_type=visa",
        "ccnumber=xxxx4242",
        "cc_number=xxxx4242",
        "responsetext=Unexpected+message",
        "future_provider_field=",
    ] {
        for response in [
            format!("response=3&response_code=301&{lifecycle_fields}"),
            format!("{lifecycle_fields}&response_code=301&response=3"),
        ] {
            let outcome = classic_payment_outcome_from_form(&response)
                .expect("Classic 301 lifecycle evidence must remain indeterminate");
            assert_eq!(outcome.status, PaymentStatus::Unknown, "{response}");
        }
    }

    let oversized_evidence = "a".repeat(MAX_NMI_FIELD_CHARS + 1);
    for field in ["authcode", "avsresponse", "cvvresponse"] {
        for response in [
            format!("response=3&response_code=301&{field}={oversized_evidence}"),
            format!("{field}={oversized_evidence}&response_code=301&response=3"),
        ] {
            let outcome = classic_payment_outcome_from_form(&response)
                .expect("invalid Classic processing evidence must remain indeterminate");
            assert_eq!(outcome.status, PaymentStatus::Unknown, "{response}");
        }
    }
}

#[test]
fn classic_payment_outcome_rejects_empty_success_form_body_as_malformed() {
    assert!(matches!(
        classic_payment_outcome_from_form(""),
        Err(WireError::MalformedResponse(_))
    ));
}

#[test]
fn classic_payment_outcome_treats_unrecognized_success_form_body_as_unknown() {
    let outcome =
        classic_payment_outcome_from_form("response=7&responsetext=wat&transactionid=txn_7")
            .expect("unrecognized success form body should parse as unknown");

    assert_eq!(outcome.status, PaymentStatus::Unknown);
    assert_eq!(
        outcome.transaction_id.as_ref().map(SensitiveText::expose),
        Some("txn_7")
    );
}

#[test]
fn classic_payment_outcome_preserves_reported_unknown_status() {
    let outcome = classic_payment_outcome_from_form(
        "status=pending&responsetext=Payment+is+pending&transactionid=txn_pending",
    )
    .expect("reported unknown classic form response should parse");

    assert_eq!(outcome.status, PaymentStatus::Unknown);
    assert_eq!(
        outcome.transaction_id.as_ref().map(SensitiveText::expose),
        Some("txn_pending")
    );
    assert_eq!(
        outcome.response_text.as_ref().map(SensitiveText::expose),
        Some("Payment is pending")
    );
}

#[test]
fn classic_payment_outcome_defers_conflicting_status_and_condition() {
    let outcome = classic_payment_outcome_from_form(
        "status=complete&condition=declined&transactionid=txn_conflicting",
    )
    .expect("conflicting classic form response should parse as unknown");

    assert_eq!(outcome.status, PaymentStatus::Unknown);
    assert_eq!(
        outcome.transaction_id.as_ref().map(SensitiveText::expose),
        Some("txn_conflicting")
    );
}

#[test]
fn classic_payment_outcome_treats_response_approved_conflict_as_unknown() {
    let outcome = classic_payment_outcome_from_form(
        "response=1&status=complete&condition=declined&transactionid=txn_response_conflicting",
    )
    .expect("conflicting response=1 classic form response should parse as unknown");

    assert_eq!(outcome.status, PaymentStatus::Unknown);
    assert_eq!(
        outcome.transaction_id.as_ref().map(SensitiveText::expose),
        Some("txn_response_conflicting")
    );
}

#[test]
fn classic_decision_duplicates_are_order_independent() {
    for response in [
        "response=1&response=2&transactionid=txn_duplicate",
        "response=2&response=1&transactionid=txn_duplicate",
        "status=approved&status=declined&transactionid=txn_duplicate",
        "status=declined&status=approved&transactionid=txn_duplicate",
        "condition=complete&condition=voided&transactionid=txn_duplicate",
        "condition=voided&condition=complete&transactionid=txn_duplicate",
        "response_code=100&responsecode=200&transactionid=txn_duplicate",
        "responsecode=200&response_code=100&transactionid=txn_duplicate",
        "response=1&response=&transactionid=txn_duplicate",
        "response=&response=1&transactionid=txn_duplicate",
    ] {
        let outcome = classic_payment_outcome_from_form(response)
            .expect("conflicting classic decision fields should parse conservatively");
        assert_eq!(outcome.status, PaymentStatus::Unknown, "{response}");
        assert_eq!(
            outcome.diagnostics,
            vec![PaymentOutcomeDiagnostic::InvalidOrConflictingDecisionField],
            "{response}"
        );
    }
}

#[test]
fn classic_auxiliary_alias_conflicts_are_cleared() {
    for response_text in [
        "responsetext=Approved&response_text=Declined",
        "response_text=Declined&responsetext=Approved",
    ] {
        let outcome = classic_payment_outcome_from_form(&format!(
            "response=1&response_code=100&condition=complete&transactionid=txn_text&{response_text}"
        ))
        .expect("auxiliary classic text conflict should parse");
        assert_eq!(outcome.status, PaymentStatus::Approved);
        assert_eq!(outcome.response_text, None);
    }
}

#[test]
fn identical_duplicate_long_response_text_is_consistent_and_bounded() {
    let response_text = "A".repeat(MAX_NMI_FIELD_CHARS + 64);
    let response = form_urlencoded::Serializer::new(String::new())
        .append_pair("response", "1")
        .append_pair("response_code", "100")
        .append_pair("condition", "complete")
        .append_pair("transactionid", "txn-long-duplicate-text")
        .append_pair("responsetext", &response_text)
        .append_pair("responsetext", &response_text)
        .finish();

    let outcome = classic_payment_outcome_from_form(&response)
        .expect("identical long auxiliary fields should parse");

    assert_eq!(outcome.status, PaymentStatus::Approved);
    assert!(outcome.diagnostics.is_empty());
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
fn classic_descriptor_conflicts_are_cleared() {
    let outcome = classic_payment_outcome_from_form(
        "response=1&response_code=100&condition=complete&transactionid=txn_descriptor&type=sale&type=auth&cctype=Visa&cctype=&cc_number=411111******1111&ccnumber=555555******4444",
    )
    .expect("descriptor conflicts should not affect payment authority");

    assert_eq!(outcome.status, PaymentStatus::Approved);
    assert_eq!(outcome.descriptor.payment_type, None);
    assert_eq!(outcome.descriptor.card_brand, None);
    assert_eq!(outcome.descriptor.card_last4, None);
}

#[test]
fn parse_expiry_accepts_mmyyyy() {
    assert_eq!(parse_expiry(Some("102029")), (Some(10), Some(2029)));
}

#[test]
fn parse_expiry_drops_invalid_months() {
    assert_eq!(parse_expiry(Some("132029")), (None, None));
    assert_eq!(parse_expiry(Some("00/29")), (None, None));
}

#[test]
fn last4_requires_masked_or_four_digit_card_text() {
    assert_eq!(
        last4("411111******1111".to_owned()).as_deref(),
        Some("1111")
    );
    assert_eq!(last4("1111".to_owned()).as_deref(), Some("1111"));
    assert_eq!(last4("4111111111111111".to_owned()), None);
}

#[test]
fn classic_approval_signals_survive_duplicate_and_status_reduction() {
    for body in [
        "status=approved&condition=declined",
        "response=1&response=2",
        "response=2&response=1",
        "condition=complete&condition=pending_settlement",
    ] {
        let outcome = classic_payment_outcome_from_form(body).unwrap();
        assert_eq!(
            outcome.approval_evidence,
            crate::PaymentApprovalEvidence::Structured,
            "{body}"
        );
    }
    let outcome =
        classic_payment_outcome_from_form("response=3&responsetext=Approved&response_text=Error")
            .unwrap();
    assert_eq!(
        outcome.approval_evidence,
        crate::PaymentApprovalEvidence::TextOnly
    );
    assert!(outcome.response_text.is_none());
}

#[test]
fn classic_text_only_pending_response_cannot_certify_absence() {
    for body in [
        "responsetext=Transaction+is+pending",
        "responsetext=Transaction+is+under+review",
    ] {
        let outcome = classic_payment_outcome_from_form(body).unwrap();
        assert_eq!(outcome.status, PaymentStatus::Unknown);
        assert_eq!(
            outcome.approval_evidence,
            crate::PaymentApprovalEvidence::Unclassified
        );
    }
}
