use chrono::{DateTime, Utc};

use crate::{
    BillingPeriod, BillingScopeId, CardLastFour, ChargeAmount, GatewayDiagnostic,
    HostChargeTargetId, PaymentAttemptId, PlanKey, SubscriberId, SubscriptionId, SubscriptionPhase,
};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct BillingEventSubject {
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
}

impl BillingEventSubject {
    pub const fn new(billing_scope_id: BillingScopeId, subscriber_id: SubscriberId) -> Self {
        Self {
            billing_scope_id,
            subscriber_id,
        }
    }

    pub const fn billing_scope_id(self) -> BillingScopeId {
        self.billing_scope_id
    }

    pub const fn subscriber_id(self) -> SubscriberId {
        self.subscriber_id
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct PaymentCardDisplay {
    brand: GatewayDiagnostic,
    last_four: CardLastFour,
}

impl PaymentCardDisplay {
    pub const fn new(brand: GatewayDiagnostic, last_four: CardLastFour) -> Self {
        Self { brand, last_four }
    }

    pub const fn brand(&self) -> &GatewayDiagnostic {
        &self.brand
    }

    pub const fn last_four(&self) -> &CardLastFour {
        &self.last_four
    }
}

impl std::fmt::Debug for PaymentCardDisplay {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PaymentCardDisplay")
            .field("has_brand", &true)
            .field("has_last_four", &true)
            .finish()
    }
}

impl std::fmt::Display for PaymentCardDisplay {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("[redacted]")
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum BillingEventKey {
    SubscriptionStarted(SubscriptionId),
    SubscriptionRenewed(PaymentAttemptId),
    SubscriptionPaymentFailed(PaymentAttemptId),
    SubscriptionEnded(SubscriptionId),
    SubscriptionCanceled(SubscriptionId),
    PaymentMethodChanged(PaymentAttemptId),
    HostChargePaid(HostChargeTargetId),
}

/// The durable scheduler or lifecycle consequence of a subscription payment
/// failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubscriptionPaymentFailureDisposition {
    /// Automatic dunning remains open and will retry at `retry_at`.
    RetryScheduled { retry_at: DateTime<Utc> },
    /// Automatic dunning ended at `exhausted_at`, but the subscription remains
    /// past due rather than ending.
    ///
    /// Under [`crate::PastDueAccessPolicy::ContinueUntilDunningExhausted`],
    /// hosts that mirror product access must treat this as the access-revocation
    /// signal. No [`BillingEvent::SubscriptionEnded`] event follows it.
    DunningExhausted { exhausted_at: DateTime<Utc> },
    /// Nonpayment ended the subscription at `ended_at`; a matching
    /// [`BillingEvent::SubscriptionEnded`] follows in the same transaction.
    SubscriptionEnded { ended_at: DateTime<Utc> },
}

/// The canonical product-access fact immediately after a subscription payment
/// failure is applied.
///
/// This is an outcome derived from the subscription's snapshotted access
/// policy and durable failure history. Event consumers must use this value
/// rather than reinterpret current offer configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubscriptionPaymentFailureAccess {
    /// Product access remains available while automatic dunning is scheduled.
    ContinuesDuringDunning,
    /// Product access ended at the durable causal boundary.
    Ended { access_ended_at: DateTime<Utc> },
}

impl SubscriptionPaymentFailureAccess {
    /// Returns whether this failure leaves product access available.
    pub const fn permits_product_access(self) -> bool {
        matches!(self, Self::ContinuesDuringDunning)
    }

    /// Returns the durable access boundary when product access has ended.
    pub const fn access_ended_at(self) -> Option<DateTime<Utc>> {
        match self {
            Self::ContinuesDuringDunning => None,
            Self::Ended { access_ended_at } => Some(access_ended_at),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SubscriptionEndReason {
    NonPayment,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BillingEvent {
    SubscriptionStarted {
        attempt_id: PaymentAttemptId,
        subscription_id: SubscriptionId,
        plan_key: PlanKey,
        charge: ChargeAmount,
        period: BillingPeriod,
        phase: SubscriptionPhase,
    },
    SubscriptionRenewed {
        attempt_id: PaymentAttemptId,
        subscription_id: SubscriptionId,
        plan_key: PlanKey,
        charge: ChargeAmount,
        period: BillingPeriod,
    },
    SubscriptionPaymentFailed {
        attempt_id: PaymentAttemptId,
        subscription_id: SubscriptionId,
        plan_key: PlanKey,
        disposition: SubscriptionPaymentFailureDisposition,
        access: SubscriptionPaymentFailureAccess,
    },
    SubscriptionEnded {
        attempt_id: PaymentAttemptId,
        subscription_id: SubscriptionId,
        plan_key: PlanKey,
        reason: SubscriptionEndReason,
        ended_at: DateTime<Utc>,
        access_ends_at: DateTime<Utc>,
    },
    SubscriptionCanceled {
        subscription_id: SubscriptionId,
        plan_key: PlanKey,
        access_ends_at: DateTime<Utc>,
    },
    PaymentMethodChanged {
        attempt_id: PaymentAttemptId,
        subscription_id: SubscriptionId,
        plan_key: PlanKey,
        card: Option<PaymentCardDisplay>,
    },
    HostChargePaid {
        attempt_id: PaymentAttemptId,
        target_id: HostChargeTargetId,
        charge: ChargeAmount,
    },
}

impl BillingEvent {
    pub const fn semantic_key(&self) -> BillingEventKey {
        match self {
            Self::SubscriptionStarted {
                subscription_id, ..
            } => BillingEventKey::SubscriptionStarted(*subscription_id),
            Self::SubscriptionRenewed { attempt_id, .. } => {
                BillingEventKey::SubscriptionRenewed(*attempt_id)
            }
            Self::SubscriptionPaymentFailed { attempt_id, .. } => {
                BillingEventKey::SubscriptionPaymentFailed(*attempt_id)
            }
            Self::SubscriptionEnded {
                subscription_id, ..
            } => BillingEventKey::SubscriptionEnded(*subscription_id),
            Self::SubscriptionCanceled {
                subscription_id, ..
            } => BillingEventKey::SubscriptionCanceled(*subscription_id),
            Self::PaymentMethodChanged { attempt_id, .. } => {
                BillingEventKey::PaymentMethodChanged(*attempt_id)
            }
            Self::HostChargePaid { target_id, .. } => BillingEventKey::HostChargePaid(*target_id),
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use uuid::Uuid;

    use super::*;
    use crate::CurrencyCode;

    fn attempt(value: u128) -> PaymentAttemptId {
        PaymentAttemptId::new(Uuid::from_u128(value))
    }

    fn subscription(value: u128) -> SubscriptionId {
        SubscriptionId::new(Uuid::from_u128(value))
    }

    #[test]
    fn every_event_derives_the_engine_owned_semantic_key() {
        let plan_key = PlanKey::new("base_subscription").unwrap();
        let usd = CurrencyCode::new("USD").unwrap();
        let charge = ChargeAmount::new(1_000, usd).unwrap();
        let start = Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap();
        let period = BillingPeriod::new(start, end).unwrap();
        let events = [
            (
                BillingEvent::SubscriptionStarted {
                    attempt_id: attempt(1),
                    subscription_id: subscription(2),
                    plan_key: plan_key.clone(),
                    charge,
                    period: period.clone(),
                    phase: SubscriptionPhase::Recurring,
                },
                BillingEventKey::SubscriptionStarted(subscription(2)),
            ),
            (
                BillingEvent::SubscriptionRenewed {
                    attempt_id: attempt(3),
                    subscription_id: subscription(2),
                    plan_key: plan_key.clone(),
                    charge,
                    period,
                },
                BillingEventKey::SubscriptionRenewed(attempt(3)),
            ),
            (
                BillingEvent::SubscriptionPaymentFailed {
                    attempt_id: attempt(4),
                    subscription_id: subscription(2),
                    plan_key: plan_key.clone(),
                    disposition: SubscriptionPaymentFailureDisposition::RetryScheduled {
                        retry_at: end,
                    },
                    access: SubscriptionPaymentFailureAccess::ContinuesDuringDunning,
                },
                BillingEventKey::SubscriptionPaymentFailed(attempt(4)),
            ),
            (
                BillingEvent::SubscriptionEnded {
                    attempt_id: attempt(4),
                    subscription_id: subscription(2),
                    plan_key: plan_key.clone(),
                    reason: SubscriptionEndReason::NonPayment,
                    ended_at: end,
                    access_ends_at: end,
                },
                BillingEventKey::SubscriptionEnded(subscription(2)),
            ),
            (
                BillingEvent::SubscriptionCanceled {
                    subscription_id: subscription(2),
                    plan_key: plan_key.clone(),
                    access_ends_at: end,
                },
                BillingEventKey::SubscriptionCanceled(subscription(2)),
            ),
            (
                BillingEvent::PaymentMethodChanged {
                    attempt_id: attempt(5),
                    subscription_id: subscription(2),
                    plan_key,
                    card: None,
                },
                BillingEventKey::PaymentMethodChanged(attempt(5)),
            ),
            (
                BillingEvent::HostChargePaid {
                    attempt_id: attempt(6),
                    target_id: HostChargeTargetId::new(Uuid::from_u128(7)),
                    charge,
                },
                BillingEventKey::HostChargePaid(HostChargeTargetId::new(Uuid::from_u128(7))),
            ),
        ];

        for (event, expected) in events {
            assert_eq!(event.semantic_key(), expected);
        }
    }

    #[test]
    fn card_display_formatting_never_reveals_provider_values() {
        let card = PaymentCardDisplay::new(
            GatewayDiagnostic::new("visa_sentinel"),
            CardLastFour::from_provider("1234").unwrap(),
        );
        let debug = format!("{card:?}");
        assert!(!debug.contains("visa_sentinel"));
        assert!(!debug.contains("1234"));
        assert_eq!(card.to_string(), "[redacted]");
    }

    #[test]
    fn payment_failure_access_is_a_self_contained_projection_fact() {
        let ended = Utc.with_ymd_and_hms(2026, 8, 2, 0, 0, 0).unwrap();

        assert!(SubscriptionPaymentFailureAccess::ContinuesDuringDunning.permits_product_access());
        assert_eq!(
            SubscriptionPaymentFailureAccess::ContinuesDuringDunning.access_ended_at(),
            None
        );

        let access = SubscriptionPaymentFailureAccess::Ended {
            access_ended_at: ended,
        };
        assert!(!access.permits_product_access());
        assert_eq!(access.access_ended_at(), Some(ended));
    }
}
