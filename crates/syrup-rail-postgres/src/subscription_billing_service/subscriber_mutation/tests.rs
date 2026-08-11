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
use chrono::{Duration as ChronoDuration, Utc};
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
    test_support::{GatewayAccountFixture, TestDatabase, create_gateway_account, immediate_offer},
};

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

#[tokio::test]
async fn discount_service_facade_admits_claim_and_clear_without_provider_or_host_events()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("srv_mut_discount").await?;
    let result = async {
        install_host_boundary(&database.pool).await?;
        let account = create_gateway_account(&database.pool, "test_gateway").await?;
        let scope = BillingScopeId::new(account.billing_scope_id);
        let subscriber = SubscriberId::new(Uuid::now_v7());
        let plan = PlanKey::new("discount_plan")?;
        let offers = Arc::new(CountingOfferStore::new(immediate_offer(
            plan.clone(),
            ChargeAmount::new(5_900, CurrencyCode::new("USD")?)?,
        )));
        let code = SubscriptionDiscountCode::new("SAVE10")?;
        create_subscription_discount_code(
            &database.pool,
            offers.as_ref(),
            &SubscriptionDiscountCodeCreation::new(
                DiscountCodeId::new(Uuid::now_v7()),
                scope,
                plan.clone(),
                code.clone(),
                None,
                SubscriptionDiscountKind::AmountOffCents(PositiveDiscountCents::new(500)?),
                CurrencyCode::new("USD")?,
                SubscriptionDiscountDuration::Indefinite,
            )?,
        )
        .await?;
        offers.calls.store(0, Ordering::SeqCst);
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

        assert!(matches!(
            service
                .claim_discount(SubscriptionDiscountClaim::new(
                    DiscountClaimId::new(Uuid::now_v7()),
                    scope,
                    subscriber,
                    plan.clone(),
                    code.clone(),
                ))
                .await?,
            SubscriptionDiscountClaimOutcome::Saved(_)
        ));
        assert!(matches!(
            service
                .claim_discount(SubscriptionDiscountClaim::new(
                    DiscountClaimId::new(Uuid::now_v7()),
                    scope,
                    subscriber,
                    plan.clone(),
                    code.clone(),
                ))
                .await?,
            SubscriptionDiscountClaimOutcome::Existing(_)
        ));
        let offer_calls_before_clear = offers.calls.load(Ordering::SeqCst);
        assert!(matches!(
            service
                .clear_discount(ClearSubscriptionDiscount::new(
                    scope,
                    subscriber,
                    plan.clone(),
                ))
                .await?,
            SubscriptionDiscountClearOutcome::Cleared(_)
        ));
        assert_eq!(
            offers.calls.load(Ordering::SeqCst),
            offer_calls_before_clear,
            "clear must not lock an offer"
        );
        assert_eq!(
            service
                .clear_discount(ClearSubscriptionDiscount::new(
                    scope,
                    subscriber,
                    plan.clone(),
                ))
                .await?,
            SubscriptionDiscountClearOutcome::NotFound
        );

        assert!(matches!(
            service
                .claim_discount(SubscriptionDiscountClaim::new(
                    DiscountClaimId::new(Uuid::now_v7()),
                    scope,
                    subscriber,
                    plan.clone(),
                    code,
                ))
                .await?,
            SubscriptionDiscountClaimOutcome::Saved(_)
        ));
        insert_pending_initial_attempt(&database.pool, account, subscriber, &plan).await?;
        assert_eq!(
            service
                .clear_discount(ClearSubscriptionDiscount::new(
                    scope,
                    subscriber,
                    plan.clone(),
                ))
                .await?,
            SubscriptionDiscountClearOutcome::BlockedByInitialAttempt
        );
        assert!(
            saved_subscription_discount_claim(&database.pool, scope, subscriber, &plan)
                .await?
                .is_some()
        );
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 0);
        assert_eq!(coordinator.begins.load(Ordering::SeqCst), 0);
        assert_eq!(outbox_count(&database.pool).await?, 0);
        assert_eq!(
            admission
                .commands
                .lock()
                .await
                .iter()
                .map(EndUserMutationCommand::operation)
                .collect::<Vec<_>>(),
            vec![
                EndUserMutationOperation::SubscriptionDiscountClaim,
                EndUserMutationOperation::SubscriptionDiscountClaim,
                EndUserMutationOperation::SubscriptionDiscountClear,
                EndUserMutationOperation::SubscriptionDiscountClear,
                EndUserMutationOperation::SubscriptionDiscountClaim,
                EndUserMutationOperation::SubscriptionDiscountClear,
            ]
        );
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn denied_discount_mutations_leave_claims_untouched_without_host_or_provider_work()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("srv_mut_disc_no").await?;
    let result = async {
        let account = create_gateway_account(&database.pool, "test_gateway").await?;
        let scope = BillingScopeId::new(account.billing_scope_id);
        let subscriber = SubscriberId::new(Uuid::now_v7());
        let plan = PlanKey::new("denied_discount")?;
        let offers = Arc::new(CountingOfferStore::new(immediate_offer(
            plan.clone(),
            ChargeAmount::new(5_900, CurrencyCode::new("USD")?)?,
        )));
        let code = SubscriptionDiscountCode::new("DENY10")?;
        create_subscription_discount_code(
            &database.pool,
            offers.as_ref(),
            &SubscriptionDiscountCodeCreation::new(
                DiscountCodeId::new(Uuid::now_v7()),
                scope,
                plan.clone(),
                code.clone(),
                None,
                SubscriptionDiscountKind::AmountOffCents(PositiveDiscountCents::new(500)?),
                CurrencyCode::new("USD")?,
                SubscriptionDiscountDuration::Indefinite,
            )?,
        )
        .await?;
        let saved = crate::claim_subscription_discount(
            &database.pool,
            offers.as_ref(),
            &SubscriptionDiscountClaim::new(
                DiscountClaimId::new(Uuid::now_v7()),
                scope,
                subscriber,
                plan.clone(),
                code.clone(),
            ),
        )
        .await?;
        assert!(matches!(saved, SubscriptionDiscountClaimOutcome::Saved(_)));
        offers.calls.store(0, Ordering::SeqCst);
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
                .claim_discount(SubscriptionDiscountClaim::new(
                    DiscountClaimId::new(Uuid::now_v7()),
                    scope,
                    subscriber,
                    plan.clone(),
                    code,
                ))
                .await,
            Err(SubscriptionBillingServiceError::AdmissionDenied { .. })
        ));
        assert!(matches!(
            service
                .clear_discount(ClearSubscriptionDiscount::new(
                    scope,
                    subscriber,
                    plan.clone()
                ))
                .await,
            Err(SubscriptionBillingServiceError::AdmissionDenied { .. })
        ));
        assert!(
            saved_subscription_discount_claim(&database.pool, scope, subscriber, &plan)
                .await?
                .is_some()
        );
        assert_eq!(offers.calls.load(Ordering::SeqCst), 0);
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 0);
        assert_eq!(coordinator.begins.load(Ordering::SeqCst), 0);
        assert_eq!(admission.commands.lock().await.len(), 2);
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

async fn install_host_boundary(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::raw_sql(
        r#"
        CREATE TABLE test_billing_subjects (
            billing_scope_id uuid NOT NULL,
            subscriber_id uuid NOT NULL,
            PRIMARY KEY (billing_scope_id, subscriber_id)
        );
        CREATE TABLE test_billing_outbox (
            id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
            event_kind text NOT NULL
        );
        "#,
    )
    .execute(pool)
    .await?;
    Ok(())
}

async fn hold_subscription_aggregate_lock(
    pool: &PgPool,
    subscriber: SubscriberId,
    plan: &PlanKey,
) -> Result<Transaction<'static, Postgres>, sqlx::Error> {
    let mut transaction = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::uuid::text || ':' || $2, 0))")
        .bind(subscriber.as_uuid())
        .bind(plan.as_str())
        .execute(&mut *transaction)
        .await?;
    Ok(transaction)
}

async fn insert_host_subject(
    pool: &PgPool,
    scope: BillingScopeId,
    subscriber: SubscriberId,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO test_billing_subjects (billing_scope_id, subscriber_id) VALUES ($1, $2)",
    )
    .bind(scope.as_uuid())
    .bind(subscriber.as_uuid())
    .execute(pool)
    .await?;
    Ok(())
}

async fn insert_active_subscription(
    pool: &PgPool,
    account: GatewayAccountFixture,
    subscriber: SubscriberId,
    plan: &PlanKey,
) -> Result<ActiveSubscriptionFixture, sqlx::Error> {
    let id = Uuid::now_v7();
    let payment_method_id = Uuid::now_v7();
    let suffix = id.simple();
    let initial_transaction_id = format!("service_initial_{suffix}");
    let period_start = Utc::now() - ChronoDuration::days(1);
    let period_end = period_start + ChronoDuration::days(30);
    sqlx::query(
        r#"
        INSERT INTO billing_payment_methods (
            id, billing_scope_id, subscriber_id, gateway_account_id,
            gateway_payment_method_reference, status
        ) VALUES ($1, $2, $3, $4, $5, 'active')
        "#,
    )
    .bind(payment_method_id)
    .bind(account.billing_scope_id)
    .bind(subscriber.as_uuid())
    .bind(account.gateway_account_id)
    .bind(format!("service_vault_{suffix}"))
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
        INSERT INTO billing_subscriptions (
            id, billing_scope_id, subscriber_id, plan_key, status,
            gateway_account_id, payment_method_id, amount_cents, currency,
            current_period_start_at, current_period_end_at, next_renewal_at,
            initial_transaction_id, phase, recurring_period_kind,
            recurring_period_count, dunning_retry_delays_seconds,
            dunning_exhaustion, past_due_access, next_payment_attempt_at
        ) VALUES (
            $1, $2, $3, $4, 'active', $5, $6, 5900, 'USD', $7, $8, $8, $9,
            'recurring', 'calendar_months', 1, ARRAY[]::bigint[],
            'remain_past_due', 'suspend_immediately', $8
        )
        "#,
    )
    .bind(id)
    .bind(account.billing_scope_id)
    .bind(subscriber.as_uuid())
    .bind(plan.as_str())
    .bind(account.gateway_account_id)
    .bind(payment_method_id)
    .bind(period_start)
    .bind(period_end)
    .bind(&initial_transaction_id)
    .execute(pool)
    .await?;
    Ok(ActiveSubscriptionFixture {
        id,
        payment_method_id,
        period_end,
        initial_transaction_id,
    })
}

async fn insert_blocking_renewal(
    pool: &PgPool,
    account: GatewayAccountFixture,
    subscriber: SubscriberId,
    plan: &PlanKey,
    subscription: &ActiveSubscriptionFixture,
) -> Result<(), sqlx::Error> {
    let attempt_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO billing_payment_attempts (
            id, billing_scope_id, subscriber_id, plan_key, subscription_id,
            payment_method_id, attempt_kind, status, idempotency_key,
            request_fingerprint, amount_cents, currency, billing_period_start_at,
            billing_period_end_at, gateway_account_id, gateway_configuration_id,
            gateway_order_id, subscription_expected_payment_method_id,
            subscription_expected_initial_transaction_id, subscription_expected_status
        ) VALUES (
            $1, $2, $3, $4, $5, $6, 'subscription_renewal', 'pending',
            $7, $8, 5900, 'USD', $9, $10, $11, $12, $13, $6, $14, 'active'
        )
        "#,
    )
    .bind(attempt_id)
    .bind(account.billing_scope_id)
    .bind(subscriber.as_uuid())
    .bind(plan.as_str())
    .bind(subscription.id)
    .bind(subscription.payment_method_id)
    .bind(format!("service_renewal_{attempt_id}"))
    .bind(format!("service_renewal_fingerprint_{attempt_id}"))
    .bind(subscription.period_end)
    .bind(subscription.period_end + ChronoDuration::days(30))
    .bind(account.gateway_account_id)
    .bind(account.gateway_configuration_id)
    .bind(format!("service_renewal_order_{attempt_id}"))
    .bind(&subscription.initial_transaction_id)
    .execute(pool)
    .await?;
    Ok(())
}

async fn insert_stale_payment_method_update(
    pool: &PgPool,
    account: GatewayAccountFixture,
    subscriber: SubscriberId,
    plan: &PlanKey,
    subscription: &ActiveSubscriptionFixture,
) -> Result<Uuid, sqlx::Error> {
    let attempt_id = Uuid::now_v7();
    let created_at = Utc::now() - ChronoDuration::minutes(4);
    sqlx::query(
        r#"
        INSERT INTO billing_payment_attempts (
            id, billing_scope_id, subscriber_id, plan_key, subscription_id,
            payment_method_id, attempt_kind, status, idempotency_key,
            request_fingerprint, amount_cents, currency, gateway_account_id,
            gateway_configuration_id, gateway_order_id,
            payment_method_update_expected_payment_method_id,
            payment_method_update_expected_initial_transaction_id,
            created_at, updated_at
        ) VALUES (
            $1, $2, $3, $4, $5, $6,
            'subscription_payment_method_update', 'pending', $7, $8, 0, 'USD',
            $9, $10, $11, $6, $12, $13, $13
        )
        "#,
    )
    .bind(attempt_id)
    .bind(account.billing_scope_id)
    .bind(subscriber.as_uuid())
    .bind(plan.as_str())
    .bind(subscription.id)
    .bind(subscription.payment_method_id)
    .bind(format!("service_update_{attempt_id}"))
    .bind(format!("service_update_fingerprint_{attempt_id}"))
    .bind(account.gateway_account_id)
    .bind(account.gateway_configuration_id)
    .bind(format!("service_update_order_{attempt_id}"))
    .bind(&subscription.initial_transaction_id)
    .bind(created_at)
    .execute(pool)
    .await?;
    Ok(attempt_id)
}

async fn insert_pending_initial_attempt(
    pool: &PgPool,
    account: GatewayAccountFixture,
    subscriber: SubscriberId,
    plan: &PlanKey,
) -> Result<(), sqlx::Error> {
    let attempt_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO billing_payment_attempts (
            id, billing_scope_id, subscriber_id, plan_key, attempt_kind,
            status, idempotency_key, request_fingerprint, amount_cents,
            currency, gateway_account_id, gateway_configuration_id,
            gateway_order_id, subscription_initial_terms_version,
            subscription_initial_start_kind,
            subscription_initial_recurring_base_amount_cents,
            subscription_initial_recurring_period_kind,
            subscription_initial_recurring_period_count,
            subscription_initial_dunning_retry_delays_seconds,
            subscription_initial_dunning_exhaustion,
            subscription_initial_past_due_access
        ) VALUES (
            $1, $2, $3, $4, 'subscription_initial', 'pending', $5, $6,
            100, 'USD', $7, $8, $9, 2, 'recurring_immediately', 100,
            'calendar_months', 1, ARRAY[]::bigint[],
            'remain_past_due', 'suspend_immediately'
        )
        "#,
    )
    .bind(attempt_id)
    .bind(account.billing_scope_id)
    .bind(subscriber.as_uuid())
    .bind(plan.as_str())
    .bind(format!("service_discount_{attempt_id}"))
    .bind(format!("service_discount_fingerprint_{attempt_id}"))
    .bind(account.gateway_account_id)
    .bind(account.gateway_configuration_id)
    .bind(format!("service_discount_order_{attempt_id}"))
    .execute(pool)
    .await?;
    Ok(())
}

async fn install_cancellation_failure_trigger(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::raw_sql(
        r#"
        CREATE FUNCTION test_reject_subscription_cancellation()
        RETURNS trigger
        LANGUAGE plpgsql
        AS $$
        BEGIN
            RAISE EXCEPTION 'injected cancellation mutation failure';
        END;
        $$;
        CREATE TRIGGER test_reject_subscription_cancellation
        BEFORE UPDATE OF status ON billing_subscriptions
        FOR EACH ROW
        EXECUTE FUNCTION test_reject_subscription_cancellation();
        "#,
    )
    .execute(pool)
    .await?;
    Ok(())
}

async fn subscription_status(pool: &PgPool, subscription_id: Uuid) -> Result<String, sqlx::Error> {
    sqlx::query_scalar("SELECT status FROM billing_subscriptions WHERE id = $1")
        .bind(subscription_id)
        .fetch_one(pool)
        .await
}

async fn payment_attempt_status(pool: &PgPool, attempt_id: Uuid) -> Result<String, sqlx::Error> {
    sqlx::query_scalar("SELECT status FROM billing_payment_attempts WHERE id = $1")
        .bind(attempt_id)
        .fetch_one(pool)
        .await
}

async fn outbox_count(pool: &PgPool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT count(*) FROM test_billing_outbox")
        .fetch_one(pool)
        .await
}
