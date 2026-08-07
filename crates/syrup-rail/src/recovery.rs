use std::fmt;

use crate::{
    BillingContact, BillingContactSnapshot, BillingPeriod, BillingScopeId, ChargeAmount,
    GatewayConfigurationId, GatewayProviderKey, GatewayTransactionId, IdempotencyKey,
    PaymentAttempt, PaymentAttemptFingerprint, PaymentAttemptId, PaymentAttemptIdentity,
    PaymentAttemptKind, PaymentAttemptRequest, PaymentAttemptTarget, PaymentMethodId, PaymentToken,
    PlanKey, ResolvedGateway, SubscriberId, SubscriptionId, SubscriptionPaymentStateSnapshot,
    SubscriptionStatus,
};
use thiserror::Error;

/// Provider-neutral request to recover the currently due period of one plan.
///
/// The one-shot token is memory-only. Amount, period, subscription identity,
/// and optimistic payment state are derived from the locked canonical
/// subscription by the PostgreSQL reservation transaction.
#[derive(Clone)]
pub struct RecoverSubscriptionPayment {
    attempt_id: PaymentAttemptId,
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    plan_key: PlanKey,
    gateway_configuration_id: GatewayConfigurationId,
    idempotency_key: IdempotencyKey,
    payment_token: PaymentToken,
    billing_contact: BillingContact,
}

impl RecoverSubscriptionPayment {
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        attempt_id: PaymentAttemptId,
        billing_scope_id: BillingScopeId,
        subscriber_id: SubscriberId,
        plan_key: PlanKey,
        gateway_configuration_id: GatewayConfigurationId,
        idempotency_key: IdempotencyKey,
        payment_token: PaymentToken,
        billing_contact: BillingContact,
    ) -> Self {
        Self {
            attempt_id,
            billing_scope_id,
            subscriber_id,
            plan_key,
            gateway_configuration_id,
            idempotency_key,
            payment_token,
            billing_contact,
        }
    }

    pub const fn attempt_id(&self) -> PaymentAttemptId {
        self.attempt_id
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

    pub const fn gateway_configuration_id(&self) -> GatewayConfigurationId {
        self.gateway_configuration_id
    }

    pub const fn idempotency_key(&self) -> &IdempotencyKey {
        &self.idempotency_key
    }

    pub const fn payment_token(&self) -> &PaymentToken {
        &self.payment_token
    }

    pub const fn billing_contact(&self) -> &BillingContact {
        &self.billing_contact
    }
}

impl fmt::Debug for RecoverSubscriptionPayment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RecoverSubscriptionPayment")
            .field("attempt_id", &self.attempt_id)
            .field("billing_scope_id", &self.billing_scope_id)
            .field("subscriber_id", &self.subscriber_id)
            .field("plan_key", &self.plan_key)
            .field("gateway_configuration_id", &self.gateway_configuration_id)
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
    ) -> Result<Self, SubscriptionRecoveryReservationBuildError> {
        if gateway.billing_scope_id() != command.billing_scope_id()
            || gateway.gateway_configuration_id() != command.gateway_configuration_id()
        {
            return Err(SubscriptionRecoveryReservationBuildError::GatewayIdentityMismatch);
        }
        let identity = PaymentAttemptIdentity::new(
            attempt_id,
            command.billing_scope_id(),
            command.subscriber_id(),
            gateway.gateway_account_id(),
            gateway.gateway_configuration_id(),
        );
        let expected_state = SubscriptionPaymentStateSnapshot::new(
            subscription_id,
            payment_method_id,
            initial_transaction_id,
            status,
        )
        .map_err(|_| SubscriptionRecoveryReservationBuildError::InvalidPaymentState)?;
        let fingerprint = PaymentAttemptFingerprint::for_subscription_recovery(
            command.plan_key(),
            subscription_id,
            payment_method_id,
            *period.start_at(),
            charge.money(),
        );
        let target = PaymentAttemptTarget::SubscriptionRecovery {
            plan_key: command.plan_key().clone(),
            payment_method_id,
            period,
            expected_state,
        };
        let request = PaymentAttemptRequest::new(
            target,
            command.idempotency_key().clone(),
            fingerprint,
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
