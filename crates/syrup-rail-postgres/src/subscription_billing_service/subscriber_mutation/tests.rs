use std::{
    error::Error,
    io,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use chrono::Utc;
use sqlx::{PgConnection, PgPool, Postgres, Transaction};
use syrup_rail::{
    BillingEvent, BillingEventSubject, BillingScopeId, CancelSubscription,
    CancelSubscriptionOutcome, ChargeAmount, ClearSubscriptionDiscount, CurrencyCode,
    DiscountClaimId, DiscountCodeId, EndUserMutationAdmission, EndUserMutationAdmissionResult,
    EndUserMutationCommand, EndUserMutationOperation, EndUserMutationRetryAfter, GatewayAccountId,
    GatewayConfigurationId, GatewayProviderKey, GatewayResolutionError, GatewayResolver, PlanKey,
    PositiveDiscountCents, ResolvedGateway, SubscriberId, SubscriptionDiscountClaim,
    SubscriptionDiscountClaimOutcome, SubscriptionDiscountClearOutcome, SubscriptionDiscountCode,
    SubscriptionDiscountCodeCreation, SubscriptionDiscountDuration, SubscriptionDiscountKind,
};
use tokio::sync::Mutex;
use uuid::Uuid;

use super::*;
use crate::{
    BillingEventWriteError, BillingTransaction, BillingTransactionCoordinator,
    BillingTransactionError, BillingTransactionSubjectState, SubscriptionBillingServiceError,
    SubscriptionOfferStore, create_subscription_discount_code, saved_subscription_discount_claim,
    test_support::{TestDatabase, create_gateway_account, immediate_offer},
};

use self::support::*;

mod discount;
mod support;

#[derive(Clone)]
struct RecordingAdmission {
    result: EndUserMutationAdmissionResult,
    commands: Arc<Mutex<Vec<EndUserMutationCommand>>>,
}

impl RecordingAdmission {
    fn allowed() -> Self {
        Self {
            result: EndUserMutationAdmissionResult::Allowed,
            commands: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn denied() -> Self {
        Self {
            result: EndUserMutationAdmissionResult::Denied {
                retry_after: EndUserMutationRetryAfter::new(Duration::from_secs(30))
                    .expect("positive test retry-after"),
            },
            commands: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[async_trait]
impl EndUserMutationAdmission for RecordingAdmission {
    async fn admit(&self, command: EndUserMutationCommand) -> EndUserMutationAdmissionResult {
        self.commands.lock().await.push(command);
        self.result
    }
}

struct CountingResolver {
    calls: AtomicUsize,
}

#[async_trait]
impl GatewayResolver for CountingResolver {
    async fn resolve(
        &self,
        _billing_scope_id: BillingScopeId,
        _gateway_account_id: GatewayAccountId,
        _gateway_configuration_id: GatewayConfigurationId,
        _provider_key: GatewayProviderKey,
    ) -> Result<ResolvedGateway, GatewayResolutionError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(GatewayResolutionError::Unavailable)
    }
}

struct CountingOfferStore {
    offer: syrup_rail::SubscriptionOffer,
    calls: AtomicUsize,
}

impl CountingOfferStore {
    fn new(offer: syrup_rail::SubscriptionOffer) -> Self {
        Self {
            offer,
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl SubscriptionOfferStore for CountingOfferStore {
    async fn lock_current_offer(
        &self,
        _connection: &mut PgConnection,
        _billing_scope_id: BillingScopeId,
        plan_key: &PlanKey,
    ) -> Result<Option<syrup_rail::SubscriptionOffer>, sqlx::Error> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok((self.offer.plan_key() == plan_key).then(|| self.offer.clone()))
    }
}

#[derive(Clone)]
struct TestCoordinator {
    pool: PgPool,
    begins: Arc<AtomicUsize>,
    subjects: Arc<Mutex<Vec<BillingEventSubject>>>,
    lock_timeouts: Arc<Mutex<Vec<Duration>>>,
    append_returns_error: bool,
}

impl TestCoordinator {
    fn new(pool: PgPool, append_returns_error: bool) -> Self {
        Self {
            pool,
            begins: Arc::new(AtomicUsize::new(0)),
            subjects: Arc::new(Mutex::new(Vec::new())),
            lock_timeouts: Arc::new(Mutex::new(Vec::new())),
            append_returns_error,
        }
    }
}

#[async_trait]
impl BillingTransactionCoordinator for TestCoordinator {
    async fn begin(
        &self,
        subject: BillingEventSubject,
        lock_timeout: Duration,
    ) -> Result<Box<dyn BillingTransaction>, BillingTransactionError> {
        self.begins.fetch_add(1, Ordering::SeqCst);
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(BillingTransactionError::new)?;
        let locked: Option<Uuid> = sqlx::query_scalar(
            r#"
            SELECT subscriber_id
            FROM test_billing_subjects
            WHERE billing_scope_id = $1 AND subscriber_id = $2
            FOR UPDATE
            "#,
        )
        .bind(subject.billing_scope_id().as_uuid())
        .bind(subject.subscriber_id().as_uuid())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(BillingTransactionError::new)?;
        if locked.is_none() {
            let _ = transaction.rollback().await;
            return Err(BillingTransactionError::new(io::Error::other(
                "missing test host billing subject",
            )));
        }
        self.subjects.lock().await.push(subject);
        self.lock_timeouts.lock().await.push(lock_timeout);
        Ok(Box::new(TestTransaction {
            transaction: Some(transaction),
            append_returns_error: self.append_returns_error,
        }))
    }
}

struct TestTransaction {
    transaction: Option<Transaction<'static, Postgres>>,
    append_returns_error: bool,
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
        let event_kind = match event {
            BillingEvent::SubscriptionCanceled { .. } => "subscription_canceled",
            _ => "unexpected",
        };
        sqlx::query("INSERT INTO test_billing_outbox (event_kind) VALUES ($1)")
            .bind(event_kind)
            .execute(&mut **self.transaction.as_mut().expect("active test transaction"))
            .await
            .map_err(BillingEventWriteError::new)?;
        if self.append_returns_error {
            return Err(BillingEventWriteError::new(io::Error::other(
                "injected outbox failure",
            )));
        }
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

struct ActiveSubscriptionFixture {
    id: Uuid,
    payment_method_id: Uuid,
    period_end: chrono::DateTime<Utc>,
    initial_transaction_id: String,
}

#[tokio::test]
async fn cancellation_admits_exactly_once_per_call_and_commits_one_host_event()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("srv_mut_cancel").await?;
    let result = async {
        install_host_boundary(&database.pool).await?;
        let account = create_gateway_account(&database.pool, "test_gateway").await?;
        let scope = BillingScopeId::new(account.billing_scope_id);
        let subscriber = SubscriberId::new(Uuid::now_v7());
        insert_host_subject(&database.pool, scope, subscriber).await?;
        let plan = PlanKey::new("cancel_plan")?;
        let active = insert_active_subscription(&database.pool, account, subscriber, &plan).await?;
        let blocked_plan = PlanKey::new("blocked_plan")?;
        let blocked =
            insert_active_subscription(&database.pool, account, subscriber, &blocked_plan).await?;
        insert_blocking_renewal(&database.pool, account, subscriber, &blocked_plan, &blocked)
            .await?;

        let offers = Arc::new(CountingOfferStore::new(immediate_offer(
            plan.clone(),
            ChargeAmount::new(5_900, CurrencyCode::new("USD")?)?,
        )));
        let admission = Arc::new(RecordingAdmission::allowed());
        let resolver = Arc::new(CountingResolver {
            calls: AtomicUsize::new(0),
        });
        let coordinator = Arc::new(TestCoordinator::new(database.pool.clone(), false));
        let service = SubscriptionBillingService::new(
            database.pool.clone(),
            offers.clone(),
            resolver.clone(),
            admission.clone(),
            coordinator.clone(),
        );
        let command = CancelSubscription::new(scope, subscriber, plan.clone());

        assert!(matches!(
            service.cancel(command.clone()).await?,
            CancelSubscriptionOutcome::Canceled { .. }
        ));
        assert_eq!(
            subscription_status(&database.pool, active.id).await?,
            "canceled"
        );
        assert_eq!(outbox_count(&database.pool).await?, 1);

        assert!(matches!(
            service.cancel(command).await?,
            CancelSubscriptionOutcome::AlreadyCanceled(_)
        ));
        assert_eq!(outbox_count(&database.pool).await?, 1);

        assert_eq!(
            service
                .cancel(CancelSubscription::new(scope, subscriber, blocked_plan))
                .await?,
            CancelSubscriptionOutcome::BlockedByRenewal
        );
        assert_eq!(outbox_count(&database.pool).await?, 1);
        assert_eq!(
            subscription_status(&database.pool, blocked.id).await?,
            "active"
        );

        assert_eq!(
            service
                .cancel(CancelSubscription::new(
                    scope,
                    subscriber,
                    PlanKey::new("missing_plan")?,
                ))
                .await?,
            CancelSubscriptionOutcome::NotFound
        );
        assert_eq!(outbox_count(&database.pool).await?, 1);
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 0);
        assert_eq!(offers.calls.load(Ordering::SeqCst), 0);
        assert_eq!(coordinator.begins.load(Ordering::SeqCst), 4);
        assert_eq!(
            *coordinator.subjects.lock().await,
            vec![BillingEventSubject::new(scope, subscriber); 4]
        );
        assert_eq!(
            *coordinator.lock_timeouts.lock().await,
            vec![Duration::from_millis(250); 4]
        );
        assert_eq!(
            admission
                .commands
                .lock()
                .await
                .iter()
                .map(EndUserMutationCommand::operation)
                .collect::<Vec<_>>(),
            vec![EndUserMutationOperation::SubscriptionCancel; 4]
        );
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn cancellation_rolls_back_when_host_append_or_the_canonical_mutation_fails()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("srv_mut_can_rb").await?;
    let result = async {
        install_host_boundary(&database.pool).await?;
        let account = create_gateway_account(&database.pool, "test_gateway").await?;
        let scope = BillingScopeId::new(account.billing_scope_id);
        let subscriber = SubscriberId::new(Uuid::now_v7());
        insert_host_subject(&database.pool, scope, subscriber).await?;
        let plan = PlanKey::new("append_failure")?;
        let fixture =
            insert_active_subscription(&database.pool, account, subscriber, &plan).await?;
        let offers = Arc::new(CountingOfferStore::new(immediate_offer(
            plan.clone(),
            ChargeAmount::new(5_900, CurrencyCode::new("USD")?)?,
        )));
        let resolver = Arc::new(CountingResolver {
            calls: AtomicUsize::new(0),
        });
        let service = SubscriptionBillingService::new(
            database.pool.clone(),
            offers,
            resolver.clone(),
            Arc::new(RecordingAdmission::allowed()),
            Arc::new(TestCoordinator::new(database.pool.clone(), true)),
        );
        assert!(matches!(
            service
                .cancel(CancelSubscription::new(scope, subscriber, plan.clone()))
                .await,
            Err(SubscriptionBillingServiceError::BillingEvent(_))
        ));
        assert_eq!(
            subscription_status(&database.pool, fixture.id).await?,
            "active"
        );
        assert_eq!(outbox_count(&database.pool).await?, 0);

        let mutation_plan = PlanKey::new("mutation_failure")?;
        let mutation =
            insert_active_subscription(&database.pool, account, subscriber, &mutation_plan).await?;
        let stale_attempt = insert_stale_payment_method_update(
            &database.pool,
            account,
            subscriber,
            &mutation_plan,
            &mutation,
        )
        .await?;
        install_cancellation_failure_trigger(&database.pool).await?;
        let service = SubscriptionBillingService::new(
            database.pool.clone(),
            Arc::new(CountingOfferStore::new(immediate_offer(
                mutation_plan.clone(),
                ChargeAmount::new(5_900, CurrencyCode::new("USD")?)?,
            ))),
            resolver.clone(),
            Arc::new(RecordingAdmission::allowed()),
            Arc::new(TestCoordinator::new(database.pool.clone(), false)),
        );
        assert!(matches!(
            service
                .cancel(CancelSubscription::new(scope, subscriber, mutation_plan))
                .await,
            Err(SubscriptionBillingServiceError::Cancellation(_))
        ));
        assert_eq!(
            subscription_status(&database.pool, mutation.id).await?,
            "active"
        );
        assert_eq!(
            payment_attempt_status(&database.pool, stale_attempt).await?,
            "pending"
        );
        assert_eq!(outbox_count(&database.pool).await?, 0);
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 0);
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn denied_cancellation_performs_no_database_or_provider_work() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("srv_mut_can_no").await?;
    let result = async {
        install_host_boundary(&database.pool).await?;
        let account = create_gateway_account(&database.pool, "test_gateway").await?;
        let scope = BillingScopeId::new(account.billing_scope_id);
        let subscriber = SubscriberId::new(Uuid::now_v7());
        insert_host_subject(&database.pool, scope, subscriber).await?;
        let plan = PlanKey::new("denied_cancel")?;
        let fixture =
            insert_active_subscription(&database.pool, account, subscriber, &plan).await?;
        let offers = Arc::new(CountingOfferStore::new(immediate_offer(
            plan.clone(),
            ChargeAmount::new(5_900, CurrencyCode::new("USD")?)?,
        )));
        let admission = Arc::new(RecordingAdmission::denied());
        let resolver = Arc::new(CountingResolver {
            calls: AtomicUsize::new(0),
        });
        let coordinator = Arc::new(TestCoordinator::new(database.pool.clone(), false));
        let service = SubscriptionBillingService::new(
            database.pool.clone(),
            offers.clone(),
            resolver.clone(),
            admission.clone(),
            coordinator.clone(),
        );

        assert!(matches!(
            service
                .cancel(CancelSubscription::new(scope, subscriber, plan))
                .await,
            Err(SubscriptionBillingServiceError::AdmissionDenied { .. })
        ));
        assert_eq!(
            subscription_status(&database.pool, fixture.id).await?,
            "active"
        );
        assert_eq!(outbox_count(&database.pool).await?, 0);
        assert_eq!(coordinator.begins.load(Ordering::SeqCst), 0);
        assert_eq!(offers.calls.load(Ordering::SeqCst), 0);
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 0);
        assert_eq!(admission.commands.lock().await.len(), 1);
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn provider_free_lock_timeouts_are_retryable_without_reclassifying_unknown_storage_errors()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("srv_mut_retry").await?;
    let result = async {
        install_host_boundary(&database.pool).await?;
        let account = create_gateway_account(&database.pool, "test_gateway").await?;
        let scope = BillingScopeId::new(account.billing_scope_id);
        let subscriber = SubscriberId::new(Uuid::now_v7());
        insert_host_subject(&database.pool, scope, subscriber).await?;
        let cancel_plan = PlanKey::new("retry_cancel")?;
        insert_active_subscription(&database.pool, account, subscriber, &cancel_plan).await?;

        let discount_plan = PlanKey::new("retry_discount")?;
        let offers = Arc::new(CountingOfferStore::new(immediate_offer(
            discount_plan.clone(),
            ChargeAmount::new(5_900, CurrencyCode::new("USD")?)?,
        )));
        let code = SubscriptionDiscountCode::new("RETRY10")?;
        create_subscription_discount_code(
            &database.pool,
            offers.as_ref(),
            &SubscriptionDiscountCodeCreation::new(
                DiscountCodeId::new(Uuid::now_v7()),
                scope,
                discount_plan.clone(),
                code.clone(),
                None,
                SubscriptionDiscountKind::AmountOffCents(PositiveDiscountCents::new(500)?),
                CurrencyCode::new("USD")?,
                SubscriptionDiscountDuration::Indefinite,
            )?,
        )
        .await?;
        let resolver = Arc::new(CountingResolver {
            calls: AtomicUsize::new(0),
        });
        let service = SubscriptionBillingService::new(
            database.pool.clone(),
            offers,
            resolver.clone(),
            Arc::new(RecordingAdmission::allowed()),
            Arc::new(TestCoordinator::new(database.pool.clone(), false)),
        );

        let cancel_blocker =
            hold_subscription_aggregate_lock(&database.pool, subscriber, &cancel_plan).await?;
        let cancel_error = service
            .cancel(CancelSubscription::new(
                scope,
                subscriber,
                cancel_plan.clone(),
            ))
            .await
            .expect_err("held aggregate lock must time out");
        assert!(matches!(
            cancel_error,
            SubscriptionBillingServiceError::StorageTemporarilyUnavailable(_)
        ));
        assert!(cancel_error.is_retryable());
        assert_eq!(cancel_error.retry_after(), None);
        cancel_blocker.rollback().await?;
        assert!(matches!(
            service
                .cancel(CancelSubscription::new(scope, subscriber, cancel_plan))
                .await?,
            CancelSubscriptionOutcome::Canceled { .. }
        ));

        let claim = SubscriptionDiscountClaim::new(
            DiscountClaimId::new(Uuid::now_v7()),
            scope,
            subscriber,
            discount_plan.clone(),
            code,
        );
        let discount_blocker =
            hold_subscription_aggregate_lock(&database.pool, subscriber, &discount_plan).await?;
        let discount_error = service
            .claim_discount(claim.clone())
            .await
            .expect_err("held aggregate lock must time out");
        assert!(matches!(
            discount_error,
            SubscriptionBillingServiceError::StorageTemporarilyUnavailable(_)
        ));
        assert!(discount_error.is_retryable());
        assert_eq!(discount_error.retry_after(), None);
        discount_blocker.rollback().await?;
        assert!(matches!(
            service.claim_discount(claim).await?,
            SubscriptionDiscountClaimOutcome::Saved(_)
        ));

        assert_eq!(resolver.calls.load(Ordering::SeqCst), 0);
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}
