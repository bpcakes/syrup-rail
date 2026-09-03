use chrono::TimeZone;

use super::*;
use crate::{CumulativeRefundCents, CurrencyCode, GatewayReferenceValueError, PaymentAttemptId};

#[test]
fn gateway_account_mode_storage_values_round_trip_exhaustively() {
    for mode in [GatewayAccountMode::Live, GatewayAccountMode::Test] {
        assert_eq!(mode.as_str().parse::<GatewayAccountMode>(), Ok(mode));
        assert_eq!(mode.to_string(), mode.as_str());
    }
    assert!("LIVE".parse::<GatewayAccountMode>().is_err());
    assert!("unknown".parse::<GatewayAccountMode>().is_err());
    assert!("".parse::<GatewayAccountMode>().is_err());
}

#[test]
fn query_and_report_requests_reject_invalid_shapes() {
    assert!(matches!(
        GatewayQueryRequest::new(None, None),
        Err(GatewayRequestError::MissingQuerySelector)
    ));
    let start = Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap();
    assert!(matches!(
        GatewayTransactionReportRequest::new(start, start, 100, 0),
        Err(GatewayRequestError::InvalidReportWindow)
    ));
    assert!(matches!(
        GatewayTransactionReportRequest::new(start, start + Duration::minutes(1), 0, 0),
        Err(GatewayRequestError::InvalidPageSize)
    ));
}

#[test]
fn sale_request_cannot_represent_zero_money() {
    let usd = CurrencyCode::new("USD").unwrap();
    let charge = ChargeAmount::new(100, usd).unwrap();
    let attempt_id: PaymentAttemptId = "00000000-0000-0000-0000-000000000000".parse().unwrap();
    let order = GatewayOrderId::from_generated_attempt(
        "ck_order_00000000000000000000000000000000",
        attempt_id,
    )
    .unwrap();
    let token = PaymentToken::new("tok_safe").unwrap();
    let request = GatewaySaleRequest::new(
        charge,
        order,
        GatewaySaleIntent::OneTime {
            payment_token: token,
        },
        None,
    );
    assert_eq!(request.charge().cents(), 100);
}

#[test]
fn card_brand_canonicalization_never_retains_unknown_provider_text() {
    let aliases = [
        ("visa", PaymentCardBrand::Visa),
        (" VISA ", PaymentCardBrand::Visa),
        ("mastercard", PaymentCardBrand::Mastercard),
        ("master card", PaymentCardBrand::Mastercard),
        ("american express", PaymentCardBrand::AmericanExpress),
        ("amex", PaymentCardBrand::AmericanExpress),
        ("discover", PaymentCardBrand::Discover),
        ("jcb", PaymentCardBrand::Jcb),
        ("diners", PaymentCardBrand::DinersClub),
        ("diners club", PaymentCardBrand::DinersClub),
        ("dinersclub", PaymentCardBrand::DinersClub),
        ("unionpay", PaymentCardBrand::UnionPay),
        ("union pay", PaymentCardBrand::UnionPay),
        ("maestro", PaymentCardBrand::Maestro),
    ];
    for (provider_value, expected) in aliases {
        assert_eq!(
            PaymentCardBrand::from_provider(provider_value),
            Some(expected)
        );
    }
    assert_eq!(PaymentCardBrand::from_provider("   "), None);

    let unknown = PaymentCardBrand::from_provider("private-provider-sentinel").unwrap();
    assert_eq!(unknown, PaymentCardBrand::Other);
    assert_eq!(unknown.as_str(), "other");
    assert!(!format!("{unknown:?}").contains("private-provider-sentinel"));
    assert_eq!(unknown.to_string(), "[redacted]");
}

#[test]
fn descriptor_drops_invalid_display_fields() {
    let descriptor = GatewayPaymentDescriptor::from_provider_parts(
        Some(GatewayDiagnostic::new("visa")),
        Some(GatewayDiagnostic::new("brand")),
        Some("12x4"),
        Some(13),
        Some(2101),
    );
    assert_eq!(
        descriptor.canonical_card_brand(),
        Some(PaymentCardBrand::Other)
    );
    assert_eq!(
        descriptor.card_brand().map(GatewayDiagnostic::expose),
        Some("brand")
    );
    assert!(descriptor.card_last_four().is_none());
    assert_eq!(descriptor.card_exp_month(), None);
    assert_eq!(descriptor.card_exp_year(), None);
}

#[test]
fn lifecycle_admission_prevents_invalid_locator_combinations() {
    assert_eq!(
        GatewayLifecycleEvidence::new(None, None, GatewayLifecycleState::Unknown, None, None, None,),
        Err(GatewayLifecycleEvidenceError::MissingLocator)
    );
    assert!(
        GatewayLifecycleQuarantine::new(
            None,
            None,
            GatewayLifecycleQuarantineReason::MalformedReportStructure,
        )
        .is_ok()
    );
    assert_eq!(
        GatewayLifecycleQuarantine::new(
            None,
            None,
            GatewayLifecycleQuarantineReason::InvalidRefundEconomics,
        ),
        Err(GatewayLifecycleQuarantineError::MissingLocator)
    );
}

#[test]
fn only_full_lifecycle_states_derive_reversals() {
    let refunded = CumulativeRefundCents::new(100).unwrap();
    assert_eq!(
        GatewayLifecycleState::Settled {
            cumulative_refunded_cents: Some(refunded)
        }
        .full_reversal_kind(),
        None
    );
    assert_eq!(
        GatewayLifecycleState::Refunded {
            cumulative_refunded_cents: refunded
        }
        .full_reversal_kind(),
        Some(PaymentReversalKind::Refunded)
    );
}

#[test]
fn query_policy_preserves_positive_overflow_safe_limits() {
    let key = GatewayLifecycleCursorKey::new("nmi_approved_lifecycle").unwrap();
    let policy =
        GatewayLifecycleQueryPolicy::new(key, Duration::minutes(5), 100, 20, 12, 2_000).unwrap();
    assert_eq!(policy.page_size().get(), 100);
    assert_eq!(policy.narrow_window_drain_page_limit().get(), 2_000);
    assert_eq!(
        GatewayLifecycleQueryPolicy::new(
            GatewayLifecycleCursorKey::new("cursor").unwrap(),
            Duration::zero(),
            100,
            20,
            12,
            2_000,
        ),
        Err(GatewayLifecycleQueryPolicyError::NonPositiveOverlap)
    );
}

#[test]
fn sensitive_debug_output_is_value_free() {
    let identifier = GatewayTransactionId::new("txn_sentinel").unwrap();
    let debug = format!("{identifier:?}");
    assert!(!debug.contains("txn_sentinel"));
    assert!(debug.contains("redacted"));
    let evidence = ProcessorEvidence::new(
        Some(identifier),
        None,
        None,
        None,
        None,
        None,
        GatewayPaymentDescriptor::default(),
    );
    assert!(evidence.has_gateway_reference());
    assert_eq!(ProcessorEvidence::default(), ProcessorEvidence::default());
    assert!(!format!("{evidence:?}").contains("txn_sentinel"));
    assert_eq!(
        GatewayTransactionId::from_correlation("bad selector"),
        Err(GatewayReferenceValueError::UnsupportedCorrelationCharacter)
    );
}

#[test]
fn approved_payment_evidence_requires_identity_and_an_authoritative_decision() {
    let approved =
        |response: Option<&str>, response_code: Option<&str>, condition: Option<&str>| {
            ProcessorEvidence::new(
                Some(GatewayTransactionId::new("txn-approved-evidence").unwrap()),
                None,
                response.map(GatewayDiagnostic::new),
                response_code.map(GatewayDiagnostic::new),
                None,
                condition.map(GatewayDiagnostic::new),
                GatewayPaymentDescriptor::default(),
            )
        };
    assert!(approved(Some("1"), None, None).indicates_approved_payment());
    assert!(approved(None, Some("100"), None).indicates_approved_payment());
    assert!(approved(None, None, Some("complete")).indicates_approved_payment());
    assert!(!approved(Some("2"), Some("200"), Some("declined")).indicates_approved_payment());
    assert!(!approved(None, None, None).indicates_approved_payment());

    let missing_identity = ProcessorEvidence::new(
        None,
        None,
        Some(GatewayDiagnostic::new("1")),
        Some(GatewayDiagnostic::new("100")),
        Some(GatewayDiagnostic::new("Approved")),
        Some(GatewayDiagnostic::new("complete")),
        GatewayPaymentDescriptor::default(),
    );
    assert!(!missing_identity.indicates_approved_payment());

    let incomplete_but_authoritative =
        GatewayPaymentOutcome::new(GatewayPaymentStatus::Approved, ProcessorEvidence::default());
    assert!(incomplete_but_authoritative.approved_evidence().is_some());
    let non_approved = GatewayPaymentOutcome::new(
        GatewayPaymentStatus::Unknown,
        approved(Some("1"), None, None),
    );
    assert!(non_approved.approved_evidence().is_none());
}

#[test]
fn gateway_outcome_carries_only_provider_neutral_payment_diagnostics() {
    let outcome =
        GatewayPaymentOutcome::new(GatewayPaymentStatus::Unknown, ProcessorEvidence::default())
            .with_diagnostics(vec![
                GatewayPaymentDiagnostic::ProcessorReportedDuplicate,
                GatewayPaymentDiagnostic::InvalidOrConflictingTransactionIdentifier,
                GatewayPaymentDiagnostic::ProcessorReportedDuplicate,
            ]);
    assert_eq!(
        outcome.diagnostics(),
        &[
            GatewayPaymentDiagnostic::InvalidOrConflictingTransactionIdentifier,
            GatewayPaymentDiagnostic::ProcessorReportedDuplicate,
        ]
    );
    assert!(outcome.has_diagnostic(GatewayPaymentDiagnostic::ProcessorReportedDuplicate));
    assert!(!outcome.has_diagnostic(GatewayPaymentDiagnostic::MissingDecisionEvidence));
    let (status, evidence, diagnostics) = outcome.into_parts_with_diagnostics();
    assert_eq!(status, GatewayPaymentStatus::Unknown);
    assert_eq!(evidence, ProcessorEvidence::default());
    assert_eq!(
        diagnostics,
        &[
            GatewayPaymentDiagnostic::InvalidOrConflictingTransactionIdentifier,
            GatewayPaymentDiagnostic::ProcessorReportedDuplicate,
        ]
    );
    let reconstructed = GatewayPaymentOutcome::new(status, evidence).with_diagnostics(diagnostics);
    assert_eq!(reconstructed.status(), GatewayPaymentStatus::Unknown);
    assert_eq!(
        reconstructed.diagnostics(),
        &[
            GatewayPaymentDiagnostic::InvalidOrConflictingTransactionIdentifier,
            GatewayPaymentDiagnostic::ProcessorReportedDuplicate,
        ]
    );
    assert!(
        GatewayPaymentOutcome::new(GatewayPaymentStatus::Unknown, ProcessorEvidence::default())
            .diagnostics()
            .is_empty()
    );

    let approved_duplicate =
        GatewayPaymentOutcome::new(GatewayPaymentStatus::Approved, ProcessorEvidence::default())
            .with_diagnostics(vec![GatewayPaymentDiagnostic::ProcessorReportedDuplicate]);
    assert_eq!(approved_duplicate.status(), GatewayPaymentStatus::Unknown);
    assert!(approved_duplicate.approved_evidence().is_none());

    let approved_unmapped =
        GatewayPaymentOutcome::new(GatewayPaymentStatus::Approved, ProcessorEvidence::default())
            .with_diagnostics(vec![GatewayPaymentDiagnostic::UnmappedProviderDiagnostic]);
    assert_eq!(approved_unmapped.status(), GatewayPaymentStatus::Unknown);
    assert!(approved_unmapped.approved_evidence().is_none());
}

#[test]
fn gateway_outcome_approval_policy_distinguishes_identity_from_decision_certainty() {
    let policy = [
        (
            GatewayPaymentDiagnostic::MissingTransactionIdentifier,
            GatewayPaymentStatus::Approved,
        ),
        (
            GatewayPaymentDiagnostic::MissingPaymentMethodReference,
            GatewayPaymentStatus::Approved,
        ),
        (
            GatewayPaymentDiagnostic::InvalidOrConflictingTransactionIdentifier,
            GatewayPaymentStatus::Unknown,
        ),
        (
            GatewayPaymentDiagnostic::InvalidOrConflictingPaymentMethodReference,
            GatewayPaymentStatus::Unknown,
        ),
        (
            GatewayPaymentDiagnostic::InvalidOrConflictingDecisionField,
            GatewayPaymentStatus::Unknown,
        ),
        (
            GatewayPaymentDiagnostic::IndeterminatePaymentOutcome,
            GatewayPaymentStatus::Unknown,
        ),
        (
            GatewayPaymentDiagnostic::ProcessorReportedDuplicate,
            GatewayPaymentStatus::Unknown,
        ),
        (
            GatewayPaymentDiagnostic::ConflictingDecisionEvidence,
            GatewayPaymentStatus::Unknown,
        ),
        (
            GatewayPaymentDiagnostic::UnrecognizedDecisionEvidence,
            GatewayPaymentStatus::Unknown,
        ),
        (
            GatewayPaymentDiagnostic::MissingDecisionEvidence,
            GatewayPaymentStatus::Unknown,
        ),
        (
            GatewayPaymentDiagnostic::UnmappedProviderDiagnostic,
            GatewayPaymentStatus::Unknown,
        ),
    ];
    assert_eq!(
        policy.map(|(diagnostic, _)| diagnostic),
        GatewayPaymentDiagnostic::ALL
    );

    for (diagnostic, expected_status) in policy {
        let outcome = GatewayPaymentOutcome::new(
            GatewayPaymentStatus::Approved,
            ProcessorEvidence::default(),
        )
        .with_diagnostics(vec![diagnostic]);

        assert_eq!(outcome.status(), expected_status, "{diagnostic:?}");
        assert_eq!(
            outcome.approved_evidence().is_some(),
            expected_status == GatewayPaymentStatus::Approved,
            "{diagnostic:?}"
        );
    }
}

#[test]
fn gateway_outcome_quarantines_every_diagnosed_identity() {
    let evidence = || {
        ProcessorEvidence::new(
            Some(GatewayTransactionId::new("txn-diagnosed").unwrap()),
            Some(GatewayPaymentMethodReference::new("method-diagnosed").unwrap()),
            None,
            None,
            None,
            None,
            GatewayPaymentDescriptor::default(),
        )
    };

    for (diagnostic, expected_status) in [
        (
            GatewayPaymentDiagnostic::MissingTransactionIdentifier,
            GatewayPaymentStatus::Approved,
        ),
        (
            GatewayPaymentDiagnostic::InvalidOrConflictingTransactionIdentifier,
            GatewayPaymentStatus::Unknown,
        ),
    ] {
        let outcome = GatewayPaymentOutcome::new(GatewayPaymentStatus::Approved, evidence())
            .with_diagnostics(vec![diagnostic]);
        assert_eq!(outcome.status(), expected_status);
        assert!(outcome.transaction_id().is_none(), "{diagnostic:?}");
        assert!(
            outcome.payment_method_reference().is_some(),
            "{diagnostic:?}"
        );
    }

    for (diagnostic, expected_status) in [
        (
            GatewayPaymentDiagnostic::MissingPaymentMethodReference,
            GatewayPaymentStatus::Approved,
        ),
        (
            GatewayPaymentDiagnostic::InvalidOrConflictingPaymentMethodReference,
            GatewayPaymentStatus::Unknown,
        ),
    ] {
        let outcome = GatewayPaymentOutcome::new(GatewayPaymentStatus::Approved, evidence())
            .with_diagnostics(vec![diagnostic]);
        assert_eq!(outcome.status(), expected_status);
        assert!(outcome.transaction_id().is_some(), "{diagnostic:?}");
        assert!(
            outcome.payment_method_reference().is_none(),
            "{diagnostic:?}"
        );
    }

    let outcome = GatewayPaymentOutcome::new(GatewayPaymentStatus::Declined, evidence())
        .with_diagnostics(vec![
            GatewayPaymentDiagnostic::InvalidOrConflictingTransactionIdentifier,
            GatewayPaymentDiagnostic::InvalidOrConflictingPaymentMethodReference,
        ]);
    assert_eq!(outcome.status(), GatewayPaymentStatus::Declined);
    assert!(outcome.transaction_id().is_none());
    assert!(outcome.payment_method_reference().is_none());
}

#[test]
fn replacing_gateway_diagnostics_cannot_restore_a_terminal_status() {
    let outcome =
        GatewayPaymentOutcome::new(GatewayPaymentStatus::Approved, ProcessorEvidence::default())
            .with_diagnostics(vec![GatewayPaymentDiagnostic::IndeterminatePaymentOutcome]);
    assert_eq!(outcome.status(), GatewayPaymentStatus::Unknown);

    let outcome = outcome.with_diagnostics(vec![
        GatewayPaymentDiagnostic::MissingPaymentMethodReference,
    ]);
    assert_eq!(outcome.status(), GatewayPaymentStatus::Unknown);
    assert!(outcome.approved_evidence().is_none());
}

#[test]
fn gateway_diagnostic_certainty_policy_applies_to_every_status() {
    for status in [
        GatewayPaymentStatus::Approved,
        GatewayPaymentStatus::Declined,
        GatewayPaymentStatus::Unknown,
        GatewayPaymentStatus::Failed,
    ] {
        for &diagnostic in GatewayPaymentDiagnostic::ALL {
            let outcome = GatewayPaymentOutcome::new(status, ProcessorEvidence::default())
                .with_diagnostics(vec![diagnostic]);
            let expected_status = if diagnostic.requires_unknown_status()
                || status == GatewayPaymentStatus::Approved && diagnostic.prevents_approval()
            {
                GatewayPaymentStatus::Unknown
            } else {
                status
            };

            assert_eq!(
                outcome.status(),
                expected_status,
                "{status:?} + {diagnostic:?}"
            );
        }
    }
}

#[test]
fn quarantine_resolution_reason_is_normalized_bounded_and_card_safe() {
    let reason = GatewayLifecycleQuarantineResolutionReason::new("  reviewed evidence  ").unwrap();
    assert_eq!(reason.expose(), "reviewed evidence");
    assert!(!format!("{reason:?}").contains("reviewed evidence"));
    assert_eq!(
        GatewayLifecycleQuarantineResolutionReason::new("   "),
        Err(GatewayLifecycleQuarantineResolutionReasonError::Empty)
    );
    assert_eq!(
        GatewayLifecycleQuarantineResolutionReason::new("x".repeat(501)),
        Err(GatewayLifecycleQuarantineResolutionReasonError::TooLong)
    );
    assert_eq!(
        GatewayLifecycleQuarantineResolutionReason::new("card 4111111111111111"),
        Err(GatewayLifecycleQuarantineResolutionReasonError::ContainsRawCardData)
    );
}

#[test]
fn gateway_errors_preserve_value_free_debug_and_stable_messages() {
    const SENTINEL: &str = "gateway-error-detail-sentinel";
    let query_errors = [
        (
            GatewayError::RequestRejected(GatewayDiagnostic::new(SENTINEL)),
            "RequestRejected",
            "gateway rejected the request before processing",
        ),
        (
            GatewayError::Malformed(GatewayDiagnostic::new(SENTINEL)),
            "Malformed",
            "gateway response was malformed",
        ),
        (
            GatewayError::Configuration(GatewayDiagnostic::new(SENTINEL)),
            "Configuration",
            "gateway configuration is invalid",
        ),
        (
            GatewayError::Unavailable(GatewayDiagnostic::new(SENTINEL)),
            "Unavailable",
            "gateway is unavailable",
        ),
        (
            GatewayError::RateLimited(GatewayDiagnostic::new(SENTINEL)),
            "RateLimited",
            "gateway rate limit exceeded",
        ),
    ];
    for (error, variant, message) in query_errors {
        assert_eq!(error.to_string(), message);
        let debug = format!("{error:?}");
        assert!(debug.starts_with(variant));
        assert!(debug.contains("has_detail: true"));
        assert!(!debug.contains(SENTINEL));
    }

    let not_submitted = GatewayNotSubmittedError::Malformed(GatewayDiagnostic::new(SENTINEL));
    let debug = format!("{not_submitted:?}");
    assert!(debug.starts_with("Malformed"));
    assert!(debug.contains("has_detail: true"));
    assert!(!debug.contains(SENTINEL));

    let mode_mismatch = GatewayNotSubmittedError::AccountModeMismatch {
        required: GatewayAccountMode::Live,
        observed: GatewayAccountMode::Test,
        detail: GatewayDiagnostic::new(SENTINEL),
    };
    let debug = format!("{mode_mismatch:?}");
    assert!(debug.starts_with("AccountModeMismatch"));
    assert!(debug.contains("has_detail: true"));
    assert!(!debug.contains(SENTINEL));

    let mode_verification = GatewayNotSubmittedError::AccountModeVerification(
        GatewayError::Unavailable(GatewayDiagnostic::new(SENTINEL)),
    );
    let debug = format!("{mode_verification:?}");
    assert!(debug.starts_with("AccountModeVerification"));
    assert!(!debug.contains(SENTINEL));

    let mutation = GatewayMutationError::Indeterminate(GatewayDiagnostic::new(SENTINEL));
    let debug = format!("{mutation:?}");
    assert!(debug.starts_with("Indeterminate"));
    assert!(debug.contains("has_detail: true"));
    assert!(!debug.contains(SENTINEL));
}
