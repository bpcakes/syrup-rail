use chrono::TimeZone;
use uuid::Uuid;

use super::*;
use crate::{
    ChargeAmount, CumulativeRefundCents, CurrencyCode, DiscountClaimId, DiscountCodeId,
    DunningExhaustion, DunningSchedule, GatewayPaymentDescriptor, GatewayPaymentMethodReference,
    LimitedDiscountMonths, PastDueAccessPolicy, PercentOffBasisPoints, RecurringSubscriptionTerms,
    RenewalFailurePolicy, SubscriptionDiscountCode, SubscriptionDiscountSnapshot,
    SubscriptionPeriodRule, SubscriptionStart,
};

fn subscription(value: u128) -> SubscriptionId {
    SubscriptionId::new(Uuid::from_u128(value))
}

fn method(value: u128) -> PaymentMethodId {
    PaymentMethodId::new(Uuid::from_u128(value))
}

fn target(value: u128) -> HostChargeTargetId {
    HostChargeTargetId::new(Uuid::from_u128(value))
}

fn identity() -> PaymentAttemptIdentity {
    PaymentAttemptIdentity::new(
        PaymentAttemptId::new(Uuid::from_u128(10)),
        BillingScopeId::new(Uuid::from_u128(11)),
        SubscriberId::new(Uuid::from_u128(12)),
        GatewayAccountId::new(Uuid::from_u128(13)),
        GatewayConfigurationId::new(Uuid::from_u128(14)),
    )
}

fn request(target: PaymentAttemptTarget, cents: i32) -> PaymentAttemptRequest {
    PaymentAttemptRequest::new(
        target,
        IdempotencyKey::new("idempotency-secret").unwrap(),
        PaymentAttemptFingerprint::new("fingerprint-secret").unwrap(),
        Money::new(cents, CurrencyCode::new("USD").unwrap()).unwrap(),
        GatewayOrderId::from_correlation("order-secret").unwrap(),
        BillingContactSnapshot::new(
            Some("Sensitive Name".to_owned()),
            Some("secret@example.test".to_owned()),
        ),
    )
}

fn instant(second: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, second).unwrap()
}

fn state(
    status: PaymentAttemptStatus,
    resolved_at: Option<DateTime<Utc>>,
    review_required_at: Option<DateTime<Utc>>,
) -> PaymentAttemptState {
    PaymentAttemptState::new(
        status,
        None,
        ProcessorEvidence::default(),
        PaymentAttemptLifecycle::default(),
        PaymentAttemptTimestamps::new(
            None,
            resolved_at,
            review_required_at,
            instant(0),
            instant(1),
        ),
    )
}

fn initial_target(application: Option<SubscriptionInitialApplication>) -> PaymentAttemptTarget {
    PaymentAttemptTarget::SubscriptionInitial {
        terms_version: SubscriptionEnrollmentTermsVersion::V2,
        offer: SubscriptionOffer::new(
            PlanKey::new("basic").unwrap(),
            RecurringSubscriptionTerms::new(
                ChargeAmount::new(1_000, CurrencyCode::new("USD").unwrap()).unwrap(),
                SubscriptionPeriodRule::calendar_months(1).unwrap(),
            ),
            SubscriptionStart::RecurringImmediately,
            RenewalFailurePolicy::new(
                DunningSchedule::default(),
                DunningExhaustion::RemainPastDue,
                PastDueAccessPolicy::SuspendImmediately,
            ),
        )
        .unwrap(),
        discount: None,
        application,
    }
}

fn enrollment_discount() -> SubscriptionEnrollmentDiscountSnapshot {
    let usd = CurrencyCode::new("USD").unwrap();
    SubscriptionEnrollmentDiscountSnapshot::new(
        DiscountClaimId::new(Uuid::from_u128(1)),
        DiscountCodeId::new(Uuid::from_u128(2)),
        SubscriptionDiscountSnapshot::new(
            SubscriptionDiscountCode::new("SAVE20").unwrap(),
            Some("characterization".to_owned()),
            SubscriptionDiscountKind::PercentOffBasisPoints(
                PercentOffBasisPoints::new(2_000).unwrap(),
            ),
            SubscriptionDiscountDuration::LimitedMonths(LimitedDiscountMonths::new(3).unwrap()),
            ChargeAmount::new(1_000, usd).unwrap(),
            ChargeAmount::new(800, usd).unwrap(),
        )
        .unwrap(),
    )
}

fn canonical_fingerprint(target: PaymentAttemptTarget, cents: i32) -> String {
    PaymentAttemptRequest::canonical(
        target,
        IdempotencyKey::new("canonical-key").unwrap(),
        Money::new(cents, CurrencyCode::new("USD").unwrap()).unwrap(),
        GatewayOrderId::from_correlation("canonical-order").unwrap(),
        BillingContactSnapshot::new(None, None),
    )
    .fingerprint()
    .expose()
    .to_owned()
}

#[test]
fn fingerprints_are_nonempty_and_value_safe_to_format() {
    assert_eq!(
        PaymentAttemptFingerprint::new(" "),
        Err(PaymentAttemptFingerprintError::Empty),
    );
    let fingerprint = PaymentAttemptFingerprint::new("secret:economics").unwrap();
    assert_eq!(fingerprint.expose(), "secret:economics");
    assert!(!format!("{fingerprint:?}").contains("secret:economics"));
    assert_eq!(fingerprint.to_string(), "[redacted]");
}

#[test]
fn canonical_request_fingerprints_cover_every_kind_version_and_discount_shape() {
    let offer = match initial_target(None) {
        PaymentAttemptTarget::SubscriptionInitial { offer, .. } => offer,
        _ => unreachable!(),
    };
    let initial = |terms_version, discount| PaymentAttemptTarget::SubscriptionInitial {
        terms_version,
        offer: offer.clone(),
        discount,
        application: None,
    };
    let period = BillingPeriod::new(instant(0), instant(1)).unwrap();
    let expected_state = SubscriptionPaymentStateSnapshot::new(
        subscription(1),
        method(2),
        GatewayTransactionId::new("txn-initial").unwrap(),
        SubscriptionStatus::Active,
    )
    .unwrap();
    let update_state = PaymentMethodUpdateSnapshot::new(
        subscription(1),
        method(2),
        GatewayTransactionId::new("txn-initial").unwrap(),
    );

    let vectors = [
        (
            PaymentAttemptTarget::HostCharge {
                target_id: target(20),
            },
            1_000,
            "host_charge:00000000-0000-0000-0000-000000000014:1000:USD",
        ),
        (
            initial(SubscriptionEnrollmentTermsVersion::V1, None),
            1_000,
            "subscription_initial:basic:1000:USD:discount:none:expected:full_price:1000:USD",
        ),
        (
            initial(
                SubscriptionEnrollmentTermsVersion::V1,
                Some(enrollment_discount()),
            ),
            800,
            "subscription_initial:basic:800:USD:discount:00000000-0000-0000-0000-000000000001:00000000-0000-0000-0000-000000000002:SAVE20:percent_off:none:2000:USD:1000:800:limited_months:3:expected:discounted:SAVE20:percent_off:none:2000:limited_months:3:USD:1000:800",
        ),
        (
            initial(SubscriptionEnrollmentTermsVersion::V2, None),
            1_000,
            "subscription_initial:v2:basic:start:recurring_immediately:trial:none:recurring:1000:USD:calendar_months:1:dunning:[]:remain_past_due:suspend_immediately:initial:1000:USD:discount:none",
        ),
        (
            initial(
                SubscriptionEnrollmentTermsVersion::V2,
                Some(enrollment_discount()),
            ),
            800,
            "subscription_initial:v2:basic:start:recurring_immediately:trial:none:recurring:1000:USD:calendar_months:1:dunning:[]:remain_past_due:suspend_immediately:initial:800:USD:discount:00000000-0000-0000-0000-000000000001:00000000-0000-0000-0000-000000000002:SAVE20:percent_off:none:2000:USD:1000:800:limited_months:3",
        ),
        (
            PaymentAttemptTarget::SubscriptionRenewal {
                plan_key: PlanKey::new("basic").unwrap(),
                payment_method_id: method(3),
                period: period.clone(),
                expected_state: expected_state.clone(),
            },
            1_000,
            "subscription_renewal:basic:00000000-0000-0000-0000-000000000001:00000000-0000-0000-0000-000000000002:2026-08-01 00:00:00 UTC:1000:USD",
        ),
        (
            PaymentAttemptTarget::SubscriptionRecovery {
                plan_key: PlanKey::new("basic").unwrap(),
                payment_method_id: method(3),
                period,
                expected_state,
            },
            1_000,
            "subscription_recovery:basic:00000000-0000-0000-0000-000000000001:00000000-0000-0000-0000-000000000002:2026-08-01 00:00:00 UTC:1000:USD",
        ),
        (
            PaymentAttemptTarget::SubscriptionPaymentMethodUpdate {
                plan_key: PlanKey::new("basic").unwrap(),
                payment_method_id: method(3),
                expected_state: update_state,
            },
            0,
            "subscription_payment_method_update:basic:00000000-0000-0000-0000-000000000001:00000000-0000-0000-0000-000000000002:txn-initial",
        ),
    ];

    for (target, cents, expected) in vectors {
        assert_eq!(canonical_fingerprint(target, cents), expected);
    }
}

#[test]
fn canonical_request_uses_expected_not_related_payment_method_identity() {
    let expected_method = method(2);
    let related_method = method(3);
    let period = BillingPeriod::new(instant(0), instant(1)).unwrap();
    let target = PaymentAttemptTarget::SubscriptionRenewal {
        plan_key: PlanKey::new("basic").unwrap(),
        payment_method_id: related_method,
        period,
        expected_state: SubscriptionPaymentStateSnapshot::new(
            subscription(1),
            expected_method,
            GatewayTransactionId::new("txn-initial").unwrap(),
            SubscriptionStatus::Active,
        )
        .unwrap(),
    };
    let fingerprint = canonical_fingerprint(target, 1_000);
    assert!(fingerprint.contains(&expected_method.to_string()));
    assert!(!fingerprint.contains(&related_method.to_string()));
}

#[test]
fn persisted_request_rehydration_preserves_opaque_legacy_fingerprint_bytes() {
    let legacy = PaymentAttemptFingerprint::new("legacy:v0:opaque/\u{df}").unwrap();
    let request = PaymentAttemptRequest::from_persisted_parts(
        PaymentAttemptTarget::HostCharge {
            target_id: target(20),
        },
        IdempotencyKey::new("legacy-key").unwrap(),
        legacy.clone(),
        Money::new(1_000, CurrencyCode::new("USD").unwrap()).unwrap(),
        GatewayOrderId::from_correlation("legacy-order").unwrap(),
        BillingContactSnapshot::new(None, None),
    );

    assert_eq!(request.fingerprint(), &legacy);
    assert_ne!(
        request.fingerprint().expose(),
        canonical_fingerprint(request.target().clone(), 1_000)
    );
}

#[test]
fn billing_contact_snapshot_preserves_structure_while_deriving_display_name() {
    let first = BillingContact::new(
        Some("Mary Ann".to_owned()),
        Some("Smith".to_owned()),
        Some("mary@example.test".to_owned()),
    )
    .unwrap();
    let second = BillingContact::new(
        Some("Mary".to_owned()),
        Some("Ann Smith".to_owned()),
        Some("mary@example.test".to_owned()),
    )
    .unwrap();

    let first = BillingContactSnapshot::from_billing_contact(&first);
    let second = BillingContactSnapshot::from_billing_contact(&second);

    assert_eq!(first.name(), Some("Mary Ann Smith"));
    assert_eq!(second.name(), Some("Mary Ann Smith"));
    assert_eq!(first.first_name(), Some("Mary Ann"));
    assert_eq!(first.last_name(), Some("Smith"));
    assert_eq!(second.first_name(), Some("Mary"));
    assert_eq!(second.last_name(), Some("Ann Smith"));
    assert_ne!(first, second);
    assert!(!format!("{first:?}").contains("Mary"));
}

#[test]
fn payment_state_snapshots_are_typed_and_redact_transaction_identity() {
    let transaction = GatewayTransactionId::new("txn-secret").unwrap();
    let update = PaymentMethodUpdateSnapshot::new(subscription(1), method(2), transaction.clone());
    let state = SubscriptionPaymentStateSnapshot::new(
        subscription(1),
        method(2),
        transaction,
        SubscriptionStatus::PastDue,
    )
    .unwrap();
    assert_eq!(state.status(), SubscriptionStatus::PastDue);
    assert!(!format!("{update:?}").contains("txn-secret"));
    assert!(!format!("{state:?}").contains("txn-secret"));
    assert_eq!(
        SubscriptionPaymentStateSnapshot::new(
            subscription(1),
            method(2),
            GatewayTransactionId::new("txn").unwrap(),
            SubscriptionStatus::Canceled,
        ),
        Err(PaymentAttemptSnapshotError::TerminalSubscription),
    );
    assert_eq!(
        SubscriptionPaymentStateSnapshot::new(
            subscription(1),
            method(2),
            GatewayTransactionId::new("txn").unwrap(),
            SubscriptionStatus::Unpaid,
        ),
        Err(PaymentAttemptSnapshotError::TerminalSubscription),
    );
}

#[test]
fn attempt_timestamps_choose_submission_as_the_economic_boundary() {
    let created = Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap();
    let submitted = Utc.with_ymd_and_hms(2026, 8, 1, 0, 1, 0).unwrap();
    assert_eq!(
        PaymentAttemptTimestamps::new(Some(submitted), None, None, created, submitted,)
            .submitted_or_created_at(),
        submitted,
    );
    assert_eq!(
        PaymentAttemptTimestamps::new(None, None, None, created, created).submitted_or_created_at(),
        created,
    );
}

#[test]
fn amount_shape_follows_attempt_kind() {
    assert_eq!(
        PaymentAttempt::new(
            identity(),
            request(
                PaymentAttemptTarget::HostCharge {
                    target_id: target(20),
                },
                0,
            ),
            state(PaymentAttemptStatus::Pending, None, None),
        ),
        Err(PaymentAttemptError::ZeroAmountRequiresPaymentMethodUpdate),
    );

    let update = PaymentAttemptTarget::SubscriptionPaymentMethodUpdate {
        plan_key: PlanKey::new("basic").unwrap(),
        payment_method_id: method(22),
        expected_state: PaymentMethodUpdateSnapshot::new(
            subscription(21),
            method(22),
            GatewayTransactionId::new("txn-initial").unwrap(),
        ),
    };
    assert_eq!(
        PaymentAttempt::new(
            identity(),
            request(update.clone(), 1),
            state(PaymentAttemptStatus::Pending, None, None),
        ),
        Err(PaymentAttemptError::PaymentMethodUpdateRequiresZeroAmount),
    );
    let accepted = PaymentAttempt::new(
        identity(),
        request(update, 0),
        state(PaymentAttemptStatus::Pending, None, None),
    )
    .unwrap();
    assert_eq!(
        accepted.kind(),
        PaymentAttemptKind::SubscriptionPaymentMethodUpdate
    );
}

#[test]
fn initial_application_identity_follows_resolution_state() {
    let application = SubscriptionInitialApplication::new(Some(subscription(30)), method(31));
    assert_eq!(
        PaymentAttempt::new(
            identity(),
            request(initial_target(Some(application)), 1_000),
            state(PaymentAttemptStatus::Pending, None, None),
        ),
        Err(PaymentAttemptError::InitialApplicationBeforeResolution),
    );
    assert_eq!(
        PaymentAttempt::new(
            identity(),
            request(initial_target(None), 1_000),
            state(PaymentAttemptStatus::Approved, Some(instant(2)), None),
        ),
        Err(PaymentAttemptError::ApprovedInitialMissingApplication),
    );
    let approved = PaymentAttempt::new(
        identity(),
        request(initial_target(Some(application)), 1_000),
        state(PaymentAttemptStatus::Approved, Some(instant(2)), None),
    )
    .unwrap();
    assert_eq!(
        approved.request().target().subscription_id(),
        Some(subscription(30))
    );
    assert_eq!(
        approved.request().target().payment_method_id(),
        Some(method(31))
    );
}

#[test]
fn terminal_and_review_states_require_their_durable_boundaries() {
    let host = || PaymentAttemptTarget::HostCharge {
        target_id: target(40),
    };
    assert_eq!(
        PaymentAttempt::new(
            identity(),
            request(host(), 1_000),
            state(PaymentAttemptStatus::Failed, None, None),
        ),
        Err(PaymentAttemptError::TerminalAttemptMissingResolvedAt),
    );
    assert_eq!(
        PaymentAttempt::new(
            identity(),
            request(host(), 1_000),
            state(PaymentAttemptStatus::ReviewRequired, None, None),
        ),
        Err(PaymentAttemptError::ReviewAttemptMissingReviewRequiredAt),
    );
    assert!(
        PaymentAttempt::new(
            identity(),
            request(host(), 1_000),
            state(PaymentAttemptStatus::ReviewRequired, None, Some(instant(2)),),
        )
        .is_ok()
    );
}

#[test]
fn lifecycle_refund_amount_must_match_the_attempt_amount() {
    let lifecycle = PaymentAttemptLifecycle::new(
        GatewayLifecycleState::Refunded {
            cumulative_refunded_cents: CumulativeRefundCents::new(999).unwrap(),
        },
        None,
        None,
        None,
    );
    let invalid_state = PaymentAttemptState::new(
        PaymentAttemptStatus::Pending,
        None,
        ProcessorEvidence::default(),
        lifecycle,
        PaymentAttemptTimestamps::new(None, None, None, instant(0), instant(1)),
    );
    assert_eq!(
        PaymentAttempt::new(
            identity(),
            request(
                PaymentAttemptTarget::HostCharge {
                    target_id: target(50),
                },
                1_000,
            ),
            invalid_state,
        ),
        Err(PaymentAttemptError::InvalidLifecycleAmount),
    );
}

#[test]
fn typed_targets_preserve_exact_relationships() {
    let period = BillingPeriod::new(instant(0), instant(2)).unwrap();
    let expected_state = SubscriptionPaymentStateSnapshot::new(
        subscription(60),
        method(61),
        GatewayTransactionId::new("txn-original").unwrap(),
        SubscriptionStatus::PastDue,
    )
    .unwrap();
    let renewal = PaymentAttemptTarget::SubscriptionRenewal {
        plan_key: PlanKey::new("premium").unwrap(),
        payment_method_id: method(62),
        period: period.clone(),
        expected_state: expected_state.clone(),
    };
    assert_eq!(renewal.kind(), PaymentAttemptKind::SubscriptionRenewal);
    assert_eq!(renewal.plan_key().unwrap().as_str(), "premium");
    assert_eq!(renewal.subscription_id(), Some(subscription(60)));
    assert_eq!(renewal.payment_method_id(), Some(method(62)));
    assert_eq!(
        renewal
            .subscription_payment_state_snapshot()
            .unwrap()
            .payment_method_id(),
        method(61)
    );
    assert_eq!(renewal.period(), Some(&period));
    assert_eq!(
        renewal.subscription_payment_state_snapshot(),
        Some(&expected_state)
    );
    assert_eq!(renewal.host_charge_target_id(), None);
}

#[test]
fn durable_attempt_debug_is_value_free() {
    let evidence = ProcessorEvidence::new(
        Some(GatewayTransactionId::new("transaction-secret").unwrap()),
        Some(GatewayPaymentMethodReference::new("method-secret").unwrap()),
        Some(GatewayDiagnostic::new("response-secret")),
        Some(GatewayDiagnostic::new("code-secret")),
        Some(GatewayDiagnostic::new("text-secret")),
        Some(GatewayDiagnostic::new("condition-secret")),
        GatewayPaymentDescriptor::default(),
    );
    let state = PaymentAttemptState::new(
        PaymentAttemptStatus::Pending,
        None,
        evidence,
        PaymentAttemptLifecycle::new(
            GatewayLifecycleState::Unknown,
            Some(GatewayDiagnostic::new("action-secret")),
            None,
            None,
        ),
        PaymentAttemptTimestamps::new(None, None, None, instant(0), instant(1)),
    );
    let attempt = PaymentAttempt::new(
        identity(),
        request(
            PaymentAttemptTarget::HostCharge {
                target_id: target(70),
            },
            1_000,
        ),
        state,
    )
    .unwrap();
    let debug = format!("{attempt:?}");
    for secret in [
        "idempotency-secret",
        "fingerprint-secret",
        "order-secret",
        "Sensitive Name",
        "secret@example.test",
        "transaction-secret",
        "method-secret",
        "response-secret",
        "code-secret",
        "text-secret",
        "condition-secret",
        "action-secret",
    ] {
        assert!(!debug.contains(secret), "debug leaked {secret}");
    }
    assert!(debug.contains("has_idempotency_key"));
    assert!(debug.contains("has_transaction_id"));
}
