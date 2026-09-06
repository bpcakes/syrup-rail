use chrono::TimeZone;
use uuid::Uuid;

use super::*;
use crate::{
    DunningExhaustion, DunningSchedule, LimitedDiscountMonths, PaidTrialTerms, PastDueAccessPolicy,
    PercentOffBasisPoints, PositiveDiscountCents, RecurringSubscriptionTerms, RenewalFailurePolicy,
    SubscriptionPeriodRule, SubscriptionStart,
};

fn code_record(duration: SubscriptionDiscountDuration) -> SubscriptionDiscountCodeRecord {
    let now = Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap();
    SubscriptionDiscountCodeRecord::new(
        DiscountCodeId::new(Uuid::from_u128(1)),
        BillingScopeId::new(Uuid::from_u128(2)),
        PlanKey::new("basic").unwrap(),
        SubscriptionDiscountCode::new("SAVE10").unwrap(),
        "SAVE10".to_owned(),
        None,
        SubscriptionDiscountCodeStatus::Active,
        SubscriptionDiscountKind::AmountOffCents(PositiveDiscountCents::new(100).unwrap()),
        CurrencyCode::new("USD").unwrap(),
        duration,
        now,
        now,
    )
    .unwrap()
}

fn offer(trial_cents: i32, recurring_period: SubscriptionPeriodRule) -> SubscriptionOffer {
    let usd = CurrencyCode::new("USD").unwrap();
    SubscriptionOffer::new(
        PlanKey::new("basic").unwrap(),
        RecurringSubscriptionTerms::new(ChargeAmount::new(1_000, usd).unwrap(), recurring_period),
        SubscriptionStart::PaidTrial(PaidTrialTerms::new(
            ChargeAmount::new(trial_cents, usd).unwrap(),
            SubscriptionPeriodRule::fixed_days(7).unwrap(),
        )),
        RenewalFailurePolicy::new(
            DunningSchedule::default(),
            DunningExhaustion::RemainPastDue,
            PastDueAccessPolicy::SuspendImmediately,
        ),
    )
    .unwrap()
}

fn claim_snapshot() -> SubscriptionDiscountSnapshot {
    let usd = CurrencyCode::new("USD").unwrap();
    SubscriptionDiscountSnapshot::new(
        SubscriptionDiscountCode::new("SAVE10").unwrap(),
        Some("Launch offer".to_owned()),
        SubscriptionDiscountKind::AmountOffCents(PositiveDiscountCents::new(100).unwrap()),
        SubscriptionDiscountDuration::Indefinite,
        ChargeAmount::new(1_000, usd).unwrap(),
        ChargeAmount::new(900, usd).unwrap(),
    )
    .unwrap()
}

fn claim_record_from_state(
    state: SubscriptionDiscountClaimState,
    claimed_at: DateTime<Utc>,
) -> SubscriptionDiscountClaimRecord {
    SubscriptionDiscountClaimRecord::from_state(
        DiscountClaimId::new(Uuid::from_u128(21)),
        BillingScopeId::new(Uuid::from_u128(22)),
        SubscriberId::new(Uuid::from_u128(23)),
        PlanKey::new("basic").unwrap(),
        DiscountCodeId::new(Uuid::from_u128(24)),
        claim_snapshot(),
        state,
        claimed_at,
    )
}

#[test]
fn claim_lifecycle_states_preserve_legacy_projections() {
    let claimed_at = Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap();
    let applied_at = Utc.with_ymd_and_hms(2026, 8, 2, 0, 0, 0).unwrap();
    let superseded_at = Utc.with_ymd_and_hms(2026, 8, 3, 0, 0, 0).unwrap();
    let subscription_id = SubscriptionId::new(Uuid::from_u128(25));
    let payment_attempt_id = PaymentAttemptId::new(Uuid::from_u128(26));
    let cases = vec![
        (
            SubscriptionDiscountClaimState::Saved,
            SubscriptionDiscountClaimStatus::Saved,
            None,
            None,
            None,
            None,
        ),
        (
            SubscriptionDiscountClaimState::Applied {
                applied_at,
                subscription_id,
                payment_attempt_id,
            },
            SubscriptionDiscountClaimStatus::Applied,
            Some(applied_at),
            Some(subscription_id),
            Some(payment_attempt_id),
            None,
        ),
        (
            SubscriptionDiscountClaimState::Superseded { superseded_at },
            SubscriptionDiscountClaimStatus::Superseded,
            None,
            None,
            None,
            Some(superseded_at),
        ),
        (
            SubscriptionDiscountClaimState::Expired,
            SubscriptionDiscountClaimStatus::Expired,
            None,
            None,
            None,
            None,
        ),
    ];

    for (
        state,
        status,
        expected_applied_at,
        expected_subscription_id,
        expected_payment_attempt_id,
        expected_superseded_at,
    ) in cases
    {
        let record = claim_record_from_state(state.clone(), claimed_at);
        assert_eq!(record.state(), &state);
        assert_eq!(record.status(), status);
        assert_eq!(record.applied_at(), expected_applied_at.as_ref());
        assert_eq!(record.applied_subscription_id(), expected_subscription_id);
        assert_eq!(
            record.applied_payment_attempt_id(),
            expected_payment_attempt_id
        );
        assert_eq!(record.superseded_at(), expected_superseded_at.as_ref());

        let legacy = SubscriptionDiscountClaimRecord::new(
            DiscountClaimId::new(Uuid::from_u128(21)),
            BillingScopeId::new(Uuid::from_u128(22)),
            SubscriberId::new(Uuid::from_u128(23)),
            PlanKey::new("basic").unwrap(),
            DiscountCodeId::new(Uuid::from_u128(24)),
            claim_snapshot(),
            status,
            claimed_at,
            expected_applied_at,
            expected_subscription_id,
            expected_payment_attempt_id,
            expected_superseded_at,
        )
        .unwrap();
        assert_eq!(legacy, record);
    }
}

#[test]
fn claim_state_rejects_invalid_legacy_shapes() {
    let claimed_at = Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap();
    let applied_at = Utc.with_ymd_and_hms(2026, 8, 2, 0, 0, 0).unwrap();
    let superseded_at = Utc.with_ymd_and_hms(2026, 8, 3, 0, 0, 0).unwrap();
    let subscription_id = SubscriptionId::new(Uuid::from_u128(25));
    let payment_attempt_id = PaymentAttemptId::new(Uuid::from_u128(26));
    let cases = vec![
        (
            "saved claim with application details",
            SubscriptionDiscountClaimStatus::Saved,
            Some(applied_at),
            Some(subscription_id),
            Some(payment_attempt_id),
            None,
        ),
        (
            "applied claim missing its timestamp",
            SubscriptionDiscountClaimStatus::Applied,
            None,
            Some(subscription_id),
            Some(payment_attempt_id),
            None,
        ),
        (
            "applied claim missing its subscription",
            SubscriptionDiscountClaimStatus::Applied,
            Some(applied_at),
            None,
            Some(payment_attempt_id),
            None,
        ),
        (
            "applied claim missing its payment attempt",
            SubscriptionDiscountClaimStatus::Applied,
            Some(applied_at),
            Some(subscription_id),
            None,
            None,
        ),
        (
            "applied claim also marked superseded",
            SubscriptionDiscountClaimStatus::Applied,
            Some(applied_at),
            Some(subscription_id),
            Some(payment_attempt_id),
            Some(superseded_at),
        ),
        (
            "superseded claim missing its timestamp",
            SubscriptionDiscountClaimStatus::Superseded,
            None,
            None,
            None,
            None,
        ),
        (
            "superseded claim with application details",
            SubscriptionDiscountClaimStatus::Superseded,
            Some(applied_at),
            Some(subscription_id),
            Some(payment_attempt_id),
            Some(superseded_at),
        ),
        (
            "expired claim with supersession details",
            SubscriptionDiscountClaimStatus::Expired,
            None,
            None,
            None,
            Some(superseded_at),
        ),
    ];

    for (description, status, applied_at, subscription_id, payment_attempt_id, superseded_at) in
        cases
    {
        assert_eq!(
            SubscriptionDiscountClaimState::from_legacy_parts(
                status,
                applied_at,
                subscription_id,
                payment_attempt_id,
                superseded_at,
            ),
            Err(SubscriptionDiscountError::InvalidState),
            "{description}"
        );
        assert_eq!(
            SubscriptionDiscountClaimRecord::new(
                DiscountClaimId::new(Uuid::from_u128(21)),
                BillingScopeId::new(Uuid::from_u128(22)),
                SubscriberId::new(Uuid::from_u128(23)),
                PlanKey::new("basic").unwrap(),
                DiscountCodeId::new(Uuid::from_u128(24)),
                claim_snapshot(),
                status,
                claimed_at,
                applied_at,
                subscription_id,
                payment_attempt_id,
                superseded_at,
            ),
            Err(SubscriptionDiscountError::InvalidState),
            "{description}"
        );
    }
}

#[test]
fn quotes_preserve_integer_discount_behavior() {
    let usd = CurrencyCode::new("USD").unwrap();
    let base = ChargeAmount::new(5_900, usd).unwrap();
    assert_eq!(
        discounted_charge(
            base,
            usd,
            SubscriptionDiscountKind::AmountOffCents(PositiveDiscountCents::new(900).unwrap(),),
        )
        .unwrap()
        .cents(),
        5_000
    );
    assert_eq!(
        discounted_charge(
            base,
            usd,
            SubscriptionDiscountKind::PercentOffBasisPoints(
                PercentOffBasisPoints::new(2_500).unwrap(),
            ),
        )
        .unwrap()
        .cents(),
        4_425
    );
}

#[test]
fn labels_preserve_the_existing_trim_and_truncate_contract() {
    assert_eq!(
        normalize_label(Some("  Launch  ".into()))
            .unwrap()
            .as_deref(),
        Some("Launch")
    );
    assert_eq!(normalize_label(Some("  ".into())).unwrap(), None);
    assert_eq!(
        normalize_label(Some("x".repeat(121)))
            .unwrap()
            .unwrap()
            .len(),
        120
    );
}

#[test]
fn limited_duration_remains_typed() {
    let duration =
        SubscriptionDiscountDuration::LimitedMonths(LimitedDiscountMonths::new(3).unwrap());
    assert_eq!(duration.as_str(), "limited_months");
}

#[test]
fn clear_command_preserves_the_exact_subscriber_aggregate() {
    let scope = BillingScopeId::new(Uuid::from_u128(11));
    let subscriber = SubscriberId::new(Uuid::from_u128(12));
    let plan = PlanKey::new("basic").unwrap();

    let command = ClearSubscriptionDiscount::new(scope, subscriber, plan.clone());

    assert_eq!(command.billing_scope_id(), scope);
    assert_eq!(command.subscriber_id(), subscriber);
    assert_eq!(command.plan_key(), &plan);
}

#[test]
fn discount_quote_uses_recurring_charge_not_trial_charge() {
    let first = SubscriptionDiscountCodeQuote::new(
        code_record(SubscriptionDiscountDuration::Indefinite),
        &offer(100, SubscriptionPeriodRule::calendar_months(1).unwrap()),
    )
    .unwrap();
    let second = SubscriptionDiscountCodeQuote::new(
        code_record(SubscriptionDiscountDuration::Indefinite),
        &offer(500, SubscriptionPeriodRule::calendar_months(1).unwrap()),
    )
    .unwrap();
    assert_eq!(first.base_charge(), second.base_charge());
    assert_eq!(first.discounted_charge(), second.discounted_charge());
    assert_eq!(first.discounted_charge().cents(), 900);
}

#[test]
fn limited_month_quote_rejects_nonmonthly_recurring_cadence() {
    let result = SubscriptionDiscountCodeQuote::new(
        code_record(SubscriptionDiscountDuration::LimitedMonths(
            LimitedDiscountMonths::new(3).unwrap(),
        )),
        &offer(100, SubscriptionPeriodRule::fixed_days(30).unwrap()),
    );
    assert_eq!(
        result,
        Err(SubscriptionDiscountError::LimitedDiscountCadence)
    );
}
