//! Compile-tested host wiring for subscriber-owned subscription billing.
//!
//! The host authenticates and authorizes the subscriber before constructing a
//! command. Its implementations of the five ports below retain ownership of
//! plan pricing, gateway credentials, abuse controls, subject authorization,
//! host-charge target state, and transactional outbox encoding. `main` performs
//! no database or provider I/O; this example is intended to be copied into a
//! host application.

#[path = "host_integration/outbox.rs"]
mod outbox;

pub use outbox::{
    HostBillingEventAppendOutcomeV1, HostBillingEventEnvelopeV1,
    HostBillingEventReplayDecodeErrorV1, HostBillingEventReplayV1, append_host_billing_event_v1,
};

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use sqlx::{PgConnection, PgPool, Postgres, Transaction};
use syrup_rail::{
    BillingEvent, BillingEventSubject, CancelSubscription, CancelSubscriptionOutcome,
    ClearSubscriptionDiscount, EndUserMutationAdmission, EnrollSubscription, EntitlementGuard,
    GatewayAccountMode, GatewayResolver, PaymentAttemptId, PaymentAttemptStatus,
    RenewalDispatchPage, RenewalDispatchPageCursor, SubscriptionBillingPortalQuery,
    SubscriptionBillingPortalSnapshot, SubscriptionDiscountClaim, SubscriptionDiscountClaimOutcome,
    SubscriptionDiscountClearOutcome, SubscriptionEnrollmentExpectedTerms, SubscriptionId,
    SubscriptionPaymentContext, SubscriptionPaymentHistoryCursor, SubscriptionPaymentHistoryPage,
    SubscriptionPaymentHistoryPageLimit,
};
use syrup_rail_postgres::{
    AdmittedEntitlementWriteTransaction, BillingEventWriteError, BillingTransaction,
    BillingTransactionCoordinator, BillingTransactionError, BillingTransactionSubjectState,
    EntitlementGuardError, EntitlementWriteTransaction, HostChargeTargetStore, RenewalStoreError,
    SchemaConformanceError, SubscriptionBillingPortalQueryError, SubscriptionBillingService,
    SubscriptionBillingServiceError, SubscriptionBillingServiceErrorDisposition,
    SubscriptionOfferStore, assert_runtime_schema_v5_compatible, due_renewals_page_for_mode,
    require_entitlement_for_update, subscription_billing_portal, subscription_payment_history_page,
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
/// `host_charge_targets` owns the optional host-charge target state machine;
/// its callbacks use only Syrup Rail's supplied connection, preserve the
/// target-before-attempt lock order, make submission admission repeat-safe,
/// and persist every successful transition so exact replay is distinguishable
/// from a stale or inapplicable target.
/// `billing_boundary` locks the host billing subject before shared billing rows
/// and writes every [`BillingEvent`] to the host outbox before commit.
pub struct HostBillingPorts {
    offers: Arc<dyn SubscriptionOfferStore>,
    gateways: Arc<dyn GatewayResolver>,
    admission: Arc<dyn EndUserMutationAdmission>,
    host_charge_targets: Arc<dyn HostChargeTargetStore>,
    billing_boundary: Arc<dyn HostBillingBoundary>,
}

impl HostBillingPorts {
    pub fn new(
        offers: Arc<dyn SubscriptionOfferStore>,
        gateways: Arc<dyn GatewayResolver>,
        admission: Arc<dyn EndUserMutationAdmission>,
        host_charge_targets: Arc<dyn HostChargeTargetStore>,
        billing_boundary: Arc<dyn HostBillingBoundary>,
    ) -> Self {
        Self {
            offers,
            gateways,
            admission,
            host_charge_targets,
            billing_boundary,
        }
    }
}

/// Constructs the service without installing or running a database migrator.
///
/// Materialize Syrup Rail's versioned schema through the host's normal
/// migration deployment before serving traffic. Pass the deployment's trusted
/// mode and route only matching renewal dispatch pages to the returned service.
pub fn build_subscription_billing_service(
    pool: PgPool,
    ports: HostBillingPorts,
    required_gateway_account_mode: GatewayAccountMode,
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
    .with_required_gateway_account_mode(required_gateway_account_mode)
    .with_host_charge_targets(ports.host_charge_targets)
}

/// Verifies the host-applied database migration before this process serves
/// billing traffic.
///
/// Run the host's immutable Syrup Rail install or forward-only upgrade
/// migration through its normal deployment workflow first. This assertion uses
/// one repeatable-read, read-only runtime-contract snapshot; it never installs,
/// upgrades, audits, or otherwise changes the database. It requires PostgreSQL
/// 18 and rejects host-specific columns on canonical relations. The schema-v5
/// migration has already validated external-reversal resolution tuples.
pub async fn assert_host_runtime_schema_compatibility(
    pool: &PgPool,
) -> Result<(), SchemaConformanceError> {
    assert_runtime_schema_v5_compatible(pool).await
}

/// Admits a host-authorized protected write and returns its only valid transaction.
///
/// The host starts [`EntitlementWriteTransaction::begin`] and may make
/// preparatory writes through its connection before this call. The pending
/// value cannot commit. It must perform and commit the protected mutation only
/// with the admitted transaction returned here. Completed denial and database
/// failure await rollback; cancellation queues rollback of the owned
/// transaction, so no unguarded continuation is possible. Finish any nested
/// savepoint opened through its connection before consuming the admitted value
/// with `commit` or `rollback`.
pub async fn admit_authorized_protected_write(
    transaction: EntitlementWriteTransaction,
    guard: &EntitlementGuard,
) -> Result<AdmittedEntitlementWriteTransaction, EntitlementGuardError> {
    require_entitlement_for_update(transaction, guard).await
}

/// The host-specific half of Syrup Rail's transaction boundary.
///
/// `lock_billing_subject` must find the durable event recipient, acquire its
/// host row lock, and classify it as live or retained before it returns.
/// `append_outbox_event` maps the typed event to the host's durable outbox;
/// [`HostBillingEventEnvelopeV1`] demonstrates an exhaustive, versioned,
/// redacted mapping, while [`append_host_billing_event_v1`] demonstrates the
/// atomic insert/read/compare replay algorithm for the example table. The
/// coordinator retains and supplies the exact subject admitted at transaction
/// start so the writer never has to rediscover it. Both methods receive the
/// same transaction connection; implementations must not acquire another one.
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
        subject: BillingEventSubject,
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
            subject,
            subject_state,
            boundary: Arc::clone(&self.boundary),
        }))
    }
}

struct HostTransaction {
    transaction: Transaction<'static, Postgres>,
    subject: BillingEventSubject,
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
            .append_outbox_event(&mut self.transaction, self.subject, event)
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

/// Conservative automatic-retry action for a failed authorized billing
/// command.
///
/// [`ResubmitSameIdempotentCommand`](Self::ResubmitSameIdempotentCommand)
/// means the host must retain the exact command and idempotency key. It does
/// not promise that a later attempt will succeed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthorizedBillingCommandRetry {
    DoNotRetry,
    ResubmitSameIdempotentCommand { retry_after: Option<Duration> },
}

/// Chooses a conservative retry action without inspecting service-error
/// variants or provider diagnostics.
///
/// A conflict is not retried as-is: the host normally reloads/rebuilds current
/// authority or reconciles the existing idempotency key. A temporary error can
/// be resubmitted only as the same idempotent command. Gateway/account
/// cooldowns have no fabricated delay, so their retry action can carry `None`.
pub fn retry_action_for_authorized_billing_command(
    error: &SubscriptionBillingServiceError,
) -> AuthorizedBillingCommandRetry {
    match error.disposition() {
        SubscriptionBillingServiceErrorDisposition::TemporarilyUnavailable => {
            AuthorizedBillingCommandRetry::ResubmitSameIdempotentCommand {
                retry_after: error.retry_after(),
            }
        }
        SubscriptionBillingServiceErrorDisposition::Conflict
        | SubscriptionBillingServiceErrorDisposition::Rejected
        | SubscriptionBillingServiceErrorDisposition::Misconfigured
        | SubscriptionBillingServiceErrorDisposition::Internal => {
            AuthorizedBillingCommandRetry::DoNotRetry
        }
        // Future service dispositions are deliberately non-retryable until
        // this host policy has an explicit decision for them.
        _ => AuthorizedBillingCommandRetry::DoNotRetry,
    }
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

/// Reads the current billing portal for a subject the host has already
/// authenticated and authorized for this exact query identity.
///
/// The result is intentionally provider-neutral: it includes canonical
/// entitlement state and an optional masked-card display, but no provider
/// payment-method reference, transaction identifier, billing contact, or raw
/// provider diagnostic.
pub async fn read_authorized_subscription_billing_portal(
    pool: &PgPool,
    query: &SubscriptionBillingPortalQuery,
) -> Result<SubscriptionBillingPortalSnapshot, SubscriptionBillingPortalQueryError> {
    subscription_billing_portal(pool, query).await
}

/// Reads one bounded, exact-plan subscription payment-history page for an
/// already authorized portal identity.
pub async fn read_authorized_subscription_payment_history(
    pool: &PgPool,
    query: &SubscriptionBillingPortalQuery,
    cursor: Option<&SubscriptionPaymentHistoryCursor>,
    limit: SubscriptionPaymentHistoryPageLimit,
) -> Result<SubscriptionPaymentHistoryPage, SubscriptionBillingPortalQueryError> {
    subscription_payment_history_page(pool, query, cursor, limit).await
}

/// Reads one stable page of automatic renewal dispatch candidates.
///
/// The host uses the same trusted deployment mode for the entire cursor chain,
/// then writes each selected dispatch to its own queue/outbox for a service
/// configured with that mode. The cursor records its mode and rejects accidental
/// cross-mode reuse. This page is deliberately not a
/// lease, claim, or cross-page snapshot: concurrent candidates behind the key
/// can wait for a fresh scan, and eventual renewal submission still revalidates
/// current canonical state. Persist/reconstruct a cursor only from trusted
/// host state returned by a prior page, never from end-user input.
pub async fn read_renewal_dispatch_page(
    pool: &PgPool,
    required_gateway_account_mode: GatewayAccountMode,
    cursor: Option<&RenewalDispatchPageCursor>,
) -> Result<RenewalDispatchPage, RenewalStoreError> {
    due_renewals_page_for_mode(pool, required_gateway_account_mode, cursor).await
}

fn main() {}
