use chrono::TimeZone;
use uuid::Uuid;

use super::*;

fn review_attempt(
    target: crate::PaymentAttemptTarget,
    cents: i32,
    submitted_at: Option<DateTime<Utc>>,
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
                submitted_at,
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
    let submitted_at = Some(Utc.with_ymd_and_hms(2026, 8, 8, 0, 0, 0).unwrap());
    let host = || crate::PaymentAttemptTarget::HostCharge {
        target_id: HostChargeTargetId::new(Uuid::from_u128(6)),
    };
    assert!(review_required_attempt_can_be_manually_failed(
        &review_attempt(host(), 500, None, ProcessorEvidence::default())
    ));
    assert!(!review_required_attempt_can_be_manually_failed(
        &review_attempt(host(), 500, submitted_at, ProcessorEvidence::default())
    ));
    let approved = ProcessorEvidence::new(
        None,
        None,
        None,
        None,
        Some(GatewayDiagnostic::new("not approved by support")),
        None,
        crate::GatewayPaymentDescriptor::default(),
    );
    assert!(!review_required_attempt_can_be_manually_failed(
        &review_attempt(host(), 500, None, approved)
    ));

    let update_evidence = ProcessorEvidence::new(
        Some(GatewayTransactionId::new("txn-update-review").unwrap()),
        None,
        Some(GatewayDiagnostic::new("1")),
        Some(GatewayDiagnostic::new("100")),
        Some(GatewayDiagnostic::new("Processor response: Approved")),
        Some(GatewayDiagnostic::new("complete")),
        crate::GatewayPaymentDescriptor::default(),
    )
    .with_avs_response(crate::GatewayAvsResponse::parse("Y"));
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
        submitted_at,
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
    assert_eq!(
        preserved.avs_response(),
        crate::GatewayAvsResponse::parse("Y")
    );
    let response_text = preserved
        .response_text()
        .expect("manual closure note")
        .expose();
    assert!(response_text.contains("Processor response: Approved"));
    assert!(response_text.contains(PAYMENT_METHOD_UPDATE_MANUAL_CLOSURE_NOTE));

    // Even when AVS is the only structured processor evidence on a payment
    // method update, manual closure treats it as processor evidence rather than
    // asserting that no processor transaction occurred.
    let avs_only_update = review_attempt(
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
        submitted_at,
        ProcessorEvidence::new(
            None,
            None,
            None,
            None,
            Some(GatewayDiagnostic::new("AVS check returned Z")),
            None,
            crate::GatewayPaymentDescriptor::default(),
        )
        .with_avs_response(crate::GatewayAvsResponse::parse("Z")),
    );
    let avs_only_preserved = review_required_manual_failure_evidence(&avs_only_update);
    assert_eq!(
        avs_only_preserved.avs_response(),
        crate::GatewayAvsResponse::parse("Z")
    );
    assert_eq!(avs_only_preserved.condition(), None);
    let avs_only_text = avs_only_preserved
        .response_text()
        .expect("manual closure note")
        .expose();
    assert!(avs_only_text.contains("AVS check returned Z"));
    assert!(avs_only_text.contains(PAYMENT_METHOD_UPDATE_MANUAL_CLOSURE_NOTE));
}
