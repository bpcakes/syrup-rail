use chrono::{DateTime, Months, Utc};
use thiserror::Error;

use crate::{BillingPeriod, BillingPeriodError};

pub fn gateway_state_is_approved(value: &str) -> bool {
    matches!(
        normalized_gateway_state_value(value).as_str(),
        "approved"
            | "complete"
            | "completed"
            | "captured"
            | "success"
            | "successful"
            | "pendingsettlement"
    )
}

pub fn gateway_response_is_approved(value: Option<&str>) -> bool {
    let Some(normalized) = value.map(normalized_gateway_state_value) else {
        return false;
    };
    matches!(normalized.as_str(), "1" | "100") || gateway_state_is_approved(&normalized)
}

fn normalized_gateway_state_value(value: &str) -> String {
    value
        .trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|character| {
            !character.is_ascii_whitespace() && *character != '_' && *character != '-'
        })
        .collect()
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum MonthlyBillingPeriodError {
    #[error("monthly billing period overflowed the supported date range")]
    Overflow,
    #[error(transparent)]
    InvalidPeriod(#[from] BillingPeriodError),
}

/// Advances one UTC calendar month from the previous period end.
///
/// Chrono clamps an invalid target day and the next advancement starts from
/// that clamped boundary. It deliberately does not restore the original day.
pub fn next_monthly_billing_period(
    previous_period_end_at: DateTime<Utc>,
) -> Result<BillingPeriod, MonthlyBillingPeriodError> {
    let next_end = previous_period_end_at
        .checked_add_months(Months::new(1))
        .ok_or(MonthlyBillingPeriodError::Overflow)?;
    BillingPeriod::new(previous_period_end_at, next_end).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Timelike};

    use super::*;

    #[test]
    fn approved_state_matching_preserves_processor_spellings() {
        for value in [
            "approved",
            "complete",
            "completed",
            "captured",
            "success",
            "successful",
            "pending settlement",
            "pending_settlement",
            "pending-settlement",
            "Pending Settlement",
        ] {
            assert!(gateway_state_is_approved(value), "missed {value}");
        }
        assert!(gateway_response_is_approved(Some("1")));
        assert!(gateway_response_is_approved(Some("100")));
        assert!(!gateway_response_is_approved(Some("200")));
        assert!(!gateway_response_is_approved(None));
    }

    #[test]
    fn monthly_policy_repeats_clamping_from_the_previous_boundary() {
        let january = Utc
            .with_ymd_and_hms(2025, 1, 31, 12, 34, 56)
            .unwrap()
            .with_nanosecond(789_000_000)
            .unwrap();
        let february = next_monthly_billing_period(january).unwrap();
        assert_eq!(
            *february.end_at(),
            Utc.with_ymd_and_hms(2025, 2, 28, 12, 34, 56)
                .unwrap()
                .with_nanosecond(789_000_000)
                .unwrap()
        );
        let march = next_monthly_billing_period(february.end_at().to_owned()).unwrap();
        assert_eq!(
            *march.end_at(),
            Utc.with_ymd_and_hms(2025, 3, 28, 12, 34, 56)
                .unwrap()
                .with_nanosecond(789_000_000)
                .unwrap()
        );
    }

    #[test]
    fn leap_year_clamping_is_equally_repeated() {
        let january = Utc.with_ymd_and_hms(2024, 1, 31, 0, 0, 0).unwrap();
        let february = next_monthly_billing_period(january).unwrap();
        assert_eq!(
            *february.end_at(),
            Utc.with_ymd_and_hms(2024, 2, 29, 0, 0, 0).unwrap()
        );
        let march = next_monthly_billing_period(february.end_at().to_owned()).unwrap();
        assert_eq!(
            *march.end_at(),
            Utc.with_ymd_and_hms(2024, 3, 29, 0, 0, 0).unwrap()
        );
    }
}
