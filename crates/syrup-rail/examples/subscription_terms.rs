use syrup_rail::{
    ChargeAmount, CurrencyCode, DunningExhaustion, DunningRetryDelay, DunningSchedule,
    PaidTrialTerms, PastDueAccessPolicy, PlanKey, RecurringSubscriptionTerms, RenewalFailurePolicy,
    SubscriptionOffer, SubscriptionPeriodRule, SubscriptionStart,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let usd = CurrencyCode::new("USD")?;
    let recurring = RecurringSubscriptionTerms::new(
        ChargeAmount::new(2_900, usd)?,
        SubscriptionPeriodRule::calendar_months(1)?,
    );
    let dunning = RenewalFailurePolicy::new(
        DunningSchedule::from_delays([DunningRetryDelay::days(1)?, DunningRetryDelay::days(3)?])?,
        DunningExhaustion::MarkUnpaid,
        PastDueAccessPolicy::ContinueUntilDunningExhausted,
    );

    let immediate = SubscriptionOffer::new(
        PlanKey::new("standard")?,
        recurring,
        SubscriptionStart::RecurringImmediately,
        dunning.clone(),
    )?;
    let paid_trial = SubscriptionOffer::new(
        PlanKey::new("standard")?,
        recurring,
        SubscriptionStart::PaidTrial(PaidTrialTerms::new(
            ChargeAmount::new(100, usd)?,
            SubscriptionPeriodRule::fixed_days(7)?,
        )),
        dunning,
    )?;

    println!("Immediate recurring offer: {immediate:?}");
    println!("Paid trial offer: {paid_trial:?}");
    Ok(())
}
