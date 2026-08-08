use std::fmt;

use syrup_rail::{
    GatewayMutationReferenceFactory, GatewayOrderId, PaymentAttemptId, PaymentAttemptKind,
};
use thiserror::Error;

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum NmiNamespaceError {
    #[error("NMI mutation namespace must contain exactly two lowercase ASCII letters")]
    Invalid,
}

#[derive(Clone, Eq, PartialEq)]
pub struct NmiMutationNamespace([u8; 2]);

impl NmiMutationNamespace {
    pub fn new(value: &str) -> Result<Self, NmiNamespaceError> {
        let bytes: [u8; 2] = value
            .as_bytes()
            .try_into()
            .map_err(|_| NmiNamespaceError::Invalid)?;
        if !bytes.iter().all(u8::is_ascii_lowercase) {
            return Err(NmiNamespaceError::Invalid);
        }
        Ok(Self(bytes))
    }

    fn as_str(&self) -> &str {
        std::str::from_utf8(&self.0).expect("validated ASCII namespace is UTF-8")
    }
}

impl fmt::Debug for NmiMutationNamespace {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("NmiMutationNamespace")
            .field(&self.as_str())
            .finish()
    }
}

#[derive(Clone, Debug)]
pub struct NmiMutationReferenceFactory {
    namespace: NmiMutationNamespace,
}

impl NmiMutationReferenceFactory {
    pub const fn new(namespace: NmiMutationNamespace) -> Self {
        Self { namespace }
    }
}

impl GatewayMutationReferenceFactory for NmiMutationReferenceFactory {
    fn for_attempt(
        &self,
        kind: PaymentAttemptKind,
        attempt_id: PaymentAttemptId,
    ) -> GatewayOrderId {
        let kind = match kind {
            PaymentAttemptKind::HostCharge => "order",
            PaymentAttemptKind::SubscriptionInitial => "base-sub",
            PaymentAttemptKind::SubscriptionRenewal => "renewal",
            PaymentAttemptKind::SubscriptionRecovery => "recovery",
            PaymentAttemptKind::SubscriptionPaymentMethodUpdate => "payment-method",
        };
        let value = format!(
            "{}_{kind}_{}",
            self.namespace.as_str(),
            attempt_id.as_uuid().simple()
        );
        GatewayOrderId::from_generated_attempt(value, attempt_id)
            .expect("validated namespace, closed kind token, and UUID form a valid NMI order ID")
    }
}

#[cfg(test)]
mod tests {
    use syrup_rail::GatewayMutationReferenceFactory;
    use uuid::Uuid;

    use super::*;

    #[test]
    fn namespace_is_exactly_two_lowercase_ascii_letters() {
        assert!(NmiMutationNamespace::new("ck").is_ok());
        for invalid in ["", "c", "ckk", "CK", "c1", "é"] {
            assert_eq!(
                NmiMutationNamespace::new(invalid),
                Err(NmiNamespaceError::Invalid)
            );
        }
    }

    #[test]
    fn generated_references_remain_byte_stable_and_bounded() {
        let id =
            PaymentAttemptId::new(Uuid::parse_str("018f52c0-8a17-7b2f-9bc8-5fa621ce1173").unwrap());
        let factory = NmiMutationReferenceFactory::new(NmiMutationNamespace::new("ck").unwrap());
        for (kind, token) in [
            (PaymentAttemptKind::HostCharge, "order"),
            (PaymentAttemptKind::SubscriptionInitial, "base-sub"),
            (PaymentAttemptKind::SubscriptionRenewal, "renewal"),
            (PaymentAttemptKind::SubscriptionRecovery, "recovery"),
            (
                PaymentAttemptKind::SubscriptionPaymentMethodUpdate,
                "payment-method",
            ),
        ] {
            let reference = factory.for_attempt(kind, id);
            assert_eq!(
                reference.expose(),
                format!("ck_{token}_018f52c08a177b2f9bc85fa621ce1173")
            );
            assert!(reference.expose().len() <= 50);
        }
    }

    #[test]
    fn host_namespaces_cannot_collide_for_the_same_attempt() {
        let id = PaymentAttemptId::new(Uuid::nil());
        let ck = NmiMutationReferenceFactory::new(NmiMutationNamespace::new("ck").unwrap());
        let ip = NmiMutationReferenceFactory::new(NmiMutationNamespace::new("ip").unwrap());
        assert_ne!(
            ck.for_attempt(PaymentAttemptKind::SubscriptionRenewal, id)
                .expose(),
            ip.for_attempt(PaymentAttemptKind::SubscriptionRenewal, id)
                .expose()
        );
    }
}
