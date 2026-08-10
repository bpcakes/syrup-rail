use uuid::Uuid;

use super::*;
use crate::{
    CurrencyCode, DunningExhaustion, DunningSchedule, LimitedDiscountMonths, PaidTrialTerms,
    PastDueAccessPolicy, PercentOffBasisPoints, RecurringSubscriptionTerms, RenewalFailurePolicy,
    SubscriptionDiscountCode, SubscriptionDiscountDuration, SubscriptionDiscountKind,
    SubscriptionPeriodRule, SubscriptionPhase, SubscriptionStart,
};

fn plan(value: &str) -> PlanKey {
    PlanKey::new(value).unwrap()
}

fn offer(plan_key: PlanKey, cents: i32) -> SubscriptionOffer {
    offer_with(
        plan_key,
        cents,
        SubscriptionPeriodRule::calendar_months(1).unwrap(),
        SubscriptionStart::RecurringImmediately,
    )
}

fn offer_with(
    plan_key: PlanKey,
    cents: i32,
    recurring_period: SubscriptionPeriodRule,
    start: SubscriptionStart,
) -> SubscriptionOffer {
    SubscriptionOffer::new(
        plan_key,
        RecurringSubscriptionTerms::new(
            ChargeAmount::new(cents, CurrencyCode::new("USD").unwrap()).unwrap(),
            recurring_period,
        ),
        start,
        RenewalFailurePolicy::new(
            DunningSchedule::from_seconds([86_400]).unwrap(),
            DunningExhaustion::RemainPastDue,
            PastDueAccessPolicy::SuspendImmediately,
        ),
    )
    .unwrap()
}

fn discount(base_cents: i32, discounted_cents: i32) -> SubscriptionDiscountSnapshot {
    discount_with_duration(
        base_cents,
        discounted_cents,
        SubscriptionDiscountDuration::LimitedMonths(LimitedDiscountMonths::new(3).unwrap()),
    )
}

fn discount_with_duration(
    base_cents: i32,
    discounted_cents: i32,
    duration: SubscriptionDiscountDuration,
) -> SubscriptionDiscountSnapshot {
    let currency = CurrencyCode::new("USD").unwrap();
    SubscriptionDiscountSnapshot::new(
        SubscriptionDiscountCode::new("SAVE20").unwrap(),
        Some("Launch offer".to_owned()),
        SubscriptionDiscountKind::PercentOffBasisPoints(PercentOffBasisPoints::new(2000).unwrap()),
        duration,
        ChargeAmount::new(base_cents, currency).unwrap(),
        ChargeAmount::new(discounted_cents, currency).unwrap(),
    )
    .unwrap()
}

#[test]
fn full_price_requires_the_exact_plan_offer_and_no_saved_claim() {
    let expected = SubscriptionEnrollmentExpectedTerms::full_price(offer(plan("basic"), 1000));
    assert!(expected.matches_locked_terms(&offer(plan("basic"), 1000), None));
    assert!(!expected.matches_locked_terms(&offer(plan("premium"), 1000), None));
    assert!(!expected.matches_locked_terms(&offer(plan("basic"), 1200), None));
    let saved = discount(1000, 800);
    assert!(!expected.matches_locked_terms(&offer(plan("basic"), 1000), Some(&saved)));
}

#[test]
fn saved_discount_keeps_its_snapshot_while_the_plan_must_still_exist() {
    let saved = discount(1000, 800);
    let expected =
        SubscriptionEnrollmentExpectedTerms::discounted(offer(plan("basic"), 1000), saved.clone())
            .unwrap();
    assert!(expected.matches_locked_terms(&offer(plan("basic"), 1400), Some(&saved)));
    let labeled = SubscriptionDiscountSnapshot::new(
        saved.code().clone(),
        Some("Internal campaign label".to_owned()),
        saved.kind(),
        saved.duration(),
        saved.base_charge(),
        saved.discounted_charge(),
    )
    .unwrap();
    assert!(expected.matches_locked_terms(&offer(plan("basic"), 1400), Some(&labeled)));
    assert!(!expected.matches_locked_terms(&offer(plan("premium"), 1400), Some(&saved)));
    assert!(
        !expected.matches_locked_terms(&offer(plan("basic"), 1400), Some(&discount(1000, 750)),)
    );
    assert!(!expected.matches_locked_terms(&offer(plan("basic"), 1400), None));
}

#[test]
fn initial_charge_uses_trial_but_discount_applies_to_recurring() {
    let usd = CurrencyCode::new("USD").unwrap();
    let trial = PaidTrialTerms::new(
        ChargeAmount::new(100, usd).unwrap(),
        SubscriptionPeriodRule::fixed_days(7).unwrap(),
    );
    let accepted_offer = offer_with(
        plan("basic"),
        1_000,
        SubscriptionPeriodRule::calendar_months(1).unwrap(),
        SubscriptionStart::PaidTrial(trial),
    );
    let expected =
        SubscriptionEnrollmentExpectedTerms::discounted(accepted_offer, discount(1_000, 800))
            .unwrap();
    assert_eq!(expected.initial_charge().cents(), 100);
    assert_eq!(
        expected
            .activation_projection()
            .recurring_charge_after_initial()
            .cents(),
        800
    );
    assert_eq!(
        SubscriptionEnrollmentExpectedTerms::full_price(offer(plan("basic"), 1_000))
            .initial_charge()
            .cents(),
        1_000
    );
}

#[test]
fn activation_projection_consumes_the_immediate_recurring_discount_period() {
    let offer = offer(plan("basic"), 1_000);
    let one_month = discount_with_duration(
        1_000,
        800,
        SubscriptionDiscountDuration::LimitedMonths(LimitedDiscountMonths::new(1).unwrap()),
    );
    let projection = SubscriptionEnrollmentExpectedTerms::discounted(offer.clone(), one_month)
        .unwrap()
        .activation_projection();

    assert_eq!(projection.phase(), SubscriptionPhase::Recurring);
    assert_eq!(projection.initial_charge().cents(), 800);
    assert_eq!(
        projection.initial_period_rule(),
        SubscriptionPeriodRule::calendar_months(1).unwrap()
    );
    assert_eq!(projection.recurring_charge_after_initial().cents(), 1_000);
    assert_eq!(projection.discount_periods_applied(), 1);

    let three_months = discount(1_000, 800);
    let continuing = SubscriptionEnrollmentExpectedTerms::discounted(offer, three_months)
        .unwrap()
        .activation_projection();
    assert_eq!(continuing.recurring_charge_after_initial().cents(), 800);
    assert_eq!(continuing.discount_periods_applied(), 1);
}

#[test]
fn activation_projection_defers_limited_discount_consumption_for_a_paid_trial() {
    let usd = CurrencyCode::new("USD").unwrap();
    let trial = PaidTrialTerms::new(
        ChargeAmount::new(100, usd).unwrap(),
        SubscriptionPeriodRule::fixed_days(7).unwrap(),
    );
    let expected = SubscriptionEnrollmentExpectedTerms::discounted(
        offer_with(
            plan("basic"),
            1_000,
            SubscriptionPeriodRule::calendar_months(1).unwrap(),
            SubscriptionStart::PaidTrial(trial),
        ),
        discount_with_duration(
            1_000,
            800,
            SubscriptionDiscountDuration::LimitedMonths(LimitedDiscountMonths::new(1).unwrap()),
        ),
    )
    .unwrap();

    let projection = expected.activation_projection();
    assert_eq!(projection.phase(), SubscriptionPhase::PaidTrial);
    assert_eq!(projection.initial_charge().cents(), 100);
    assert_eq!(expected.initial_charge(), projection.initial_charge());
    assert_eq!(
        projection.initial_period_rule(),
        SubscriptionPeriodRule::fixed_days(7).unwrap()
    );
    assert_eq!(projection.recurring_charge_after_initial().cents(), 800);
    assert_eq!(projection.discount_periods_applied(), 0);
}

#[test]
fn limited_month_discount_requires_monthly_cadence_but_indefinite_does_not() {
    let fixed_offer = offer_with(
        plan("basic"),
        1_000,
        SubscriptionPeriodRule::fixed_days(30).unwrap(),
        SubscriptionStart::RecurringImmediately,
    );
    assert_eq!(
        SubscriptionEnrollmentExpectedTerms::discounted(fixed_offer.clone(), discount(1_000, 800),),
        Err(SubscriptionEnrollmentTermsError::LimitedDiscountCadence)
    );
    let multi_month_offer = offer_with(
        plan("basic"),
        1_000,
        SubscriptionPeriodRule::calendar_months(2).unwrap(),
        SubscriptionStart::RecurringImmediately,
    );
    assert_eq!(
        SubscriptionEnrollmentExpectedTerms::discounted(multi_month_offer, discount(1_000, 800),),
        Err(SubscriptionEnrollmentTermsError::LimitedDiscountCadence)
    );

    let saved = discount(1_000, 800);
    let indefinite = SubscriptionDiscountSnapshot::new(
        saved.code().clone(),
        None,
        saved.kind(),
        SubscriptionDiscountDuration::Indefinite,
        saved.base_charge(),
        saved.discounted_charge(),
    )
    .unwrap();
    assert!(SubscriptionEnrollmentExpectedTerms::discounted(fixed_offer, indefinite).is_ok());
}

#[test]
fn discounted_terms_reject_a_currency_mismatch() {
    let eur = CurrencyCode::new("EUR").unwrap();
    let eur_discount = SubscriptionDiscountSnapshot::new(
        SubscriptionDiscountCode::new("SAVE20EUR").unwrap(),
        None,
        SubscriptionDiscountKind::PercentOffBasisPoints(PercentOffBasisPoints::new(2000).unwrap()),
        SubscriptionDiscountDuration::LimitedMonths(LimitedDiscountMonths::new(3).unwrap()),
        ChargeAmount::new(1_000, eur).unwrap(),
        ChargeAmount::new(800, eur).unwrap(),
    )
    .unwrap();

    assert_eq!(
        SubscriptionEnrollmentExpectedTerms::discounted(offer(plan("basic"), 1_000), eur_discount,),
        Err(SubscriptionEnrollmentTermsError::CurrencyMismatch)
    );
}

#[test]
fn enrollment_debug_omits_token_key_and_contact_values() {
    let command = EnrollSubscription::new(
        PaymentAttemptId::new(Uuid::from_u128(1)),
        BillingScopeId::new(Uuid::from_u128(2)),
        SubscriberId::new(Uuid::from_u128(3)),
        GatewayConfigurationId::new(Uuid::from_u128(4)),
        IdempotencyKey::new("secret-key").unwrap(),
        PaymentToken::new("secret-token").unwrap(),
        BillingContact::new(
            None,
            Some("Secret Name".to_owned()),
            Some("secret@example.test".to_owned()),
        )
        .unwrap(),
        SubscriptionEnrollmentExpectedTerms::full_price(offer(plan("basic"), 1000)),
    );
    let debug = format!("{command:?}");
    for secret in [
        "secret-key",
        "secret-token",
        "Secret Name",
        "secret@example.test",
    ] {
        assert!(!debug.contains(secret));
    }
    assert!(debug.contains("has_payment_token"));
}

#[test]
fn durable_discount_debug_omits_code_and_label_values() {
    let expected = SubscriptionEnrollmentExpectedTerms::discounted(
        offer(plan("basic"), 1_000),
        SubscriptionDiscountSnapshot::new(
            SubscriptionDiscountCode::new("SECRET20").unwrap(),
            Some("Sensitive campaign label".to_owned()),
            SubscriptionDiscountKind::PercentOffBasisPoints(
                PercentOffBasisPoints::new(2_000).unwrap(),
            ),
            SubscriptionDiscountDuration::LimitedMonths(LimitedDiscountMonths::new(3).unwrap()),
            ChargeAmount::new(1_000, CurrencyCode::new("USD").unwrap()).unwrap(),
            ChargeAmount::new(800, CurrencyCode::new("USD").unwrap()).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    let expected_debug = format!("{expected:?}");
    assert!(!expected_debug.contains("SECRET20"));
    assert!(!expected_debug.contains("Sensitive campaign label"));
    assert!(expected_debug.contains("has_code"));
    assert!(expected_debug.contains("has_label"));

    let snapshot = SubscriptionEnrollmentDiscountSnapshot::new(
        DiscountClaimId::new(Uuid::from_u128(10)),
        DiscountCodeId::new(Uuid::from_u128(11)),
        expected
            .discount_snapshot()
            .expect("discounted expectation should retain snapshot")
            .clone(),
    );
    let debug = format!("{snapshot:?}");
    assert!(!debug.contains("SECRET20"));
    assert!(!debug.contains("Sensitive campaign label"));
    assert!(debug.contains("has_code"));
    assert!(debug.contains("has_label"));
}
