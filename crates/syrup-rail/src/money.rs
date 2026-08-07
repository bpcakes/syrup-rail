use std::{fmt, str::FromStr};

use chrono::{DateTime, Utc};
use thiserror::Error;

use crate::PlanKey;

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum CurrencyCodeError {
    #[error("currency code must contain exactly three uppercase ASCII characters")]
    Invalid,
}

#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CurrencyCode([u8; 3]);

impl CurrencyCode {
    pub fn new(value: &str) -> Result<Self, CurrencyCodeError> {
        let bytes: [u8; 3] = value
            .as_bytes()
            .try_into()
            .map_err(|_| CurrencyCodeError::Invalid)?;
        if !bytes.iter().all(u8::is_ascii_uppercase) {
            return Err(CurrencyCodeError::Invalid);
        }
        Ok(Self(bytes))
    }

    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.0).expect("validated ASCII currency code is UTF-8")
    }
}

impl fmt::Debug for CurrencyCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("CurrencyCode")
            .field(&self.as_str())
            .finish()
    }
}

impl fmt::Display for CurrencyCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for CurrencyCode {
    type Err = CurrencyCodeError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum MoneyError {
    #[error("money amount cannot be negative")]
    Negative,
    #[error("charge amount must be positive")]
    NonPositiveCharge,
    #[error("cumulative refund amount must be positive")]
    NonPositiveRefund,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Money {
    cents: i32,
    currency: CurrencyCode,
}

impl Money {
    pub fn new(cents: i32, currency: CurrencyCode) -> Result<Self, MoneyError> {
        if cents < 0 {
            return Err(MoneyError::Negative);
        }
        Ok(Self { cents, currency })
    }

    pub const fn cents(self) -> i32 {
        self.cents
    }

    pub const fn currency(self) -> CurrencyCode {
        self.currency
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ChargeAmount(Money);

impl ChargeAmount {
    pub fn new(cents: i32, currency: CurrencyCode) -> Result<Self, MoneyError> {
        if cents <= 0 {
            return Err(MoneyError::NonPositiveCharge);
        }
        Ok(Self(Money { cents, currency }))
    }

    pub const fn money(self) -> Money {
        self.0
    }

    pub const fn cents(self) -> i32 {
        self.0.cents()
    }

    pub const fn currency(self) -> CurrencyCode {
        self.0.currency()
    }

    pub const fn currency_code(&self) -> &CurrencyCode {
        &self.0.currency
    }
}

impl TryFrom<Money> for ChargeAmount {
    type Error = MoneyError;

    fn try_from(value: Money) -> Result<Self, Self::Error> {
        if value.cents() <= 0 {
            return Err(MoneyError::NonPositiveCharge);
        }
        Ok(Self(value))
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CumulativeRefundCents(i32);

impl CumulativeRefundCents {
    pub fn new(value: i32) -> Result<Self, MoneyError> {
        if value <= 0 {
            return Err(MoneyError::NonPositiveRefund);
        }
        Ok(Self(value))
    }

    pub const fn get(self) -> i32 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum BillingPeriodError {
    #[error("billing period end must be after its start")]
    EndNotAfterStart,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BillingPeriod {
    start_at: DateTime<Utc>,
    end_at: DateTime<Utc>,
}

impl BillingPeriod {
    pub fn new(start_at: DateTime<Utc>, end_at: DateTime<Utc>) -> Result<Self, BillingPeriodError> {
        if end_at <= start_at {
            return Err(BillingPeriodError::EndNotAfterStart);
        }
        Ok(Self { start_at, end_at })
    }

    pub const fn start_at(&self) -> &DateTime<Utc> {
        &self.start_at
    }

    pub const fn end_at(&self) -> &DateTime<Utc> {
        &self.end_at
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubscriptionOffer {
    plan_key: PlanKey,
    base_charge: ChargeAmount,
}

impl SubscriptionOffer {
    pub const fn new(plan_key: PlanKey, base_charge: ChargeAmount) -> Self {
        Self {
            plan_key,
            base_charge,
        }
    }

    pub const fn plan_key(&self) -> &PlanKey {
        &self.plan_key
    }

    pub const fn base_charge(&self) -> ChargeAmount {
        self.base_charge
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    #[test]
    fn currency_is_exactly_three_uppercase_ascii_bytes() {
        assert_eq!(CurrencyCode::new("USD").unwrap().as_str(), "USD");
        for invalid in ["", "US", "USDD", "usd", "U1D", "ÉUR"] {
            assert!(CurrencyCode::new(invalid).is_err(), "accepted {invalid:?}");
        }
    }

    #[test]
    fn ledger_money_and_charge_amount_have_distinct_zero_rules() {
        let usd = CurrencyCode::new("USD").unwrap();
        assert_eq!(Money::new(0, usd).unwrap().cents(), 0);
        assert_eq!(ChargeAmount::new(1, usd).unwrap().cents(), 1);
        assert_eq!(
            ChargeAmount::new(0, usd),
            Err(MoneyError::NonPositiveCharge)
        );
        assert_eq!(Money::new(-1, usd), Err(MoneyError::Negative));
    }

    #[test]
    fn billing_period_requires_a_strictly_later_end() {
        let start = Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap();
        assert!(BillingPeriod::new(start, end).is_ok());
        assert_eq!(
            BillingPeriod::new(start, start),
            Err(BillingPeriodError::EndNotAfterStart)
        );
    }
}
