use chrono::{DateTime, Duration, Utc};
use thiserror::Error;

use crate::{
    BillingContactSnapshot, BillingPeriod, BillingScopeId, ChargeAmount, GatewayAccountId,
    GatewayProviderKey, IdempotencyKey, PaymentAttempt, PaymentAttemptId, PaymentAttemptIdentity,
    PaymentAttemptKind, PaymentAttemptRequest, PaymentAttemptTarget, PaymentMethodId, PlanKey,
    RenewalFailurePolicy, ResolvedGateway, SubscriberId, SubscriptionId,
    SubscriptionPaymentStateSnapshot, SubscriptionStatus,
};

pub const RENEWAL_DISPATCH_LIMIT: i64 = 100;
pub const RENEWAL_INFRASTRUCTURE_RETRY_AFTER_SECONDS: i64 = 24 * 60 * 60;
pub const RENEWAL_PROVIDER_RATE_LIMIT_SLOW_RETRY_AFTER_SECONDS: i64 = 24 * 60 * 60;
pub const RENEWAL_PROVIDER_RATE_LIMIT_RETRY_AFTER_SECONDS: i64 = 60;
pub const MAX_RENEWAL_INFRASTRUCTURE_ATTEMPTS_PER_PERIOD_CONFIGURATION: i64 = 8;
pub const RENEWAL_PROVIDER_RATE_LIMIT_FAST_RETRY_ATTEMPTS: i64 = 5;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RenewalFailureDisposition {
    RetryScheduled { retry_at: DateTime<Utc> },
    RemainPastDue { exhausted_at: DateTime<Utc> },
    MarkUnpaid { ended_at: DateTime<Utc> },
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum RenewalFailurePolicyError {
    #[error("automatic renewal failure count is one-based")]
    ZeroFailureCount,
    #[error("dunning retry timestamp overflowed the supported date range")]
    RetryTimestampOverflow,
}

pub fn renewal_failure_disposition(
    policy: &RenewalFailurePolicy,
    automatic_failure_count: u16,
    failed_at: DateTime<Utc>,
) -> Result<RenewalFailureDisposition, RenewalFailurePolicyError> {
    let schedule_index = automatic_failure_count
        .checked_sub(1)
        .ok_or(RenewalFailurePolicyError::ZeroFailureCount)?;
    if let Some(delay) = policy
        .schedule()
        .retry_delays()
        .get(usize::from(schedule_index))
    {
        let retry_at = failed_at
            .checked_add_signed(Duration::seconds(i64::from(delay.seconds().get())))
            .ok_or(RenewalFailurePolicyError::RetryTimestampOverflow)?;
        return Ok(RenewalFailureDisposition::RetryScheduled { retry_at });
    }
    Ok(match policy.exhaustion() {
        crate::DunningExhaustion::RemainPastDue => RenewalFailureDisposition::RemainPastDue {
            exhausted_at: failed_at,
        },
        crate::DunningExhaustion::MarkUnpaid => RenewalFailureDisposition::MarkUnpaid {
            ended_at: failed_at,
        },
    })
}

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

/// Continuation key returned by a prior renewal-dispatch page.
///
/// `observed_at` is the PostgreSQL clock timestamp captured when the scan's
/// first page was read. Every continuation retains that same eligibility-time
/// bound. The remaining fields are the last returned row's strict ascending
/// `(next_payment_attempt_at, subscription_id)` key; they are data values,
/// never caller-supplied SQL.
///
/// Hosts may reconstruct this value from their own trusted persisted page
/// state. It must originate from a prior renewal page and must never be
/// accepted from an end user; the host remains the trusted dispatch boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RenewalDispatchPageCursor {
    observed_at: DateTime<Utc>,
    next_payment_attempt_at: DateTime<Utc>,
    subscription_id: SubscriptionId,
}

impl RenewalDispatchPageCursor {
    pub const fn new(
        observed_at: DateTime<Utc>,
        next_payment_attempt_at: DateTime<Utc>,
        subscription_id: SubscriptionId,
    ) -> Self {
        Self {
            observed_at,
            next_payment_attempt_at,
            subscription_id,
        }
    }

    /// Database time observed for the first page of this scan.
    pub const fn observed_at(self) -> DateTime<Utc> {
        self.observed_at
    }

    /// Scheduling timestamp from the last row returned by the prior page.
    pub const fn next_payment_attempt_at(self) -> DateTime<Utc> {
        self.next_payment_attempt_at
    }

    /// Subscription identifier from the last row returned by the prior page.
    pub const fn subscription_id(self) -> SubscriptionId {
        self.subscription_id
    }
}

/// One deterministic, bounded page of due renewal dispatches.
///
/// A next cursor is present only when PostgreSQL observed another eligible row
/// after this page. For candidates whose ordering and eligibility do not
/// change during traversal, strict keyset order avoids offset and timestamp-tie
/// gaps or repeats. This is not a cross-page snapshot: inserted, retimed, or
/// newly unblocked candidates behind the continuation key can wait for a fresh
/// scan. Hosts own queue/outbox dispatch and should still submit every item
/// through the normal renewal preflight, which revalidates mutable current
/// state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenewalDispatchPage {
    dispatches: Vec<RenewalDispatch>,
    next_cursor: Option<RenewalDispatchPageCursor>,
}

impl RenewalDispatchPage {
    pub fn new(
        dispatches: Vec<RenewalDispatch>,
        next_cursor: Option<RenewalDispatchPageCursor>,
    ) -> Self {
        Self {
            dispatches,
            next_cursor,
        }
    }

    pub fn dispatches(&self) -> &[RenewalDispatch] {
        &self.dispatches
    }

    pub fn into_dispatches(self) -> Vec<RenewalDispatch> {
        self.dispatches
    }

    pub const fn next_cursor(&self) -> Option<RenewalDispatchPageCursor> {
        self.next_cursor
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
    pub automatic_infrastructure_attempt_count: i64,
    pub last_automatic_infrastructure_failure_at: Option<DateTime<Utc>>,
    pub provider_rate_limited_attempt_count: i64,
    pub last_provider_rate_limited_at: Option<DateTime<Utc>>,
    pub has_blocking_attempt: bool,
}

impl RenewalAttemptState {
    pub fn blocks_automatic_retry(&self, now: DateTime<Utc>) -> bool {
        self.automatic_infrastructure_attempt_count
            >= MAX_RENEWAL_INFRASTRUCTURE_ATTEMPTS_PER_PERIOD_CONFIGURATION
            || self.has_blocking_attempt
            || !retry_window_elapsed(
                self.last_automatic_infrastructure_failure_at,
                now,
                RENEWAL_INFRASTRUCTURE_RETRY_AFTER_SECONDS,
            )
            || !retry_window_elapsed(
                self.last_provider_rate_limited_at,
                now,
                provider_rate_limit_retry_after_seconds(self.provider_rate_limited_attempt_count),
            )
    }
}

pub const fn provider_rate_limit_retry_after_seconds(attempt_count: i64) -> i64 {
    if attempt_count >= RENEWAL_PROVIDER_RATE_LIMIT_FAST_RETRY_ATTEMPTS {
        RENEWAL_PROVIDER_RATE_LIMIT_SLOW_RETRY_AFTER_SECONDS
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

/// Validated subscription terms read while preparing one renewal attempt.
///
/// The PostgreSQL owner constructs this from a locked subscription row before
/// creating the secret-free reservation. Keeping the exact optimistic payment
/// state together with the charge period prevents individual row fields from
/// being reconstructed by each caller.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubscriptionRenewalLockedTerms {
    gateway_account_id: GatewayAccountId,
    expected_state: SubscriptionPaymentStateSnapshot,
    period: BillingPeriod,
    charge: ChargeAmount,
    attempt_sequence_count: i64,
}

impl SubscriptionRenewalLockedTerms {
    pub const fn new(
        gateway_account_id: GatewayAccountId,
        expected_state: SubscriptionPaymentStateSnapshot,
        period: BillingPeriod,
        charge: ChargeAmount,
        attempt_sequence_count: i64,
    ) -> Self {
        Self {
            gateway_account_id,
            expected_state,
            period,
            charge,
            attempt_sequence_count,
        }
    }
}

/// Secret-free authority for one exact automatic recurring charge.
#[derive(Clone, Eq, PartialEq)]
pub struct SubscriptionRenewalReservation {
    identity: PaymentAttemptIdentity,
    provider_key: GatewayProviderKey,
    request: PaymentAttemptRequest,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubscriptionRenewalReservationRejection {
    SubscriptionNotFound,
    PaymentNotDue,
    AttemptInProgress,
    PaymentMethodUpdateInProgress,
    RetryBlocked,
    GatewayConfigurationChanged,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SubscriptionRenewalReservationOutcome {
    Reserved(Box<SubscriptionRenewalReservation>, Box<PaymentAttempt>),
    Rejected(SubscriptionRenewalReservationRejection),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubscriptionRenewalSubmissionRejection {
    BillingStateChanged,
    GatewayConfigurationChanged,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SubscriptionRenewalSubmissionOutcome {
    Admitted(PaymentAttempt),
    AlreadyAdmitted(PaymentAttempt),
    Rejected {
        attempt: PaymentAttempt,
        reason: SubscriptionRenewalSubmissionRejection,
    },
}

#[derive(Debug)]
pub enum SubscriptionRenewalOutcome {
    Noop,
    Payment(Box<crate::SubscriptionEnrollmentPaymentResult>),
    NotSubmitted {
        payment: Box<crate::SubscriptionEnrollmentPaymentResult>,
        error: crate::GatewayNotSubmittedError,
    },
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
        let expected_state = SubscriptionPaymentStateSnapshot::new(
            command.subscription_id(),
            payment_method_id,
            initial_transaction_id,
            status,
        )
        .map_err(|_| SubscriptionRenewalReservationBuildError::InvalidPaymentState)?;
        Self::from_locked_subscription_terms(
            command,
            gateway,
            attempt_id,
            subscriber_id,
            plan_key,
            SubscriptionRenewalLockedTerms::new(
                gateway.gateway_account_id(),
                expected_state,
                period,
                charge,
                attempt_sequence_count,
            ),
        )
    }

    /// Builds a renewal reservation from validated terms read under the
    /// subscription lock.
    pub fn from_locked_subscription_terms(
        command: ChargeRenewal,
        gateway: &ResolvedGateway,
        attempt_id: PaymentAttemptId,
        subscriber_id: SubscriberId,
        plan_key: PlanKey,
        terms: SubscriptionRenewalLockedTerms,
    ) -> Result<Self, SubscriptionRenewalReservationBuildError> {
        let SubscriptionRenewalLockedTerms {
            gateway_account_id,
            expected_state,
            period,
            charge,
            attempt_sequence_count,
        } = terms;
        if gateway.billing_scope_id() != command.billing_scope_id()
            || gateway_account_id != gateway.gateway_account_id()
        {
            return Err(SubscriptionRenewalReservationBuildError::GatewayIdentityMismatch);
        }
        if expected_state.subscription_id() != command.subscription_id() {
            return Err(SubscriptionRenewalReservationBuildError::InvalidPaymentState);
        }
        let identity = PaymentAttemptIdentity::new(
            attempt_id,
            command.billing_scope_id(),
            subscriber_id,
            gateway_account_id,
            gateway.gateway_configuration_id(),
        );
        let idempotency_key = renewal_attempt_idempotency_key(
            command.subscription_id(),
            *command.period_start_at(),
            attempt_sequence_count,
        )
        .map_err(|_| SubscriptionRenewalReservationBuildError::InvalidIdempotencyKey)?;
        let target = PaymentAttemptTarget::SubscriptionRenewal {
            plan_key,
            payment_method_id: expected_state.payment_method_id(),
            period,
            expected_state,
        };
        let request = PaymentAttemptRequest::canonical(
            target,
            idempotency_key,
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
    use uuid::Uuid;

    #[test]
    fn renewal_dispatch_page_keeps_its_observed_scan_and_strict_key() {
        let observed_at = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let next_payment_attempt_at = observed_at + Duration::seconds(60);
        let subscription_id = SubscriptionId::new(Uuid::from_u128(2));
        let cursor =
            RenewalDispatchPageCursor::new(observed_at, next_payment_attempt_at, subscription_id);
        let dispatch = RenewalDispatch::new(
            BillingScopeId::new(Uuid::from_u128(1)),
            subscription_id,
            observed_at,
            3,
        );
        let page = RenewalDispatchPage::new(vec![dispatch.clone()], Some(cursor));

        assert_eq!(cursor.observed_at(), observed_at);
        assert_eq!(cursor.next_payment_attempt_at(), next_payment_attempt_at);
        assert_eq!(cursor.subscription_id(), subscription_id);
        assert_eq!(page.dispatches(), std::slice::from_ref(&dispatch));
        assert_eq!(page.next_cursor(), Some(cursor));
        assert_eq!(page.into_dispatches(), vec![dispatch]);
    }

    #[test]
    fn retry_boundaries_are_inclusive() {
        let now = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let mut state = RenewalAttemptState {
            last_automatic_infrastructure_failure_at: Some(
                now - Duration::seconds(RENEWAL_INFRASTRUCTURE_RETRY_AFTER_SECONDS),
            ),
            ..RenewalAttemptState::default()
        };
        assert!(!state.blocks_automatic_retry(now));
        state.last_automatic_infrastructure_failure_at = Some(
            now - Duration::seconds(RENEWAL_INFRASTRUCTURE_RETRY_AFTER_SECONDS.saturating_sub(1)),
        );
        assert!(state.blocks_automatic_retry(now));
    }

    #[test]
    fn fifth_provider_throttle_switches_to_daily_pacing() {
        assert_eq!(provider_rate_limit_retry_after_seconds(4), 60);
        assert_eq!(
            provider_rate_limit_retry_after_seconds(5),
            RENEWAL_PROVIDER_RATE_LIMIT_SLOW_RETRY_AFTER_SECONDS
        );
    }

    fn failure_policy(
        delays: &[u32],
        exhaustion: crate::DunningExhaustion,
    ) -> RenewalFailurePolicy {
        RenewalFailurePolicy::new(
            crate::DunningSchedule::from_seconds(delays.iter().copied()).unwrap(),
            exhaustion,
            crate::PastDueAccessPolicy::SuspendImmediately,
        )
    }

    #[test]
    fn dunning_failure_count_is_one_based_and_indexes_the_current_step() {
        let failed_at = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let policy = failure_policy(&[60, 300], crate::DunningExhaustion::RemainPastDue);
        assert_eq!(
            renewal_failure_disposition(&policy, 0, failed_at),
            Err(RenewalFailurePolicyError::ZeroFailureCount)
        );
        assert_eq!(
            renewal_failure_disposition(&policy, 1, failed_at),
            Ok(RenewalFailureDisposition::RetryScheduled {
                retry_at: failed_at + Duration::seconds(60),
            })
        );
        assert_eq!(
            renewal_failure_disposition(&policy, 2, failed_at),
            Ok(RenewalFailureDisposition::RetryScheduled {
                retry_at: failed_at + Duration::seconds(300),
            })
        );
        assert_eq!(
            renewal_failure_disposition(&policy, 3, failed_at),
            Ok(RenewalFailureDisposition::RemainPastDue {
                exhausted_at: failed_at,
            })
        );
    }

    #[test]
    fn empty_schedule_and_mark_unpaid_exhaust_immediately() {
        let failed_at = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let policy = failure_policy(&[], crate::DunningExhaustion::MarkUnpaid);
        assert_eq!(
            renewal_failure_disposition(&policy, 1, failed_at),
            Ok(RenewalFailureDisposition::MarkUnpaid {
                ended_at: failed_at,
            })
        );
    }

    #[test]
    fn retry_timestamp_overflow_is_typed() {
        let policy = failure_policy(&[u32::MAX], crate::DunningExhaustion::RemainPastDue);
        assert_eq!(
            renewal_failure_disposition(&policy, 1, DateTime::<Utc>::MAX_UTC),
            Err(RenewalFailurePolicyError::RetryTimestampOverflow)
        );
    }
}
