use std::time::Duration;

use async_trait::async_trait;
use thiserror::Error;

use crate::{BillingScopeId, SubscriberId};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EndUserMutationOperation {
    HostCharge,
    SubscriptionInitial,
    SubscriptionRecovery,
    SubscriptionPaymentMethodUpdate,
    SubscriptionCancel,
    SubscriptionDiscountClear,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EndUserMutationCommand {
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    operation: EndUserMutationOperation,
}

impl EndUserMutationCommand {
    pub const fn new(
        billing_scope_id: BillingScopeId,
        subscriber_id: SubscriberId,
        operation: EndUserMutationOperation,
    ) -> Self {
        Self {
            billing_scope_id,
            subscriber_id,
            operation,
        }
    }

    pub const fn billing_scope_id(&self) -> BillingScopeId {
        self.billing_scope_id
    }

    pub const fn subscriber_id(&self) -> SubscriberId {
        self.subscriber_id
    }

    pub const fn operation(&self) -> EndUserMutationOperation {
        self.operation
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("end-user mutation retry-after duration must be positive")]
pub struct EndUserMutationRetryAfterError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EndUserMutationRetryAfter(Duration);

impl EndUserMutationRetryAfter {
    pub fn new(value: Duration) -> Result<Self, EndUserMutationRetryAfterError> {
        if value.is_zero() {
            return Err(EndUserMutationRetryAfterError);
        }
        Ok(Self(value))
    }

    pub const fn get(self) -> Duration {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EndUserMutationAdmissionResult {
    Allowed,
    Denied {
        retry_after: EndUserMutationRetryAfter,
    },
    Timeout,
    Unavailable,
}

#[async_trait]
pub trait EndUserMutationAdmission: Send + Sync {
    async fn admit(&self, command: EndUserMutationCommand) -> EndUserMutationAdmissionResult;
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn command_preserves_exact_scope_subscriber_and_operation() {
        let scope = BillingScopeId::new(Uuid::from_u128(1));
        let subscriber = SubscriberId::new(Uuid::from_u128(2));
        let command = EndUserMutationCommand::new(
            scope,
            subscriber,
            EndUserMutationOperation::SubscriptionRecovery,
        );

        assert_eq!(command.billing_scope_id(), scope);
        assert_eq!(command.subscriber_id(), subscriber);
        assert_eq!(
            command.operation(),
            EndUserMutationOperation::SubscriptionRecovery
        );
    }

    #[test]
    fn denied_retry_after_is_positive_by_construction() {
        assert!(EndUserMutationRetryAfter::new(Duration::ZERO).is_err());

        let duration = Duration::from_secs(60);
        let retry_after = EndUserMutationRetryAfter::new(duration).expect("positive duration");
        assert_eq!(retry_after.get(), duration);
    }
}
