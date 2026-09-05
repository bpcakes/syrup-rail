use chrono::TimeZone;
use uuid::Uuid;

use super::*;

fn review_attempt(
    target: crate::PaymentAttemptTarget,
    cents: i32,
    submitted: bool,
    evidence: ProcessorEvidence,
) -> PaymentAttempt {
    let created_at = Utc.with_ymd_and_hms(2026, 8, 8, 0, 0, 0).unwrap();
    PaymentAttempt::new(
        crate::PaymentAttemptIdentity::new(
            PaymentAttemptId::new(Uuid::from_u128(1)),
            BillingScopeId::new(Uuid::from_u128(2)),
            SubscriberId::new(Uuid::from_u128(3)),
            GatewayAccountId::new(Uuid::from_u128(4)),
            GatewayConfigurationId::new(Uuid::from_u128(5)),
            crate::GatewayAccountMode::Live,
        ),
        crate::PaymentAttemptRequest::new(
            target,
            crate::IdempotencyKey::new("manual-review").unwrap(),
            crate::PaymentAttemptFingerprint::new("manual-review-fingerprint").unwrap(),
            crate::Money::new(cents, crate::CurrencyCode::new("USD").unwrap()).unwrap(),
            GatewayOrderId::from_correlation("manual-review-order").unwrap(),
            crate::BillingContactSnapshot::new(None, None),
        ),
        crate::PaymentAttemptState::new(
            PaymentAttemptStatus::ReviewRequired,
            None,
            evidence,
            crate::PaymentAttemptLifecycle::default(),
            crate::PaymentAttemptTimestamps::new(
                submitted.then_some(created_at),
                None,
                Some(created_at),
                created_at,
                created_at,
            ),
        ),
    )
    .unwrap()
}

#[test]
fn external_reversal_reason_is_normalized_bounded_and_card_safe() {
    assert!(ExternalReversalReason::new(format!("  {}  ", "é".repeat(500))).is_ok());
    assert_eq!(
        ExternalReversalReason::new("é".repeat(501)),
        Err(ExternalReversalReasonError::TooLong)
    );
    assert_eq!(
        ExternalReversalReason::new(format!("{}4111111111111111", "x".repeat(500))),
        Err(ExternalReversalReasonError::TooLong)
    );
    let reason = ExternalReversalReason::new("  processor refund verified  ").unwrap();
    assert_eq!(reason.expose(), "processor refund verified");
    assert!(!format!("{reason:?}").contains("processor refund verified"));
    assert_eq!(
        ExternalReversalReason::new(" "),
        Err(ExternalReversalReasonError::Empty)
    );
    assert_eq!(
        ExternalReversalReason::new("x".repeat(501)),
        Err(ExternalReversalReasonError::TooLong)
    );
    assert_eq!(
        ExternalReversalReason::new("card 4111111111111111"),
        Err(ExternalReversalReasonError::ContainsRawCardData)
    );
}

#[test]
fn external_reversal_resolution_exhaustively_closes_prior_and_outcome_labels() {
    let priors = [
        ExternalReversalPriorClassification::SubscriptionInitialCurrentGrantConflict,
        ExternalReversalPriorClassification::ProcessorChargeExternalReversalRequired,
    ];
    assert_eq!(ExternalReversalOutcome::ALL.len(), 4);

    for prior in priors {
        for outcome in ExternalReversalOutcome::ALL {
            let resolution = ExternalReversalResolution::new(prior, *outcome);
            let is_invalid = prior
                == ExternalReversalPriorClassification::SubscriptionInitialCurrentGrantConflict
                && matches!(
                    *outcome,
                    ExternalReversalOutcome::ProcessorChargeRefunded
                        | ExternalReversalOutcome::ProcessorChargeVoided
                );
            if is_invalid {
                assert_eq!(
                    resolution,
                    Err(ExternalReversalResolutionError::IncompatiblePriorOutcome)
                );
                continue;
            }

            let resolution = resolution.expect("the remaining tuple labels are valid");
            assert_eq!(resolution.prior(), prior);
            assert_eq!(resolution.outcome(), *outcome);
            assert_eq!(resolution.prior_resolution_code(), prior.resolution_code());
            assert_eq!(resolution.kind(), outcome.kind());
            assert_eq!(
                resolution.final_resolution_code(),
                outcome.final_resolution_code()
            );
            assert_eq!(
                ExternalReversalOutcome::from_kind_and_final_resolution_code(
                    resolution.kind(),
                    resolution.final_resolution_code(),
                ),
                Ok(*outcome)
            );
        }
    }

    assert_eq!(
        ExternalReversalPriorClassification::from_resolution_code("unexpected"),
        Err(ExternalReversalResolutionError::InvalidPriorClassification)
    );
    assert_eq!(
        ExternalReversalOutcome::from_kind_and_final_resolution_code(
            ExternalReversalKind::Refund,
            PaymentResolutionCode::SubscriptionInitialExternallyVoided,
        ),
        Err(ExternalReversalResolutionError::InvalidOutcome)
    );
}

#[test]
fn external_reversal_legacy_parts_are_fallible() {
    let attestation = ExternalReversalAttestation::from_legacy_parts(
        ProcessorChargeId::new(Uuid::from_u128(10)),
        PaymentAttemptId::new(Uuid::from_u128(11)),
        ActorId::new(Uuid::from_u128(12)),
        ExternalReversalKind::Refund,
        ExternalReversalReason::new("processor refund verified").unwrap(),
        "processor_charge_external_reversal_required",
        PaymentResolutionCode::ProcessorChargeExternallyRefunded,
        GatewayAccountId::new(Uuid::from_u128(13)),
        GatewayConfigurationId::new(Uuid::from_u128(14)),
        GatewayOrderId::from_correlation("legacy-reversal-order").unwrap(),
        ChargeAmount::new(500, crate::CurrencyCode::new("USD").unwrap()).unwrap(),
        GatewayTransactionId::new("txn-legacy-reversal").unwrap(),
        ProcessorEvidence::default(),
        Utc.with_ymd_and_hms(2026, 8, 23, 0, 0, 0).unwrap(),
    )
    .unwrap();

    assert_eq!(
        attestation.resolution(),
        ExternalReversalResolution::new(
            ExternalReversalPriorClassification::ProcessorChargeExternalReversalRequired,
            ExternalReversalOutcome::ProcessorChargeRefunded,
        )
        .unwrap()
    );
    assert_eq!(attestation.kind(), ExternalReversalKind::Refund);
    assert_eq!(
        attestation.final_resolution_code(),
        PaymentResolutionCode::ProcessorChargeExternallyRefunded
    );

    assert_eq!(
        ExternalReversalAttestation::from_legacy_parts(
            ProcessorChargeId::new(Uuid::from_u128(10)),
            PaymentAttemptId::new(Uuid::from_u128(11)),
            ActorId::new(Uuid::from_u128(12)),
            ExternalReversalKind::Refund,
            ExternalReversalReason::new("processor refund verified").unwrap(),
            "subscription_initial_current_grant_conflict",
            PaymentResolutionCode::ProcessorChargeExternallyRefunded,
            GatewayAccountId::new(Uuid::from_u128(13)),
            GatewayConfigurationId::new(Uuid::from_u128(14)),
            GatewayOrderId::from_correlation("legacy-reversal-order").unwrap(),
            ChargeAmount::new(500, crate::CurrencyCode::new("USD").unwrap()).unwrap(),
            GatewayTransactionId::new("txn-legacy-reversal").unwrap(),
            ProcessorEvidence::default(),
            Utc.with_ymd_and_hms(2026, 8, 23, 0, 0, 0).unwrap(),
        ),
        Err(ExternalReversalResolutionError::IncompatiblePriorOutcome)
    );
}

#[test]
fn processor_charge_state_codes_cover_charge_specific_workflow_states() {
    assert_eq!(
        ProcessorChargeStateCode::AdditionalApprovedChargeIdentified.as_str(),
        "additional_approved_charge_identified"
    );
    assert_eq!(
        ProcessorChargeStateCode::TransactionIdentityRequired.as_str(),
        "processor_charge_transaction_identity_required"
    );
    assert_eq!(
        ProcessorChargeStateCode::ApprovedChargeWaitingForApplication.as_str(),
        "approved_charge_waiting_for_application"
    );
    assert_eq!(
        ProcessorChargeStateCode::ZeroAmountAdditionalApprovedCharge.as_str(),
        "zero_amount_additional_approved_charge"
    );
}

#[test]
fn operator_review_page_limit_is_closed_and_bounded() {
    assert_eq!(OperatorReviewPageLimit::new(1).unwrap().get(), 1);
    assert_eq!(
        OperatorReviewPageLimit::new(OPERATOR_REVIEW_PAGE_LIMIT)
            .unwrap()
            .get(),
        OPERATOR_REVIEW_PAGE_LIMIT
    );
    assert_eq!(
        OperatorReviewPageLimit::new(0),
        Err(OperatorReviewPageLimitError)
    );
    assert_eq!(
        OperatorReviewPageLimit::new(OPERATOR_REVIEW_PAGE_LIMIT + 1),
        Err(OperatorReviewPageLimitError)
    );
}

#[test]
fn manual_failure_policy_refuses_charge_risk_and_preserves_update_evidence() {
    let host = || crate::PaymentAttemptTarget::HostCharge {
        target_id: HostChargeTargetId::new(Uuid::from_u128(6)),
    };
    assert!(review_required_attempt_can_be_manually_failed(
        &review_attempt(host(), 500, false, ProcessorEvidence::default())
    ));
    assert!(!review_required_attempt_can_be_manually_failed(
        &review_attempt(host(), 500, true, ProcessorEvidence::default())
    ));
    let approved = ProcessorEvidence::new(
        crate::ProcessorApprovalEvidence::Unclassified,
        None,
        None,
        None,
        None,
        Some(GatewayDiagnostic::new("not approved by support")),
        None,
        crate::GatewayPaymentDescriptor::default(),
    );
    assert!(!review_required_attempt_can_be_manually_failed(
        &review_attempt(host(), 500, false, approved)
    ));

    let update_evidence = ProcessorEvidence::new(
        crate::ProcessorApprovalEvidence::Unclassified,
        Some(GatewayTransactionId::new("txn-update-review").unwrap()),
        None,
        Some(GatewayDiagnostic::new("1")),
        Some(GatewayDiagnostic::new("100")),
        Some(GatewayDiagnostic::new("Processor response: Approved")),
        Some(GatewayDiagnostic::new("complete")),
        crate::GatewayPaymentDescriptor::default(),
    );
    let update = review_attempt(
        crate::PaymentAttemptTarget::SubscriptionPaymentMethodUpdate {
            plan_key: crate::PlanKey::new("test_plan").unwrap(),
            payment_method_id: crate::PaymentMethodId::new(Uuid::from_u128(7)),
            expected_state: crate::PaymentMethodUpdateSnapshot::new(
                crate::SubscriptionId::new(Uuid::from_u128(8)),
                crate::PaymentMethodId::new(Uuid::from_u128(7)),
                GatewayTransactionId::new("txn-initial").unwrap(),
            ),
        },
        0,
        true,
        update_evidence,
    );
    assert!(review_required_attempt_can_be_manually_failed(&update));
    let preserved = review_required_manual_failure_evidence(&update);
    assert_eq!(
        preserved.transaction_id().map(GatewayTransactionId::expose),
        Some("txn-update-review")
    );
    assert_eq!(
        preserved.condition().map(GatewayDiagnostic::expose),
        Some("complete")
    );
    let response_text = preserved
        .response_text()
        .expect("manual closure note")
        .expose();
    assert!(response_text.contains("Processor response: Approved"));
    assert!(response_text.contains(PAYMENT_METHOD_UPDATE_MANUAL_CLOSURE_NOTE));

    let local_note = ProcessorEvidence::new(
        crate::ProcessorApprovalEvidence::Absent,
        None,
        None,
        None,
        None,
        Some(GatewayDiagnostic::new(
            "Exact gateway query found no transaction.",
        )),
        None,
        crate::GatewayPaymentDescriptor::default(),
    );
    let update_with_local_note = review_attempt(
        crate::PaymentAttemptTarget::SubscriptionPaymentMethodUpdate {
            plan_key: crate::PlanKey::new("test_plan").unwrap(),
            payment_method_id: crate::PaymentMethodId::new(Uuid::from_u128(9)),
            expected_state: crate::PaymentMethodUpdateSnapshot::new(
                crate::SubscriptionId::new(Uuid::from_u128(10)),
                crate::PaymentMethodId::new(Uuid::from_u128(9)),
                GatewayTransactionId::new("txn-current-method").unwrap(),
            ),
        },
        0,
        true,
        local_note,
    );
    assert!(review_required_attempt_can_be_manually_failed(
        &update_with_local_note
    ));
    let preserved = review_required_manual_failure_evidence(&update_with_local_note);
    assert_eq!(
        preserved.approval_evidence(),
        crate::ProcessorApprovalEvidence::Absent
    );
    let response_text = preserved
        .response_text()
        .expect("retained local note and closure note")
        .expose();
    assert!(response_text.contains("Exact gateway query found no transaction."));
    assert!(response_text.contains(PAYMENT_METHOD_UPDATE_MANUAL_CLOSURE_NOTE));
}

#[test]
fn payment_method_update_closure_marker_survives_bounded_retained_evidence() {
    for existing in ["x".repeat(crate::MAX_GATEWAY_TEXT_BYTES), "é".repeat(256)] {
        let response = payment_method_update_manual_failure_response_text(Some(&existing));
        assert!(
            response
                .expose()
                .starts_with(PAYMENT_METHOD_UPDATE_MANUAL_CLOSURE_NOTE)
        );
        assert!(response.expose().len() <= crate::MAX_GATEWAY_TEXT_BYTES);
        assert!(response.expose().is_char_boundary(response.expose().len()));
    }
}
