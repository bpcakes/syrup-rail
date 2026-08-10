use std::{
    num::{NonZeroU16, NonZeroU32},
    str::FromStr,
};

use thiserror::Error;

use crate::{ChargeAmount, PlanKey};

pub const MAX_DUNNING_RETRY_STEPS: usize = 16;

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum SubscriptionTermsError {
    #[error("subscription period count must be positive")]
    ZeroPeriodCount,
    #[error("dunning retry delay must be positive")]
    ZeroDunningDelay,
    #[error("dunning schedule exceeds the supported retry-step limit")]
    TooManyDunningRetrySteps,
    #[error("paid-trial and recurring charges must use the same currency")]
    CurrencyMismatch,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("unknown subscription terms value")]
pub struct SubscriptionTermsParseError;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SubscriptionPeriodRule {
    FixedDays(NonZeroU16),
    CalendarMonths(NonZeroU16),
}

impl SubscriptionPeriodRule {
    pub fn fixed_days(count: u16) -> Result<Self, SubscriptionTermsError> {
        NonZeroU16::new(count)
            .map(Self::FixedDays)
            .ok_or(SubscriptionTermsError::ZeroPeriodCount)
    }

    pub fn calendar_months(count: u16) -> Result<Self, SubscriptionTermsError> {
        NonZeroU16::new(count)
            .map(Self::CalendarMonths)
            .ok_or(SubscriptionTermsError::ZeroPeriodCount)
    }

    pub fn from_kind_and_count(
        kind: &str,
        count: u16,
    ) -> Result<Self, SubscriptionTermsParseError> {
        let count = NonZeroU16::new(count).ok_or(SubscriptionTermsParseError)?;
        match kind {
            "fixed_days" => Ok(Self::FixedDays(count)),
            "calendar_months" => Ok(Self::CalendarMonths(count)),
            _ => Err(SubscriptionTermsParseError),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FixedDays(_) => "fixed_days",
            Self::CalendarMonths(_) => "calendar_months",
        }
    }

    pub const fn count(self) -> NonZeroU16 {
        match self {
            Self::FixedDays(count) | Self::CalendarMonths(count) => count,
        }
    }

    pub const fn is_one_calendar_month(self) -> bool {
        matches!(self, Self::CalendarMonths(count) if count.get() == 1)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RecurringSubscriptionTerms {
    charge: ChargeAmount,
    period: SubscriptionPeriodRule,
}

impl RecurringSubscriptionTerms {
    pub const fn new(charge: ChargeAmount, period: SubscriptionPeriodRule) -> Self {
        Self { charge, period }
    }

    pub const fn charge(self) -> ChargeAmount {
        self.charge
    }

    pub const fn period(self) -> SubscriptionPeriodRule {
        self.period
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PaidTrialTerms {
    charge: ChargeAmount,
    period: SubscriptionPeriodRule,
}

impl PaidTrialTerms {
    pub const fn new(charge: ChargeAmount, period: SubscriptionPeriodRule) -> Self {
        Self { charge, period }
    }

    pub const fn charge(self) -> ChargeAmount {
        self.charge
    }

    pub const fn period(self) -> SubscriptionPeriodRule {
        self.period
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SubscriptionStart {
    RecurringImmediately,
    PaidTrial(PaidTrialTerms),
}

impl SubscriptionStart {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RecurringImmediately => "recurring_immediately",
            Self::PaidTrial(_) => "paid_trial",
        }
    }

    pub const fn paid_trial(self) -> Option<PaidTrialTerms> {
        match self {
            Self::RecurringImmediately => None,
            Self::PaidTrial(terms) => Some(terms),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DunningRetryDelay {
    seconds: NonZeroU32,
}

impl DunningRetryDelay {
    pub fn new(seconds: u32) -> Result<Self, SubscriptionTermsError> {
        NonZeroU32::new(seconds)
            .map(|seconds| Self { seconds })
            .ok_or(SubscriptionTermsError::ZeroDunningDelay)
    }

    pub const fn from_non_zero_seconds(seconds: NonZeroU32) -> Self {
        Self { seconds }
    }

    pub const fn seconds(self) -> NonZeroU32 {
        self.seconds
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DunningSchedule {
    retry_delays: Vec<DunningRetryDelay>,
}

impl DunningSchedule {
    pub fn new(retry_delays: Vec<DunningRetryDelay>) -> Result<Self, SubscriptionTermsError> {
        if retry_delays.len() > MAX_DUNNING_RETRY_STEPS {
            return Err(SubscriptionTermsError::TooManyDunningRetrySteps);
        }
        Ok(Self { retry_delays })
    }

    pub fn from_seconds<I>(seconds: I) -> Result<Self, SubscriptionTermsError>
    where
        I: IntoIterator<Item = u32>,
    {
        let retry_delays = seconds
            .into_iter()
            .map(DunningRetryDelay::new)
            .collect::<Result<Vec<_>, _>>()?;
        Self::new(retry_delays)
    }

    pub fn retry_delays(&self) -> &[DunningRetryDelay] {
        &self.retry_delays
    }

    pub fn is_empty(&self) -> bool {
        self.retry_delays.is_empty()
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DunningExhaustion {
    /// Keep the subscription past due with no further automatic payment
    /// scheduled. Exhaustion emits a payment-failure disposition but does not
    /// end the subscription.
    RemainPastDue,
    /// End the subscription as unpaid when its retry schedule is exhausted.
    MarkUnpaid,
}

impl DunningExhaustion {
    pub const ALL: [Self; 2] = [Self::RemainPastDue, Self::MarkUnpaid];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RemainPastDue => "remain_past_due",
            Self::MarkUnpaid => "mark_unpaid",
        }
    }
}

impl FromStr for DunningExhaustion {
    type Err = SubscriptionTermsParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "remain_past_due" => Ok(Self::RemainPastDue),
            "mark_unpaid" => Ok(Self::MarkUnpaid),
            _ => Err(SubscriptionTermsParseError),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PastDueAccessPolicy {
    SuspendImmediately,
    ContinueUntilDunningExhausted,
}

impl PastDueAccessPolicy {
    pub const ALL: [Self; 2] = [
        Self::SuspendImmediately,
        Self::ContinueUntilDunningExhausted,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SuspendImmediately => "suspend_immediately",
            Self::ContinueUntilDunningExhausted => "continue_until_dunning_exhausted",
        }
    }
}

impl FromStr for PastDueAccessPolicy {
    type Err = SubscriptionTermsParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "suspend_immediately" => Ok(Self::SuspendImmediately),
            "continue_until_dunning_exhausted" => Ok(Self::ContinueUntilDunningExhausted),
            _ => Err(SubscriptionTermsParseError),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenewalFailurePolicy {
    schedule: DunningSchedule,
    exhaustion: DunningExhaustion,
    past_due_access: PastDueAccessPolicy,
}

impl RenewalFailurePolicy {
    pub const fn new(
        schedule: DunningSchedule,
        exhaustion: DunningExhaustion,
        past_due_access: PastDueAccessPolicy,
    ) -> Self {
        Self {
            schedule,
            exhaustion,
            past_due_access,
        }
    }

    pub const fn schedule(&self) -> &DunningSchedule {
        &self.schedule
    }

    pub const fn exhaustion(&self) -> DunningExhaustion {
        self.exhaustion
    }

    pub const fn past_due_access(&self) -> PastDueAccessPolicy {
        self.past_due_access
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubscriptionOffer {
    plan_key: PlanKey,
    recurring: RecurringSubscriptionTerms,
    start: SubscriptionStart,
    renewal_failure: RenewalFailurePolicy,
}

impl SubscriptionOffer {
    pub fn new(
        plan_key: PlanKey,
        recurring: RecurringSubscriptionTerms,
        start: SubscriptionStart,
        renewal_failure: RenewalFailurePolicy,
    ) -> Result<Self, SubscriptionTermsError> {
        if let SubscriptionStart::PaidTrial(trial) = start
            && trial.charge().currency() != recurring.charge().currency()
        {
            return Err(SubscriptionTermsError::CurrencyMismatch);
        }
        Ok(Self {
            plan_key,
            recurring,
            start,
            renewal_failure,
        })
    }

    pub const fn plan_key(&self) -> &PlanKey {
        &self.plan_key
    }

    pub const fn recurring(&self) -> RecurringSubscriptionTerms {
        self.recurring
    }

    pub const fn start(&self) -> SubscriptionStart {
        self.start
    }

    pub const fn renewal_failure(&self) -> &RenewalFailurePolicy {
        &self.renewal_failure
    }

    pub const fn currency(&self) -> crate::CurrencyCode {
        self.recurring.charge().currency()
    }

    pub fn has_same_non_price_terms(&self, other: &Self) -> bool {
        self.plan_key == other.plan_key
            && self.recurring.period() == other.recurring.period()
            && self.start == other.start
            && self.renewal_failure == other.renewal_failure
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CurrencyCode;

    #[test]
    fn period_and_delay_constructors_reject_zero() {
        assert_eq!(
            SubscriptionPeriodRule::fixed_days(0),
            Err(SubscriptionTermsError::ZeroPeriodCount)
        );
        assert_eq!(
            SubscriptionPeriodRule::calendar_months(0),
            Err(SubscriptionTermsError::ZeroPeriodCount)
        );
        assert_eq!(
            DunningRetryDelay::new(0),
            Err(SubscriptionTermsError::ZeroDunningDelay)
        );
    }

    #[test]
    fn schedule_accepts_empty_and_sixteen_steps_but_not_seventeen() {
        assert!(DunningSchedule::new(Vec::new()).unwrap().is_empty());
        let delay = DunningRetryDelay::new(1).unwrap();
        assert_eq!(
            DunningSchedule::new(vec![delay; MAX_DUNNING_RETRY_STEPS])
                .unwrap()
                .retry_delays()
                .len(),
            MAX_DUNNING_RETRY_STEPS
        );
        assert_eq!(
            DunningSchedule::new(vec![delay; MAX_DUNNING_RETRY_STEPS + 1]),
            Err(SubscriptionTermsError::TooManyDunningRetrySteps)
        );
    }

    #[test]
    fn paid_trial_and_recurring_currency_must_match() {
        let usd = CurrencyCode::new("USD").unwrap();
        let eur = CurrencyCode::new("EUR").unwrap();
        let recurring = RecurringSubscriptionTerms::new(
            ChargeAmount::new(1_000, usd).unwrap(),
            SubscriptionPeriodRule::calendar_months(1).unwrap(),
        );
        let trial = PaidTrialTerms::new(
            ChargeAmount::new(100, eur).unwrap(),
            SubscriptionPeriodRule::fixed_days(7).unwrap(),
        );
        assert_eq!(
            SubscriptionOffer::new(
                PlanKey::new("basic").unwrap(),
                recurring,
                SubscriptionStart::PaidTrial(trial),
                RenewalFailurePolicy::new(
                    DunningSchedule::default(),
                    DunningExhaustion::MarkUnpaid,
                    PastDueAccessPolicy::ContinueUntilDunningExhausted,
                ),
            ),
            Err(SubscriptionTermsError::CurrencyMismatch)
        );
    }

    #[test]
    fn persisted_policy_values_round_trip_exhaustively() {
        for value in DunningExhaustion::ALL {
            assert_eq!(value.as_str().parse(), Ok(value));
        }
        for value in PastDueAccessPolicy::ALL {
            assert_eq!(value.as_str().parse(), Ok(value));
        }
        for rule in [
            SubscriptionPeriodRule::fixed_days(7).unwrap(),
            SubscriptionPeriodRule::calendar_months(1).unwrap(),
        ] {
            assert_eq!(
                SubscriptionPeriodRule::from_kind_and_count(rule.as_str(), rule.count().get()),
                Ok(rule)
            );
        }
    }
}
