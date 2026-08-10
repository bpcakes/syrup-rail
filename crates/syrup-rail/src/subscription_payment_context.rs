use std::fmt;

use crate::{
    BillingContact, BillingScopeId, GatewayConfigurationId, IdempotencyKey, PaymentAttemptId,
    PaymentToken, SubscriberId,
};

/// Request-scoped input shared by subscription payment commands.
///
/// The payment token remains in memory only. Durable reservations retain only
/// the permitted billing-contact snapshot and never the token.
#[derive(Clone, Eq, PartialEq)]
pub struct SubscriptionPaymentContext {
    attempt_id: PaymentAttemptId,
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    gateway_configuration_id: GatewayConfigurationId,
    idempotency_key: IdempotencyKey,
    payment_token: PaymentToken,
    billing_contact: BillingContact,
}

impl SubscriptionPaymentContext {
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        attempt_id: PaymentAttemptId,
        billing_scope_id: BillingScopeId,
        subscriber_id: SubscriberId,
        gateway_configuration_id: GatewayConfigurationId,
        idempotency_key: IdempotencyKey,
        payment_token: PaymentToken,
        billing_contact: BillingContact,
    ) -> Self {
        Self {
            attempt_id,
            billing_scope_id,
            subscriber_id,
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

impl fmt::Debug for SubscriptionPaymentContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SubscriptionPaymentContext")
            .field("attempt_id", &self.attempt_id)
            .field("billing_scope_id", &self.billing_scope_id)
            .field("subscriber_id", &self.subscriber_id)
            .field("gateway_configuration_id", &self.gateway_configuration_id)
            .field("has_idempotency_key", &true)
            .field("has_payment_token", &true)
            .field("has_billing_contact", &true)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;

    #[test]
    fn context_accessors_preserve_exact_request_values_and_debug_redacts_values() {
        let attempt_id = PaymentAttemptId::new(Uuid::from_u128(1));
        let billing_scope_id = BillingScopeId::new(Uuid::from_u128(2));
        let subscriber_id = SubscriberId::new(Uuid::from_u128(3));
        let gateway_configuration_id = GatewayConfigurationId::new(Uuid::from_u128(4));
        let idempotency_key = IdempotencyKey::new("secret-key").unwrap();
        let payment_token = PaymentToken::new("secret-token").unwrap();
        let billing_contact = BillingContact::new(
            None,
            Some("Secret Name".to_owned()),
            Some("secret@example.test".to_owned()),
        )
        .unwrap();
        let context = SubscriptionPaymentContext::new(
            attempt_id,
            billing_scope_id,
            subscriber_id,
            gateway_configuration_id,
            idempotency_key.clone(),
            payment_token.clone(),
            billing_contact.clone(),
        );

        assert_eq!(context.attempt_id(), attempt_id);
        assert_eq!(context.billing_scope_id(), billing_scope_id);
        assert_eq!(context.subscriber_id(), subscriber_id);
        assert_eq!(context.gateway_configuration_id(), gateway_configuration_id);
        assert_eq!(context.idempotency_key(), &idempotency_key);
        assert_eq!(context.payment_token(), &payment_token);
        assert_eq!(context.billing_contact(), &billing_contact);

        let debug = format!("{context:?}");
        for secret in [
            "secret-key",
            "secret-token",
            "Secret Name",
            "secret@example.test",
        ] {
            assert!(!debug.contains(secret));
        }
        assert!(debug.contains("has_idempotency_key"));
        assert!(debug.contains("has_payment_token"));
        assert!(debug.contains("has_billing_contact"));
    }
}
