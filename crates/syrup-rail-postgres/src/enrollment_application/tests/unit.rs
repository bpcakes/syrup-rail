use super::*;

fn initial_attempt_for_matching(
    identity: PaymentAttemptIdentity,
    plan_key: PlanKey,
    gateway_order_id: GatewayOrderId,
    idempotency_key: &str,
) -> PaymentAttempt {
    let timestamp = Utc::now();
    let request = PaymentAttemptRequest::new(
        PaymentAttemptTarget::SubscriptionInitial {
            terms_version: syrup_rail::SubscriptionEnrollmentTermsVersion::V2,
            offer: immediate_offer(
                plan_key,
                ChargeAmount::new(1_000, CurrencyCode::new("USD").unwrap()).unwrap(),
            ),
            discount: None,
            application: None,
        },
        IdempotencyKey::new(idempotency_key).expect("test idempotency key"),
        PaymentAttemptFingerprint::new(format!("fingerprint-{idempotency_key}"))
            .expect("test fingerprint"),
        Money::new(1_000, CurrencyCode::new("USD").expect("test currency")).expect("test amount"),
        gateway_order_id,
        BillingContactSnapshot::new(None, None),
    );
    PaymentAttempt::new(
        identity,
        request,
        PaymentAttemptState::new(
            PaymentAttemptStatus::Pending,
            None,
            ProcessorEvidence::default(),
            PaymentAttemptLifecycle::default(),
            PaymentAttemptTimestamps::new(None, None, None, timestamp, timestamp),
        ),
    )
    .expect("valid test initial attempt")
}

#[test]
fn reservation_attempt_matching_preserves_initial_and_exact_rules() {
    assert_eq!(
        ReservationOperation::Initial.expected_kind(),
        PaymentAttemptKind::SubscriptionInitial
    );
    assert_eq!(
        ReservationOperation::Recovery.expected_kind(),
        PaymentAttemptKind::SubscriptionRecovery
    );
    assert_eq!(
        ReservationOperation::Renewal.expected_kind(),
        PaymentAttemptKind::SubscriptionRenewal
    );
    assert_eq!(
        ReservationOperation::PaymentMethodReplacement.expected_kind(),
        PaymentAttemptKind::SubscriptionPaymentMethodUpdate
    );
    let identity = PaymentAttemptIdentity::new(
        PaymentAttemptId::new(Uuid::from_u128(1)),
        BillingScopeId::new(Uuid::from_u128(2)),
        SubscriberId::new(Uuid::from_u128(3)),
        GatewayAccountId::new(Uuid::from_u128(4)),
        GatewayConfigurationId::new(Uuid::from_u128(5)),
    );
    let plan_key = PlanKey::new("base_subscription").expect("test plan key");
    let gateway_order_id =
        GatewayOrderId::from_correlation("matching-order").expect("test gateway order");
    let original = initial_attempt_for_matching(
        identity,
        plan_key.clone(),
        gateway_order_id.clone(),
        "matching-key-one",
    );
    let changed_request = initial_attempt_for_matching(
        identity,
        plan_key.clone(),
        gateway_order_id.clone(),
        "matching-key-two",
    );
    let different_order = initial_attempt_for_matching(
        identity,
        plan_key.clone(),
        GatewayOrderId::from_correlation("other-order").expect("test gateway order"),
        "matching-key-three",
    );

    let initial = ReservationAttemptExpectation::Initial {
        identity,
        plan_key: &plan_key,
        gateway_order_id: &gateway_order_id,
    };
    assert!(initial.matches(&original));
    assert!(initial.matches(&changed_request));
    assert!(!initial.matches(&different_order));

    let exact = ReservationAttemptExpectation::Exact {
        identity,
        kind: PaymentAttemptKind::SubscriptionInitial,
        request: original.request(),
    };
    assert!(exact.matches(&original));
    assert!(!exact.matches(&changed_request));
}

#[test]
fn resolution_command_keeps_boundaries_and_replacement_review_typed() {
    let prepared = OutcomeResolutionCommand::non_approved(
        AttemptResolutionStatus::Failed,
        None,
        Some(RateLimitCooldown::Account),
        OutcomeResolutionBoundary::Prepared,
    );
    assert!(prepared.may_resolve(PaymentAttemptStatus::Pending, false));
    assert!(!prepared.may_resolve(PaymentAttemptStatus::Pending, true));
    assert!(!prepared.may_resolve(PaymentAttemptStatus::Declined, false));
    assert!(!prepared.clears_submitted_at());

    let admitted = OutcomeResolutionCommand::non_approved(
        AttemptResolutionStatus::Failed,
        None,
        None,
        OutcomeResolutionBoundary::AdmittedNotSubmitted,
    );
    assert!(!admitted.may_resolve(PaymentAttemptStatus::Pending, false));
    assert!(admitted.may_resolve(PaymentAttemptStatus::Pending, true));
    assert!(admitted.clears_submitted_at());

    let unknown = OutcomeResolutionCommand::unknown(Some(RateLimitCooldown::Provider));
    assert_eq!(
        unknown.resolved_status(
            ReservationOperation::PaymentMethodReplacement,
            PaymentAttemptStatus::ReviewRequired,
        ),
        AttemptResolutionStatus::ReviewRequired,
    );
    assert_eq!(
        unknown.resolved_status(
            ReservationOperation::Recovery,
            PaymentAttemptStatus::ReviewRequired,
        ),
        AttemptResolutionStatus::Unknown,
    );
    assert!(unknown.records_pending_evidence(AttemptResolutionStatus::Unknown));
    assert!(!unknown.records_pending_evidence(AttemptResolutionStatus::ReviewRequired));
}

#[test]
fn approved_parking_policies_cover_every_operation_without_drift() {
    let aggregate_operations = [
        ReservationOperation::Initial,
        ReservationOperation::Recovery,
        ReservationOperation::Renewal,
    ];
    for operation in aggregate_operations {
        assert_eq!(
            operation.approved_parking_lock_scope(),
            ApprovedParkingLockScope::SubscriptionAggregate,
        );
    }
    assert_eq!(
        ReservationOperation::PaymentMethodReplacement.approved_parking_lock_scope(),
        ApprovedParkingLockScope::AttemptOnly,
    );
    assert!(ReservationOperation::Initial.has_lock_free_approved_evidence_fallback());
    assert!(ReservationOperation::Recovery.has_lock_free_approved_evidence_fallback());
    assert!(!ReservationOperation::Renewal.has_lock_free_approved_evidence_fallback());
    assert!(
        ReservationOperation::PaymentMethodReplacement.has_lock_free_approved_evidence_fallback()
    );

    let attempt = initial_attempt_for_matching(
        PaymentAttemptIdentity::new(
            PaymentAttemptId::new(Uuid::from_u128(10)),
            BillingScopeId::new(Uuid::from_u128(11)),
            SubscriberId::new(Uuid::from_u128(12)),
            GatewayAccountId::new(Uuid::from_u128(13)),
            GatewayConfigurationId::new(Uuid::from_u128(14)),
        ),
        PlanKey::new("base_subscription").unwrap(),
        GatewayOrderId::from_correlation("parking-policy-order").unwrap(),
        "parking-policy-key",
    );
    let charged = ProcessorEvidence::new(
        Some(GatewayTransactionId::new("charged-transaction").unwrap()),
        None,
        None,
        None,
        None,
        None,
        GatewayPaymentDescriptor::default(),
    );
    for operation in aggregate_operations {
        assert_eq!(
            operation.terminal_approved_progression(&attempt, &charged),
            ProcessorChargeProgression::ExternalReversalRequired,
        );
        assert_eq!(
            operation.terminal_approved_progression(&attempt, &ProcessorEvidence::default()),
            ProcessorChargeProgression::ReconciliationRequired,
        );
    }
    assert_eq!(
        ReservationOperation::PaymentMethodReplacement
            .terminal_approved_progression(&attempt, &charged),
        ProcessorChargeProgression::ReconciliationRequired,
    );
}
