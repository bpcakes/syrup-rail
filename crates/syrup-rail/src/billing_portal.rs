use std::fmt;

use chrono::{DateTime, Utc};
use thiserror::Error;

use crate::{
    BillingPeriod, BillingScopeId, Entitlement, Money, PaymentAttemptId, PaymentAttemptKind,
    PaymentAttemptStatus, PlanKey, SubscriberId, string_contains_raw_card_data,
};

/// Exact subscriber and plan identity for a customer-facing billing read.
///
/// Hosts must authenticate and authorize this identity before using it. This
/// query value does not create an authorization boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubscriptionBillingPortalQuery {
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    plan_key: PlanKey,
}

impl SubscriptionBillingPortalQuery {
    pub const fn new(
        billing_scope_id: BillingScopeId,
        subscriber_id: SubscriberId,
        plan_key: PlanKey,
    ) -> Self {
        Self {
            billing_scope_id,
            subscriber_id,
            plan_key,
        }
    }

    pub const fn billing_scope_id(&self) -> BillingScopeId {
        self.billing_scope_id
    }

    pub const fn subscriber_id(&self) -> SubscriberId {
        self.subscriber_id
    }

    pub const fn plan_key(&self) -> &PlanKey {
        &self.plan_key
    }
}

/// A customer-renderable, masked display of the stored card for a current
/// subscription.
///
/// This intentionally excludes every provider payment-method reference and
/// contains only the card presentation fields needed by an ordinary billing
/// portal. Accessors expose those values deliberately; ordinary formatting
/// remains value-free.
#[derive(Clone, Eq, PartialEq)]
pub struct SubscriptionPaymentMethodDisplay {
    card_brand: Option<String>,
    card_last_four: Option<String>,
    card_expiration_month: Option<u8>,
    card_expiration_year: Option<u16>,
}

impl SubscriptionPaymentMethodDisplay {
    pub fn new(
        card_brand: Option<String>,
        card_last_four: Option<String>,
        card_expiration_month: Option<u8>,
        card_expiration_year: Option<u16>,
    ) -> Result<Self, SubscriptionPaymentMethodDisplayError> {
        if card_brand
            .as_deref()
            .is_some_and(string_contains_raw_card_data)
        {
            return Err(SubscriptionPaymentMethodDisplayError::CardBrandContainsRawCardData);
        }
        if card_last_four.as_deref().is_some_and(|value| {
            value.len() != 4 || !value.bytes().all(|byte| byte.is_ascii_digit())
        }) {
            return Err(SubscriptionPaymentMethodDisplayError::InvalidCardLastFour);
        }
        if card_expiration_month.is_some_and(|value| !(1..=12).contains(&value)) {
            return Err(SubscriptionPaymentMethodDisplayError::InvalidExpirationMonth);
        }
        if card_expiration_year.is_some_and(|value| value < 2000) {
            return Err(SubscriptionPaymentMethodDisplayError::InvalidExpirationYear);
        }
        Ok(Self {
            card_brand,
            card_last_four,
            card_expiration_month,
            card_expiration_year,
        })
    }

    pub fn card_brand(&self) -> Option<&str> {
        self.card_brand.as_deref()
    }

    pub fn card_last_four(&self) -> Option<&str> {
        self.card_last_four.as_deref()
    }

    pub const fn card_expiration_month(&self) -> Option<u8> {
        self.card_expiration_month
    }

    pub const fn card_expiration_year(&self) -> Option<u16> {
        self.card_expiration_year
    }
}

impl fmt::Debug for SubscriptionPaymentMethodDisplay {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SubscriptionPaymentMethodDisplay")
            .field("has_card_brand", &self.card_brand.is_some())
            .field("has_card_last_four", &self.card_last_four.is_some())
            .field(
                "has_card_expiration_month",
                &self.card_expiration_month.is_some(),
            )
            .field(
                "has_card_expiration_year",
                &self.card_expiration_year.is_some(),
            )
            .finish()
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum SubscriptionPaymentMethodDisplayError {
    #[error("subscription payment-method card brand cannot contain raw card data")]
    CardBrandContainsRawCardData,
    #[error("subscription payment-method card last four must contain exactly four ASCII digits")]
    InvalidCardLastFour,
    #[error("subscription payment-method expiration month must be between 1 and 12")]
    InvalidExpirationMonth,
    #[error("subscription payment-method expiration year must be at least 2000")]
    InvalidExpirationYear,
}

/// A provider-neutral, customer-facing billing projection for one exact plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubscriptionBillingPortalSnapshot {
    entitlement: Entitlement,
    payment_method_display: Option<SubscriptionPaymentMethodDisplay>,
}

impl SubscriptionBillingPortalSnapshot {
    pub const fn new(
        entitlement: Entitlement,
        payment_method_display: Option<SubscriptionPaymentMethodDisplay>,
    ) -> Self {
        Self {
            entitlement,
            payment_method_display,
        }
    }

    pub const fn entitlement(&self) -> &Entitlement {
        &self.entitlement
    }

    pub const fn payment_method_display(&self) -> Option<&SubscriptionPaymentMethodDisplay> {
        self.payment_method_display.as_ref()
    }
}

/// Checked number of subscription payment-history entries in one page.
pub const SUBSCRIPTION_PAYMENT_HISTORY_PAGE_LIMIT: i64 = 100;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SubscriptionPaymentHistoryPageLimit(i64);

impl SubscriptionPaymentHistoryPageLimit {
    pub fn new(value: i64) -> Result<Self, SubscriptionPaymentHistoryPageLimitError> {
        if !(1..=SUBSCRIPTION_PAYMENT_HISTORY_PAGE_LIMIT).contains(&value) {
            return Err(SubscriptionPaymentHistoryPageLimitError);
        }
        Ok(Self(value))
    }

    pub const fn get(self) -> i64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("subscription payment-history page limit must be between 1 and 100")]
pub struct SubscriptionPaymentHistoryPageLimitError;

/// Continuation key returned by a prior subscription payment-history page.
///
/// The key is a bound value, never SQL text. The PostgreSQL reader applies it
/// strictly after the preceding row in descending `(created_at, id)` order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SubscriptionPaymentHistoryCursor {
    created_at: DateTime<Utc>,
    payment_attempt_id: PaymentAttemptId,
}

impl SubscriptionPaymentHistoryCursor {
    pub const fn new(created_at: DateTime<Utc>, payment_attempt_id: PaymentAttemptId) -> Self {
        Self {
            created_at,
            payment_attempt_id,
        }
    }

    pub const fn created_at(self) -> DateTime<Utc> {
        self.created_at
    }

    pub const fn payment_attempt_id(self) -> PaymentAttemptId {
        self.payment_attempt_id
    }
}

/// One safe, provider-neutral subscription payment attempt for a billing portal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubscriptionPaymentHistoryItem {
    payment_attempt_id: PaymentAttemptId,
    kind: PaymentAttemptKind,
    status: PaymentAttemptStatus,
    amount: Money,
    billing_period: Option<BillingPeriod>,
    submitted_at: Option<DateTime<Utc>>,
    resolved_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
}

impl SubscriptionPaymentHistoryItem {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        payment_attempt_id: PaymentAttemptId,
        kind: PaymentAttemptKind,
        status: PaymentAttemptStatus,
        amount: Money,
        billing_period: Option<BillingPeriod>,
        submitted_at: Option<DateTime<Utc>>,
        resolved_at: Option<DateTime<Utc>>,
        created_at: DateTime<Utc>,
    ) -> Result<Self, SubscriptionPaymentHistoryItemError> {
        if kind == PaymentAttemptKind::HostCharge {
            return Err(SubscriptionPaymentHistoryItemError);
        }
        Ok(Self {
            payment_attempt_id,
            kind,
            status,
            amount,
            billing_period,
            submitted_at,
            resolved_at,
            created_at,
        })
    }

    pub const fn payment_attempt_id(&self) -> PaymentAttemptId {
        self.payment_attempt_id
    }

    pub const fn kind(&self) -> PaymentAttemptKind {
        self.kind
    }

    pub const fn status(&self) -> PaymentAttemptStatus {
        self.status
    }

    pub const fn amount(&self) -> Money {
        self.amount
    }

    pub const fn billing_period(&self) -> Option<&BillingPeriod> {
        self.billing_period.as_ref()
    }

    pub const fn submitted_at(&self) -> Option<DateTime<Utc>> {
        self.submitted_at
    }

    pub const fn resolved_at(&self) -> Option<DateTime<Utc>> {
        self.resolved_at
    }

    pub const fn created_at(&self) -> DateTime<Utc> {
        self.created_at
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("subscription payment history cannot contain a host charge")]
pub struct SubscriptionPaymentHistoryItemError;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubscriptionPaymentHistoryPage {
    items: Vec<SubscriptionPaymentHistoryItem>,
    next_cursor: Option<SubscriptionPaymentHistoryCursor>,
}

impl SubscriptionPaymentHistoryPage {
    pub fn new(
        items: Vec<SubscriptionPaymentHistoryItem>,
        next_cursor: Option<SubscriptionPaymentHistoryCursor>,
    ) -> Self {
        Self { items, next_cursor }
    }

    pub fn items(&self) -> &[SubscriptionPaymentHistoryItem] {
        &self.items
    }

    pub fn into_items(self) -> Vec<SubscriptionPaymentHistoryItem> {
        self.items
    }

    pub const fn next_cursor(&self) -> Option<SubscriptionPaymentHistoryCursor> {
        self.next_cursor
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::*;
    use crate::{CurrencyCode, PaymentAttemptId};
    use uuid::Uuid;

    #[test]
    fn payment_method_display_exposes_values_only_through_accessors_and_redacts_debug() {
        let display = SubscriptionPaymentMethodDisplay::new(
            Some("Visa".to_owned()),
            Some("4242".to_owned()),
            Some(12),
            Some(2031),
        )
        .expect("valid masked card display");

        assert_eq!(display.card_brand(), Some("Visa"));
        assert_eq!(display.card_last_four(), Some("4242"));
        assert_eq!(display.card_expiration_month(), Some(12));
        assert_eq!(display.card_expiration_year(), Some(2031));

        let debug = format!("{display:?}");
        for value in ["Visa", "4242", "12", "2031"] {
            assert!(
                !debug.contains(value),
                "masked payment-method value appeared in Debug: {debug}"
            );
        }
        assert!(debug.contains("has_card_brand: true"));
        assert!(debug.contains("has_card_last_four: true"));
    }

    #[test]
    fn payment_method_display_rejects_raw_or_invalid_card_presentation() {
        assert_eq!(
            SubscriptionPaymentMethodDisplay::new(
                Some("4111111111111111".to_owned()),
                Some("4242".to_owned()),
                None,
                None,
            ),
            Err(SubscriptionPaymentMethodDisplayError::CardBrandContainsRawCardData)
        );
        assert_eq!(
            SubscriptionPaymentMethodDisplay::new(None, Some("42".to_owned()), None, None),
            Err(SubscriptionPaymentMethodDisplayError::InvalidCardLastFour)
        );
        assert_eq!(
            SubscriptionPaymentMethodDisplay::new(None, None, Some(13), None),
            Err(SubscriptionPaymentMethodDisplayError::InvalidExpirationMonth)
        );
        assert_eq!(
            SubscriptionPaymentMethodDisplay::new(None, None, None, Some(1999)),
            Err(SubscriptionPaymentMethodDisplayError::InvalidExpirationYear)
        );
    }

    #[test]
    fn payment_history_limit_and_host_charge_boundary_are_checked() {
        assert_eq!(
            SubscriptionPaymentHistoryPageLimit::new(0),
            Err(SubscriptionPaymentHistoryPageLimitError)
        );
        assert_eq!(
            SubscriptionPaymentHistoryPageLimit::new(1)
                .expect("lower boundary")
                .get(),
            1
        );
        assert_eq!(
            SubscriptionPaymentHistoryPageLimit::new(SUBSCRIPTION_PAYMENT_HISTORY_PAGE_LIMIT)
                .expect("upper boundary")
                .get(),
            SUBSCRIPTION_PAYMENT_HISTORY_PAGE_LIMIT
        );
        assert_eq!(
            SubscriptionPaymentHistoryPageLimit::new(SUBSCRIPTION_PAYMENT_HISTORY_PAGE_LIMIT + 1),
            Err(SubscriptionPaymentHistoryPageLimitError)
        );

        let at = Utc.with_ymd_and_hms(2026, 8, 11, 12, 0, 0).unwrap();
        let amount = Money::new(500, CurrencyCode::new("USD").unwrap()).unwrap();
        assert_eq!(
            SubscriptionPaymentHistoryItem::new(
                PaymentAttemptId::new(Uuid::now_v7()),
                PaymentAttemptKind::HostCharge,
                PaymentAttemptStatus::Approved,
                amount,
                None,
                Some(at),
                Some(at),
                at,
            ),
            Err(SubscriptionPaymentHistoryItemError)
        );
    }
}
