use std::fmt;

use crate::{
    BillingContact, BillingContactSnapshot, BillingScopeId, ChargeAmount, DiscountClaimId,
    DiscountCodeId, GatewayConfigurationId, GatewayOrderId, GatewayProviderKey, IdempotencyKey,
    PaymentAttempt, PaymentAttemptId, PaymentAttemptIdentity, PaymentAttemptKind, PaymentToken,
    PlanKey, ResolvedGateway, SubscriberId, SubscriptionDiscountSnapshot, SubscriptionOffer,
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

/// The exact economic terms a subscriber accepted for a new subscription.
///
/// A full-price expectation follows the current locked offer. A discounted
/// expectation follows the immutable saved-claim snapshot while still requiring
/// the requested plan to have a current locked offer. This distinction prevents
/// a saved claim from being silently removed or introduced between the request,
/// durable reservation, and final gateway admission.
#[derive(Clone, Eq, PartialEq)]
pub enum SubscriptionEnrollmentExpectedCharge {
    FullPrice(SubscriptionOffer),
    Discounted {
        plan_key: PlanKey,
        snapshot: SubscriptionDiscountSnapshot,
    },
}

impl fmt::Debug for SubscriptionEnrollmentExpectedCharge {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FullPrice(offer) => formatter
                .debug_struct("SubscriptionEnrollmentExpectedCharge::FullPrice")
                .field("plan_key", offer.plan_key())
                .field("charge", &offer.base_charge())
                .finish(),
            Self::Discounted { plan_key, snapshot } => formatter
                .debug_struct("SubscriptionEnrollmentExpectedCharge::Discounted")
                .field("plan_key", plan_key)
                .field("has_code", &true)
                .field("has_label", &snapshot.label().is_some())
                .field("kind", &snapshot.kind())
                .field("duration", &snapshot.duration())
                .field("base_charge", &snapshot.base_charge())
                .field("discounted_charge", &snapshot.discounted_charge())
                .finish(),
        }
    }
}

impl SubscriptionEnrollmentExpectedCharge {
    pub const fn full_price(offer: SubscriptionOffer) -> Self {
        Self::FullPrice(offer)
    }

    pub const fn discounted(plan_key: PlanKey, snapshot: SubscriptionDiscountSnapshot) -> Self {
        Self::Discounted { plan_key, snapshot }
    }

    pub const fn plan_key(&self) -> &PlanKey {
        match self {
            Self::FullPrice(offer) => offer.plan_key(),
            Self::Discounted { plan_key, .. } => plan_key,
        }
    }

    pub const fn charge(&self) -> ChargeAmount {
        match self {
            Self::FullPrice(offer) => offer.base_charge(),
            Self::Discounted { snapshot, .. } => snapshot.discounted_charge(),
        }
    }

    pub const fn discount_snapshot(&self) -> Option<&SubscriptionDiscountSnapshot> {
        match self {
            Self::FullPrice(_) => None,
            Self::Discounted { snapshot, .. } => Some(snapshot),
        }
    }

    /// Compares the accepted terms with rows locked by the owning transaction.
    ///
    /// A saved discount deliberately keeps its captured economics when the host
    /// later changes the catalog price. The current offer is still required so
    /// a removed plan cannot be enrolled through an old claim.
    pub fn matches_locked_terms(
        &self,
        current_offer: &SubscriptionOffer,
        saved_discount: Option<&SubscriptionDiscountSnapshot>,
    ) -> bool {
        if self.plan_key() != current_offer.plan_key() {
            return false;
        }
        match (self, saved_discount) {
            (Self::FullPrice(expected), None) => expected == current_offer,
            (Self::Discounted { snapshot, .. }, Some(saved)) => snapshot == saved,
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
    attempt_id: PaymentAttemptId,
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    gateway_configuration_id: GatewayConfigurationId,
    idempotency_key: IdempotencyKey,
    payment_token: PaymentToken,
    billing_contact: BillingContact,
    expected_charge: SubscriptionEnrollmentExpectedCharge,
}

impl EnrollSubscription {
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        attempt_id: PaymentAttemptId,
        billing_scope_id: BillingScopeId,
        subscriber_id: SubscriberId,
        gateway_configuration_id: GatewayConfigurationId,
        idempotency_key: IdempotencyKey,
        payment_token: PaymentToken,
        billing_contact: BillingContact,
        expected_charge: SubscriptionEnrollmentExpectedCharge,
    ) -> Self {
        Self {
            attempt_id,
            billing_scope_id,
            subscriber_id,
            gateway_configuration_id,
            idempotency_key,
            payment_token,
            billing_contact,
            expected_charge,
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
        self.expected_charge.plan_key()
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

    pub const fn expected_charge(&self) -> &SubscriptionEnrollmentExpectedCharge {
        &self.expected_charge
    }
}

impl fmt::Debug for EnrollSubscription {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EnrollSubscription")
            .field("attempt_id", &self.attempt_id)
            .field("billing_scope_id", &self.billing_scope_id)
            .field("subscriber_id", &self.subscriber_id)
            .field("plan_key", &self.plan_key())
            .field("gateway_configuration_id", &self.gateway_configuration_id)
            .field("has_idempotency_key", &true)
            .field("has_payment_token", &true)
            .field("has_billing_contact", &true)
            .field("expected_charge", &self.expected_charge)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum SubscriptionEnrollmentReservationBuildError {
    #[error("resolved gateway identity does not match the enrollment command")]
    GatewayIdentityMismatch,
}

/// Secret-free input for the durable enrollment reservation transaction.
///
/// Construction binds the command to the exact provider-free resolver result
/// and derives the provider order reference before any database lock is held.
#[derive(Clone)]
pub struct SubscriptionEnrollmentReservation {
    identity: PaymentAttemptIdentity,
    provider_key: GatewayProviderKey,
    idempotency_key: IdempotencyKey,
    gateway_order_id: GatewayOrderId,
    billing_contact: BillingContactSnapshot,
    expected_charge: SubscriptionEnrollmentExpectedCharge,
}

impl SubscriptionEnrollmentReservation {
    pub fn from_command(
        command: &EnrollSubscription,
        gateway: &ResolvedGateway,
    ) -> Result<Self, SubscriptionEnrollmentReservationBuildError> {
        if gateway.billing_scope_id() != command.billing_scope_id()
            || gateway.gateway_configuration_id() != command.gateway_configuration_id()
        {
            return Err(SubscriptionEnrollmentReservationBuildError::GatewayIdentityMismatch);
        }
        let identity = PaymentAttemptIdentity::new(
            command.attempt_id(),
            command.billing_scope_id(),
            command.subscriber_id(),
            gateway.gateway_account_id(),
            gateway.gateway_configuration_id(),
        );
        let gateway_order_id = gateway.mutation_reference_factory().for_attempt(
            PaymentAttemptKind::SubscriptionInitial,
            command.attempt_id(),
        );
        Ok(Self {
            identity,
            provider_key: gateway.provider_key().clone(),
            idempotency_key: command.idempotency_key().clone(),
            gateway_order_id,
            billing_contact: BillingContactSnapshot::from_billing_contact(
                command.billing_contact(),
            ),
            expected_charge: command.expected_charge().clone(),
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

    pub const fn expected_charge(&self) -> &SubscriptionEnrollmentExpectedCharge {
        &self.expected_charge
    }

    pub const fn plan_key(&self) -> &PlanKey {
        self.expected_charge.plan_key()
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
            .field("expected_charge", &self.expected_charge)
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

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;
    use crate::{
        CurrencyCode, LimitedDiscountMonths, PercentOffBasisPoints, SubscriptionDiscountCode,
        SubscriptionDiscountDuration, SubscriptionDiscountKind,
    };

    fn plan(value: &str) -> PlanKey {
        PlanKey::new(value).unwrap()
    }

    fn offer(plan_key: PlanKey, cents: i32) -> SubscriptionOffer {
        SubscriptionOffer::new(
            plan_key,
            ChargeAmount::new(cents, CurrencyCode::new("USD").unwrap()).unwrap(),
        )
    }

    fn discount(base_cents: i32, discounted_cents: i32) -> SubscriptionDiscountSnapshot {
        let currency = CurrencyCode::new("USD").unwrap();
        SubscriptionDiscountSnapshot::new(
            SubscriptionDiscountCode::new("SAVE20").unwrap(),
            Some("Launch offer".to_owned()),
            SubscriptionDiscountKind::PercentOffBasisPoints(
                PercentOffBasisPoints::new(2000).unwrap(),
            ),
            SubscriptionDiscountDuration::LimitedMonths(LimitedDiscountMonths::new(3).unwrap()),
            ChargeAmount::new(base_cents, currency).unwrap(),
            ChargeAmount::new(discounted_cents, currency).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn full_price_requires_the_exact_plan_offer_and_no_saved_claim() {
        let expected = SubscriptionEnrollmentExpectedCharge::full_price(offer(plan("basic"), 1000));
        assert!(expected.matches_locked_terms(&offer(plan("basic"), 1000), None));
        assert!(!expected.matches_locked_terms(&offer(plan("premium"), 1000), None));
        assert!(!expected.matches_locked_terms(&offer(plan("basic"), 1200), None));
        let saved = discount(1000, 800);
        assert!(!expected.matches_locked_terms(&offer(plan("basic"), 1000), Some(&saved)));
    }

    #[test]
    fn saved_discount_keeps_its_snapshot_while_the_plan_must_still_exist() {
        let saved = discount(1000, 800);
        let expected =
            SubscriptionEnrollmentExpectedCharge::discounted(plan("basic"), saved.clone());
        assert!(expected.matches_locked_terms(&offer(plan("basic"), 1400), Some(&saved)));
        assert!(!expected.matches_locked_terms(&offer(plan("premium"), 1400), Some(&saved)));
        assert!(
            !expected
                .matches_locked_terms(&offer(plan("basic"), 1400), Some(&discount(1000, 750)),)
        );
        assert!(!expected.matches_locked_terms(&offer(plan("basic"), 1400), None));
    }

    #[test]
    fn enrollment_debug_omits_token_key_and_contact_values() {
        let command = EnrollSubscription::new(
            PaymentAttemptId::new(Uuid::from_u128(1)),
            BillingScopeId::new(Uuid::from_u128(2)),
            SubscriberId::new(Uuid::from_u128(3)),
            GatewayConfigurationId::new(Uuid::from_u128(4)),
            IdempotencyKey::new("secret-key").unwrap(),
            PaymentToken::new("secret-token").unwrap(),
            BillingContact::new(
                None,
                Some("Secret Name".to_owned()),
                Some("secret@example.test".to_owned()),
            )
            .unwrap(),
            SubscriptionEnrollmentExpectedCharge::full_price(offer(plan("basic"), 1000)),
        );
        let debug = format!("{command:?}");
        for secret in [
            "secret-key",
            "secret-token",
            "Secret Name",
            "secret@example.test",
        ] {
            assert!(!debug.contains(secret));
        }
        assert!(debug.contains("has_payment_token"));
    }

    #[test]
    fn durable_discount_debug_omits_code_and_label_values() {
        let expected = SubscriptionEnrollmentExpectedCharge::discounted(
            plan("basic"),
            SubscriptionDiscountSnapshot::new(
                SubscriptionDiscountCode::new("SECRET20").unwrap(),
                Some("Sensitive campaign label".to_owned()),
                SubscriptionDiscountKind::PercentOffBasisPoints(
                    PercentOffBasisPoints::new(2_000).unwrap(),
                ),
                SubscriptionDiscountDuration::LimitedMonths(LimitedDiscountMonths::new(3).unwrap()),
                ChargeAmount::new(1_000, CurrencyCode::new("USD").unwrap()).unwrap(),
                ChargeAmount::new(800, CurrencyCode::new("USD").unwrap()).unwrap(),
            )
            .unwrap(),
        );
        let expected_debug = format!("{expected:?}");
        assert!(!expected_debug.contains("SECRET20"));
        assert!(!expected_debug.contains("Sensitive campaign label"));
        assert!(expected_debug.contains("has_code"));
        assert!(expected_debug.contains("has_label"));

        let snapshot = SubscriptionEnrollmentDiscountSnapshot::new(
            DiscountClaimId::new(Uuid::from_u128(10)),
            DiscountCodeId::new(Uuid::from_u128(11)),
            expected
                .discount_snapshot()
                .expect("discounted expectation should retain snapshot")
                .clone(),
        );
        let debug = format!("{snapshot:?}");
        assert!(!debug.contains("SECRET20"));
        assert!(!debug.contains("Sensitive campaign label"));
        assert!(debug.contains("has_code"));
        assert!(debug.contains("has_label"));
    }
}
