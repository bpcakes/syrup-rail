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
fn grant_reason_is_trimmed_bounded_and_card_safe() {
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
