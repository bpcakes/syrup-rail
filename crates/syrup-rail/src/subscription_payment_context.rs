use std::fmt;

use crate::{
    BillingContact, BillingScopeId, CardholderIpAddress, ChargeAmount, GatewayConfigurationId,
    GatewayOrderId, GatewaySaleIntent, GatewaySaleRequest, IdempotencyKey, PaymentAttemptId,
    PaymentToken, SubscriberId,
};

/// Request-scoped input shared by subscription payment commands.
///
/// The payment token remains in memory only. Durable reservations retain only
/// the permitted billing-contact snapshot and never the token.
///
/// The optional cardholder network address is memory-only too. It is sent
/// with the token sale an enrollment or recovery submits and is otherwise
/// ignored: it is never persisted, and a replay may carry a different address
/// or none without conflicting with the original request. A payment-method
/// replacement does not send it.
///
/// `PartialEq` compares every field, including the token and the address. It
/// is not replay equality, which the durable reservation decides.
#[derive(Clone, Eq, PartialEq)]
pub struct SubscriptionPaymentContext {
    attempt_id: PaymentAttemptId,
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    gateway_configuration_id: GatewayConfigurationId,
    idempotency_key: IdempotencyKey,
    payment_token: PaymentToken,
    billing_contact: BillingContact,
    cardholder_ip: Option<CardholderIpAddress>,
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
            cardholder_ip: None,
        }
    }

    /// Sets the network address the cardholder used when they authorized this
    /// payment, replacing any address already set. See
    /// [`CardholderIpAddress`] for what the host vouches for.
    pub const fn with_cardholder_ip(mut self, cardholder_ip: Option<CardholderIpAddress>) -> Self {
        self.cardholder_ip = cardholder_ip;
        self
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

    pub const fn cardholder_ip(&self) -> Option<CardholderIpAddress> {
        self.cardholder_ip
    }

    /// The token sale the customer submits with this context, as enrollment
    /// and recovery send it: the one-shot token stored as an initial
    /// credential, the billing contact, and the cardholder address if the
    /// host supplied one. The charge and order come from the durable attempt.
    pub fn token_sale_request(
        &self,
        charge: ChargeAmount,
        order_id: GatewayOrderId,
    ) -> GatewaySaleRequest {
        GatewaySaleRequest::new(
            charge,
            order_id,
            GatewaySaleIntent::InitialStoredCredential {
                payment_token: self.payment_token.clone(),
            },
            Some(self.billing_contact.clone()),
        )
        .with_cardholder_ip(self.cardholder_ip)
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
            .field("has_cardholder_ip", &self.cardholder_ip.is_some())
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
        assert_eq!(context.cardholder_ip(), None);
        assert!(format!("{context:?}").contains("has_cardholder_ip: false"));

        let address = CardholderIpAddress::new(std::net::IpAddr::V4(std::net::Ipv4Addr::new(
            203, 0, 113, 7,
        )));
        let context = context.with_cardholder_ip(Some(address));
        assert_eq!(context.cardholder_ip(), Some(address));

        // The sale built from the context is the customer's own token sale
        // and carries exactly what the context holds.
        let charge = ChargeAmount::new(100, crate::CurrencyCode::new("USD").unwrap()).unwrap();
        let order = GatewayOrderId::from_correlation("ck_context_sale").unwrap();
        let sale = context.token_sale_request(charge, order.clone());
        assert_eq!(sale.charge(), charge);
        assert_eq!(sale.order_id(), &order);
        assert_eq!(sale.billing_contact(), Some(&billing_contact));
        assert_eq!(sale.cardholder_ip(), Some(address));
        assert_eq!(sale.descriptor(), None);
        assert!(matches!(
            sale.intent(),
            GatewaySaleIntent::InitialStoredCredential { payment_token: token }
                if token == &payment_token
        ));
        assert_eq!(
            context
                .clone()
                .with_cardholder_ip(None)
                .token_sale_request(charge, order)
                .cardholder_ip(),
            None
        );

        let debug = format!("{context:?}");
        for secret in [
            "secret-key",
            "secret-token",
            "Secret Name",
            "secret@example.test",
            "203.0.113.7",
        ] {
            assert!(!debug.contains(secret));
        }
        assert!(debug.contains("has_idempotency_key"));
        assert!(debug.contains("has_payment_token"));
        assert!(debug.contains("has_billing_contact"));
        assert!(debug.contains("has_cardholder_ip: true"));
    }
}
