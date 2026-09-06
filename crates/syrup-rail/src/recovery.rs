use std::fmt;

use crate::{
    BillingContact, BillingContactSnapshot, BillingPeriod, BillingScopeId, ChargeAmount,
    GatewayAccountId, GatewayAccountMode, GatewayConfigurationId, GatewayProviderKey,
    GatewayTransactionId, IdempotencyKey, PaymentAttempt, PaymentAttemptId, PaymentAttemptIdentity,
    PaymentAttemptKind, PaymentAttemptRequest, PaymentAttemptTarget, PaymentMethodId, PaymentToken,
    PlanKey, ResolvedGateway, SubscriberId, SubscriptionId, SubscriptionPaymentContext,
    SubscriptionPaymentStateSnapshot, SubscriptionStatus,
};
use thiserror::Error;

/// Provider-neutral request to recover the currently due period of one plan.
///
/// The one-shot token is memory-only. Amount, period, subscription identity,
/// and optimistic payment state are derived from the locked canonical
/// subscription by the PostgreSQL reservation transaction.
#[derive(Clone)]
pub struct RecoverSubscriptionPayment {
    context: SubscriptionPaymentContext,
    plan_key: PlanKey,
}

impl RecoverSubscriptionPayment {
    pub const fn new(context: SubscriptionPaymentContext, plan_key: PlanKey) -> Self {
        Self { context, plan_key }
    }

    pub const fn context(&self) -> &SubscriptionPaymentContext {
        &self.context
    }

    pub const fn attempt_id(&self) -> PaymentAttemptId {
        self.context.attempt_id()
    }

    pub const fn billing_scope_id(&self) -> BillingScopeId {
        self.context.billing_scope_id()
    }

    pub const fn subscriber_id(&self) -> SubscriberId {
        self.context.subscriber_id()
    }

    pub const fn plan_key(&self) -> &PlanKey {
        &self.plan_key
    }

    pub const fn gateway_configuration_id(&self) -> GatewayConfigurationId {
        self.context.gateway_configuration_id()
    }

    pub const fn idempotency_key(&self) -> &IdempotencyKey {
        self.context.idempotency_key()
    }

    pub const fn payment_token(&self) -> &PaymentToken {
        self.context.payment_token()
    }

    pub const fn billing_contact(&self) -> &BillingContact {
        self.context.billing_contact()
    }
}

impl fmt::Debug for RecoverSubscriptionPayment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RecoverSubscriptionPayment")
            .field("attempt_id", &self.attempt_id())
            .field("billing_scope_id", &self.billing_scope_id())
            .field("subscriber_id", &self.subscriber_id())
            .field("plan_key", &self.plan_key)
            .field("gateway_configuration_id", &self.gateway_configuration_id())
            .field("has_idempotency_key", &true)
            .field("has_payment_token", &true)
            .field("has_billing_contact", &true)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum SubscriptionRecoveryReservationBuildError {
    #[error("resolved gateway identity does not match the recovery command")]
    GatewayIdentityMismatch,
    #[error("only subscription-recovery attempts can become recovery reservations")]
    AttemptKindMismatch,
    #[error("subscription recovery has an invalid payment-state snapshot")]
    InvalidPaymentState,
    #[error("subscription recovery attempt has an invalid charge amount")]
    InvalidCharge,
}

/// Validated subscription terms read while preparing one recovery attempt.
///
/// The durable reservation uses the same optimistic payment-state snapshot it
/// later revalidates before provider submission.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubscriptionRecoveryLockedTerms {
    gateway_account_id: GatewayAccountId,
    expected_state: SubscriptionPaymentStateSnapshot,
    period: BillingPeriod,
    charge: ChargeAmount,
}

impl SubscriptionRecoveryLockedTerms {
    pub const fn new(
        gateway_account_id: GatewayAccountId,
        expected_state: SubscriptionPaymentStateSnapshot,
        period: BillingPeriod,
        charge: ChargeAmount,
    ) -> Self {
        Self {
            gateway_account_id,
            expected_state,
            period,
            charge,
        }
    }
}

/// Secret-free authority for one exact recovery request.
#[derive(Clone, Eq, PartialEq)]
pub struct SubscriptionRecoveryReservation {
    identity: PaymentAttemptIdentity,
    provider_key: GatewayProviderKey,
    request: PaymentAttemptRequest,
}

impl SubscriptionRecoveryReservation {
    #[allow(clippy::too_many_arguments)]
    pub fn from_locked_subscription(
        command: &RecoverSubscriptionPayment,
        gateway: &ResolvedGateway,
        attempt_id: PaymentAttemptId,
        subscription_id: SubscriptionId,
        payment_method_id: PaymentMethodId,
        initial_transaction_id: GatewayTransactionId,
        status: SubscriptionStatus,
        period: BillingPeriod,
        charge: ChargeAmount,
        required_gateway_account_mode: GatewayAccountMode,
    ) -> Result<Self, SubscriptionRecoveryReservationBuildError> {
        if gateway.billing_scope_id() != command.billing_scope_id()
            || gateway.gateway_configuration_id() != command.gateway_configuration_id()
        {
            return Err(SubscriptionRecoveryReservationBuildError::GatewayIdentityMismatch);
        }
        let expected_state = SubscriptionPaymentStateSnapshot::new(
            subscription_id,
            payment_method_id,
            initial_transaction_id,
            status,
        )
        .map_err(|_| SubscriptionRecoveryReservationBuildError::InvalidPaymentState)?;
        Self::from_locked_subscription_terms(
            command,
            gateway,
            attempt_id,
            SubscriptionRecoveryLockedTerms::new(
                gateway.gateway_account_id(),
                expected_state,
                period,
                charge,
            ),
            required_gateway_account_mode,
        )
    }

    /// Builds a recovery reservation from validated terms read under the
    /// subscription lock.
    pub fn from_locked_subscription_terms(
        command: &RecoverSubscriptionPayment,
        gateway: &ResolvedGateway,
        attempt_id: PaymentAttemptId,
        terms: SubscriptionRecoveryLockedTerms,
        required_gateway_account_mode: GatewayAccountMode,
    ) -> Result<Self, SubscriptionRecoveryReservationBuildError> {
        let SubscriptionRecoveryLockedTerms {
            gateway_account_id,
            expected_state,
            period,
            charge,
        } = terms;
        if gateway.billing_scope_id() != command.billing_scope_id()
            || gateway.gateway_configuration_id() != command.gateway_configuration_id()
            || gateway_account_id != gateway.gateway_account_id()
        {
            return Err(SubscriptionRecoveryReservationBuildError::GatewayIdentityMismatch);
        }
        let identity = PaymentAttemptIdentity::new(
            attempt_id,
            command.billing_scope_id(),
            command.subscriber_id(),
            gateway_account_id,
            gateway.gateway_configuration_id(),
            required_gateway_account_mode,
        );
        let target = PaymentAttemptTarget::SubscriptionRecovery {
            plan_key: command.plan_key().clone(),
            payment_method_id: expected_state.payment_method_id(),
            period,
            expected_state,
        };
        let request = PaymentAttemptRequest::canonical(
            target,
            command.idempotency_key().clone(),
            charge.money(),
            gateway
                .mutation_reference_factory()
                .for_attempt(PaymentAttemptKind::SubscriptionRecovery, attempt_id),
            BillingContactSnapshot::from_billing_contact(command.billing_contact()),
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
    ) -> Result<Self, SubscriptionRecoveryReservationBuildError> {
        let PaymentAttemptTarget::SubscriptionRecovery {
            plan_key,
            period,
            expected_state,
            ..
        } = attempt.request().target()
        else {
            return Err(SubscriptionRecoveryReservationBuildError::AttemptKindMismatch);
        };
        ChargeAmount::try_from(attempt.request().amount())
            .map_err(|_| SubscriptionRecoveryReservationBuildError::InvalidCharge)?;
        if !attempt
            .request()
            .fingerprint()
            .matches_subscription_recovery(
                plan_key,
                expected_state.subscription_id(),
                expected_state.payment_method_id(),
                *period.start_at(),
                attempt.request().amount(),
            )
        {
            return Err(SubscriptionRecoveryReservationBuildError::AttemptKindMismatch);
        }
        Ok(Self {
            identity: attempt.identity(),
            provider_key,
            request: attempt.request().clone(),
        })
    }

    /// Returns whether a retry command and resolved gateway reproduce this
    /// exact durable submission authority.
    ///
    /// Retry-only candidate attempt IDs and payment tokens are intentionally
    /// excluded. Reconstructing the reservation through its canonical builder
    /// keeps every durable command field and gateway identity equality-bound.
    pub fn matches_submission(
        &self,
        command: &RecoverSubscriptionPayment,
        gateway: &ResolvedGateway,
    ) -> bool {
        let Ok(charge) = ChargeAmount::try_from(self.request.amount()) else {
            return false;
        };
        Self::from_locked_subscription_terms(
            command,
            gateway,
            self.identity.attempt_id(),
            SubscriptionRecoveryLockedTerms::new(
                self.identity.gateway_account_id(),
                self.expected_state().clone(),
                self.period().clone(),
                charge,
            ),
            self.identity.required_gateway_account_mode(),
        )
        .is_ok_and(|candidate| candidate.eq(self))
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

impl fmt::Debug for SubscriptionRecoveryReservation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SubscriptionRecoveryReservation")
            .field("identity", &self.identity)
            .field("provider_key", &self.provider_key)
            .field("request", &self.request)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubscriptionRecoveryReservationRejection {
    SubscriptionNotFound,
    PaymentNotDue,
    AttemptInProgress,
    PaymentMethodUpdateInProgress,
    GatewayConfigurationChanged,
    GatewayAccountModeChanged,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SubscriptionRecoveryReservationOutcome {
    Reserved(Box<SubscriptionRecoveryReservation>, Box<PaymentAttempt>),
    Replay(Box<PaymentAttempt>),
    IdempotencyConflict,
    Rejected(SubscriptionRecoveryReservationRejection),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SubscriptionRecoveryPreflightOutcome {
    Continue,
    Replay(Box<PaymentAttempt>),
    IdempotencyConflict,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubscriptionRecoverySubmissionRejection {
    BillingStateChanged,
    GatewayConfigurationChanged,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SubscriptionRecoverySubmissionOutcome {
    Admitted(PaymentAttempt),
    AlreadyAdmitted(PaymentAttempt),
    Rejected {
        attempt: PaymentAttempt,
        reason: SubscriptionRecoverySubmissionRejection,
    },
}
