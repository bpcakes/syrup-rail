//! Fixed-shape persistence decoding for canonical subscription state.
//!
//! Callers retain ownership of their query text and any outer nullable-row
//! classification. This module only decodes the complete subscription
//! projection used by subscription reads and the shared scalar terms stored in
//! subscription and enrollment-attempt rows.

use chrono::{DateTime, Utc};
use sqlx::{Row, postgres::PgRow};
use syrup_rail::{
    BillingPeriod, ChargeAmount, CurrencyCode, DunningExhaustion, DunningRetryDelay,
    DunningSchedule, GatewayAccountMode, PastDueAccessPolicy, PaymentMethodId, PlanKey,
    RenewalFailurePolicy, Subscription, SubscriptionId, SubscriptionPeriodRule, SubscriptionPhase,
    SubscriptionStatus,
};
use thiserror::Error;
use uuid::Uuid;

/// Failure while decoding canonical subscription persistence state.
///
/// Row decoding errors remain distinct from values that PostgreSQL returned
/// successfully but cannot represent one valid domain value. Callers map this
/// boundary into their established operation-specific error contracts.
#[derive(Debug, Error)]
pub(crate) enum SubscriptionPersistenceCodecError {
    #[error("subscription persistence row could not be read")]
    RowRead(#[from] sqlx::Error),
    #[error("canonical subscription persistence state is invalid")]
    InvalidState,
}

/// Persisted scalar values for a subscription cadence.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SubscriptionPeriodRuleScalars<'a> {
    kind: &'a str,
    count: i32,
}

impl<'a> SubscriptionPeriodRuleScalars<'a> {
    pub(crate) const fn new(kind: &'a str, count: i32) -> Self {
        Self { kind, count }
    }
}

/// Persisted scalar values for automatic renewal-failure policy.
#[derive(Debug)]
pub(crate) struct RenewalFailurePolicyScalars<'a> {
    retry_delays_seconds: Vec<i64>,
    exhaustion: &'a str,
    past_due_access: &'a str,
}

impl<'a> RenewalFailurePolicyScalars<'a> {
    pub(crate) const fn new(
        retry_delays_seconds: Vec<i64>,
        exhaustion: &'a str,
        past_due_access: &'a str,
    ) -> Self {
        Self {
            retry_delays_seconds,
            exhaustion,
            past_due_access,
        }
    }
}

/// Decodes one persisted period rule after the caller has read its fixed
/// scalar projection.
pub(crate) fn subscription_period_rule_from_scalars(
    scalars: SubscriptionPeriodRuleScalars<'_>,
) -> Result<SubscriptionPeriodRule, SubscriptionPersistenceCodecError> {
    SubscriptionPeriodRule::from_kind_and_count(
        scalars.kind,
        u16::try_from(scalars.count)
            .map_err(|_| SubscriptionPersistenceCodecError::InvalidState)?,
    )
    .map_err(|_| SubscriptionPersistenceCodecError::InvalidState)
}

/// Decodes one persisted renewal-failure policy after the caller has read its
/// fixed scalar projection.
pub(crate) fn renewal_failure_policy_from_scalars(
    scalars: RenewalFailurePolicyScalars<'_>,
) -> Result<RenewalFailurePolicy, SubscriptionPersistenceCodecError> {
    let schedule = DunningSchedule::new(
        scalars
            .retry_delays_seconds
            .into_iter()
            .map(|seconds| {
                DunningRetryDelay::new(
                    u32::try_from(seconds)
                        .map_err(|_| SubscriptionPersistenceCodecError::InvalidState)?,
                )
                .map_err(|_| SubscriptionPersistenceCodecError::InvalidState)
            })
            .collect::<Result<Vec<_>, _>>()?,
    )
    .map_err(|_| SubscriptionPersistenceCodecError::InvalidState)?;
    Ok(RenewalFailurePolicy::new(
        schedule,
        scalars
            .exhaustion
            .parse::<DunningExhaustion>()
            .map_err(|_| SubscriptionPersistenceCodecError::InvalidState)?,
        scalars
            .past_due_access
            .parse::<PastDueAccessPolicy>()
            .map_err(|_| SubscriptionPersistenceCodecError::InvalidState)?,
    ))
}

/// Decodes the explicit complete subscription projection shared by
/// cancellation and applied-enrollment reads.
///
/// The input columns are deliberately fixed here. Joined or nullable
/// projections retain their shape validation in their owning callers.
pub(crate) fn subscription_from_row(
    row: &PgRow,
) -> Result<Subscription, SubscriptionPersistenceCodecError> {
    FullSubscriptionRow::read(row)?.into_subscription()
}

struct FullSubscriptionRow {
    id: Uuid,
    plan_key: String,
    status: String,
    required_gateway_account_mode: String,
    payment_method_id: Uuid,
    amount_cents: i32,
    currency: String,
    current_period_start_at: DateTime<Utc>,
    current_period_end_at: DateTime<Utc>,
    next_renewal_at: DateTime<Utc>,
    phase: String,
    recurring_period_kind: String,
    recurring_period_count: i32,
    dunning_retry_delays_seconds: Vec<i64>,
    dunning_exhaustion: String,
    past_due_access: String,
    next_payment_attempt_at: Option<DateTime<Utc>>,
}

impl FullSubscriptionRow {
    fn read(row: &PgRow) -> Result<Self, SubscriptionPersistenceCodecError> {
        Ok(Self {
            id: row.try_get("id")?,
            plan_key: row.try_get("plan_key")?,
            status: row.try_get("status")?,
            required_gateway_account_mode: row.try_get("required_gateway_account_mode")?,
            payment_method_id: row.try_get("payment_method_id")?,
            amount_cents: row.try_get("amount_cents")?,
            currency: row.try_get("currency")?,
            current_period_start_at: row.try_get("current_period_start_at")?,
            current_period_end_at: row.try_get("current_period_end_at")?,
            next_renewal_at: row.try_get("next_renewal_at")?,
            phase: row.try_get("phase")?,
            recurring_period_kind: row.try_get("recurring_period_kind")?,
            recurring_period_count: row.try_get("recurring_period_count")?,
            dunning_retry_delays_seconds: row.try_get("dunning_retry_delays_seconds")?,
            dunning_exhaustion: row.try_get("dunning_exhaustion")?,
            past_due_access: row.try_get("past_due_access")?,
            next_payment_attempt_at: row.try_get("next_payment_attempt_at")?,
        })
    }

    fn into_subscription(self) -> Result<Subscription, SubscriptionPersistenceCodecError> {
        let Self {
            id,
            plan_key,
            status,
            required_gateway_account_mode,
            payment_method_id,
            amount_cents,
            currency,
            current_period_start_at,
            current_period_end_at,
            next_renewal_at,
            phase,
            recurring_period_kind,
            recurring_period_count,
            dunning_retry_delays_seconds,
            dunning_exhaustion,
            past_due_access,
            next_payment_attempt_at,
        } = self;
        let recurring_period = subscription_period_rule_from_scalars(
            SubscriptionPeriodRuleScalars::new(&recurring_period_kind, recurring_period_count),
        )?;
        let renewal_failure =
            renewal_failure_policy_from_scalars(RenewalFailurePolicyScalars::new(
                dunning_retry_delays_seconds,
                &dunning_exhaustion,
                &past_due_access,
            ))?;

        Ok(Subscription::new(
            SubscriptionId::new(id),
            PlanKey::new(plan_key).map_err(|_| SubscriptionPersistenceCodecError::InvalidState)?,
            status
                .parse::<SubscriptionStatus>()
                .map_err(|_| SubscriptionPersistenceCodecError::InvalidState)?,
            phase
                .parse::<SubscriptionPhase>()
                .map_err(|_| SubscriptionPersistenceCodecError::InvalidState)?,
            required_gateway_account_mode
                .parse::<GatewayAccountMode>()
                .map_err(|_| SubscriptionPersistenceCodecError::InvalidState)?,
            PaymentMethodId::new(payment_method_id),
            ChargeAmount::new(
                amount_cents,
                CurrencyCode::new(&currency)
                    .map_err(|_| SubscriptionPersistenceCodecError::InvalidState)?,
            )
            .map_err(|_| SubscriptionPersistenceCodecError::InvalidState)?,
            recurring_period,
            renewal_failure,
            BillingPeriod::new(current_period_start_at, current_period_end_at)
                .map_err(|_| SubscriptionPersistenceCodecError::InvalidState)?,
            next_renewal_at,
            next_payment_attempt_at,
        ))
    }
}

#[cfg(test)]
mod tests {
    use syrup_rail::{DunningExhaustion, PastDueAccessPolicy, SubscriptionPeriodRule};

    use super::*;

    #[test]
    fn period_scalars_decode_fixed_days_and_calendar_months() {
        assert_eq!(
            subscription_period_rule_from_scalars(SubscriptionPeriodRuleScalars::new(
                "fixed_days",
                7,
            ))
            .expect("valid fixed-day persistence scalar"),
            SubscriptionPeriodRule::fixed_days(7).expect("valid fixed-day cadence"),
        );
        assert_eq!(
            subscription_period_rule_from_scalars(SubscriptionPeriodRuleScalars::new(
                "calendar_months",
                3,
            ))
            .expect("valid monthly persistence scalar"),
            SubscriptionPeriodRule::calendar_months(3).expect("valid monthly cadence"),
        );
    }

    #[test]
    fn period_scalars_reject_zero_negative_overflow_and_unknown_values() {
        for scalars in [
            SubscriptionPeriodRuleScalars::new("fixed_days", 0),
            SubscriptionPeriodRuleScalars::new("fixed_days", -1),
            SubscriptionPeriodRuleScalars::new("calendar_months", i32::MAX),
            SubscriptionPeriodRuleScalars::new("billing_weeks", 1),
        ] {
            assert!(matches!(
                subscription_period_rule_from_scalars(scalars),
                Err(SubscriptionPersistenceCodecError::InvalidState)
            ));
        }
    }

    #[test]
    fn renewal_failure_scalars_decode_valid_policy() {
        let policy = renewal_failure_policy_from_scalars(RenewalFailurePolicyScalars::new(
            vec![60, 3_600],
            "mark_unpaid",
            "continue_until_dunning_exhausted",
        ))
        .expect("valid dunning policy");

        assert_eq!(policy.exhaustion(), DunningExhaustion::MarkUnpaid);
        assert_eq!(
            policy.past_due_access(),
            PastDueAccessPolicy::ContinueUntilDunningExhausted
        );
        assert_eq!(
            policy
                .schedule()
                .retry_delays()
                .iter()
                .map(|delay| delay.seconds().get())
                .collect::<Vec<_>>(),
            vec![60, 3_600]
        );
    }

    #[test]
    fn renewal_failure_scalars_reject_invalid_delay_and_policy_values() {
        for retry_delays_seconds in [vec![0], vec![-1], vec![i64::from(u32::MAX) + 1]] {
            assert!(matches!(
                renewal_failure_policy_from_scalars(RenewalFailurePolicyScalars::new(
                    retry_delays_seconds,
                    "mark_unpaid",
                    "suspend_immediately",
                )),
                Err(SubscriptionPersistenceCodecError::InvalidState)
            ));
        }
        for (exhaustion, past_due_access) in [
            ("unknown", "suspend_immediately"),
            ("mark_unpaid", "unknown"),
        ] {
            assert!(matches!(
                renewal_failure_policy_from_scalars(RenewalFailurePolicyScalars::new(
                    vec![60],
                    exhaustion,
                    past_due_access,
                )),
                Err(SubscriptionPersistenceCodecError::InvalidState)
            ));
        }
        assert!(matches!(
            renewal_failure_policy_from_scalars(RenewalFailurePolicyScalars::new(
                vec![60; 17],
                "mark_unpaid",
                "suspend_immediately",
            )),
            Err(SubscriptionPersistenceCodecError::InvalidState)
        ));
    }
}
