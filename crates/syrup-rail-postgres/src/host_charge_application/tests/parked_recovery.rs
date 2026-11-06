use super::*;

#[tokio::test]
async fn parked_approval_recovers_once_without_another_sale() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("host_park_retry").await?;
    let result = async {
        sqlx::query("CREATE TABLE host_charge_targets (id uuid PRIMARY KEY, billing_scope_id uuid NOT NULL, subscriber_id uuid NOT NULL, status text NOT NULL, amount_cents integer NOT NULL, currency text NOT NULL, paid_at timestamptz)")
            .execute(&database.pool).await?;
        let account = create_gateway_account(&database.pool, "nmi").await?;
        let subscriber = Uuid::now_v7();
        let target = Uuid::now_v7();
        sqlx::query("INSERT INTO host_charge_targets VALUES ($1, $2, $3, 'pending', 1250, 'USD', NULL)")
            .bind(target).bind(account.billing_scope_id).bind(subscriber).execute(&database.pool).await?;
        let gateway = Arc::new(ScriptedGateway {
            account_mode: GatewayAccountMode::Test,
            sale_calls: AtomicUsize::new(0),
            outcome: Mutex::new(None),
        });
        let resolved = resolved_gateway(account, gateway.clone());
        let command = ChargeHostTarget::new(
            syrup_rail::BillingScopeId::new(account.billing_scope_id),
            syrup_rail::SubscriberId::new(subscriber), HostChargeTargetId::new(target),
            GatewayConfigurationId::new(account.gateway_configuration_id),
            PaymentToken::new("parked-token")?, IdempotencyKey::new("parked-recovery")?, None,
        );
        let attempt_id = PaymentAttemptId::new(Uuid::now_v7());
        let reservation = HostChargeReservation::from_command(
            &command, syrup_rail::HostChargeTargetSnapshot::new(
                HostChargeTargetId::new(target), ChargeAmount::new(1250, CurrencyCode::new("USD")?)?,
            ), &resolved, attempt_id, GatewayAccountMode::Test,
        )?;
        let mut transaction = database.pool.begin().await?;
        assert!(matches!(reserve_host_charge_in_transaction(&mut transaction, &TestTargets, &reservation).await?, HostChargeReservationOutcome::Reserved(_)));
        transaction.commit().await?;
        assert!(matches!(admit_host_charge_submission(&database.pool, &TestTargets, &reservation).await?, HostChargeAdmissionOutcome::Admitted(_)));
        let outcome = approved_outcome("parked-recovery-txn");
        let parked = park_host_charge_approved(&database.pool, &reservation, outcome.evidence()).await?;
        assert_eq!(parked.status(), PaymentAttemptStatus::ReviewRequired);
        let state: String = sqlx::query_scalar("SELECT progression_state FROM billing_processor_charges WHERE attempt_id = $1")
            .bind(attempt_id.as_uuid()).fetch_one(&database.pool).await?;
        assert_eq!(state, "external_reversal_required");
        let events = Arc::new(Mutex::new(Vec::new()));
        let coordinator = TestCoordinator { pool: database.pool.clone(), events: events.clone() };
        for _ in 0..2 {
            let recovered = apply_reconciled_host_charge_gateway_outcome(
                &database.pool, &coordinator, &TestTargets, command.billing_scope_id(), attempt_id, &outcome,
            ).await?;
            assert_eq!(recovered.status(), PaymentAttemptStatus::Approved);
        }
        let state: (String, String) = sqlx::query_as("SELECT c.progression_state, t.status FROM billing_processor_charges c JOIN host_charge_targets t ON t.id = c.host_charge_target_id WHERE c.attempt_id = $1")
            .bind(attempt_id.as_uuid()).fetch_one(&database.pool).await?;
        assert_eq!(state, ("applied".to_owned(), "paid".to_owned()));
        assert_eq!(events.lock().await.iter().filter(|e| matches!(e, BillingEvent::HostChargePaid { .. })).count(), 1);
        assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 0);
        Ok::<_, Box<dyn Error>>(())
    }.await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}
