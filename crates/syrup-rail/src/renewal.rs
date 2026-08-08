use chrono::{DateTime, Duration, Utc};
use thiserror::Error;

use crate::{
    BillingContactSnapshot, BillingPeriod, BillingScopeId, ChargeAmount, GatewayProviderKey,
    IdempotencyKey, PaymentAttempt, PaymentAttemptFingerprint, PaymentAttemptId,
    PaymentAttemptIdentity, PaymentAttemptKind, PaymentAttemptRequest, PaymentAttemptTarget,
    PaymentMethodId, PlanKey, ResolvedGateway, SubscriberId, SubscriptionId,
    SubscriptionPaymentStateSnapshot, SubscriptionStatus,
};

pub const RENEWAL_DISPATCH_LIMIT: i64 = 100;
pub const RENEWAL_RETRY_AFTER_SECONDS: i64 = 24 * 60 * 60;
pub const RENEWAL_PROVIDER_RATE_LIMIT_RETRY_AFTER_SECONDS: i64 = 60;
pub const MAX_RENEWAL_TERMINAL_ATTEMPTS_PER_PERIOD: i64 = 5;
pub const MAX_RENEWAL_INFRASTRUCTURE_ATTEMPTS_PER_PERIOD_CONFIGURATION: i64 = 8;
pub const RENEWAL_PROVIDER_RATE_LIMIT_FAST_RETRY_ATTEMPTS: i64 = 5;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenewalDispatch {
    billing_scope_id: BillingScopeId,
    subscription_id: SubscriptionId,
    period_start_at: DateTime<Utc>,
    attempt_sequence_count: i64,
}

impl RenewalDispatch {
    pub const fn new(
        billing_scope_id: BillingScopeId,
        subscription_id: SubscriptionId,
        period_start_at: DateTime<Utc>,
        attempt_sequence_count: i64,
    ) -> Self {
        Self {
            billing_scope_id,
            subscription_id,
            period_start_at,
            attempt_sequence_count,
        }
    }

    pub const fn billing_scope_id(&self) -> BillingScopeId {
        self.billing_scope_id
    }

    pub const fn subscription_id(&self) -> SubscriptionId {
        self.subscription_id
    }

    pub const fn period_start_at(&self) -> &DateTime<Utc> {
        &self.period_start_at
    }

    pub const fn attempt_sequence_count(&self) -> i64 {
        self.attempt_sequence_count
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChargeRenewal {
    billing_scope_id: BillingScopeId,
    subscription_id: SubscriptionId,
    period_start_at: DateTime<Utc>,
}

impl ChargeRenewal {
    pub const fn new(
        billing_scope_id: BillingScopeId,
        subscription_id: SubscriptionId,
        period_start_at: DateTime<Utc>,
    ) -> Self {
        Self {
            billing_scope_id,
            subscription_id,
            period_start_at,
        }
    }

    pub const fn billing_scope_id(&self) -> BillingScopeId {
        self.billing_scope_id
    }

    pub const fn subscription_id(&self) -> SubscriptionId {
        self.subscription_id
    }

    pub const fn period_start_at(&self) -> &DateTime<Utc> {
        &self.period_start_at
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RenewalAttemptState {
    pub attempt_sequence_count: i64,
    pub terminal_attempt_count: i64,
    pub automatic_infrastructure_attempt_count: i64,
    pub last_terminal_at: Option<DateTime<Utc>>,
    pub provider_rate_limited_attempt_count: i64,
    pub last_provider_rate_limited_at: Option<DateTime<Utc>>,
    pub has_blocking_attempt: bool,
}

impl RenewalAttemptState {
    pub fn blocks_automatic_retry(&self, now: DateTime<Utc>) -> bool {
        self.terminal_attempt_count >= MAX_RENEWAL_TERMINAL_ATTEMPTS_PER_PERIOD
            || self.automatic_infrastructure_attempt_count
                >= MAX_RENEWAL_INFRASTRUCTURE_ATTEMPTS_PER_PERIOD_CONFIGURATION
            || self.has_blocking_attempt
            || !retry_window_elapsed(self.last_terminal_at, now, RENEWAL_RETRY_AFTER_SECONDS)
            || !retry_window_elapsed(
                self.last_provider_rate_limited_at,
                now,
                provider_rate_limit_retry_after_seconds(self.provider_rate_limited_attempt_count),
            )
    }
}

pub const fn provider_rate_limit_retry_after_seconds(attempt_count: i64) -> i64 {
    if attempt_count >= RENEWAL_PROVIDER_RATE_LIMIT_FAST_RETRY_ATTEMPTS {
        RENEWAL_RETRY_AFTER_SECONDS
    } else {
        RENEWAL_PROVIDER_RATE_LIMIT_RETRY_AFTER_SECONDS
    }
}

fn retry_window_elapsed(
    last_attempt_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    retry_after_seconds: i64,
) -> bool {
    last_attempt_at.is_none_or(|last_attempt_at| {
        last_attempt_at <= now - Duration::seconds(retry_after_seconds)
    })
}

pub fn renewal_attempt_idempotency_key(
    subscription_id: SubscriptionId,
    period_start_at: DateTime<Utc>,
    attempt_sequence_count: i64,
) -> Result<IdempotencyKey, crate::IdempotencyKeyError> {
    IdempotencyKey::new(format!(
        "subscription-renewal:{subscription_id}:{}:{attempt_sequence_count}",
        period_start_at.timestamp()
    ))
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum SubscriptionRenewalReservationBuildError {
    #[error("resolved gateway identity does not match the renewal scope")]
    GatewayIdentityMismatch,
    #[error("only subscription-renewal attempts can become renewal reservations")]
    AttemptKindMismatch,
    #[error("subscription renewal has an invalid payment-state snapshot")]
    InvalidPaymentState,
    #[error("subscription renewal attempt has an invalid charge amount")]
    InvalidCharge,
    #[error("subscription renewal idempotency key is invalid")]
    InvalidIdempotencyKey,
}

/// Secret-free authority for one exact automatic recurring charge.
#[derive(Clone, Eq, PartialEq)]
pub struct SubscriptionRenewalReservation {
    identity: PaymentAttemptIdentity,
    provider_key: GatewayProviderKey,
    request: PaymentAttemptRequest,
}

impl SubscriptionRenewalReservation {
    #[allow(clippy::too_many_arguments)]
    pub fn from_locked_subscription(
        command: ChargeRenewal,
        gateway: &ResolvedGateway,
        attempt_id: PaymentAttemptId,
        subscriber_id: SubscriberId,
        plan_key: PlanKey,
        payment_method_id: PaymentMethodId,
        initial_transaction_id: crate::GatewayTransactionId,
        status: SubscriptionStatus,
        period: BillingPeriod,
        charge: ChargeAmount,
        attempt_sequence_count: i64,
    ) -> Result<Self, SubscriptionRenewalReservationBuildError> {
        if gateway.billing_scope_id() != command.billing_scope_id() {
            return Err(SubscriptionRenewalReservationBuildError::GatewayIdentityMismatch);
        }
        let identity = PaymentAttemptIdentity::new(
            attempt_id,
            command.billing_scope_id(),
            subscriber_id,
            gateway.gateway_account_id(),
            gateway.gateway_configuration_id(),
        );
        let expected_state = SubscriptionPaymentStateSnapshot::new(
            command.subscription_id(),
            payment_method_id,
            initial_transaction_id,
            status,
        )
        .map_err(|_| SubscriptionRenewalReservationBuildError::InvalidPaymentState)?;
        let idempotency_key = renewal_attempt_idempotency_key(
            command.subscription_id(),
            *command.period_start_at(),
            attempt_sequence_count,
        )
        .map_err(|_| SubscriptionRenewalReservationBuildError::InvalidIdempotencyKey)?;
        let fingerprint = PaymentAttemptFingerprint::for_subscription_renewal(
            &plan_key,
            command.subscription_id(),
            payment_method_id,
            *period.start_at(),
            charge.money(),
        );
        let target = PaymentAttemptTarget::SubscriptionRenewal {
            plan_key,
            payment_method_id,
            period,
            expected_state,
        };
        let request = PaymentAttemptRequest::new(
            target,
            idempotency_key,
            fingerprint,
            charge.money(),
            gateway
                .mutation_reference_factory()
                .for_attempt(PaymentAttemptKind::SubscriptionRenewal, attempt_id),
            BillingContactSnapshot::new(None, None),
        );
        Ok(Self {
            identity,
            provider_key: gateway.provider_key().clone(),
            request,
        })
    }

    pub fn from_attempt(
        attempt: &PaymentAttempt,
        provider_key: GatewayProviderKey,
    ) -> Result<Self, SubscriptionRenewalReservationBuildError> {
        let PaymentAttemptTarget::SubscriptionRenewal {
            plan_key,
            payment_method_id,
            period,
            expected_state,
        } = attempt.request().target()
        else {
            return Err(SubscriptionRenewalReservationBuildError::AttemptKindMismatch);
        };
        ChargeAmount::try_from(attempt.request().amount())
            .map_err(|_| SubscriptionRenewalReservationBuildError::InvalidCharge)?;
        if !attempt
            .request()
            .fingerprint()
            .matches_subscription_renewal(
                plan_key,
                expected_state.subscription_id(),
                *payment_method_id,
                *period.start_at(),
                attempt.request().amount(),
            )
        {
            return Err(SubscriptionRenewalReservationBuildError::AttemptKindMismatch);
        }
        Ok(Self {
            identity: attempt.identity(),
            provider_key,
            request: attempt.request().clone(),
        })
    }

    pub const fn identity(&self) -> PaymentAttemptIdentity {
        self.identity
    }

    pub const fn provider_key(&self) -> &GatewayProviderKey {
        &self.provider_key
    }

    pub const fn request(&self) -> &PaymentAttemptRequest {
        &self.request
    }

    pub const fn plan_key(&self) -> &PlanKey {
        match self.request.target().plan_key() {
            Some(plan_key) => plan_key,
            None => unreachable!(),
        }
    }

    pub const fn subscription_id(&self) -> SubscriptionId {
        match self.request.target().subscription_id() {
            Some(subscription_id) => subscription_id,
            None => unreachable!(),
        }
    }

    pub const fn period(&self) -> &BillingPeriod {
        match self.request.target().period() {
            Some(period) => period,
            None => unreachable!(),
        }
    }

    pub const fn expected_state(&self) -> &SubscriptionPaymentStateSnapshot {
        match self.request.target().subscription_payment_state_snapshot() {
            Some(expected_state) => expected_state,
            None => unreachable!(),
        }
    }
}

impl std::fmt::Debug for SubscriptionRenewalReservation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SubscriptionRenewalReservation")
            .field("identity", &self.identity)
            .field("provider_key", &self.provider_key)
            .field("request", &self.request)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_boundaries_are_inclusive() {
        let now = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let mut state = RenewalAttemptState {
            last_terminal_at: Some(now - Duration::seconds(RENEWAL_RETRY_AFTER_SECONDS)),
            ..RenewalAttemptState::default()
        };
        assert!(!state.blocks_automatic_retry(now));
        state.last_terminal_at =
            Some(now - Duration::seconds(RENEWAL_RETRY_AFTER_SECONDS.saturating_sub(1)));
        assert!(state.blocks_automatic_retry(now));
    }

    #[test]
    fn fifth_provider_throttle_switches_to_daily_pacing() {
        assert_eq!(provider_rate_limit_retry_after_seconds(4), 60);
        assert_eq!(
            provider_rate_limit_retry_after_seconds(5),
            RENEWAL_RETRY_AFTER_SECONDS
        );
    }
}
