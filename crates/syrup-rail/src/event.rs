#![warn(missing_docs)]

use chrono::{DateTime, Utc};

use crate::{
    BillingPeriod, BillingScopeId, CardLastFour, ChargeAmount, HostChargeTargetId,
    PaymentAttemptId, PaymentCardBrand, PlanKey, SubscriberId, SubscriptionId, SubscriptionPhase,
};

/// Host authorization identity associated with a billing event transaction.
///
/// The host supplies this exact scope/subscriber pair when it begins the
/// transaction and should retain it in the durable outbox envelope.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct BillingEventSubject {
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
}

impl BillingEventSubject {
    /// Creates the exact host subject for an event-producing transaction.
    pub const fn new(billing_scope_id: BillingScopeId, subscriber_id: SubscriberId) -> Self {
        Self {
            billing_scope_id,
            subscriber_id,
        }
    }

    /// Returns the host billing scope.
    pub const fn billing_scope_id(self) -> BillingScopeId {
        self.billing_scope_id
    }

    /// Returns the subscriber within the billing scope.
    pub const fn subscriber_id(self) -> SubscriberId {
        self.subscriber_id
    }
}

/// A value-redacted payment-card label safe for ordinary billing UI display.
///
/// The contained brand and last four digits are still explicit data and are
/// exposed only through accessors. `Debug` and `Display` never print them.
#[derive(Clone, Eq, PartialEq)]
pub struct PaymentCardDisplay {
    brand: PaymentCardBrand,
    last_four: CardLastFour,
}

impl PaymentCardDisplay {
    /// Creates a display value from sanitized provider card metadata.
    pub const fn new(brand: PaymentCardBrand, last_four: CardLastFour) -> Self {
        Self { brand, last_four }
    }

    /// Explicitly exposes the sanitized card brand.
    pub const fn brand(&self) -> &PaymentCardBrand {
        &self.brand
    }

    /// Explicitly exposes the validated last four digits.
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

/// Engine-owned idempotency identity for one semantic billing event.
///
/// Hosts should preserve both the variant and contained identifier when
/// constructing a unique outbox key. Debug output is not a wire format.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum BillingEventKey {
    /// The unique start event for a subscription lifecycle.
    SubscriptionStarted(SubscriptionId),
    /// The successful renewal event produced by a payment attempt.
    SubscriptionRenewed(PaymentAttemptId),
    /// The failure event produced by a payment attempt.
    SubscriptionPaymentFailed(PaymentAttemptId),
    /// The terminal nonpayment event for a subscription lifecycle.
    SubscriptionEnded(SubscriptionId),
    /// The voluntary cancellation event for a subscription lifecycle.
    SubscriptionCanceled(SubscriptionId),
    /// The stored-payment-method change event produced by an attempt.
    PaymentMethodChanged(PaymentAttemptId),
    /// The successful host-target charge event.
    HostChargePaid(HostChargeTargetId),
}

/// The durable scheduler or lifecycle consequence of a subscription payment
/// failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubscriptionPaymentFailureDisposition {
    /// Automatic dunning remains open and will retry at `retry_at`.
    RetryScheduled {
        /// Exact durable time at which automatic collection becomes eligible.
        retry_at: DateTime<Utc>,
    },
    /// Automatic dunning ended at `exhausted_at`, but the subscription remains
    /// past due rather than ending.
    ///
    /// Under [`crate::PastDueAccessPolicy::ContinueUntilDunningExhausted`],
    /// hosts that mirror product access must treat this as the access-revocation
    /// signal. No [`BillingEvent::SubscriptionEnded`] event follows it.
    DunningExhausted {
        /// Time at which the configured dunning schedule was exhausted.
        exhausted_at: DateTime<Utc>,
    },
    /// Nonpayment ended the subscription at `ended_at`; a matching
    /// [`BillingEvent::SubscriptionEnded`] follows in the same transaction.
    SubscriptionEnded {
        /// Time at which nonpayment made the subscription terminal.
        ended_at: DateTime<Utc>,
    },
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
    Ended {
        /// Durable causal boundary at which product access ended.
        access_ended_at: DateTime<Utc>,
    },
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

/// Complete scheduler, lifecycle, and product-access consequence of one
/// subscription payment failure.
///
/// The variants encode only states the billing engine can produce. In
/// particular, exhausted or terminal collection can never claim that product
/// access continues.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubscriptionPaymentFailureOutcome {
    /// Automatic collection remains scheduled. Product access may either
    /// continue during dunning or have ended at an earlier causal boundary.
    RetryScheduled {
        /// Exact durable time at which automatic collection becomes eligible.
        retry_at: DateTime<Utc>,
        /// Canonical product-access fact immediately after the failure.
        access: SubscriptionPaymentFailureAccess,
    },
    /// Automatic collection was exhausted while the subscription remains past
    /// due, and product access has ended.
    DunningExhausted {
        /// Time at which the configured dunning schedule was exhausted.
        exhausted_at: DateTime<Utc>,
        /// Durable causal boundary at which product access ended.
        access_ended_at: DateTime<Utc>,
    },
    /// Nonpayment ended the subscription and product access has ended.
    SubscriptionEnded {
        /// Time at which nonpayment made the subscription terminal.
        ended_at: DateTime<Utc>,
        /// Durable causal boundary at which product access ended.
        access_ended_at: DateTime<Utc>,
    },
}

impl SubscriptionPaymentFailureOutcome {
    /// Projects the legacy scheduler or lifecycle consequence.
    pub const fn disposition(self) -> SubscriptionPaymentFailureDisposition {
        match self {
            Self::RetryScheduled { retry_at, .. } => {
                SubscriptionPaymentFailureDisposition::RetryScheduled { retry_at }
            }
            Self::DunningExhausted { exhausted_at, .. } => {
                SubscriptionPaymentFailureDisposition::DunningExhausted { exhausted_at }
            }
            Self::SubscriptionEnded { ended_at, .. } => {
                SubscriptionPaymentFailureDisposition::SubscriptionEnded { ended_at }
            }
        }
    }

    /// Projects the legacy product-access consequence.
    pub const fn access(self) -> SubscriptionPaymentFailureAccess {
        match self {
            Self::RetryScheduled { access, .. } => access,
            Self::DunningExhausted {
                access_ended_at, ..
            }
            | Self::SubscriptionEnded {
                access_ended_at, ..
            } => SubscriptionPaymentFailureAccess::Ended { access_ended_at },
        }
    }
}

/// Provider-neutral reason that a subscription lifecycle ended.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SubscriptionEndReason {
    /// The configured automatic-collection policy ended the lifecycle.
    NonPayment,
}

/// Typed provider-neutral event emitted at an atomic billing boundary.
///
/// This is a closed domain enum rather than a serialized wire schema. Hosts
/// should map it exhaustively into a versioned host-owned outbox DTO; adding a
/// future variant will then force every mapper to make an explicit decision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BillingEvent {
    /// A newly activated subscription lifecycle.
    SubscriptionStarted {
        /// Payment attempt that established activation authority.
        attempt_id: PaymentAttemptId,
        /// Newly activated subscription.
        subscription_id: SubscriptionId,
        /// Host plan selected by the authorized request.
        plan_key: PlanKey,
        /// Provider-approved initial charge.
        charge: ChargeAmount,
        /// Initial paid or trial billing period.
        period: BillingPeriod,
        /// Activated introductory or recurring phase.
        phase: SubscriptionPhase,
    },
    /// An existing subscription advanced after an approved payment.
    SubscriptionRenewed {
        /// Payment attempt that approved the renewal or recovery.
        attempt_id: PaymentAttemptId,
        /// Renewed subscription.
        subscription_id: SubscriptionId,
        /// Exact plan owned by the subscription.
        plan_key: PlanKey,
        /// Provider-approved recurring charge.
        charge: ChargeAmount,
        /// Newly opened billing period.
        period: BillingPeriod,
    },
    /// A determinate automatic-renewal failure changed dunning state.
    SubscriptionPaymentFailed {
        /// Failed automatic-renewal attempt.
        attempt_id: PaymentAttemptId,
        /// Subscription whose collection failed.
        subscription_id: SubscriptionId,
        /// Exact plan owned by the subscription.
        plan_key: PlanKey,
        /// Complete scheduler, lifecycle, and product-access consequence.
        outcome: SubscriptionPaymentFailureOutcome,
    },
    /// Nonpayment made a subscription lifecycle terminal.
    SubscriptionEnded {
        /// Attempt whose failure exhausted collection authority.
        attempt_id: PaymentAttemptId,
        /// Terminal subscription lifecycle.
        subscription_id: SubscriptionId,
        /// Exact plan owned by the subscription.
        plan_key: PlanKey,
        /// Provider-neutral terminal reason.
        reason: SubscriptionEndReason,
        /// Time the financial lifecycle became terminal.
        ended_at: DateTime<Utc>,
        /// Causal boundary at which product access ends.
        access_ends_at: DateTime<Utc>,
    },
    /// A subscriber voluntarily canceled a subscription lifecycle.
    SubscriptionCanceled {
        /// Canceled subscription.
        subscription_id: SubscriptionId,
        /// Exact plan owned by the subscription.
        plan_key: PlanKey,
        /// Paid-through or immediate access boundary after cancellation.
        access_ends_at: DateTime<Utc>,
    },
    /// A subscription stored a newly approved payment method.
    PaymentMethodChanged {
        /// Payment-method replacement attempt.
        attempt_id: PaymentAttemptId,
        /// Subscription whose method changed.
        subscription_id: SubscriptionId,
        /// Exact plan owned by the subscription.
        plan_key: PlanKey,
        /// Optional value-redacted display metadata for the new card.
        card: Option<PaymentCardDisplay>,
    },
    /// A host-defined target was charged successfully.
    HostChargePaid {
        /// Canonical host-charge payment attempt.
        attempt_id: PaymentAttemptId,
        /// Host-owned target that received the charge.
        target_id: HostChargeTargetId,
        /// Provider-approved charge.
        charge: ChargeAmount,
    },
}

impl BillingEvent {
    /// Returns the engine-owned semantic idempotency key for this event.
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
                    outcome: SubscriptionPaymentFailureOutcome::RetryScheduled {
                        retry_at: end,
                        access: SubscriptionPaymentFailureAccess::ContinuesDuringDunning,
                    },
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
            PaymentCardBrand::Visa,
            CardLastFour::from_provider("1234").unwrap(),
        );
        let debug = format!("{card:?}");
        assert!(!debug.contains("Visa"));
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

    #[test]
    fn payment_failure_outcomes_project_exactly_the_four_valid_matrices() {
        let failed_at = Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap();
        let retry_at = Utc.with_ymd_and_hms(2026, 8, 2, 0, 0, 0).unwrap();
        let exhausted_at = Utc.with_ymd_and_hms(2026, 8, 3, 0, 0, 0).unwrap();
        let ended_at = Utc.with_ymd_and_hms(2026, 8, 4, 0, 0, 0).unwrap();
        let cases = [
            (
                SubscriptionPaymentFailureOutcome::RetryScheduled {
                    retry_at,
                    access: SubscriptionPaymentFailureAccess::ContinuesDuringDunning,
                },
                SubscriptionPaymentFailureDisposition::RetryScheduled { retry_at },
                SubscriptionPaymentFailureAccess::ContinuesDuringDunning,
            ),
            (
                SubscriptionPaymentFailureOutcome::RetryScheduled {
                    retry_at,
                    access: SubscriptionPaymentFailureAccess::Ended {
                        access_ended_at: failed_at,
                    },
                },
                SubscriptionPaymentFailureDisposition::RetryScheduled { retry_at },
                SubscriptionPaymentFailureAccess::Ended {
                    access_ended_at: failed_at,
                },
            ),
            (
                SubscriptionPaymentFailureOutcome::DunningExhausted {
                    exhausted_at,
                    access_ended_at: exhausted_at,
                },
                SubscriptionPaymentFailureDisposition::DunningExhausted { exhausted_at },
                SubscriptionPaymentFailureAccess::Ended {
                    access_ended_at: exhausted_at,
                },
            ),
            (
                SubscriptionPaymentFailureOutcome::SubscriptionEnded {
                    ended_at,
                    access_ended_at: failed_at,
                },
                SubscriptionPaymentFailureDisposition::SubscriptionEnded { ended_at },
                SubscriptionPaymentFailureAccess::Ended {
                    access_ended_at: failed_at,
                },
            ),
        ];

        for (outcome, disposition, access) in cases {
            assert_eq!(outcome.disposition(), disposition);
            assert_eq!(outcome.access(), access);
        }
    }
}
