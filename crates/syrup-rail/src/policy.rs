use chrono::{DateTime, Duration, Months, Utc};
use thiserror::Error;

use crate::{BillingPeriod, BillingPeriodError, SubscriptionPeriodRule};

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum BillingPeriodPolicyError {
    #[error("billing period overflowed the supported date range")]
    Overflow,
    #[error(transparent)]
    InvalidPeriod(#[from] BillingPeriodError),
}

/// Advances one checked fixed-day or calendar-month period from `start_at`.
///
/// Calendar-month advancement uses Chrono's clamping, and the next call starts
/// from that clamped boundary rather than restoring an earlier day of month.
pub fn next_billing_period(
    start_at: DateTime<Utc>,
    rule: SubscriptionPeriodRule,
) -> Result<BillingPeriod, BillingPeriodPolicyError> {
    let next_end = match rule {
        SubscriptionPeriodRule::FixedDays(days) => start_at
            .checked_add_signed(Duration::days(i64::from(days.get())))
            .ok_or(BillingPeriodPolicyError::Overflow)?,
        SubscriptionPeriodRule::CalendarMonths(months) => start_at
            .checked_add_months(Months::new(u32::from(months.get())))
            .ok_or(BillingPeriodPolicyError::Overflow)?,
    };
    BillingPeriod::new(start_at, next_end).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Timelike};

    use super::*;

    #[test]
    fn monthly_policy_repeats_clamping_from_the_previous_boundary() {
        let january = Utc
            .with_ymd_and_hms(2025, 1, 31, 12, 34, 56)
            .unwrap()
            .with_nanosecond(789_000_000)
            .unwrap();
        let monthly = SubscriptionPeriodRule::calendar_months(1).unwrap();
        let february = next_billing_period(january, monthly).unwrap();
        assert_eq!(
            *february.end_at(),
            Utc.with_ymd_and_hms(2025, 2, 28, 12, 34, 56)
                .unwrap()
                .with_nanosecond(789_000_000)
                .unwrap()
        );
        let march = next_billing_period(*february.end_at(), monthly).unwrap();
        assert_eq!(
            *march.end_at(),
            Utc.with_ymd_and_hms(2025, 3, 28, 12, 34, 56)
                .unwrap()
                .with_nanosecond(789_000_000)
                .unwrap()
        );
    }

    #[test]
    fn fixed_day_policy_adds_exact_utc_days() {
        let start = Utc.with_ymd_and_hms(2026, 3, 27, 12, 0, 0).unwrap();
        let period =
            next_billing_period(start, SubscriptionPeriodRule::fixed_days(7).unwrap()).unwrap();
        assert_eq!(*period.end_at(), start + Duration::days(7));
    }

    #[test]
    fn multi_month_policy_clamps_from_each_previous_boundary() {
        let january = Utc.with_ymd_and_hms(2025, 1, 31, 0, 0, 0).unwrap();
        let two_months =
            next_billing_period(january, SubscriptionPeriodRule::calendar_months(2).unwrap())
                .unwrap();
        assert_eq!(
            *two_months.end_at(),
            Utc.with_ymd_and_hms(2025, 3, 31, 0, 0, 0).unwrap()
        );
        let next = next_billing_period(
            *two_months.end_at(),
            SubscriptionPeriodRule::calendar_months(2).unwrap(),
        )
        .unwrap();
        assert_eq!(
            *next.end_at(),
            Utc.with_ymd_and_hms(2025, 5, 31, 0, 0, 0).unwrap()
        );
    }

    #[test]
    fn billing_period_overflow_is_typed() {
        let start = DateTime::<Utc>::MAX_UTC;
        assert_eq!(
            next_billing_period(start, SubscriptionPeriodRule::fixed_days(1).unwrap()),
            Err(BillingPeriodPolicyError::Overflow)
        );
    }

    #[test]
    fn leap_year_clamping_is_equally_repeated() {
        let january = Utc.with_ymd_and_hms(2024, 1, 31, 0, 0, 0).unwrap();
        let monthly = SubscriptionPeriodRule::calendar_months(1).unwrap();
        let february = next_billing_period(january, monthly).unwrap();
        assert_eq!(
            *february.end_at(),
            Utc.with_ymd_and_hms(2024, 2, 29, 0, 0, 0).unwrap()
        );
        let march = next_billing_period(*february.end_at(), monthly).unwrap();
        assert_eq!(
            *march.end_at(),
            Utc.with_ymd_and_hms(2024, 3, 29, 0, 0, 0).unwrap()
        );
    }
}
