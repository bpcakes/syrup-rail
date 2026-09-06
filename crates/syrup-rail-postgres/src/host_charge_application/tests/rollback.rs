use super::*;

#[derive(Clone)]
struct RollbackRecordingCoordinator {
    pool: PgPool,
    rollback_calls: Arc<AtomicUsize>,
}

#[async_trait]
impl BillingTransactionCoordinator for RollbackRecordingCoordinator {
    async fn begin(
        &self,
        _subject: BillingEventSubject,
        _lock_timeout: Duration,
    ) -> Result<Box<dyn BillingTransaction>, BillingTransactionError> {
        Ok(Box::new(RollbackRecordingTransaction {
            transaction: Some(
                self.pool
                    .begin()
                    .await
                    .map_err(BillingTransactionError::new)?,
            ),
            rollback_calls: Arc::clone(&self.rollback_calls),
        }))
    }
}

struct RollbackRecordingTransaction {
    transaction: Option<Transaction<'static, Postgres>>,
    rollback_calls: Arc<AtomicUsize>,
}

#[async_trait]
impl BillingTransaction for RollbackRecordingTransaction {
    fn connection(&mut self) -> &mut PgConnection {
        &mut *self.transaction.as_mut().expect("active transaction")
    }

    fn subject_state(&self) -> BillingTransactionSubjectState {
        BillingTransactionSubjectState::LiveRecipient
    }

    async fn append_event(&mut self, _event: &BillingEvent) -> Result<(), BillingEventWriteError> {
        panic!("a rejected host charge must not append an event")
    }

    async fn commit(mut self: Box<Self>) -> Result<(), BillingTransactionError> {
        self.transaction
            .take()
            .expect("active transaction")
            .commit()
            .await
            .map_err(BillingTransactionError::new)
    }

    async fn rollback(mut self: Box<Self>) -> Result<(), BillingTransactionError> {
        self.rollback_calls.fetch_add(1, Ordering::SeqCst);
        self.transaction
            .take()
            .expect("active transaction")
            .rollback()
            .await
            .map_err(BillingTransactionError::new)
    }
}

async fn create_host_charge_target_table(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        CREATE TABLE host_charge_targets (
            id uuid PRIMARY KEY,
            billing_scope_id uuid NOT NULL,
            subscriber_id uuid NOT NULL,
            status text NOT NULL,
            amount_cents integer NOT NULL,
            currency text NOT NULL,
            paid_at timestamptz
        )
        "#,
    )
    .execute(pool)
    .await?;
    Ok(())
}

async fn prepare_submitted_host_charge(
    pool: &PgPool,
    account: crate::test_support::GatewayAccountFixture,
    subscriber_id: Uuid,
    target_id: Uuid,
    idempotency_key: &str,
) -> Result<HostChargeReservation, Box<dyn Error>> {
    sqlx::query(
        "INSERT INTO host_charge_targets VALUES ($1, $2, $3, 'pending', 1250, 'USD', NULL)",
    )
    .bind(target_id)
    .bind(account.billing_scope_id)
    .bind(subscriber_id)
    .execute(pool)
    .await?;
    let gateway = resolved_gateway(
        account,
        Arc::new(ScriptedGateway {
            account_mode: GatewayAccountMode::Live,
            sale_calls: AtomicUsize::new(0),
            outcome: Mutex::new(None),
        }),
    );
    let command = ChargeHostTarget::new(
        syrup_rail::BillingScopeId::new(account.billing_scope_id),
        syrup_rail::SubscriberId::new(subscriber_id),
        HostChargeTargetId::new(target_id),
        GatewayConfigurationId::new(account.gateway_configuration_id),
        PaymentToken::new(format!("tok_{}", target_id.simple()))?,
        IdempotencyKey::new(idempotency_key)?,
        None,
    );
    let reservation = HostChargeReservation::from_command(
        &command,
        syrup_rail::HostChargeTargetSnapshot::new(
            HostChargeTargetId::new(target_id),
            ChargeAmount::new(1250, CurrencyCode::new("USD")?)?,
        ),
        &gateway,
        PaymentAttemptId::new(Uuid::now_v7()),
        GatewayAccountMode::Live,
    )?;
    let mut transaction = pool.begin().await?;
    assert!(matches!(
        reserve_host_charge_in_transaction(&mut transaction, &TestTargets, &reservation).await?,
        HostChargeReservationOutcome::Reserved(_)
    ));
    transaction.commit().await?;
    assert!(matches!(
        admit_host_charge_submission(pool, &TestTargets, &reservation).await?,
        HostChargeAdmissionOutcome::Admitted(_)
    ));
    Ok(reservation)
}

async fn record_primary_host_charge(
    pool: &PgPool,
    reservation: &HostChargeReservation,
    transaction_id: &str,
) -> Result<(), Box<dyn Error>> {
    let outcome = approved_outcome(transaction_id);
    let mut transaction = pool.begin().await?;
    let attempt = lock_expected_host_charge(&mut transaction, reservation).await?;
    let ObservedCharge::Owned(charge) = observe_processor_charge(
        &mut transaction,
        &attempt,
        outcome.evidence(),
        ProcessorChargeProgression::Pending,
    )
    .await?
    else {
        panic!("new transaction evidence must belong to its payment attempt");
    };
    assert_eq!(charge.role, ProcessorChargeRole::Primary);
    transaction.commit().await?;
    Ok(())
}

#[tokio::test]
async fn approval_owned_by_another_attempt_explicitly_rolls_back_target_transition()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("rail_host_own_rb").await?;
    let result = async {
        create_host_charge_target_table(&database.pool).await?;
        let account = create_gateway_account(&database.pool, "nmi").await?;
        let rejected_target_id = Uuid::now_v7();
        let rejected = prepare_submitted_host_charge(
            &database.pool,
            account,
            Uuid::now_v7(),
            rejected_target_id,
            "host-owner-rejected",
        )
        .await?;
        let owner = prepare_submitted_host_charge(
            &database.pool,
            account,
            Uuid::now_v7(),
            Uuid::now_v7(),
            "host-owner-existing",
        )
        .await?;
        record_primary_host_charge(&database.pool, &owner, "host_txn_owned_elsewhere").await?;

        let rollback_calls = Arc::new(AtomicUsize::new(0));
        let coordinator = RollbackRecordingCoordinator {
            pool: database.pool.clone(),
            rollback_calls: Arc::clone(&rollback_calls),
        };
        let outcome = approved_outcome("host_txn_owned_elsewhere");
        let evidence = outcome
            .approved_evidence()
            .expect("approved outcome has approved evidence");
        let error = apply_host_charge_approved(&coordinator, &TestTargets, &rejected, &evidence)
            .await
            .expect_err("another attempt's transaction must be rejected");
        assert!(matches!(
            error,
            HostChargeApplicationError::InvalidState(
                "the approved gateway transaction belongs to another payment attempt"
            )
        ));
        assert_eq!(rollback_calls.load(Ordering::SeqCst), 1);
        let target_status: String =
            sqlx::query_scalar("SELECT status FROM host_charge_targets WHERE id = $1")
                .bind(rejected_target_id)
                .fetch_one(&database.pool)
                .await?;
        assert_eq!(target_status, "pending");
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup?;
    Ok(())
}

#[tokio::test]
async fn additional_approval_explicitly_rolls_back_target_and_charge() -> Result<(), Box<dyn Error>>
{
    let database = TestDatabase::start("rail_host_add_rb").await?;
    let result = async {
        create_host_charge_target_table(&database.pool).await?;
        let account = create_gateway_account(&database.pool, "nmi").await?;
        let target_id = Uuid::now_v7();
        let reservation = prepare_submitted_host_charge(
            &database.pool,
            account,
            Uuid::now_v7(),
            target_id,
            "host-additional-rejected",
        )
        .await?;
        record_primary_host_charge(&database.pool, &reservation, "host_txn_primary").await?;

        let rollback_calls = Arc::new(AtomicUsize::new(0));
        let coordinator = RollbackRecordingCoordinator {
            pool: database.pool.clone(),
            rollback_calls: Arc::clone(&rollback_calls),
        };
        let outcome = approved_outcome("host_txn_additional");
        let evidence = outcome
            .approved_evidence()
            .expect("approved outcome has approved evidence");
        let error = apply_host_charge_approved(&coordinator, &TestTargets, &reservation, &evidence)
            .await
            .expect_err("an additional approved charge must be rejected");
        assert!(matches!(
            error,
            HostChargeApplicationError::InvalidState(
                "an additional approved host charge requires external reversal"
            )
        ));
        assert_eq!(rollback_calls.load(Ordering::SeqCst), 1);
        let target_status: String =
            sqlx::query_scalar("SELECT status FROM host_charge_targets WHERE id = $1")
                .bind(target_id)
                .fetch_one(&database.pool)
                .await?;
        assert_eq!(target_status, "pending");
        let charge_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM billing_processor_charges WHERE attempt_id = $1",
        )
        .bind(reservation.identity().attempt_id().as_uuid())
        .fetch_one(&database.pool)
        .await?;
        assert_eq!(charge_count, 1);
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup?;
    Ok(())
}
