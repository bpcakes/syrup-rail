use std::{fmt, sync::Arc};

use async_trait::async_trait;
use thiserror::Error;

use crate::{
    BillingScopeId, GatewayAccountId, GatewayAccountMode, GatewayConfigurationId, GatewayError,
    GatewayLifecycleQueryPolicy, GatewayMutationError, GatewayMutationReferenceFactory,
    GatewayPaymentOutcome, GatewayProviderKey, GatewayQueryRequest, GatewaySaleRequest,
    GatewayStorePaymentMethodRequest, GatewayTransactionReport, GatewayTransactionReportRequest,
    PaymentGateway,
};

#[derive(Clone)]
pub struct ResolvedGateway {
    billing_scope_id: BillingScopeId,
    gateway_account_id: GatewayAccountId,
    gateway_configuration_id: GatewayConfigurationId,
    provider_key: GatewayProviderKey,
    lifecycle_query_policy: GatewayLifecycleQueryPolicy,
    mutation_reference_factory: Arc<dyn GatewayMutationReferenceFactory>,
    gateway: Arc<dyn PaymentGateway>,
}

impl ResolvedGateway {
    pub fn new(
        billing_scope_id: BillingScopeId,
        gateway_account_id: GatewayAccountId,
        gateway_configuration_id: GatewayConfigurationId,
        provider_key: GatewayProviderKey,
        lifecycle_query_policy: GatewayLifecycleQueryPolicy,
        mutation_reference_factory: Arc<dyn GatewayMutationReferenceFactory>,
        gateway: Arc<dyn PaymentGateway>,
    ) -> Self {
        Self {
            billing_scope_id,
            gateway_account_id,
            gateway_configuration_id,
            provider_key,
            lifecycle_query_policy,
            mutation_reference_factory,
            gateway,
        }
    }

    pub const fn billing_scope_id(&self) -> BillingScopeId {
        self.billing_scope_id
    }

    pub const fn gateway_account_id(&self) -> GatewayAccountId {
        self.gateway_account_id
    }

    pub const fn gateway_configuration_id(&self) -> GatewayConfigurationId {
        self.gateway_configuration_id
    }

    pub const fn provider_key(&self) -> &GatewayProviderKey {
        &self.provider_key
    }

    pub const fn lifecycle_query_policy(&self) -> &GatewayLifecycleQueryPolicy {
        &self.lifecycle_query_policy
    }

    pub fn mutation_reference_factory(&self) -> Arc<dyn GatewayMutationReferenceFactory> {
        Arc::clone(&self.mutation_reference_factory)
    }

    pub async fn account_mode(&self) -> Result<GatewayAccountMode, GatewayError> {
        self.gateway.account_mode().await
    }

    pub async fn sale(
        &self,
        request: GatewaySaleRequest,
    ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
        self.gateway.sale(request).await
    }

    pub async fn store_payment_method(
        &self,
        request: GatewayStorePaymentMethodRequest,
    ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
        self.gateway.store_payment_method(request).await
    }

    pub async fn query_transaction(
        &self,
        request: GatewayQueryRequest,
    ) -> Result<Option<GatewayPaymentOutcome>, GatewayError> {
        self.gateway.query_transaction(request).await
    }

    pub async fn query_transaction_reports(
        &self,
        request: GatewayTransactionReportRequest,
    ) -> Result<Vec<GatewayTransactionReport>, GatewayError> {
        self.gateway.query_transaction_reports(request).await
    }
}

impl fmt::Debug for ResolvedGateway {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedGateway")
            .field("billing_scope_id", &self.billing_scope_id)
            .field("gateway_account_id", &self.gateway_account_id)
            .field("gateway_configuration_id", &self.gateway_configuration_id)
            .field("provider_key", &self.provider_key)
            .field("lifecycle_query_policy", &self.lifecycle_query_policy)
            .field("has_mutation_reference_factory", &true)
            .field("has_gateway", &true)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum GatewayResolutionError {
    #[error("gateway configuration was not found")]
    NotFound,
    #[error("gateway configuration changed")]
    ConfigurationChanged,
    #[error("gateway configuration is invalid")]
    InvalidConfiguration,
    #[error("gateway resolution is temporarily unavailable")]
    Unavailable,
}

#[async_trait]
pub trait GatewayResolver: Send + Sync {
    async fn resolve(
        &self,
        billing_scope_id: BillingScopeId,
        gateway_account_id: GatewayAccountId,
        gateway_configuration_id: GatewayConfigurationId,
        provider_key: GatewayProviderKey,
    ) -> Result<ResolvedGateway, GatewayResolutionError>;
}

#[cfg(test)]
mod tests {
    use chrono::Duration;
    use uuid::Uuid;

    use super::*;
    use crate::{
        BillingContact, BillingPeriod, ChargeAmount, CurrencyCode, GatewayLifecycleCursorKey,
        GatewayOrderId, GatewayTransactionId, IdempotencyKey, PaymentAttemptId, PaymentAttemptKind,
        PaymentMethodId, PaymentMethodUpdateSnapshot, PaymentToken, PlanKey, SubscriberId,
        SubscriptionId, SubscriptionPaymentStateSnapshot, SubscriptionStatus,
    };

    struct NeverCalledGateway;

    #[async_trait]
    impl PaymentGateway for NeverCalledGateway {
        async fn account_mode(&self) -> Result<GatewayAccountMode, GatewayError> {
            panic!("resolved gateway construction must not perform provider I/O")
        }

        async fn sale(
            &self,
            _request: GatewaySaleRequest,
        ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
            panic!("resolved gateway construction must not perform provider I/O")
        }

        async fn store_payment_method(
            &self,
            _request: GatewayStorePaymentMethodRequest,
        ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
            panic!("resolved gateway construction must not perform provider I/O")
        }

        async fn query_transaction(
            &self,
            _request: GatewayQueryRequest,
        ) -> Result<Option<GatewayPaymentOutcome>, GatewayError> {
            panic!("resolved gateway construction must not perform provider I/O")
        }

        async fn query_transaction_reports(
            &self,
            _request: GatewayTransactionReportRequest,
        ) -> Result<Vec<GatewayTransactionReport>, GatewayError> {
            panic!("resolved gateway construction must not perform provider I/O")
        }
    }

    struct NeverCalledReferenceFactory;

    impl GatewayMutationReferenceFactory for NeverCalledReferenceFactory {
        fn for_attempt(
            &self,
            _kind: PaymentAttemptKind,
            _attempt_id: PaymentAttemptId,
        ) -> GatewayOrderId {
            panic!("resolved gateway construction must not format a mutation reference")
        }
    }

    struct TestReferenceFactory;

    impl GatewayMutationReferenceFactory for TestReferenceFactory {
        fn for_attempt(
            &self,
            kind: PaymentAttemptKind,
            attempt_id: PaymentAttemptId,
        ) -> GatewayOrderId {
            GatewayOrderId::from_generated_attempt(
                format!("test_{}_{}", kind.as_str(), attempt_id.as_uuid().simple()),
                attempt_id,
            )
            .expect("test order ID should be valid")
        }
    }

    fn test_policy() -> GatewayLifecycleQueryPolicy {
        GatewayLifecycleQueryPolicy::new(
            GatewayLifecycleCursorKey::new("test_cursor").expect("valid cursor key"),
            Duration::minutes(1),
            10,
            2,
            2,
            20,
        )
        .expect("valid lifecycle policy")
    }

    fn test_gateway(
        mutation_reference_factory: Arc<dyn GatewayMutationReferenceFactory>,
    ) -> ResolvedGateway {
        ResolvedGateway::new(
            BillingScopeId::new(Uuid::from_u128(1)),
            GatewayAccountId::new(Uuid::from_u128(2)),
            GatewayConfigurationId::new(Uuid::from_u128(3)),
            GatewayProviderKey::new("test_gateway").expect("valid provider key"),
            test_policy(),
            mutation_reference_factory,
            Arc::new(NeverCalledGateway),
        )
    }

    #[test]
    fn resolved_gateway_preserves_exact_identity_without_provider_io() {
        let scope = BillingScopeId::new(Uuid::from_u128(1));
        let account = GatewayAccountId::new(Uuid::from_u128(2));
        let configuration = GatewayConfigurationId::new(Uuid::from_u128(3));
        let provider = GatewayProviderKey::new("test_gateway").expect("valid provider key");
        let policy = test_policy();
        let resolved = test_gateway(Arc::new(NeverCalledReferenceFactory));

        assert_eq!(resolved.billing_scope_id(), scope);
        assert_eq!(resolved.gateway_account_id(), account);
        assert_eq!(resolved.gateway_configuration_id(), configuration);
        assert_eq!(resolved.provider_key(), &provider);
        assert_eq!(resolved.lifecycle_query_policy(), &policy);
        let debug = format!("{resolved:?}");
        assert!(debug.contains("has_mutation_reference_factory: true"));
        assert!(debug.contains("has_gateway: true"));
    }

    #[test]
    fn locked_terms_build_the_same_reservations_as_legacy_arguments() {
        let gateway = test_gateway(Arc::new(TestReferenceFactory));
        let subscription_id = SubscriptionId::new(Uuid::from_u128(4));
        let payment_method_id = PaymentMethodId::new(Uuid::from_u128(5));
        let initial_transaction_id =
            GatewayTransactionId::new("initial-transaction").expect("valid transaction ID");
        let status = SubscriptionStatus::PastDue;
        let start_at = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let period = BillingPeriod::new(start_at, start_at + Duration::days(30)).unwrap();
        let charge = ChargeAmount::new(1_000, CurrencyCode::new("USD").unwrap()).unwrap();
        let expected_state = SubscriptionPaymentStateSnapshot::new(
            subscription_id,
            payment_method_id,
            initial_transaction_id.clone(),
            status,
        )
        .unwrap();
        let subscriber_id = SubscriberId::new(Uuid::from_u128(7));
        let plan_key = PlanKey::new("premium").unwrap();
        let required_mode = GatewayAccountMode::Test;

        let renewal =
            crate::ChargeRenewal::new(gateway.billing_scope_id(), subscription_id, start_at);
        let renewal_attempt_id = PaymentAttemptId::new(Uuid::from_u128(8));
        assert_eq!(
            crate::SubscriptionRenewalReservation::from_locked_subscription(
                renewal,
                &gateway,
                renewal_attempt_id,
                subscriber_id,
                plan_key.clone(),
                payment_method_id,
                initial_transaction_id.clone(),
                status,
                period.clone(),
                charge,
                3,
                required_mode,
            )
            .unwrap(),
            crate::SubscriptionRenewalReservation::from_locked_subscription_terms(
                renewal,
                &gateway,
                renewal_attempt_id,
                subscriber_id,
                plan_key.clone(),
                crate::SubscriptionRenewalLockedTerms::new(
                    gateway.gateway_account_id(),
                    expected_state.clone(),
                    period.clone(),
                    charge,
                    3,
                ),
                required_mode,
            )
            .unwrap(),
        );

        let recovery = crate::RecoverSubscriptionPayment::new(
            crate::SubscriptionPaymentContext::new(
                PaymentAttemptId::new(Uuid::from_u128(9)),
                gateway.billing_scope_id(),
                subscriber_id,
                gateway.gateway_configuration_id(),
                IdempotencyKey::new("recovery-key").unwrap(),
                PaymentToken::new("recovery-token").unwrap(),
                BillingContact::new(None, Some("Test".to_owned()), None).unwrap(),
            ),
            plan_key.clone(),
        );
        assert_eq!(
            crate::SubscriptionRecoveryReservation::from_locked_subscription(
                &recovery,
                &gateway,
                recovery.attempt_id(),
                subscription_id,
                payment_method_id,
                initial_transaction_id.clone(),
                status,
                period.clone(),
                charge,
                required_mode,
            )
            .unwrap(),
            crate::SubscriptionRecoveryReservation::from_locked_subscription_terms(
                &recovery,
                &gateway,
                recovery.attempt_id(),
                crate::SubscriptionRecoveryLockedTerms::new(
                    gateway.gateway_account_id(),
                    expected_state.clone(),
                    period.clone(),
                    charge,
                ),
                required_mode,
            )
            .unwrap(),
        );

        let replacement = crate::ReplaceSubscriptionPaymentMethod::new(
            crate::SubscriptionPaymentContext::new(
                PaymentAttemptId::new(Uuid::from_u128(10)),
                gateway.billing_scope_id(),
                subscriber_id,
                gateway.gateway_configuration_id(),
                IdempotencyKey::new("replacement-key").unwrap(),
                PaymentToken::new("replacement-token").unwrap(),
                BillingContact::new(None, Some("Test".to_owned()), None).unwrap(),
            ),
            plan_key,
        );
        let currency = CurrencyCode::new("USD").unwrap();
        assert_eq!(
            crate::SubscriptionPaymentMethodReplacement::from_locked_subscription(
                &replacement,
                &gateway,
                subscription_id,
                payment_method_id,
                initial_transaction_id.clone(),
                currency,
                required_mode,
            )
            .unwrap(),
            crate::SubscriptionPaymentMethodReplacement::from_locked_subscription_terms(
                &replacement,
                &gateway,
                crate::SubscriptionPaymentMethodReplacementLockedTerms::new(
                    gateway.gateway_account_id(),
                    PaymentMethodUpdateSnapshot::new(
                        subscription_id,
                        payment_method_id,
                        initial_transaction_id,
                    ),
                    currency,
                ),
                required_mode,
            )
            .unwrap(),
        );
    }

    #[test]
    fn retry_submission_matching_binds_durable_fields_but_not_one_shot_inputs() {
        let gateway = test_gateway(Arc::new(TestReferenceFactory));
        let subscriber_id = SubscriberId::new(Uuid::from_u128(20));
        let subscription_id = SubscriptionId::new(Uuid::from_u128(21));
        let payment_method_id = PaymentMethodId::new(Uuid::from_u128(22));
        let initial_transaction_id =
            GatewayTransactionId::new("submission-initial").expect("valid transaction ID");
        let plan_key = PlanKey::new("submission-plan").expect("valid plan key");
        let canonical_contact = BillingContact::new(
            Some("Ada".to_owned()),
            Some("Lovelace".to_owned()),
            Some("ada@example.test".to_owned()),
        )
        .expect("valid billing contact");
        let changed_contact = BillingContact::new(
            Some("Grace".to_owned()),
            Some("Hopper".to_owned()),
            Some("grace@example.test".to_owned()),
        )
        .expect("valid billing contact");
        let context = |attempt_id, key: &str, token: &str, contact| {
            crate::SubscriptionPaymentContext::new(
                PaymentAttemptId::new(Uuid::from_u128(attempt_id)),
                gateway.billing_scope_id(),
                subscriber_id,
                gateway.gateway_configuration_id(),
                IdempotencyKey::new(key).expect("valid idempotency key"),
                PaymentToken::new(token).expect("valid payment token"),
                contact,
            )
        };

        let recovery_command = crate::RecoverSubscriptionPayment::new(
            context(
                23,
                "recovery-submission-key",
                "recovery-token",
                canonical_contact.clone(),
            ),
            plan_key.clone(),
        );
        let start_at = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let recovery_reservation =
            crate::SubscriptionRecoveryReservation::from_locked_subscription(
                &recovery_command,
                &gateway,
                recovery_command.attempt_id(),
                subscription_id,
                payment_method_id,
                initial_transaction_id.clone(),
                SubscriptionStatus::PastDue,
                BillingPeriod::new(start_at, start_at + Duration::days(30)).unwrap(),
                ChargeAmount::new(1_000, CurrencyCode::new("USD").unwrap()).unwrap(),
                GatewayAccountMode::Live,
            )
            .unwrap();
        let recovery_retry = crate::RecoverSubscriptionPayment::new(
            context(
                24,
                "recovery-submission-key",
                "refreshed-recovery-token",
                canonical_contact.clone(),
            ),
            plan_key.clone(),
        );
        assert!(recovery_reservation.matches_submission(&recovery_retry, &gateway));
        assert!(!recovery_reservation.matches_submission(
            &crate::RecoverSubscriptionPayment::new(
                context(
                    25,
                    "changed-recovery-submission-key",
                    "refreshed-recovery-token",
                    canonical_contact.clone(),
                ),
                plan_key.clone(),
            ),
            &gateway,
        ));
        assert!(!recovery_reservation.matches_submission(
            &crate::RecoverSubscriptionPayment::new(
                context(
                    26,
                    "recovery-submission-key",
                    "refreshed-recovery-token",
                    changed_contact.clone(),
                ),
                plan_key.clone(),
            ),
            &gateway,
        ));

        let replacement_command = crate::ReplaceSubscriptionPaymentMethod::new(
            context(
                27,
                "replacement-submission-key",
                "replacement-token",
                canonical_contact.clone(),
            ),
            plan_key.clone(),
        );
        let replacement_reservation =
            crate::SubscriptionPaymentMethodReplacement::from_locked_subscription(
                &replacement_command,
                &gateway,
                subscription_id,
                payment_method_id,
                initial_transaction_id,
                CurrencyCode::new("USD").unwrap(),
                GatewayAccountMode::Live,
            )
            .unwrap();
        let replacement_retry = crate::ReplaceSubscriptionPaymentMethod::new(
            context(
                28,
                "replacement-submission-key",
                "refreshed-replacement-token",
                canonical_contact.clone(),
            ),
            plan_key.clone(),
        );
        assert!(replacement_reservation.matches_submission(&replacement_retry, &gateway));
        assert!(!replacement_reservation.matches_submission(
            &crate::ReplaceSubscriptionPaymentMethod::new(
                context(
                    29,
                    "changed-replacement-submission-key",
                    "refreshed-replacement-token",
                    canonical_contact,
                ),
                plan_key.clone(),
            ),
            &gateway,
        ));
        assert!(!replacement_reservation.matches_submission(
            &crate::ReplaceSubscriptionPaymentMethod::new(
                context(
                    30,
                    "replacement-submission-key",
                    "refreshed-replacement-token",
                    changed_contact,
                ),
                plan_key,
            ),
            &gateway,
        ));
    }
}
