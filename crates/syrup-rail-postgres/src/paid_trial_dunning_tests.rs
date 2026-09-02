use std::{
    error::Error,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use sqlx::{PgConnection, PgPool, Postgres, Transaction};
use syrup_rail::{
    BillingContact, BillingEvent, BillingEventSubject, BillingPeriod, BillingScopeId,
    CancelSubscription, CancelSubscriptionOutcome, ChargeAmount, ChargeRenewal, CurrencyCode,
    DunningExhaustion, DunningSchedule, EndUserMutationAdmission, EndUserMutationAdmissionResult,
    EndUserMutationCommand, Entitlement, EntitlementGuard, EntitlementQuery, GatewayAccountId,
    GatewayAccountMode, GatewayConfigurationId, GatewayDiagnostic, GatewayError,
    GatewayLifecycleCursorKey, GatewayLifecycleQueryPolicy, GatewayMutationError,
    GatewayMutationReferenceFactory, GatewayOrderId, GatewayPaymentDescriptor,
    GatewayPaymentDiagnostic, GatewayPaymentMethodReference, GatewayPaymentOutcome,
    GatewayPaymentStatus, GatewayProviderKey, GatewayQueryRequest, GatewayResolutionError,
    GatewayResolver, GatewaySaleRequest, GatewayStorePaymentMethodRequest, GatewayTransactionId,
    GatewayTransactionReport, GatewayTransactionReportRequest, IdempotencyKey,
    LimitedDiscountMonths, PaidTrialTerms, PastDueAccessPolicy, PaymentAttemptId,
    PaymentAttemptKind, PaymentGateway, PaymentToken, PercentOffBasisPoints, PlanKey,
    ProcessorEvidence, RecoverSubscriptionPayment, RecurringSubscriptionTerms,
    RenewalFailurePolicy, ResolvedGateway, SubscriberId, SubscriptionDiscountCode,
    SubscriptionDiscountDuration, SubscriptionDiscountKind, SubscriptionDiscountSnapshot,
    SubscriptionEndReason, SubscriptionEnrollmentExpectedTerms,
    SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentReservation,
    SubscriptionEnrollmentReservationOutcome, SubscriptionPaymentFailureAccess,
    SubscriptionPaymentFailureDisposition, SubscriptionPeriodRule, SubscriptionPhase,
    SubscriptionRenewalOutcome, SubscriptionStatus,
};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::{
    BillingEventWriteError, BillingTransaction, BillingTransactionCoordinator,
    BillingTransactionError, BillingTransactionSubjectState, EntitlementWriteTransaction,
    SubscriptionBillingService, SubscriptionEnrollmentAdmissionOutcome, SubscriptionOfferStore,
    SubscriptionRecoveryAdmissionOutcome, SubscriptionRenewalAdmissionOutcome,
    admit_subscription_enrollment_submission, admit_subscription_recovery_submission,
    admit_subscription_renewal_submission,
    apply_reconciled_subscription_enrollment_gateway_outcome,
    apply_reconciled_subscription_recovery_gateway_outcome,
    apply_reconciled_subscription_renewal_gateway_outcome,
    apply_subscription_enrollment_gateway_outcome, apply_subscription_recovery_gateway_outcome,
    apply_subscription_renewal_gateway_outcome, cancel_subscription_in_transaction, due_renewals,
    entitlement, find_payment_attempt_by_id_in_transaction,
    renewal_failure::{RenewalFailureApplication, apply_resolved_automatic_renewal_failure},
    require_entitlement_for_update, reserve_subscription_enrollment_in_transaction,
    reserve_subscription_recovery_in_transaction, reserve_subscription_renewal_in_transaction,
    test_support::{GatewayAccountFixture, TestDatabase, create_gateway_account},
};

type SubscriptionDunningProjection = (
    String,
    DateTime<Utc>,
    Option<DateTime<Utc>>,
    Option<DateTime<Utc>>,
);
struct TestReferenceFactory;

impl GatewayMutationReferenceFactory for TestReferenceFactory {
    fn for_attempt(
        &self,
        _kind: PaymentAttemptKind,
        attempt_id: PaymentAttemptId,
    ) -> GatewayOrderId {
        GatewayOrderId::from_generated_attempt(
            format!("paid_trial_{}", attempt_id.as_uuid().simple()),
            attempt_id,
        )
        .expect("valid generated test order")
    }
}

struct NeverCalledGateway;

#[async_trait]
impl PaymentGateway for NeverCalledGateway {
    async fn account_mode(&self) -> Result<GatewayAccountMode, GatewayError> {
        panic!("direct outcome tests must not perform provider I/O")
    }

    async fn sale(
        &self,
        _request: GatewaySaleRequest,
    ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
        panic!("direct outcome tests must not perform provider I/O")
    }

    async fn store_payment_method(
        &self,
        _request: GatewayStorePaymentMethodRequest,
    ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
        panic!("direct outcome tests must not perform provider I/O")
    }

    async fn query_transaction(
        &self,
        _request: GatewayQueryRequest,
    ) -> Result<Option<GatewayPaymentOutcome>, GatewayError> {
        panic!("direct outcome tests must not perform provider I/O")
    }

    async fn query_transaction_reports(
        &self,
        _request: GatewayTransactionReportRequest,
    ) -> Result<Vec<GatewayTransactionReport>, GatewayError> {
        panic!("direct outcome tests must not perform provider I/O")
    }
}

struct CountingResolver {
    gateway: ResolvedGateway,
    calls: AtomicUsize,
}

#[async_trait]
impl GatewayResolver for CountingResolver {
    async fn resolve(
        &self,
        billing_scope_id: BillingScopeId,
        gateway_account_id: GatewayAccountId,
        gateway_configuration_id: GatewayConfigurationId,
        provider_key: GatewayProviderKey,
    ) -> Result<ResolvedGateway, GatewayResolutionError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if billing_scope_id != self.gateway.billing_scope_id()
            || gateway_account_id != self.gateway.gateway_account_id()
            || gateway_configuration_id != self.gateway.gateway_configuration_id()
            || provider_key != *self.gateway.provider_key()
        {
            return Err(GatewayResolutionError::ConfigurationChanged);
        }
        Ok(self.gateway.clone())
    }
}

struct PermitAdmission;

#[async_trait]
impl EndUserMutationAdmission for PermitAdmission {
    async fn admit(&self, _command: EndUserMutationCommand) -> EndUserMutationAdmissionResult {
        EndUserMutationAdmissionResult::Allowed
    }
}

#[derive(Clone)]
struct StaticOfferStore {
    offer: syrup_rail::SubscriptionOffer,
}

#[async_trait]
impl SubscriptionOfferStore for StaticOfferStore {
    async fn lock_current_offer(
        &self,
        _connection: &mut PgConnection,
        _billing_scope_id: BillingScopeId,
        plan_key: &PlanKey,
    ) -> Result<Option<syrup_rail::SubscriptionOffer>, sqlx::Error> {
        Ok((self.offer.plan_key() == plan_key).then(|| self.offer.clone()))
    }
}

#[derive(Clone)]
struct TestCoordinator {
    pool: PgPool,
    events: Arc<Mutex<Vec<BillingEvent>>>,
}

#[async_trait]
impl BillingTransactionCoordinator for TestCoordinator {
    async fn begin(
        &self,
        _subject: BillingEventSubject,
        _lock_timeout: Duration,
    ) -> Result<Box<dyn BillingTransaction>, BillingTransactionError> {
        Ok(Box::new(TestTransaction {
            transaction: Some(
                self.pool
                    .begin()
                    .await
                    .map_err(BillingTransactionError::new)?,
            ),
            events: Arc::clone(&self.events),
        }))
    }
}

struct TestTransaction {
    transaction: Option<Transaction<'static, Postgres>>,
    events: Arc<Mutex<Vec<BillingEvent>>>,
}

#[async_trait]
impl BillingTransaction for TestTransaction {
    fn connection(&mut self) -> &mut PgConnection {
        &mut *self.transaction.as_mut().expect("active test transaction")
    }

    fn subject_state(&self) -> BillingTransactionSubjectState {
        BillingTransactionSubjectState::LiveRecipient
    }

    async fn append_event(&mut self, event: &BillingEvent) -> Result<(), BillingEventWriteError> {
        self.events.lock().await.push(event.clone());
        Ok(())
    }

    async fn commit(mut self: Box<Self>) -> Result<(), BillingTransactionError> {
        self.transaction
            .take()
            .expect("active test transaction")
            .commit()
            .await
            .map_err(BillingTransactionError::new)
    }

    async fn rollback(mut self: Box<Self>) -> Result<(), BillingTransactionError> {
        self.transaction
            .take()
            .expect("active test transaction")
            .rollback()
            .await
            .map_err(BillingTransactionError::new)
    }
}

fn paid_trial_offer() -> Result<syrup_rail::SubscriptionOffer, Box<dyn Error>> {
    paid_trial_offer_with_policy(RenewalFailurePolicy::new(
        DunningSchedule::from_seconds([86_400, 259_200])?,
        DunningExhaustion::MarkUnpaid,
        PastDueAccessPolicy::ContinueUntilDunningExhausted,
    ))
}

fn paid_trial_offer_with_policy(
    renewal_failure: RenewalFailurePolicy,
) -> Result<syrup_rail::SubscriptionOffer, Box<dyn Error>> {
    Ok(syrup_rail::SubscriptionOffer::new(
        PlanKey::new("identity_pro")?,
        RecurringSubscriptionTerms::new(
            ChargeAmount::new(2_900, CurrencyCode::new("USD")?)?,
            SubscriptionPeriodRule::calendar_months(1)?,
        ),
        syrup_rail::SubscriptionStart::PaidTrial(PaidTrialTerms::new(
            ChargeAmount::new(100, CurrencyCode::new("USD")?)?,
            SubscriptionPeriodRule::fixed_days(7)?,
        )),
        renewal_failure,
    )?)
}

fn resolved_gateway(account: GatewayAccountFixture) -> Result<ResolvedGateway, Box<dyn Error>> {
    Ok(ResolvedGateway::new(
        BillingScopeId::new(account.billing_scope_id),
        GatewayAccountId::new(account.gateway_account_id),
        GatewayConfigurationId::new(account.gateway_configuration_id),
        GatewayProviderKey::new("nmi")?,
        GatewayLifecycleQueryPolicy::new(
            GatewayLifecycleCursorKey::new("paid_trial_test")?,
            ChronoDuration::minutes(1),
            10,
            2,
            2,
            20,
        )?,
        Arc::new(TestReferenceFactory),
        Arc::new(NeverCalledGateway),
    ))
}

fn approved_outcome(transaction_id: &str) -> GatewayPaymentOutcome {
    approved_outcome_with_reference(transaction_id, "vault_paid_trial")
}

fn approved_outcome_with_reference(
    transaction_id: &str,
    payment_method_reference: &str,
) -> GatewayPaymentOutcome {
    GatewayPaymentOutcome::new(
        GatewayPaymentStatus::Approved,
        ProcessorEvidence::new(
            Some(GatewayTransactionId::new(transaction_id).expect("valid transaction ID")),
            Some(
                GatewayPaymentMethodReference::new(payment_method_reference)
                    .expect("valid payment method reference"),
            ),
            Some(GatewayDiagnostic::new("1")),
            Some(GatewayDiagnostic::new("100")),
            Some(GatewayDiagnostic::new("Approved")),
            Some(GatewayDiagnostic::new("complete")),
            GatewayPaymentDescriptor::default(),
        ),
    )
}

fn declined_outcome(transaction_id: &str) -> GatewayPaymentOutcome {
    GatewayPaymentOutcome::new(
        GatewayPaymentStatus::Declined,
        ProcessorEvidence::new(
            Some(GatewayTransactionId::new(transaction_id).expect("valid transaction ID")),
            None,
            Some(GatewayDiagnostic::new("2")),
            Some(GatewayDiagnostic::new("200")),
            Some(GatewayDiagnostic::new("Declined")),
            Some(GatewayDiagnostic::new("declined")),
            GatewayPaymentDescriptor::default(),
        ),
    )
}

fn unknown_outcome() -> GatewayPaymentOutcome {
    GatewayPaymentOutcome::new(
        GatewayPaymentStatus::Unknown,
        ProcessorEvidence::new(
            None,
            None,
            None,
            None,
            Some(GatewayDiagnostic::new("provider outcome unknown")),
            Some(GatewayDiagnostic::new("unknown")),
            GatewayPaymentDescriptor::default(),
        ),
    )
}

fn processor_duplicate_outcome() -> GatewayPaymentOutcome {
    GatewayPaymentOutcome::new(
        GatewayPaymentStatus::Unknown,
        ProcessorEvidence::new(
            None,
            None,
            Some(GatewayDiagnostic::new("3")),
            Some(GatewayDiagnostic::new("430")),
            Some(GatewayDiagnostic::new("Duplicate transaction")),
            None,
            GatewayPaymentDescriptor::default(),
        ),
    )
    .with_diagnostics(vec![GatewayPaymentDiagnostic::ProcessorReportedDuplicate])
}

async fn reserve_and_admit_renewal(
    pool: &PgPool,
    gateway: &ResolvedGateway,
    command: ChargeRenewal,
) -> Result<syrup_rail::SubscriptionRenewalReservation, Box<dyn Error>> {
    let mut transaction = pool.begin().await?;
    let reservation = match reserve_subscription_renewal_in_transaction(
        &mut transaction,
        command,
        gateway,
        GatewayAccountMode::Live,
    )
    .await?
    {
        syrup_rail::SubscriptionRenewalReservationOutcome::Reserved(reservation, _) => *reservation,
        other => return Err(format!("unexpected renewal reservation: {other:?}").into()),
    };
    transaction.commit().await?;
    match admit_subscription_renewal_submission(pool, &reservation).await? {
        SubscriptionRenewalAdmissionOutcome::Admitted(_) => Ok(reservation),
        other => Err(format!("unexpected renewal admission: {other:?}").into()),
    }
}

async fn reserve_and_admit_recovery(
    pool: &PgPool,
    gateway: &ResolvedGateway,
    command: &RecoverSubscriptionPayment,
) -> Result<syrup_rail::SubscriptionRecoveryReservation, Box<dyn Error>> {
    let mut transaction = pool.begin().await?;
    let reservation = match reserve_subscription_recovery_in_transaction(
        &mut transaction,
        command,
        gateway,
        GatewayAccountMode::Live,
    )
    .await?
    {
        syrup_rail::SubscriptionRecoveryReservationOutcome::Reserved(reservation, _) => {
            *reservation
        }
        other => return Err(format!("unexpected recovery reservation: {other:?}").into()),
    };
    transaction.commit().await?;
    match admit_subscription_recovery_submission(pool, &reservation).await? {
        SubscriptionRecoveryAdmissionOutcome::Admitted(_) => Ok(reservation),
        other => Err(format!("unexpected recovery admission: {other:?}").into()),
    }
}

async fn resolved_at(
    pool: &PgPool,
    attempt_id: PaymentAttemptId,
) -> Result<DateTime<Utc>, sqlx::Error> {
    sqlx::query_scalar("SELECT resolved_at FROM billing_payment_attempts WHERE id = $1")
        .bind(attempt_id.as_uuid())
        .fetch_one(pool)
        .await
}

async fn make_trial_due(
    pool: &PgPool,
    subscription_id: syrup_rail::SubscriptionId,
) -> Result<DateTime<Utc>, sqlx::Error> {
    let due_at: DateTime<Utc> =
        sqlx::query_scalar("SELECT clock_timestamp() - interval '1 second'")
            .fetch_one(pool)
            .await?;
    sqlx::query(
        r#"
        UPDATE billing_subscriptions
        SET current_period_start_at = $2 - interval '7 days',
            current_period_end_at = $2,
            next_renewal_at = $2,
            next_payment_attempt_at = $2
        WHERE id = $1
        "#,
    )
    .bind(subscription_id.as_uuid())
    .bind(due_at)
    .execute(pool)
    .await?;
    Ok(due_at)
}

async fn make_retry_due(
    pool: &PgPool,
    subscription_id: syrup_rail::SubscriptionId,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE billing_subscriptions SET next_payment_attempt_at = clock_timestamp() - interval '1 second' WHERE id = $1",
    )
    .bind(subscription_id.as_uuid())
    .execute(pool)
    .await?;
    Ok(())
}

async fn decline_due_renewal(
    pool: &PgPool,
    gateway: &ResolvedGateway,
    coordinator: &dyn BillingTransactionCoordinator,
    billing_scope_id: BillingScopeId,
    subscription_id: syrup_rail::SubscriptionId,
    due_at: DateTime<Utc>,
    transaction_id: &str,
) -> Result<(SubscriptionEnrollmentPaymentResult, DateTime<Utc>), Box<dyn Error>> {
    let reservation = reserve_and_admit_renewal(
        pool,
        gateway,
        ChargeRenewal::new(billing_scope_id, subscription_id, due_at),
    )
    .await?;
    let result = apply_subscription_renewal_gateway_outcome(
        pool,
        coordinator,
        &reservation,
        &declined_outcome(transaction_id),
    )
    .await?;
    let at = resolved_at(pool, result.attempt().identity().attempt_id()).await?;
    Ok((result, at))
}

#[allow(clippy::too_many_arguments)]
async fn approve_enrollment(
    pool: &PgPool,
    offers: &dyn SubscriptionOfferStore,
    gateway: &ResolvedGateway,
    coordinator: &dyn BillingTransactionCoordinator,
    account: GatewayAccountFixture,
    subscriber_id: SubscriberId,
    idempotency_key: &str,
    expected_terms: SubscriptionEnrollmentExpectedTerms,
    transaction_id: &str,
    payment_method_reference: &str,
) -> Result<SubscriptionEnrollmentPaymentResult, Box<dyn Error>> {
    let command = syrup_rail::EnrollSubscription::new(
        syrup_rail::SubscriptionPaymentContext::new(
            PaymentAttemptId::new(Uuid::now_v7()),
            BillingScopeId::new(account.billing_scope_id),
            subscriber_id,
            GatewayConfigurationId::new(account.gateway_configuration_id),
            IdempotencyKey::new(idempotency_key)?,
            PaymentToken::new("opaque-enrollment-token")?,
            BillingContact::new(None, None, Some("subscriber@example.test".to_owned()))?,
        ),
        expected_terms,
    );
    let reservation = SubscriptionEnrollmentReservation::from_command(
        &command,
        gateway,
        GatewayAccountMode::Live,
    )?;
    let mut transaction = pool.begin().await?;
    match reserve_subscription_enrollment_in_transaction(&mut transaction, offers, &reservation)
        .await?
    {
        SubscriptionEnrollmentReservationOutcome::Reserved(_) => {}
        other => return Err(format!("unexpected enrollment reservation: {other:?}").into()),
    }
    transaction.commit().await?;
    match admit_subscription_enrollment_submission(pool, offers, &reservation).await? {
        SubscriptionEnrollmentAdmissionOutcome::Admitted(_) => {}
        other => return Err(format!("unexpected enrollment admission: {other:?}").into()),
    }
    Ok(apply_subscription_enrollment_gateway_outcome(
        pool,
        coordinator,
        &reservation,
        &approved_outcome_with_reference(transaction_id, payment_method_reference),
    )
    .await?)
}

mod access_policy;
mod cancellation;
mod consumer_edges;
mod enrollment;
mod infrastructure;
mod migration;
mod reclassification;
mod recovery;
mod terminal;
