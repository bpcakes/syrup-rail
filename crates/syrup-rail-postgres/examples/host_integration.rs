//! Compile-tested host wiring for subscriber-owned subscription billing.
//!
//! The host authenticates and authorizes the subscriber before constructing a
//! command. Its implementations of the four ports below retain ownership of
//! plan pricing, gateway credentials, abuse controls, subject authorization,
//! and transactional outbox encoding. `main` performs no database or provider
//! I/O; this example is intended to be copied into a host application.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use sqlx::{PgConnection, PgPool, Postgres, Transaction};
use syrup_rail::{
    BillingEvent, BillingEventSubject, CancelSubscription, CancelSubscriptionOutcome,
    ClearSubscriptionDiscount, EndUserMutationAdmission, EnrollSubscription, GatewayResolver,
    PaymentAttemptId, PaymentAttemptStatus, SubscriptionDiscountClaim,
    SubscriptionDiscountClaimOutcome, SubscriptionDiscountClearOutcome,
    SubscriptionEnrollmentExpectedTerms, SubscriptionId, SubscriptionPaymentContext,
};
use syrup_rail_postgres::{
    BillingEventWriteError, BillingTransaction, BillingTransactionCoordinator,
    BillingTransactionError, BillingTransactionSubjectState, SubscriptionBillingService,
    SubscriptionBillingServiceError, SubscriptionOfferStore,
};

/// Host-owned implementations required by [`SubscriptionBillingService`].
///
/// `offers` must lock the exact host plan row on the supplied connection. A
/// subscriber-aware implementation should override
/// `SubscriptionOfferStore::lock_enrollment_offer`, lock eligibility state on
/// that same connection, and exclude the supplied in-flight attempt ID from
/// attempt-history eligibility checks.
///
/// `gateways` resolves the exact canonical configuration to a provider adapter
/// without placing credentials in Syrup Rail's durable model. `admission`
/// applies host abuse controls after host authentication and authorization.
/// `billing_boundary` locks the host billing subject before shared billing rows
/// and writes every [`BillingEvent`] to the host outbox before commit.
pub struct HostBillingPorts {
    offers: Arc<dyn SubscriptionOfferStore>,
    gateways: Arc<dyn GatewayResolver>,
    admission: Arc<dyn EndUserMutationAdmission>,
    billing_boundary: Arc<dyn HostBillingBoundary>,
}

impl HostBillingPorts {
    pub fn new(
        offers: Arc<dyn SubscriptionOfferStore>,
        gateways: Arc<dyn GatewayResolver>,
        admission: Arc<dyn EndUserMutationAdmission>,
        billing_boundary: Arc<dyn HostBillingBoundary>,
    ) -> Self {
        Self {
            offers,
            gateways,
            admission,
            billing_boundary,
        }
    }
}

/// Constructs the service without installing or running a database migrator.
///
/// Materialize Syrup Rail's versioned schema through the host's normal
/// migration deployment before serving traffic.
pub fn build_subscription_billing_service(
    pool: PgPool,
    ports: HostBillingPorts,
) -> SubscriptionBillingService {
    let transactions = Arc::new(HostTransactionCoordinator::new(
        pool.clone(),
        ports.billing_boundary,
    ));
    SubscriptionBillingService::new(
        pool,
        ports.offers,
        ports.gateways,
        ports.admission,
        transactions,
    )
}

/// The host-specific half of Syrup Rail's transaction boundary.
///
/// `lock_billing_subject` must find the durable event recipient, acquire its
/// host row lock, and classify it as live or retained before it returns.
/// `append_outbox_event` maps the typed event to the host's durable outbox.
/// Both methods receive the same transaction connection; implementations must
/// not acquire another one.
#[async_trait]
pub trait HostBillingBoundary: Send + Sync {
    async fn lock_billing_subject(
        &self,
        connection: &mut PgConnection,
        subject: BillingEventSubject,
    ) -> Result<BillingTransactionSubjectState, BillingTransactionError>;

    async fn append_outbox_event(
        &self,
        connection: &mut PgConnection,
        event: &BillingEvent,
    ) -> Result<(), BillingEventWriteError>;
}

/// Adapter from a host subject/outbox boundary to Syrup Rail's coordinator.
#[derive(Clone)]
pub struct HostTransactionCoordinator {
    pool: PgPool,
    boundary: Arc<dyn HostBillingBoundary>,
}

impl HostTransactionCoordinator {
    pub fn new(pool: PgPool, boundary: Arc<dyn HostBillingBoundary>) -> Self {
        Self { pool, boundary }
    }
}

#[async_trait]
impl BillingTransactionCoordinator for HostTransactionCoordinator {
    async fn begin(
        &self,
        subject: BillingEventSubject,
        lock_timeout: Duration,
    ) -> Result<Box<dyn BillingTransaction>, BillingTransactionError> {
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(BillingTransactionError::new)?;

        // This session-local setting does not acquire a row lock. The host
        // subject lock below remains the first row lock in the transaction.
        sqlx::query("SELECT set_config('lock_timeout', $1, true)")
            .bind(format!("{}ms", lock_timeout.as_millis().max(1)))
            .execute(&mut *transaction)
            .await
            .map_err(BillingTransactionError::new)?;

        let subject_state = self
            .boundary
            .lock_billing_subject(&mut transaction, subject)
            .await?;

        Ok(Box::new(HostTransaction {
            transaction,
            subject_state,
            boundary: Arc::clone(&self.boundary),
        }))
    }
}

struct HostTransaction {
    transaction: Transaction<'static, Postgres>,
    subject_state: BillingTransactionSubjectState,
    boundary: Arc<dyn HostBillingBoundary>,
}

#[async_trait]
impl BillingTransaction for HostTransaction {
    fn connection(&mut self) -> &mut PgConnection {
        &mut self.transaction
    }

    fn subject_state(&self) -> BillingTransactionSubjectState {
        self.subject_state
    }

    async fn append_event(&mut self, event: &BillingEvent) -> Result<(), BillingEventWriteError> {
        self.boundary
            .append_outbox_event(&mut self.transaction, event)
            .await
    }

    async fn commit(self: Box<Self>) -> Result<(), BillingTransactionError> {
        let Self { transaction, .. } = *self;
        transaction
            .commit()
            .await
            .map_err(BillingTransactionError::new)
    }

    async fn rollback(self: Box<Self>) -> Result<(), BillingTransactionError> {
        let Self { transaction, .. } = *self;
        transaction
            .rollback()
            .await
            .map_err(BillingTransactionError::new)
    }
}

/// Product-facing interpretation of a durable enrollment result.
///
/// Only `Activated` grants subscription access. Unknown or review-required
/// evidence stays confirmation-pending until reconciliation applies it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnrollmentDecision {
    Activated {
        attempt_id: PaymentAttemptId,
        subscription_id: SubscriptionId,
    },
    ConfirmationPending {
        attempt_id: PaymentAttemptId,
    },
    NotActivated {
        attempt_id: PaymentAttemptId,
        status: PaymentAttemptStatus,
    },
}

/// Enrolls a subject that the host has already authenticated and authorized.
///
/// Construct `payment` directly from that authorized request. Its payment
/// token remains request-scoped; the service persists only the permitted
/// billing-contact snapshot and canonical attempt/economic facts.
pub async fn enroll_authorized_subscriber(
    service: &SubscriptionBillingService,
    payment: SubscriptionPaymentContext,
    expected_terms: SubscriptionEnrollmentExpectedTerms,
) -> Result<EnrollmentDecision, SubscriptionBillingServiceError> {
    let result = service
        .enroll(EnrollSubscription::new(payment, expected_terms))
        .await?;
    let attempt_id = result.attempt().identity().attempt_id();

    if let Some(subscription) = result.subscription() {
        return Ok(EnrollmentDecision::Activated {
            attempt_id,
            subscription_id: subscription.id(),
        });
    }
    if result.is_confirmation_pending() {
        return Ok(EnrollmentDecision::ConfirmationPending { attempt_id });
    }
    Ok(EnrollmentDecision::NotActivated {
        attempt_id,
        status: result.status(),
    })
}

/// Cancels an exact subscription that the host has already authorized for its
/// scope, subscriber, and plan.
///
/// A changed cancellation writes its typed event through `HostBillingBoundary`
/// in the same host-prepared transaction as the canonical mutation. The
/// service never resolves a gateway for this operation.
pub async fn cancel_authorized_subscription(
    service: &SubscriptionBillingService,
    command: CancelSubscription,
) -> Result<CancelSubscriptionOutcome, SubscriptionBillingServiceError> {
    service.cancel(command).await
}

/// Claims an authorized subscriber's exact-plan discount code.
///
/// This uses the host offer lock supplied through `SubscriptionOfferStore` but
/// does not resolve a gateway or emit a billing event.
pub async fn claim_authorized_subscription_discount(
    service: &SubscriptionBillingService,
    command: SubscriptionDiscountClaim,
) -> Result<SubscriptionDiscountClaimOutcome, SubscriptionBillingServiceError> {
    service.claim_discount(command).await
}

/// Clears the authorized subscriber's saved exact-plan discount claim.
///
/// This performs no gateway resolution or provider I/O.
pub async fn clear_authorized_subscription_discount(
    service: &SubscriptionBillingService,
    command: ClearSubscriptionDiscount,
) -> Result<SubscriptionDiscountClearOutcome, SubscriptionBillingServiceError> {
    service.clear_discount(command).await
}

fn main() {}
