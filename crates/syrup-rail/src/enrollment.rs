use std::fmt;

use crate::{
    ApprovedProcessorEvidence, BillingContact, BillingContactSnapshot, BillingScopeId,
    ChargeAmount, DiscountClaimId, DiscountCodeId, GatewayConfigurationId, GatewayOrderId,
    GatewayProviderKey, IdempotencyKey, PaymentAttempt, PaymentAttemptId, PaymentAttemptIdentity,
    PaymentAttemptKind, PaymentAttemptStatus, PaymentAttemptTarget, PaymentToken, PlanKey,
    ProcessorEvidence, ResolvedGateway, SubscriberId, Subscription, SubscriptionDiscountSnapshot,
    SubscriptionOffer, SubscriptionPaymentContext,
};
use thiserror::Error;

/// Immutable discount evidence copied onto an initial payment attempt.
///
/// Both durable identities are retained: the claim proves which subscriber
/// allocation was consumed, while the code identifies the catalog definition
/// from which the immutable economic snapshot was captured.
#[derive(Clone, Eq, PartialEq)]
pub struct SubscriptionEnrollmentDiscountSnapshot {
    claim_id: DiscountClaimId,
    code_id: DiscountCodeId,
    snapshot: SubscriptionDiscountSnapshot,
}

impl fmt::Debug for SubscriptionEnrollmentDiscountSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SubscriptionEnrollmentDiscountSnapshot")
            .field("claim_id", &self.claim_id)
            .field("code_id", &self.code_id)
            .field("has_code", &true)
            .field("has_label", &self.snapshot.label().is_some())
            .field("kind", &self.snapshot.kind())
            .field("duration", &self.snapshot.duration())
            .field("base_charge", &self.snapshot.base_charge())
            .field("discounted_charge", &self.snapshot.discounted_charge())
            .finish()
    }
}

impl SubscriptionEnrollmentDiscountSnapshot {
    pub const fn new(
        claim_id: DiscountClaimId,
        code_id: DiscountCodeId,
        snapshot: SubscriptionDiscountSnapshot,
    ) -> Self {
        Self {
            claim_id,
            code_id,
            snapshot,
        }
    }

    pub const fn claim_id(&self) -> DiscountClaimId {
        self.claim_id
    }

    pub const fn code_id(&self) -> DiscountCodeId {
        self.code_id
    }

    pub const fn snapshot(&self) -> &SubscriptionDiscountSnapshot {
        &self.snapshot
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum SubscriptionEnrollmentTermsError {
    #[error("saved discount currency does not match the accepted offer")]
    CurrencyMismatch,
    #[error("limited-month discounts require a one-calendar-month recurring period")]
    LimitedDiscountCadence,
}

/// The complete lifecycle and price projection produced when accepted terms
/// create a subscription.
///
/// `SubscriptionEnrollmentExpectedTerms::activation_projection` is the only
/// construction path, so any offer and saved-discount pairing has already
/// passed the accepted-terms invariants. Durable attempts retain those same
/// immutable terms for replay and reconciliation.
///
/// A paid trial charges and schedules its trial period without consuming a
/// recurring discount. An immediate recurring start charges its discounted
/// recurring amount and consumes one discount period. Consequently, a
/// one-month discount returns the base recurring charge after an immediate
/// enrollment, but remains discounted after a paid-trial enrollment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SubscriptionActivationProjection {
    phase: crate::SubscriptionPhase,
    initial_charge: ChargeAmount,
    initial_period_rule: crate::SubscriptionPeriodRule,
    recurring_charge_after_initial: ChargeAmount,
    discount_periods_applied: u8,
}

impl SubscriptionActivationProjection {
    const fn from_expected_terms(expected: &SubscriptionEnrollmentExpectedTerms) -> Self {
        let offer = &expected.offer;
        let discount_snapshot = expected.discount_snapshot.as_ref();
        let start = offer.start();
        let (phase, initial_charge, initial_period_rule) = match start {
            crate::SubscriptionStart::RecurringImmediately => (
                crate::SubscriptionPhase::Recurring,
                match discount_snapshot {
                    Some(snapshot) => snapshot.discounted_charge(),
                    None => offer.recurring().charge(),
                },
                offer.recurring().period(),
            ),
            crate::SubscriptionStart::PaidTrial(trial) => (
                crate::SubscriptionPhase::PaidTrial,
                trial.charge(),
                trial.period(),
            ),
        };
        let discount_periods_applied = match (start, discount_snapshot) {
            (crate::SubscriptionStart::RecurringImmediately, Some(_)) => 1,
            (crate::SubscriptionStart::PaidTrial(_), _) | (_, None) => 0,
        };
        let recurring_charge_after_initial = match discount_snapshot {
            None => offer.recurring().charge(),
            Some(snapshot) => match snapshot.duration() {
                crate::SubscriptionDiscountDuration::Indefinite => snapshot.discounted_charge(),
                crate::SubscriptionDiscountDuration::LimitedMonths(months)
                    if months.get() <= discount_periods_applied =>
                {
                    snapshot.base_charge()
                }
                crate::SubscriptionDiscountDuration::LimitedMonths(_) => {
                    snapshot.discounted_charge()
                }
            },
        };
        Self {
            phase,
            initial_charge,
            initial_period_rule,
            recurring_charge_after_initial,
            discount_periods_applied,
        }
    }

    /// Phase persisted on the newly activated subscription.
    pub const fn phase(self) -> crate::SubscriptionPhase {
        self.phase
    }

    /// Exact charge authorized by the enrollment payment attempt.
    pub const fn initial_charge(self) -> ChargeAmount {
        self.initial_charge
    }

    /// Period opened by a successful enrollment charge.
    pub const fn initial_period_rule(self) -> crate::SubscriptionPeriodRule {
        self.initial_period_rule
    }

    /// Recurring charge persisted for the payment after the initial period.
    pub const fn recurring_charge_after_initial(self) -> ChargeAmount {
        self.recurring_charge_after_initial
    }

    /// Recurring discount periods consumed by the enrollment charge.
    pub const fn discount_periods_applied(self) -> u8 {
        self.discount_periods_applied
    }
}

/// The complete commercial and renewal-failure terms accepted at enrollment.
///
/// A saved discount remains the recurring-price authority after all non-price
/// offer terms match. It never changes the paid-trial charge or cadence.
#[derive(Clone, Eq, PartialEq)]
pub struct SubscriptionEnrollmentExpectedTerms {
    offer: SubscriptionOffer,
    discount_snapshot: Option<SubscriptionDiscountSnapshot>,
}

impl fmt::Debug for SubscriptionEnrollmentExpectedTerms {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = formatter.debug_struct("SubscriptionEnrollmentExpectedTerms");
        debug
            .field("offer", &self.offer)
            .field("has_discount", &self.discount_snapshot.is_some());
        if let Some(snapshot) = &self.discount_snapshot {
            debug
                .field("has_code", &true)
                .field("has_label", &snapshot.label().is_some())
                .field("discount_kind", &snapshot.kind())
                .field("discount_duration", &snapshot.duration())
                .field("saved_base_charge", &snapshot.base_charge())
                .field("saved_discounted_charge", &snapshot.discounted_charge());
        }
        debug.finish()
    }
}

impl SubscriptionEnrollmentExpectedTerms {
    pub const fn full_price(offer: SubscriptionOffer) -> Self {
        Self {
            offer,
            discount_snapshot: None,
        }
    }

    pub fn discounted(
        offer: SubscriptionOffer,
        snapshot: SubscriptionDiscountSnapshot,
    ) -> Result<Self, SubscriptionEnrollmentTermsError> {
        if offer.currency() != *snapshot.currency() {
            return Err(SubscriptionEnrollmentTermsError::CurrencyMismatch);
        }
        if matches!(
            snapshot.duration(),
            crate::SubscriptionDiscountDuration::LimitedMonths(_)
        ) && !offer.recurring().period().is_one_calendar_month()
        {
            return Err(SubscriptionEnrollmentTermsError::LimitedDiscountCadence);
        }
        Ok(Self {
            offer,
            discount_snapshot: Some(snapshot),
        })
    }

    pub const fn offer(&self) -> &SubscriptionOffer {
        &self.offer
    }

    pub const fn plan_key(&self) -> &PlanKey {
        self.offer.plan_key()
    }

    /// Projects the exact accepted terms into their initial subscription state.
    pub const fn activation_projection(&self) -> SubscriptionActivationProjection {
        SubscriptionActivationProjection::from_expected_terms(self)
    }

    /// Exact charge authorized by the enrollment payment attempt.
    pub const fn initial_charge(&self) -> ChargeAmount {
        self.activation_projection().initial_charge()
    }

    pub const fn discount_snapshot(&self) -> Option<&SubscriptionDiscountSnapshot> {
        self.discount_snapshot.as_ref()
    }

    /// Returns the offer representation persisted on a durable initial attempt.
    ///
    /// A saved discount's captured base charge replaces later catalog price
    /// drift while trial, cadence, and failure policy remain unchanged.
    pub fn durable_offer(&self) -> SubscriptionOffer {
        let recurring_charge = self
            .discount_snapshot
            .as_ref()
            .map_or(self.offer.recurring().charge(), |snapshot| {
                snapshot.base_charge()
            });
        SubscriptionOffer::new(
            self.offer.plan_key().clone(),
            crate::RecurringSubscriptionTerms::new(
                recurring_charge,
                self.offer.recurring().period(),
            ),
            self.offer.start(),
            self.offer.renewal_failure().clone(),
        )
        .expect("validated expected terms preserve one currency")
    }

    /// Compares accepted terms with rows locked by the owning transaction.
    ///
    /// For a discounted enrollment, catalog recurring-price drift is ignored
    /// only after plan, trial, cadence, currency, failure policy, and saved
    /// discount identity all match.
    pub fn matches_locked_terms(
        &self,
        current_offer: &SubscriptionOffer,
        saved_discount: Option<&SubscriptionDiscountSnapshot>,
    ) -> bool {
        match (&self.discount_snapshot, saved_discount) {
            (None, None) => &self.offer == current_offer,
            (Some(expected_snapshot), Some(saved_snapshot)) => {
                self.offer
                    .has_same_terms_except_recurring_amount(current_offer)
                    && current_offer.currency() == *expected_snapshot.currency()
                    && expected_snapshot.has_same_charge_terms(saved_snapshot)
            }
            _ => false,
        }
    }
}

/// Provider-neutral request to begin one subscription enrollment attempt.
///
/// The payment token remains in memory only. Durable reservation persists the
/// typed identities and economic snapshot, never the token.
#[derive(Clone)]
pub struct EnrollSubscription {
    context: SubscriptionPaymentContext,
    expected_terms: SubscriptionEnrollmentExpectedTerms,
}

impl EnrollSubscription {
    pub const fn new(
        context: SubscriptionPaymentContext,
        expected_terms: SubscriptionEnrollmentExpectedTerms,
    ) -> Self {
        Self {
            context,
            expected_terms,
        }
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
        self.expected_terms.plan_key()
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

    pub const fn expected_terms(&self) -> &SubscriptionEnrollmentExpectedTerms {
        &self.expected_terms
    }
}

impl fmt::Debug for EnrollSubscription {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EnrollSubscription")
            .field("attempt_id", &self.attempt_id())
            .field("billing_scope_id", &self.billing_scope_id())
            .field("subscriber_id", &self.subscriber_id())
            .field("plan_key", &self.plan_key())
            .field("gateway_configuration_id", &self.gateway_configuration_id())
            .field("has_idempotency_key", &true)
            .field("has_payment_token", &true)
            .field("has_billing_contact", &true)
            .field("expected_terms", &self.expected_terms)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum SubscriptionEnrollmentReservationBuildError {
    #[error("resolved gateway identity does not match the enrollment command")]
    GatewayIdentityMismatch,
    #[error("only subscription-initial attempts can become enrollment reservations")]
    AttemptKindMismatch,
    #[error("subscription-initial attempt has an invalid charge amount")]
    InvalidCharge,
    #[error("subscription-initial attempt has invalid accepted terms")]
    InvalidTerms,
}

/// Secret-free input for the durable enrollment reservation transaction.
///
/// Construction binds the command to the exact provider-free resolver result
/// and derives the provider order reference before any database lock is held.
#[derive(Clone, Eq, PartialEq)]
pub struct SubscriptionEnrollmentReservation {
    identity: PaymentAttemptIdentity,
    provider_key: GatewayProviderKey,
    idempotency_key: IdempotencyKey,
    gateway_order_id: GatewayOrderId,
    billing_contact: BillingContactSnapshot,
    expected_terms: SubscriptionEnrollmentExpectedTerms,
}

impl SubscriptionEnrollmentReservation {
    pub fn from_command(
        command: &EnrollSubscription,
        gateway: &ResolvedGateway,
    ) -> Result<Self, SubscriptionEnrollmentReservationBuildError> {
        Self::from_command_for_attempt(command, gateway, command.attempt_id())
    }

    /// Builds the same request for an already-durable matching attempt.
    ///
    /// The candidate ID on a retried command is not part of idempotency. Once
    /// reservation discovers a matching prepared attempt, orchestration binds
    /// all later admission and submission work to that durable attempt ID.
    pub fn from_command_for_attempt(
        command: &EnrollSubscription,
        gateway: &ResolvedGateway,
        attempt_id: PaymentAttemptId,
    ) -> Result<Self, SubscriptionEnrollmentReservationBuildError> {
        if gateway.billing_scope_id() != command.billing_scope_id()
            || gateway.gateway_configuration_id() != command.gateway_configuration_id()
        {
            return Err(SubscriptionEnrollmentReservationBuildError::GatewayIdentityMismatch);
        }
        let identity = PaymentAttemptIdentity::new(
            attempt_id,
            command.billing_scope_id(),
            command.subscriber_id(),
            gateway.gateway_account_id(),
            gateway.gateway_configuration_id(),
        );
        let gateway_order_id = gateway
            .mutation_reference_factory()
            .for_attempt(PaymentAttemptKind::SubscriptionInitial, attempt_id);
        Ok(Self {
            identity,
            provider_key: gateway.provider_key().clone(),
            idempotency_key: command.idempotency_key().clone(),
            gateway_order_id,
            billing_contact: BillingContactSnapshot::from_billing_contact(
                command.billing_contact(),
            ),
            expected_terms: command.expected_terms().clone(),
        })
    }

    /// Reconstructs application authority for a durable initial attempt.
    ///
    /// Reconciliation supplies the canonical provider key joined through the
    /// attempt's gateway account. No payment token, live offer, or gateway
    /// resolver is needed because this path applies an already-observed
    /// provider outcome and never submits another mutation.
    pub fn from_attempt(
        attempt: &PaymentAttempt,
        provider_key: GatewayProviderKey,
    ) -> Result<Self, SubscriptionEnrollmentReservationBuildError> {
        let request = attempt.request();
        let PaymentAttemptTarget::SubscriptionInitial {
            offer, discount, ..
        } = request.target()
        else {
            return Err(SubscriptionEnrollmentReservationBuildError::AttemptKindMismatch);
        };
        let expected_terms = match discount {
            Some(discount) => SubscriptionEnrollmentExpectedTerms::discounted(
                offer.clone(),
                discount.snapshot().clone(),
            )
            .map_err(|_| SubscriptionEnrollmentReservationBuildError::InvalidTerms)?,
            None => SubscriptionEnrollmentExpectedTerms::full_price(offer.clone()),
        };
        Ok(Self {
            identity: attempt.identity(),
            provider_key,
            idempotency_key: request.idempotency_key().clone(),
            gateway_order_id: request.gateway_order_id().clone(),
            billing_contact: request.billing_contact().clone(),
            expected_terms,
        })
    }

    pub const fn identity(&self) -> PaymentAttemptIdentity {
        self.identity
    }

    pub const fn provider_key(&self) -> &GatewayProviderKey {
        &self.provider_key
    }

    pub const fn idempotency_key(&self) -> &IdempotencyKey {
        &self.idempotency_key
    }

    pub const fn gateway_order_id(&self) -> &GatewayOrderId {
        &self.gateway_order_id
    }

    pub const fn billing_contact(&self) -> &BillingContactSnapshot {
        &self.billing_contact
    }

    pub const fn expected_terms(&self) -> &SubscriptionEnrollmentExpectedTerms {
        &self.expected_terms
    }

    pub const fn plan_key(&self) -> &PlanKey {
        self.expected_terms.plan_key()
    }
}

impl fmt::Debug for SubscriptionEnrollmentReservation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SubscriptionEnrollmentReservation")
            .field("identity", &self.identity)
            .field("provider_key", &self.provider_key)
            .field("has_idempotency_key", &true)
            .field("has_gateway_order_id", &true)
            .field("billing_contact", &self.billing_contact)
            .field("expected_terms", &self.expected_terms)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubscriptionEnrollmentReservationRejection {
    CurrentSubscription,
    ActiveGrant,
    UnresolvedProcessorCharge,
    EnrollmentTermsChanged,
    GatewayConfigurationChanged,
    AttemptInProgress,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SubscriptionEnrollmentReservationOutcome {
    Reserved(PaymentAttempt),
    Replay(PaymentAttempt),
    IdempotencyConflict,
    Rejected(SubscriptionEnrollmentReservationRejection),
}

/// Replay decision made before consuming host mutation admission.
///
/// A matching still-prepared attempt continues through normal admission. A
/// submitted or terminal attempt is returned immediately, and a stale
/// prepared attempt is terminalized and returned without consulting live
/// enrollment terms or resolving a gateway.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SubscriptionEnrollmentPreflightOutcome {
    Continue,
    Replay(Box<PaymentAttempt>),
    IdempotencyConflict,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubscriptionEnrollmentSubmissionRejection {
    BillingStateChanged,
    EnrollmentTermsChanged,
    GatewayConfigurationChanged,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SubscriptionEnrollmentSubmissionOutcome {
    Admitted(PaymentAttempt),
    AlreadyAdmitted(PaymentAttempt),
    Rejected {
        attempt: PaymentAttempt,
        reason: SubscriptionEnrollmentSubmissionRejection,
    },
}

/// Durable result of applying one subscription provider outcome.
///
/// Construction distinguishes an applied approval, a result that was not
/// applied, and approved evidence whose confirmation is still pending. A
/// subscription is therefore present only after the approval, payment method,
/// recurring economics, discount, processor charge, and host event have
/// committed in one transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubscriptionEnrollmentPaymentResult {
    attempt: PaymentAttempt,
    state: SubscriptionEnrollmentPaymentResultState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum SubscriptionEnrollmentPaymentResultState {
    Applied(Subscription),
    NotApplied,
    ConfirmationPending(ApprovedProcessorEvidence),
}

/// Invalid attempt state supplied to a payment-result constructor.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum SubscriptionEnrollmentPaymentResultBuildError {
    #[error("a subscription payment result requires a subscription attempt")]
    AttemptNotSubscription,
    #[error("an applied payment result requires an approved attempt")]
    AppliedAttemptNotApproved,
    #[error("the applied subscription ID does not match the payment attempt target")]
    AppliedSubscriptionIdMismatch,
    #[error("the applied subscription plan does not match the payment attempt target")]
    AppliedSubscriptionPlanMismatch,
    #[error("an approved attempt cannot produce a not-applied payment result")]
    NotAppliedAttemptApproved,
    #[error("an approved attempt cannot produce a confirmation-pending payment result")]
    ConfirmationPendingAttemptApproved,
}

impl SubscriptionEnrollmentPaymentResult {
    /// Builds a result whose approved outcome was applied atomically.
    pub fn applied(
        attempt: PaymentAttempt,
        subscription: Subscription,
    ) -> Result<Self, SubscriptionEnrollmentPaymentResultBuildError> {
        require_subscription_attempt(&attempt)?;
        if attempt.status() != PaymentAttemptStatus::Approved {
            return Err(SubscriptionEnrollmentPaymentResultBuildError::AppliedAttemptNotApproved);
        }
        if attempt.request().target().subscription_id() != Some(subscription.id()) {
            return Err(
                SubscriptionEnrollmentPaymentResultBuildError::AppliedSubscriptionIdMismatch,
            );
        }
        if attempt.request().target().plan_key() != Some(subscription.plan_key()) {
            return Err(
                SubscriptionEnrollmentPaymentResultBuildError::AppliedSubscriptionPlanMismatch,
            );
        }
        Ok(Self {
            attempt,
            state: SubscriptionEnrollmentPaymentResultState::Applied(subscription),
        })
    }

    /// Builds a canonical result that did not apply subscription state.
    pub fn not_applied(
        attempt: PaymentAttempt,
    ) -> Result<Self, SubscriptionEnrollmentPaymentResultBuildError> {
        require_subscription_attempt(&attempt)?;
        if attempt.status() == PaymentAttemptStatus::Approved {
            return Err(SubscriptionEnrollmentPaymentResultBuildError::NotAppliedAttemptApproved);
        }
        Ok(Self {
            attempt,
            state: SubscriptionEnrollmentPaymentResultState::NotApplied,
        })
    }

    /// Returns a result for approved evidence that is durable but could not be
    /// attached to the locked attempt in this call. The durable attempt remains
    /// authoritative; callers must present this observation as confirmation
    /// pending rather than as the attempt's older status.
    pub fn confirmation_pending(
        attempt: PaymentAttempt,
        evidence: ApprovedProcessorEvidence,
    ) -> Result<Self, SubscriptionEnrollmentPaymentResultBuildError> {
        require_subscription_attempt(&attempt)?;
        if attempt.status() == PaymentAttemptStatus::Approved {
            return Err(
                SubscriptionEnrollmentPaymentResultBuildError::ConfirmationPendingAttemptApproved,
            );
        }
        Ok(Self {
            attempt,
            state: SubscriptionEnrollmentPaymentResultState::ConfirmationPending(evidence),
        })
    }

    pub const fn attempt(&self) -> &PaymentAttempt {
        &self.attempt
    }

    pub const fn subscription(&self) -> Option<&Subscription> {
        match &self.state {
            SubscriptionEnrollmentPaymentResultState::Applied(subscription) => Some(subscription),
            SubscriptionEnrollmentPaymentResultState::NotApplied
            | SubscriptionEnrollmentPaymentResultState::ConfirmationPending(_) => None,
        }
    }

    pub const fn status(&self) -> PaymentAttemptStatus {
        match &self.state {
            SubscriptionEnrollmentPaymentResultState::ConfirmationPending(_) => {
                PaymentAttemptStatus::Unknown
            }
            SubscriptionEnrollmentPaymentResultState::Applied(_)
            | SubscriptionEnrollmentPaymentResultState::NotApplied => self.attempt.status(),
        }
    }

    pub fn processor_evidence(&self) -> &ProcessorEvidence {
        match &self.state {
            SubscriptionEnrollmentPaymentResultState::ConfirmationPending(evidence) => {
                evidence.evidence()
            }
            SubscriptionEnrollmentPaymentResultState::Applied(_)
            | SubscriptionEnrollmentPaymentResultState::NotApplied => {
                self.attempt.state().processor_evidence()
            }
        }
    }

    pub const fn is_confirmation_pending(&self) -> bool {
        matches!(
            &self.state,
            SubscriptionEnrollmentPaymentResultState::ConfirmationPending(_)
        )
    }

    pub fn into_parts(
        self,
    ) -> (
        PaymentAttempt,
        Option<Subscription>,
        Option<ProcessorEvidence>,
    ) {
        match self.state {
            SubscriptionEnrollmentPaymentResultState::Applied(subscription) => {
                (self.attempt, Some(subscription), None)
            }
            SubscriptionEnrollmentPaymentResultState::NotApplied => (self.attempt, None, None),
            SubscriptionEnrollmentPaymentResultState::ConfirmationPending(evidence) => {
                (self.attempt, None, Some(evidence.into_evidence()))
            }
        }
    }
}

fn require_subscription_attempt(
    attempt: &PaymentAttempt,
) -> Result<(), SubscriptionEnrollmentPaymentResultBuildError> {
    match attempt.kind() {
        PaymentAttemptKind::SubscriptionInitial
        | PaymentAttemptKind::SubscriptionRenewal
        | PaymentAttemptKind::SubscriptionRecovery
        | PaymentAttemptKind::SubscriptionPaymentMethodUpdate => Ok(()),
        PaymentAttemptKind::HostCharge => {
            Err(SubscriptionEnrollmentPaymentResultBuildError::AttemptNotSubscription)
        }
    }
}

#[cfg(test)]
mod tests;
