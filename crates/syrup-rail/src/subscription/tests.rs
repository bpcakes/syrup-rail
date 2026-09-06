use chrono::TimeZone;
use uuid::Uuid;

use super::*;
use crate::{
    BillingPeriod, CurrencyCode, DunningExhaustion, DunningSchedule, MoneyError, PaymentMethodId,
    RenewalFailurePolicy, SubscriptionId, SubscriptionPeriodRule, SubscriptionPhase,
    SubscriptionStatus,
};

#[test]
fn canonical_discount_code_has_one_probe_identity() {
    let code = SubscriptionDiscountCode::new(" summer-25 ").unwrap();
    assert_eq!(code.as_str(), "SUMMER-25");
    assert_eq!(
        SubscriptionDiscountCode::new("bad code"),
        Err(SubscriptionDiscountError::InvalidCode)
    );
    assert!(!format!("{code:?}").contains("SUMMER"));
}

#[test]
fn discount_snapshot_rejects_cross_currency_or_increased_price() {
    let usd = CurrencyCode::new("USD").unwrap();
    let eur = CurrencyCode::new("EUR").unwrap();
    let code = SubscriptionDiscountCode::new("SAVE10").unwrap();
    let kind =
        SubscriptionDiscountKind::PercentOffBasisPoints(PercentOffBasisPoints::new(1_000).unwrap());
    assert_eq!(
        SubscriptionDiscountSnapshot::new(
            code.clone(),
            None,
            kind,
            SubscriptionDiscountDuration::Indefinite,
            ChargeAmount::new(1_000, usd).unwrap(),
            ChargeAmount::new(900, eur).unwrap(),
        ),
        Err(SubscriptionDiscountError::InvalidChargeSnapshot)
    );
    assert_eq!(
        SubscriptionDiscountSnapshot::new(
            code,
            None,
            kind,
            SubscriptionDiscountDuration::Indefinite,
            ChargeAmount::new(1_000, usd).unwrap(),
            ChargeAmount::new(1_001, usd).unwrap(),
        ),
        Err(SubscriptionDiscountError::InvalidChargeSnapshot)
    );
    assert_eq!(
        ChargeAmount::new(0, usd),
        Err(MoneyError::NonPositiveCharge)
    );
}

#[test]
fn limited_discount_allows_zero_applied_recurring_periods() {
    let usd = CurrencyCode::new("USD").unwrap();
    let total = LimitedDiscountMonths::new(3).unwrap();
    let snapshot = SubscriptionDiscountSnapshot::new(
        SubscriptionDiscountCode::new("SAVE10").unwrap(),
        None,
        SubscriptionDiscountKind::AmountOffCents(PositiveDiscountCents::new(100).unwrap()),
        SubscriptionDiscountDuration::LimitedMonths(total),
        ChargeAmount::new(1_000, usd).unwrap(),
        ChargeAmount::new(900, usd).unwrap(),
    )
    .unwrap();
    let before_first_recurring_charge =
        AppliedSubscriptionDiscount::new(None, snapshot.clone(), Some(total.get())).unwrap();
    assert_eq!(
        before_first_recurring_charge.periods_remaining(),
        Some(total.get())
    );
    assert!(AppliedSubscriptionDiscount::new(None, snapshot, Some(total.get() + 1)).is_err());
}

#[test]
fn past_due_access_requires_both_continuing_policy_and_a_scheduled_payment() {
    assert_eq!(
        classify_past_due_access(PastDueAccessPolicy::ContinueUntilDunningExhausted, true,),
        PastDueAccess::AllowedDuringDunning
    );
    for (policy, scheduled) in [
        (PastDueAccessPolicy::SuspendImmediately, true),
        (PastDueAccessPolicy::SuspendImmediately, false),
        (PastDueAccessPolicy::ContinueUntilDunningExhausted, false),
    ] {
        assert_eq!(
            classify_past_due_access(policy, scheduled),
            PastDueAccess::Suspended
        );
    }
}

#[test]
fn subscription_lifecycle_validates_the_complete_status_schedule_matrix() {
    let starts = Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap();
    let ends = Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap();
    let before_end = ends - chrono::Duration::seconds(1);
    let after_end = ends + chrono::Duration::seconds(1);

    for (status, next_payment_attempt_at) in [
        (SubscriptionStatus::Active, Some(ends)),
        (SubscriptionStatus::PastDue, None),
        (SubscriptionStatus::PastDue, Some(ends)),
        (SubscriptionStatus::PastDue, Some(after_end)),
        (SubscriptionStatus::Canceled, None),
        (SubscriptionStatus::Unpaid, None),
    ] {
        let lifecycle = SubscriptionLifecycle::from_parts(
            status,
            BillingPeriod::new(starts, ends).unwrap(),
            ends,
            next_payment_attempt_at,
        )
        .unwrap();
        assert_eq!(lifecycle.status(), status);
        assert_eq!(lifecycle.current_period().start_at(), &starts);
        assert_eq!(lifecycle.current_period().end_at(), &ends);
        assert_eq!(lifecycle.next_renewal_at(), &ends);
        assert_eq!(
            lifecycle.next_payment_attempt_at(),
            next_payment_attempt_at.as_ref()
        );
    }

    for (status, next_payment_attempt_at) in [
        (SubscriptionStatus::Active, None),
        (SubscriptionStatus::Active, Some(before_end)),
        (SubscriptionStatus::Active, Some(after_end)),
        (SubscriptionStatus::PastDue, Some(before_end)),
        (SubscriptionStatus::Canceled, Some(ends)),
        (SubscriptionStatus::Unpaid, Some(ends)),
    ] {
        assert_eq!(
            SubscriptionLifecycle::from_parts(
                status,
                BillingPeriod::new(starts, ends).unwrap(),
                ends,
                next_payment_attempt_at,
            ),
            Err(SubscriptionLifecycleError::InvalidPaymentSchedule),
        );
    }

    for status in SubscriptionStatus::ALL {
        assert_eq!(
            SubscriptionLifecycle::from_parts(
                status,
                BillingPeriod::new(starts, ends).unwrap(),
                ends + chrono::Duration::seconds(1),
                None,
            ),
            Err(SubscriptionLifecycleError::NextRenewalDoesNotMatchPeriod),
        );
    }
}

#[test]
fn subscription_from_lifecycle_derives_status_and_schedule_projections() {
    let starts = Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap();
    let ends = Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap();
    let subscription = Subscription::from_lifecycle(
        SubscriptionId::new(Uuid::from_u128(1)),
        PlanKey::new("plan").unwrap(),
        SubscriptionPhase::Recurring,
        GatewayAccountMode::Live,
        PaymentMethodId::new(Uuid::from_u128(2)),
        ChargeAmount::new(1_000, CurrencyCode::new("USD").unwrap()).unwrap(),
        SubscriptionPeriodRule::calendar_months(1).unwrap(),
        RenewalFailurePolicy::new(
            DunningSchedule::default(),
            DunningExhaustion::MarkUnpaid,
            PastDueAccessPolicy::SuspendImmediately,
        ),
        SubscriptionLifecycle::active(BillingPeriod::new(starts, ends).unwrap()),
    );

    assert_eq!(subscription.status(), SubscriptionStatus::Active);
    assert_eq!(subscription.current_period().end_at(), &ends);
    assert_eq!(subscription.next_renewal_at(), &ends);
    assert_eq!(subscription.next_payment_attempt_at(), Some(&ends));
}

#[test]
fn grant_period_is_valid_by_construction() {
    let starts = Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap();
    let id = SubscriptionGrantId::new(Uuid::from_u128(1));
    let actor = ActorId::new(Uuid::from_u128(2));
    assert_eq!(
        SubscriptionGrant::new(
            id,
            PlanKey::new("plan").unwrap(),
            SubscriptionGrantKind::Testing,
            starts,
            starts,
            actor,
        ),
        Err(SubscriptionGrantError::InvalidPeriod)
    );
}

#[test]
fn grant_revocation_is_one_validated_audit_state() {
    let starts = Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap();
    let ends = Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap();
    let scope = BillingScopeId::new(Uuid::from_u128(1));
    let subscriber = SubscriberId::new(Uuid::from_u128(2));
    let granting_actor = ActorId::new(Uuid::from_u128(3));
    let revoking_actor = ActorId::new(Uuid::from_u128(4));
    let grant = SubscriptionGrant::new(
        SubscriptionGrantId::new(Uuid::from_u128(5)),
        PlanKey::new("plan").unwrap(),
        SubscriptionGrantKind::Testing,
        starts,
        ends,
        granting_actor,
    )
    .unwrap();
    let grant_reason = SubscriptionGrantReason::new("testing access").unwrap();

    let active = SubscriptionGrantRecord::from_revocation_state(
        scope,
        subscriber,
        grant.clone(),
        grant_reason.clone(),
        SubscriptionGrantRevocationState::Active,
        starts,
        starts,
    )
    .unwrap();
    assert_eq!(
        active.revocation_state(),
        &SubscriptionGrantRevocationState::Active
    );
    assert_eq!(active.revoked_at(), None);
    assert_eq!(active.revoked_by_actor_id(), None);
    assert_eq!(active.revocation_reason(), None);

    let revocation_reason = SubscriptionGrantReason::new("testing complete").unwrap();
    let revoked = SubscriptionGrantRecord::from_revocation_state(
        scope,
        subscriber,
        grant.clone(),
        grant_reason.clone(),
        SubscriptionGrantRevocationState::Revoked(SubscriptionGrantRevocationAudit::new(
            starts,
            revoking_actor,
            revocation_reason.clone(),
        )),
        starts,
        starts,
    )
    .unwrap();
    assert!(matches!(
        revoked.revocation_state(),
        SubscriptionGrantRevocationState::Revoked(audit)
            if audit.revoked_at() == &starts
                && audit.revoked_by_actor_id() == revoking_actor
                && audit.reason() == &revocation_reason
    ));
    assert_eq!(revoked.revoked_at(), Some(&starts));
    assert_eq!(revoked.revoked_by_actor_id(), Some(revoking_actor));
    assert_eq!(revoked.revocation_reason(), Some(&revocation_reason));

    assert_eq!(
        SubscriptionGrantRecord::new(
            scope,
            subscriber,
            grant.clone(),
            grant_reason.clone(),
            Some(starts),
            None,
            None,
            starts,
            starts,
        ),
        Err(SubscriptionGrantRecordError::InvalidRevocation)
    );
    assert_eq!(
        SubscriptionGrantRecord::from_revocation_state(
            scope,
            subscriber,
            grant,
            grant_reason,
            SubscriptionGrantRevocationState::Revoked(SubscriptionGrantRevocationAudit::new(
                starts - chrono::Duration::seconds(1),
                revoking_actor,
                revocation_reason,
            )),
            starts,
            starts,
        ),
        Err(SubscriptionGrantRecordError::RevocationBeforeStart)
    );
}

#[test]
fn grant_reason_is_trimmed_bounded_and_card_safe() {
    assert!(SubscriptionGrantReason::new(format!("  {}  ", "é".repeat(500))).is_ok());
    assert_eq!(
        SubscriptionGrantReason::new("é".repeat(501)),
        Err(SubscriptionGrantReasonError::TooLong)
    );
    assert_eq!(
        SubscriptionGrantReason::new(format!("{}4111111111111111", "x".repeat(500))),
        Err(SubscriptionGrantReasonError::TooLong)
    );
    assert_eq!(
        SubscriptionGrantReason::new("  launch partner  ")
            .unwrap()
            .as_str(),
        "launch partner"
    );
    assert_eq!(
        SubscriptionGrantReason::new("   "),
        Err(SubscriptionGrantReasonError::Empty)
    );
    assert_eq!(
        SubscriptionGrantReason::new("x".repeat(501)),
        Err(SubscriptionGrantReasonError::TooLong)
    );
    assert_eq!(
        SubscriptionGrantReason::new("customer supplied 4242 4242 4242 4242"),
        Err(SubscriptionGrantReasonError::ContainsRawCardData)
    );
}

#[test]
fn entitlement_variants_cannot_mix_grant_and_paid_owners() {
    let starts = Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap();
    let grant = SubscriptionGrant::new(
        SubscriptionGrantId::new(Uuid::from_u128(1)),
        PlanKey::new("plan").unwrap(),
        SubscriptionGrantKind::Promotion,
        starts,
        starts + chrono::Duration::days(30),
        ActorId::new(Uuid::from_u128(2)),
    )
    .unwrap();
    assert!(matches!(
        Entitlement::Granted { grant },
        Entitlement::Granted { .. }
    ));
}

#[test]
fn entitlement_product_access_policy_covers_every_variant() {
    let starts = Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap();
    let subscription = Subscription::new(
        SubscriptionId::new(Uuid::from_u128(1)),
        PlanKey::new("plan").unwrap(),
        SubscriptionStatus::Active,
        SubscriptionPhase::Recurring,
        GatewayAccountMode::Live,
        PaymentMethodId::new(Uuid::from_u128(2)),
        ChargeAmount::new(1_000, CurrencyCode::new("USD").unwrap()).unwrap(),
        SubscriptionPeriodRule::calendar_months(1).unwrap(),
        RenewalFailurePolicy::new(
            DunningSchedule::default(),
            DunningExhaustion::MarkUnpaid,
            PastDueAccessPolicy::SuspendImmediately,
        ),
        BillingPeriod::new(starts, starts + chrono::Duration::days(30)).unwrap(),
        starts,
        None,
    );
    let grant = SubscriptionGrant::new(
        SubscriptionGrantId::new(Uuid::from_u128(3)),
        PlanKey::new("plan").unwrap(),
        SubscriptionGrantKind::Promotion,
        starts,
        starts + chrono::Duration::days(30),
        ActorId::new(Uuid::from_u128(4)),
    )
    .unwrap();

    let decisions = [
        (
            Entitlement::Missing {
                next_action: MissingSubscriptionAction::StartSubscription,
                saved_discount: None,
            },
            false,
        ),
        (
            Entitlement::PaidActive {
                subscription: subscription.clone(),
                applied_discount: None,
            },
            true,
        ),
        (
            Entitlement::PaidThroughCancellation {
                subscription: subscription.clone(),
                applied_discount: None,
            },
            true,
        ),
        (
            Entitlement::PastDue {
                subscription: subscription.clone(),
                access: PastDueAccess::AllowedDuringDunning,
                next_action: PastDueAction::RecoverPayment,
                applied_discount: None,
            },
            true,
        ),
        (
            Entitlement::PastDue {
                subscription,
                access: PastDueAccess::Suspended,
                next_action: PastDueAction::RecoverPayment,
                applied_discount: None,
            },
            false,
        ),
        (Entitlement::Granted { grant }, true),
    ];

    for (entitlement, expected_access) in decisions {
        assert_eq!(entitlement.permits_product_access(), expected_access);
    }
}

#[test]
fn entitlement_query_and_guard_share_selector_rules_without_sharing_type_identity() {
    let billing_scope_id = BillingScopeId::new(Uuid::from_u128(10));
    let subscriber_id = SubscriberId::new(Uuid::from_u128(11));
    let plan_key = PlanKey::new("selector_plan").unwrap();
    let query = EntitlementQuery::new(billing_scope_id, subscriber_id, plan_key.clone());
    let guard = EntitlementGuard::new(billing_scope_id, subscriber_id, plan_key.clone());

    assert_eq!(query.billing_scope_id(), guard.billing_scope_id());
    assert_eq!(query.subscriber_id(), guard.subscriber_id());
    assert_eq!(query.plan_key(), guard.plan_key());
    assert_eq!(
        query.required_gateway_account_mode(),
        guard.required_gateway_account_mode()
    );
    assert_eq!(
        query.required_gateway_account_mode(),
        Some(GatewayAccountMode::Live)
    );

    let query = query.with_required_gateway_account_mode(GatewayAccountMode::Test);
    let guard = guard.with_required_gateway_account_mode(GatewayAccountMode::Test);
    assert_eq!(
        query.required_gateway_account_mode(),
        Some(GatewayAccountMode::Test)
    );
    assert_eq!(
        guard.required_gateway_account_mode(),
        Some(GatewayAccountMode::Test)
    );
    assert_eq!(
        query
            .clone()
            .across_gateway_account_modes()
            .required_gateway_account_mode(),
        None
    );
    assert_eq!(
        guard
            .clone()
            .across_gateway_account_modes()
            .required_gateway_account_mode(),
        None
    );

    let query_debug = format!("{query:?}");
    let guard_debug = format!("{guard:?}");
    assert!(query_debug.starts_with("EntitlementQuery { billing_scope_id:"));
    assert!(guard_debug.starts_with("EntitlementGuard { billing_scope_id:"));
    assert!(!query_debug.contains("selector:"));
    assert!(!guard_debug.contains("selector:"));
}
