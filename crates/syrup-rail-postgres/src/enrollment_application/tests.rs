use std::{
    collections::VecDeque,
    error::Error,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use chrono::Duration as ChronoDuration;
use sqlx::{PgConnection, Postgres, Transaction};
use syrup_rail::{
    BillingContact, BillingContactSnapshot, BillingEventKey, BillingEventSubject, ChargeAmount,
    ChargeRenewal, CurrencyCode, EndUserMutationAdmission, EndUserMutationAdmissionResult,
    EndUserMutationCommand, EnrollSubscription, GatewayAccountId, GatewayAccountMode,
    GatewayConfigurationId, GatewayError, GatewayLifecycleCursorKey, GatewayLifecycleQueryPolicy,
    GatewayMutationError, GatewayMutationReferenceFactory, GatewayOrderId,
    GatewayPaymentDescriptor, GatewayPaymentDiagnostic, GatewayPaymentMethodReference,
    GatewayPaymentOutcome, GatewayPaymentStatus, GatewayProviderKey, GatewayQueryRequest,
    GatewayResolutionError, GatewayResolver, GatewaySaleRequest, GatewayStorePaymentMethodRequest,
    GatewayTransactionId, GatewayTransactionReport, GatewayTransactionReportRequest,
    IdempotencyKey, ManualAttemptFailureOutcome, ManualFailureHostCharge, Money, PaymentAttempt,
    PaymentAttemptFingerprint, PaymentAttemptId, PaymentAttemptIdentity, PaymentAttemptLifecycle,
    PaymentAttemptRequest, PaymentAttemptState, PaymentAttemptTarget, PaymentAttemptTimestamps,
    PaymentCardBrand, PaymentGateway, PaymentToken, PercentOffBasisPoints,
    RecoverSubscriptionPayment, ReplaceSubscriptionPaymentMethod, ResolvedGateway,
    SubscriptionDiscountCode, SubscriptionDiscountDuration, SubscriptionDiscountKind,
    SubscriptionDiscountSnapshot, SubscriptionEnrollmentExpectedTerms,
    SubscriptionEnrollmentReservationOutcome, SubscriptionPaymentMethodReplacement,
    SubscriptionPaymentMethodReplacementRejection,
    SubscriptionPaymentMethodReplacementReservationOutcome, SubscriptionRecoveryReservationOutcome,
    SubscriptionRecoveryReservationRejection, SubscriptionRenewalOutcome,
    SubscriptionRenewalReservationOutcome, SubscriptionRenewalReservationRejection,
    SubscriptionStatus,
};
use tokio::sync::Mutex;

use super::*;
use crate::{
    BillingEventWriteError, BillingTransaction, BillingTransactionCoordinator,
    BillingTransactionSubjectState, GatewayMutationCooldownScope, ManualAttemptFailureHostStore,
    ManualAttemptFailureHostStoreError, ManualAttemptFailureHostTransitionOutcome,
    SubscriptionBillingService, SubscriptionBillingServiceError, SubscriptionOfferStore,
    due_renewals, fail_review_required_attempt, reserve_subscription_enrollment_in_transaction,
    reserve_subscription_payment_method_replacement_in_transaction,
    reserve_subscription_recovery_in_transaction, reserve_subscription_renewal_in_transaction,
    test_support::{TestDatabase, create_gateway_account, immediate_offer},
};

#[derive(Debug, Error)]
#[error("injected host transaction failure")]
struct InjectedHostError;

struct TestReferenceFactory;

impl GatewayMutationReferenceFactory for TestReferenceFactory {
    fn for_attempt(
        &self,
        _kind: PaymentAttemptKind,
        attempt_id: PaymentAttemptId,
    ) -> GatewayOrderId {
        GatewayOrderId::from_generated_attempt(
            format!("test_initial_{}", attempt_id.as_uuid().simple()),
            attempt_id,
        )
        .expect("valid generated test order")
    }
}

struct NeverCalledGateway;

#[async_trait]
impl PaymentGateway for NeverCalledGateway {
    async fn account_mode(&self) -> Result<GatewayAccountMode, GatewayError> {
        panic!("application fixture construction must not call the provider")
    }

    async fn sale(
        &self,
        _request: GatewaySaleRequest,
    ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
        panic!("application fixture construction must not call the provider")
    }

    async fn store_payment_method(
        &self,
        _request: GatewayStorePaymentMethodRequest,
    ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
        panic!("application fixture construction must not call the provider")
    }

    async fn query_transaction(
        &self,
        _request: GatewayQueryRequest,
    ) -> Result<Option<GatewayPaymentOutcome>, GatewayError> {
        panic!("application fixture construction must not call the provider")
    }

    async fn query_transaction_reports(
        &self,
        _request: GatewayTransactionReportRequest,
    ) -> Result<Vec<GatewayTransactionReport>, GatewayError> {
        panic!("application fixture construction must not call the provider")
    }
}

struct ScriptedGateway {
    account_mode_calls: AtomicUsize,
    account_mode_results: Mutex<VecDeque<Result<GatewayAccountMode, GatewayError>>>,
    sale_calls: AtomicUsize,
    sale_order_ids: Mutex<Vec<GatewayOrderId>>,
    sale_result: Mutex<Option<Result<GatewayPaymentOutcome, GatewayMutationError>>>,
    store_calls: AtomicUsize,
    store_order_ids: Mutex<Vec<GatewayOrderId>>,
    store_result: Mutex<Option<Result<GatewayPaymentOutcome, GatewayMutationError>>>,
}

struct RateLimitedReadinessGateway;

#[async_trait]
impl PaymentGateway for RateLimitedReadinessGateway {
    async fn account_mode(&self) -> Result<GatewayAccountMode, GatewayError> {
        Err(GatewayError::RateLimited(GatewayDiagnostic::new(
            "query throttle",
        )))
    }

    async fn sale(
        &self,
        _request: GatewaySaleRequest,
    ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
        panic!("readiness throttle must prevent provider mutation")
    }

    async fn store_payment_method(
        &self,
        _request: GatewayStorePaymentMethodRequest,
    ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
        panic!("initial enrollment must not store without a sale")
    }

    async fn query_transaction(
        &self,
        _request: GatewayQueryRequest,
    ) -> Result<Option<GatewayPaymentOutcome>, GatewayError> {
        panic!("initial enrollment readiness must not query transactions")
    }

    async fn query_transaction_reports(
        &self,
        _request: GatewayTransactionReportRequest,
    ) -> Result<Vec<GatewayTransactionReport>, GatewayError> {
        panic!("initial enrollment readiness must not query reports")
    }
}

struct CooldownDuringReadinessGateway {
    pool: PgPool,
    account_id: Uuid,
}

#[async_trait]
impl PaymentGateway for CooldownDuringReadinessGateway {
    async fn account_mode(&self) -> Result<GatewayAccountMode, GatewayError> {
        sqlx::query(
            r#"
            UPDATE billing_gateway_accounts
            SET mutation_rate_limited_until = clock_timestamp() + interval '1 minute'
            WHERE id = $1
            "#,
        )
        .bind(self.account_id)
        .execute(&self.pool)
        .await
        .expect("test readiness cooldown write");
        Ok(GatewayAccountMode::Live)
    }

    async fn sale(
        &self,
        _request: GatewaySaleRequest,
    ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
        panic!("fresh cooldown must prevent provider mutation")
    }

    async fn store_payment_method(
        &self,
        _request: GatewayStorePaymentMethodRequest,
    ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
        panic!("initial enrollment must not store without a sale")
    }

    async fn query_transaction(
        &self,
        _request: GatewayQueryRequest,
    ) -> Result<Option<GatewayPaymentOutcome>, GatewayError> {
        panic!("initial enrollment readiness must not query transactions")
    }

    async fn query_transaction_reports(
        &self,
        _request: GatewayTransactionReportRequest,
    ) -> Result<Vec<GatewayTransactionReport>, GatewayError> {
        panic!("initial enrollment readiness must not query reports")
    }
}

struct PermitAdmission {
    calls: AtomicUsize,
}

#[async_trait]
impl EndUserMutationAdmission for PermitAdmission {
    async fn admit(&self, _command: EndUserMutationCommand) -> EndUserMutationAdmissionResult {
        self.calls.fetch_add(1, Ordering::SeqCst);
        EndUserMutationAdmissionResult::Allowed
    }
}

struct StaticResolver {
    gateway: ResolvedGateway,
    calls: AtomicUsize,
}

#[async_trait]
impl GatewayResolver for StaticResolver {
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

impl ScriptedGateway {
    fn new(result: Result<GatewayPaymentOutcome, GatewayMutationError>) -> Self {
        Self {
            account_mode_calls: AtomicUsize::new(0),
            account_mode_results: Mutex::new(VecDeque::new()),
            sale_calls: AtomicUsize::new(0),
            sale_order_ids: Mutex::new(Vec::new()),
            sale_result: Mutex::new(Some(result)),
            store_calls: AtomicUsize::new(0),
            store_order_ids: Mutex::new(Vec::new()),
            store_result: Mutex::new(None),
        }
    }

    fn for_stored_method(result: Result<GatewayPaymentOutcome, GatewayMutationError>) -> Self {
        Self {
            account_mode_calls: AtomicUsize::new(0),
            account_mode_results: Mutex::new(VecDeque::new()),
            sale_calls: AtomicUsize::new(0),
            sale_order_ids: Mutex::new(Vec::new()),
            sale_result: Mutex::new(None),
            store_calls: AtomicUsize::new(0),
            store_order_ids: Mutex::new(Vec::new()),
            store_result: Mutex::new(Some(result)),
        }
    }

    fn for_stored_method_with_readiness(
        readiness: impl IntoIterator<Item = Result<GatewayAccountMode, GatewayError>>,
        result: Result<GatewayPaymentOutcome, GatewayMutationError>,
    ) -> Self {
        Self {
            account_mode_calls: AtomicUsize::new(0),
            account_mode_results: Mutex::new(readiness.into_iter().collect()),
            sale_calls: AtomicUsize::new(0),
            sale_order_ids: Mutex::new(Vec::new()),
            sale_result: Mutex::new(None),
            store_calls: AtomicUsize::new(0),
            store_order_ids: Mutex::new(Vec::new()),
            store_result: Mutex::new(Some(result)),
        }
    }

    fn for_sale_with_readiness(
        readiness: impl IntoIterator<Item = Result<GatewayAccountMode, GatewayError>>,
        result: Result<GatewayPaymentOutcome, GatewayMutationError>,
    ) -> Self {
        Self {
            account_mode_calls: AtomicUsize::new(0),
            account_mode_results: Mutex::new(readiness.into_iter().collect()),
            sale_calls: AtomicUsize::new(0),
            sale_order_ids: Mutex::new(Vec::new()),
            sale_result: Mutex::new(Some(result)),
            store_calls: AtomicUsize::new(0),
            store_order_ids: Mutex::new(Vec::new()),
            store_result: Mutex::new(None),
        }
    }
}

#[async_trait]
impl PaymentGateway for ScriptedGateway {
    async fn account_mode(&self) -> Result<GatewayAccountMode, GatewayError> {
        self.account_mode_calls.fetch_add(1, Ordering::SeqCst);
        self.account_mode_results
            .lock()
            .await
            .pop_front()
            .unwrap_or(Ok(GatewayAccountMode::Live))
    }

    async fn sale(
        &self,
        request: GatewaySaleRequest,
    ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
        self.sale_calls.fetch_add(1, Ordering::SeqCst);
        self.sale_order_ids
            .lock()
            .await
            .push(request.order_id().clone());
        self.sale_result
            .lock()
            .await
            .take()
            .expect("submission capability permits one scripted sale")
    }

    async fn store_payment_method(
        &self,
        request: GatewayStorePaymentMethodRequest,
    ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
        self.store_calls.fetch_add(1, Ordering::SeqCst);
        self.store_order_ids
            .lock()
            .await
            .push(request.order_id().clone());
        self.store_result
            .lock()
            .await
            .take()
            .expect("submission capability permits one scripted stored-method mutation")
    }

    async fn query_transaction(
        &self,
        _request: GatewayQueryRequest,
    ) -> Result<Option<GatewayPaymentOutcome>, GatewayError> {
        panic!("initial submission must not query")
    }

    async fn query_transaction_reports(
        &self,
        _request: GatewayTransactionReportRequest,
    ) -> Result<Vec<GatewayTransactionReport>, GatewayError> {
        panic!("initial submission must not query reports")
    }
}

fn scripted_resolved_gateway<G>(
    account: crate::test_support::GatewayAccountFixture,
    gateway: Arc<G>,
) -> ResolvedGateway
where
    G: PaymentGateway + 'static,
{
    ResolvedGateway::new(
        BillingScopeId::new(account.billing_scope_id),
        GatewayAccountId::new(account.gateway_account_id),
        GatewayConfigurationId::new(account.gateway_configuration_id),
        GatewayProviderKey::new("nmi").unwrap(),
        GatewayLifecycleQueryPolicy::new(
            GatewayLifecycleCursorKey::new("test_cursor").unwrap(),
            ChronoDuration::minutes(1),
            10,
            2,
            2,
            20,
        )
        .unwrap(),
        Arc::new(TestReferenceFactory),
        gateway,
    )
}

struct TestOfferStore;

#[async_trait]
impl SubscriptionOfferStore for TestOfferStore {
    async fn lock_current_offer(
        &self,
        connection: &mut PgConnection,
        billing_scope_id: BillingScopeId,
        plan_key: &PlanKey,
    ) -> Result<Option<syrup_rail::SubscriptionOffer>, sqlx::Error> {
        let row = sqlx::query_as::<_, (i32, String)>(
            r#"
            SELECT amount_cents, currency
            FROM host_subscription_offers
            WHERE billing_scope_id = $1 AND plan_key = $2
            FOR UPDATE
            "#,
        )
        .bind(billing_scope_id.as_uuid())
        .bind(plan_key.as_str())
        .fetch_optional(connection)
        .await?;
        row.map(|(amount_cents, currency)| {
            Ok(immediate_offer(
                plan_key.clone(),
                ChargeAmount::new(amount_cents, CurrencyCode::new(&currency).unwrap()).unwrap(),
            ))
        })
        .transpose()
    }
}

#[derive(Clone)]
struct TestCoordinator {
    pool: PgPool,
    events: Arc<Mutex<Vec<BillingEvent>>>,
    fail_begin: bool,
    fail_event: bool,
}

struct NeverManualFailureHost;

#[async_trait]
impl ManualAttemptFailureHostStore for NeverManualFailureHost {
    async fn lock_payment_failure_target(
        &self,
        _connection: &mut PgConnection,
        _charge: ManualFailureHostCharge,
    ) -> Result<(), ManualAttemptFailureHostStoreError> {
        panic!("subscription enrollment review does not have a host-charge target")
    }

    async fn mark_payment_failed(
        &self,
        _connection: &mut PgConnection,
        _charge: ManualFailureHostCharge,
    ) -> Result<ManualAttemptFailureHostTransitionOutcome, ManualAttemptFailureHostStoreError> {
        panic!("subscription enrollment review does not have a host-charge target")
    }
}

#[async_trait]
impl BillingTransactionCoordinator for TestCoordinator {
    async fn begin(
        &self,
        _subject: BillingEventSubject,
        _lock_timeout: Duration,
    ) -> Result<Box<dyn BillingTransaction>, BillingTransactionError> {
        if self.fail_begin {
            return Err(BillingTransactionError::new(InjectedHostError));
        }
        Ok(Box::new(TestTransaction {
            transaction: Some(
                self.pool
                    .begin()
                    .await
                    .map_err(BillingTransactionError::new)?,
            ),
            events: Arc::clone(&self.events),
            fail_event: self.fail_event,
        }))
    }
}

struct TestTransaction {
    transaction: Option<Transaction<'static, Postgres>>,
    events: Arc<Mutex<Vec<BillingEvent>>>,
    fail_event: bool,
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
        if self.fail_event {
            return Err(BillingEventWriteError::new(InjectedHostError));
        }
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

struct ApplicationFixture {
    database: TestDatabase,
    reservation: SubscriptionEnrollmentReservation,
    coordinator: TestCoordinator,
    command: EnrollSubscription,
    gateway_account: crate::test_support::GatewayAccountFixture,
    admission: Option<Box<AdmittedSubscriptionEnrollment>>,
}

impl ApplicationFixture {
    async fn cleanup(self) -> Result<(), Box<dyn Error>> {
        self.database.cleanup().await
    }
}

async fn application_fixture(
    project: &str,
    discounted: bool,
    fail_event: bool,
) -> Result<ApplicationFixture, Box<dyn Error>> {
    enrollment_fixture(project, discounted, fail_event, true).await
}

async fn enrollment_fixture(
    project: &str,
    discounted: bool,
    fail_event: bool,
    prepare_submission: bool,
) -> Result<ApplicationFixture, Box<dyn Error>> {
    let database = TestDatabase::start(project).await?;
    sqlx::query(
        r#"
        CREATE TABLE host_subscription_offers (
            billing_scope_id uuid NOT NULL,
            plan_key text NOT NULL,
            amount_cents integer NOT NULL,
            currency text NOT NULL,
            PRIMARY KEY (billing_scope_id, plan_key)
        )
        "#,
    )
    .execute(&database.pool)
    .await?;
    let account = create_gateway_account(&database.pool, "nmi").await?;
    sqlx::query(
        "INSERT INTO host_subscription_offers VALUES ($1, 'base_subscription', 1000, 'USD')",
    )
    .bind(account.billing_scope_id)
    .execute(&database.pool)
    .await?;
    let subscriber_id = Uuid::now_v7();
    let expected_charge = if discounted {
        let code_id = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO billing_subscription_discount_codes (
                id, billing_scope_id, plan_key, code_normalized, display_code,
                status, discount_kind, percent_off_bps, currency,
                duration, duration_months
            ) VALUES (
                $1, $2, 'base_subscription', 'SAVE20', 'SAVE20',
                'active', 'percent_off', 2000, 'USD', 'limited_months', 3
            )
            "#,
        )
        .bind(code_id)
        .bind(account.billing_scope_id)
        .execute(&database.pool)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO billing_subscription_discount_claims (
                id, billing_scope_id, subscriber_id, plan_key,
                discount_code_id, code_snapshot, discount_kind,
                percent_off_bps, currency, duration, duration_months,
                base_amount_cents, discounted_amount_cents, status
            ) VALUES (
                $1, $2, $3, 'base_subscription', $4, 'SAVE20',
                'percent_off', 2000, 'USD', 'limited_months', 3,
                1000, 800, 'saved'
            )
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(account.billing_scope_id)
        .bind(subscriber_id)
        .bind(code_id)
        .execute(&database.pool)
        .await?;
        SubscriptionEnrollmentExpectedTerms::discounted(
            immediate_offer(
                PlanKey::new("base_subscription")?,
                ChargeAmount::new(1000, CurrencyCode::new("USD")?)?,
            ),
            SubscriptionDiscountSnapshot::new(
                SubscriptionDiscountCode::new("SAVE20")?,
                None,
                SubscriptionDiscountKind::PercentOffBasisPoints(PercentOffBasisPoints::new(2000)?),
                SubscriptionDiscountDuration::LimitedMonths(
                    syrup_rail::LimitedDiscountMonths::new(3)?,
                ),
                ChargeAmount::new(1000, CurrencyCode::new("USD")?)?,
                ChargeAmount::new(800, CurrencyCode::new("USD")?)?,
            )?,
        )?
    } else {
        SubscriptionEnrollmentExpectedTerms::full_price(immediate_offer(
            PlanKey::new("base_subscription")?,
            ChargeAmount::new(1000, CurrencyCode::new("USD")?)?,
        ))
    };
    let attempt_id = PaymentAttemptId::new(Uuid::now_v7());
    let command = EnrollSubscription::new(
        syrup_rail::SubscriptionPaymentContext::new(
            attempt_id,
            BillingScopeId::new(account.billing_scope_id),
            SubscriberId::new(subscriber_id),
            GatewayConfigurationId::new(account.gateway_configuration_id),
            IdempotencyKey::new("application-key")?,
            PaymentToken::new("opaque-payment-token")?,
            BillingContact::new(
                Some("Ada".to_owned()),
                Some("Lovelace".to_owned()),
                Some("ada@example.test".to_owned()),
            )?,
        ),
        expected_charge,
    );
    let gateway = ResolvedGateway::new(
        BillingScopeId::new(account.billing_scope_id),
        GatewayAccountId::new(account.gateway_account_id),
        GatewayConfigurationId::new(account.gateway_configuration_id),
        GatewayProviderKey::new("nmi")?,
        GatewayLifecycleQueryPolicy::new(
            GatewayLifecycleCursorKey::new("test_cursor")?,
            ChronoDuration::minutes(1),
            10,
            2,
            2,
            20,
        )?,
        Arc::new(TestReferenceFactory),
        Arc::new(NeverCalledGateway),
    );
    let reservation = SubscriptionEnrollmentReservation::from_command(
        &command,
        &gateway,
        GatewayAccountMode::Live,
    )?;
    let admission = if prepare_submission {
        let mut transaction = database.pool.begin().await?;
        assert!(matches!(
            reserve_subscription_enrollment_in_transaction(
                &mut transaction,
                &TestOfferStore,
                &reservation,
            )
            .await?,
            SubscriptionEnrollmentReservationOutcome::Reserved(_)
        ));
        transaction.commit().await?;
        Some(
            match admit_subscription_enrollment_submission(
                &database.pool,
                &TestOfferStore,
                &reservation,
            )
            .await?
            {
                SubscriptionEnrollmentAdmissionOutcome::Admitted(admission) => admission,
                other => return Err(format!("unexpected final admission: {other:?}").into()),
            },
        )
    } else {
        None
    };
    let coordinator = TestCoordinator {
        pool: database.pool.clone(),
        events: Arc::new(Mutex::new(Vec::new())),
        fail_begin: false,
        fail_event,
    };
    Ok(ApplicationFixture {
        database,
        reservation,
        coordinator,
        command,
        gateway_account: account,
        admission,
    })
}

async fn reconciled_payment_method_replacement_fixture(
    project: &str,
) -> Result<(ApplicationFixture, SubscriptionPaymentMethodReplacement), Box<dyn Error>> {
    let fixture = enrollment_fixture(project, false, false, false).await?;
    let initial_gateway = Arc::new(ScriptedGateway::for_sale_with_readiness(
        [Ok(GatewayAccountMode::Test), Ok(GatewayAccountMode::Test)],
        Ok(approved_outcome_with_reference(
            Some(&format!("txn_{project}_initial")),
            &format!("vault_{project}_initial"),
        )),
    ));
    let initial_service = SubscriptionBillingService::new(
        fixture.database.pool.clone(),
        Arc::new(TestOfferStore),
        Arc::new(StaticResolver {
            gateway: scripted_resolved_gateway(
                fixture.gateway_account,
                Arc::clone(&initial_gateway),
            ),
            calls: AtomicUsize::new(0),
        }),
        Arc::new(PermitAdmission {
            calls: AtomicUsize::new(0),
        }),
        Arc::new(fixture.coordinator.clone()),
    )
    .with_required_gateway_account_mode(GatewayAccountMode::Test);
    initial_service.enroll(fixture.command.clone()).await?;

    let replacement_gateway =
        scripted_resolved_gateway(fixture.gateway_account, Arc::new(NeverCalledGateway));
    let command = ReplaceSubscriptionPaymentMethod::new(
        syrup_rail::SubscriptionPaymentContext::new(
            PaymentAttemptId::new(Uuid::now_v7()),
            fixture.command.billing_scope_id(),
            fixture.command.subscriber_id(),
            fixture.command.gateway_configuration_id(),
            IdempotencyKey::new(format!("{project}-replacement-key"))?,
            PaymentToken::new(format!("opaque-{project}-replacement-token"))?,
            fixture.command.billing_contact().clone(),
        ),
        fixture.command.plan_key().clone(),
    );
    let mut transaction = fixture.database.pool.begin().await?;
    let reservation = match reserve_subscription_payment_method_replacement_in_transaction(
        &mut transaction,
        &command,
        &replacement_gateway,
        GatewayAccountMode::Test,
    )
    .await?
    {
        SubscriptionPaymentMethodReplacementReservationOutcome::Reserved(reservation, _) => {
            *reservation
        }
        other => return Err(format!("unexpected replacement reservation: {other:?}").into()),
    };
    transaction.commit().await?;
    match admit_subscription_payment_method_replacement(&fixture.database.pool, &reservation)
        .await?
    {
        SubscriptionPaymentMethodReplacementAdmissionOutcome::Admitted(_) => {}
        other => return Err(format!("unexpected replacement admission: {other:?}").into()),
    }
    Ok((fixture, reservation))
}

async fn hold_subscription_aggregate_lock(
    pool: &sqlx::PgPool,
    subscriber_id: Uuid,
    plan_key: &str,
) -> Result<Transaction<'static, Postgres>, sqlx::Error> {
    let mut transaction = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::uuid::text || ':' || $2, 0))")
        .bind(subscriber_id)
        .bind(plan_key)
        .execute(&mut *transaction)
        .await?;
    Ok(transaction)
}

fn approved_outcome(transaction_id: &str) -> GatewayPaymentOutcome {
    approved_outcome_with_reference(Some(transaction_id), "vault_application")
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

fn indeterminate_processor_error_outcome() -> GatewayPaymentOutcome {
    GatewayPaymentOutcome::new(
        GatewayPaymentStatus::Approved,
        ProcessorEvidence::new(
            None,
            None,
            Some(GatewayDiagnostic::new("3")),
            Some(GatewayDiagnostic::new("400")),
            Some(GatewayDiagnostic::new("Processor error")),
            None,
            GatewayPaymentDescriptor::default(),
        ),
    )
    .with_diagnostics(vec![GatewayPaymentDiagnostic::IndeterminatePaymentOutcome])
}

fn approved_outcome_with_transaction(transaction_id: Option<&str>) -> GatewayPaymentOutcome {
    approved_outcome_with_reference(transaction_id, "vault_application")
}

fn approved_outcome_with_reference(
    transaction_id: Option<&str>,
    payment_method_reference: &str,
) -> GatewayPaymentOutcome {
    approved_outcome_with_optional_reference(transaction_id, Some(payment_method_reference))
}

fn approved_outcome_with_optional_reference(
    transaction_id: Option<&str>,
    payment_method_reference: Option<&str>,
) -> GatewayPaymentOutcome {
    GatewayPaymentOutcome::new(
        GatewayPaymentStatus::Approved,
        ProcessorEvidence::new(
            transaction_id.map(|value| GatewayTransactionId::new(value).unwrap()),
            payment_method_reference
                .map(|value| GatewayPaymentMethodReference::new(value).unwrap()),
            Some(GatewayDiagnostic::new("1")),
            Some(GatewayDiagnostic::new("100")),
            Some(GatewayDiagnostic::new("Approved")),
            Some(GatewayDiagnostic::new("complete")),
            GatewayPaymentDescriptor::from_provider_parts(
                Some(GatewayDiagnostic::new("creditcard")),
                Some(GatewayDiagnostic::new("visa")),
                Some("4242"),
                Some(12),
                Some(2031),
            ),
        ),
    )
}

mod application;
mod foreground;
mod unit;
