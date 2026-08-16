use std::fmt;

use crate::{
    BillingContact, BillingContactSnapshot, BillingScopeId, CurrencyCode, GatewayAccountId,
    GatewayConfigurationId, GatewayProviderKey, GatewayTransactionId, IdempotencyKey, Money,
    PaymentAttempt, PaymentAttemptFingerprint, PaymentAttemptId, PaymentAttemptIdentity,
    PaymentAttemptKind, PaymentAttemptRequest, PaymentAttemptTarget, PaymentMethodId,
    PaymentMethodUpdateSnapshot, PaymentToken, PlanKey, ResolvedGateway, SubscriberId,
    SubscriptionId, SubscriptionPaymentContext,
};
use thiserror::Error;

/// Provider-neutral request to replace the stored credential for one plan.
///
/// The browser token remains memory-only. The canonical subscription and its
/// current method/initial-transaction baseline are derived under lock.
#[derive(Clone)]
pub struct ReplaceSubscriptionPaymentMethod {
    context: SubscriptionPaymentContext,
    plan_key: PlanKey,
}

impl ReplaceSubscriptionPaymentMethod {
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

impl fmt::Debug for ReplaceSubscriptionPaymentMethod {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReplaceSubscriptionPaymentMethod")
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
pub enum SubscriptionPaymentMethodReplacementBuildError {
    #[error("resolved gateway identity does not match the payment-method replacement command")]
    GatewayIdentityMismatch,
    #[error("attempt is not a valid subscription payment-method replacement")]
    AttemptKindMismatch,
}

/// Validated subscription terms read while preparing a payment-method
/// replacement attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubscriptionPaymentMethodReplacementLockedTerms {
    gateway_account_id: GatewayAccountId,
    expected_state: PaymentMethodUpdateSnapshot,
    currency: CurrencyCode,
}

impl SubscriptionPaymentMethodReplacementLockedTerms {
    pub const fn new(
        gateway_account_id: GatewayAccountId,
        expected_state: PaymentMethodUpdateSnapshot,
        currency: CurrencyCode,
    ) -> Self {
        Self {
            gateway_account_id,
            expected_state,
            currency,
        }
    }
}

/// Secret-free authority for one exact stored-method replacement.
#[derive(Clone, Eq, PartialEq)]
pub struct SubscriptionPaymentMethodReplacement {
    identity: PaymentAttemptIdentity,
    provider_key: GatewayProviderKey,
    request: PaymentAttemptRequest,
}

impl SubscriptionPaymentMethodReplacement {
    #[allow(clippy::too_many_arguments)]
    pub fn from_locked_subscription(
        command: &ReplaceSubscriptionPaymentMethod,
        gateway: &ResolvedGateway,
        subscription_id: SubscriptionId,
        payment_method_id: PaymentMethodId,
        initial_transaction_id: GatewayTransactionId,
        currency: CurrencyCode,
    ) -> Result<Self, SubscriptionPaymentMethodReplacementBuildError> {
        if gateway.billing_scope_id() != command.billing_scope_id()
            || gateway.gateway_configuration_id() != command.gateway_configuration_id()
        {
            return Err(SubscriptionPaymentMethodReplacementBuildError::GatewayIdentityMismatch);
        }
        let expected_state = PaymentMethodUpdateSnapshot::new(
            subscription_id,
            payment_method_id,
            initial_transaction_id,
        );
        Self::from_locked_subscription_terms(
            command,
            gateway,
            SubscriptionPaymentMethodReplacementLockedTerms::new(
                gateway.gateway_account_id(),
                expected_state,
                currency,
            ),
        )
    }

    /// Builds a payment-method replacement reservation from validated terms
    /// read under the subscription lock.
    pub fn from_locked_subscription_terms(
        command: &ReplaceSubscriptionPaymentMethod,
        gateway: &ResolvedGateway,
        terms: SubscriptionPaymentMethodReplacementLockedTerms,
    ) -> Result<Self, SubscriptionPaymentMethodReplacementBuildError> {
        Self::from_locked_subscription_terms_for_attempt(
            command,
            gateway,
            command.attempt_id(),
            terms,
        )
    }

    fn from_locked_subscription_terms_for_attempt(
        command: &ReplaceSubscriptionPaymentMethod,
        gateway: &ResolvedGateway,
        attempt_id: PaymentAttemptId,
        terms: SubscriptionPaymentMethodReplacementLockedTerms,
    ) -> Result<Self, SubscriptionPaymentMethodReplacementBuildError> {
        let SubscriptionPaymentMethodReplacementLockedTerms {
            gateway_account_id,
            expected_state,
            currency,
        } = terms;
        if gateway.billing_scope_id() != command.billing_scope_id()
            || gateway.gateway_configuration_id() != command.gateway_configuration_id()
            || gateway_account_id != gateway.gateway_account_id()
        {
            return Err(SubscriptionPaymentMethodReplacementBuildError::GatewayIdentityMismatch);
        }
        let identity = PaymentAttemptIdentity::new(
            attempt_id,
            command.billing_scope_id(),
            command.subscriber_id(),
            gateway_account_id,
            gateway.gateway_configuration_id(),
        );
        let fingerprint = PaymentAttemptFingerprint::for_subscription_payment_method_update(
            command.plan_key(),
            expected_state.subscription_id(),
            expected_state.payment_method_id(),
            expected_state.expected_initial_transaction_id(),
        );
        let request = PaymentAttemptRequest::new(
            PaymentAttemptTarget::SubscriptionPaymentMethodUpdate {
                plan_key: command.plan_key().clone(),
                payment_method_id: expected_state.payment_method_id(),
                expected_state,
            },
            command.idempotency_key().clone(),
            fingerprint,
            Money::new(0, currency).expect("zero payment-method replacement amount is valid"),
            gateway.mutation_reference_factory().for_attempt(
                PaymentAttemptKind::SubscriptionPaymentMethodUpdate,
                attempt_id,
            ),
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
    ) -> Result<Self, SubscriptionPaymentMethodReplacementBuildError> {
        let PaymentAttemptTarget::SubscriptionPaymentMethodUpdate {
            plan_key,
            expected_state,
            ..
        } = attempt.request().target()
        else {
            return Err(SubscriptionPaymentMethodReplacementBuildError::AttemptKindMismatch);
        };
        if attempt.request().amount().cents() != 0
            || !attempt
                .request()
                .fingerprint()
                .matches_subscription_payment_method_update(plan_key, expected_state)
        {
            return Err(SubscriptionPaymentMethodReplacementBuildError::AttemptKindMismatch);
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
        command: &ReplaceSubscriptionPaymentMethod,
        gateway: &ResolvedGateway,
    ) -> bool {
        Self::from_locked_subscription_terms_for_attempt(
            command,
            gateway,
            self.identity.attempt_id(),
            SubscriptionPaymentMethodReplacementLockedTerms::new(
                self.identity.gateway_account_id(),
                self.expected_state().clone(),
                self.request.amount().currency(),
            ),
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
            Some(value) => value,
            None => unreachable!(),
        }
    }
    pub const fn subscription_id(&self) -> SubscriptionId {
        match self.request.target().subscription_id() {
            Some(value) => value,
            None => unreachable!(),
        }
    }
    pub const fn expected_state(&self) -> &PaymentMethodUpdateSnapshot {
        match self.request.target().payment_method_update_snapshot() {
            Some(value) => value,
            None => unreachable!(),
        }
    }
}

impl fmt::Debug for SubscriptionPaymentMethodReplacement {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SubscriptionPaymentMethodReplacement")
            .field("identity", &self.identity)
            .field("provider_key", &self.provider_key)
            .field("request", &self.request)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubscriptionPaymentMethodReplacementRejection {
    SubscriptionNotFound,
    SubscriptionIneligible,
    ChargeAttemptInProgress,
    PaymentMethodUpdateInProgress,
    GatewayConfigurationChanged,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SubscriptionPaymentMethodReplacementReservationOutcome {
    Reserved(
        Box<SubscriptionPaymentMethodReplacement>,
        Box<PaymentAttempt>,
    ),
    Replay(Box<PaymentAttempt>),
    IdempotencyConflict,
    Rejected(SubscriptionPaymentMethodReplacementRejection),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SubscriptionPaymentMethodReplacementPreflightOutcome {
    Continue,
    Replay(Box<PaymentAttempt>),
    IdempotencyConflict,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubscriptionPaymentMethodReplacementSubmissionRejection {
    BillingStateChanged,
    GatewayConfigurationChanged,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SubscriptionPaymentMethodReplacementSubmissionOutcome {
    Admitted(PaymentAttempt),
    AlreadyAdmitted(PaymentAttempt),
    Rejected {
        attempt: PaymentAttempt,
        reason: SubscriptionPaymentMethodReplacementSubmissionRejection,
    },
}
