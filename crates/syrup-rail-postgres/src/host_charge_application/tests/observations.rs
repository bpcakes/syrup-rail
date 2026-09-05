use super::*;

#[tokio::test]
async fn later_decline_does_not_manufacture_a_charge_from_accumulated_risk()
-> Result<(), Box<dyn Error>> {
    assert_decline_preserves_actual_charge_observations(false).await
}

#[tokio::test]
async fn later_decline_does_not_rewrite_an_existing_charge_observation()
-> Result<(), Box<dyn Error>> {
    assert_decline_preserves_actual_charge_observations(true).await
}

async fn assert_decline_preserves_actual_charge_observations(
    identified: bool,
) -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("host_obs_risk").await?;
    let result = async {
        sqlx::query("CREATE TABLE host_charge_targets (
            id uuid PRIMARY KEY, billing_scope_id uuid NOT NULL, subscriber_id uuid NOT NULL,
            status text NOT NULL, amount_cents integer NOT NULL, currency text NOT NULL,
            paid_at timestamptz)").execute(&database.pool).await?;
        let account = create_gateway_account(&database.pool, "nmi").await?;
        let subscriber = Uuid::now_v7();
        let target = Uuid::now_v7();
        sqlx::query("INSERT INTO host_charge_targets VALUES ($1, $2, $3, 'pending', 1250, 'USD', NULL)")
            .bind(target).bind(account.billing_scope_id).bind(subscriber)
            .execute(&database.pool).await?;
        let transaction_id = GatewayTransactionId::new("txn_host_observation")?;
        let gateway = Arc::new(ScriptedGateway {
            account_mode: GatewayAccountMode::Test,
            sale_calls: AtomicUsize::new(0),
            outcome: Mutex::new(Some(GatewayPaymentOutcome::new(
                GatewayPaymentStatus::Unknown,
                ProcessorEvidence::new(
                    syrup_rail::ProcessorApprovalEvidence::Structured,
                    identified.then(|| transaction_id.clone()), None,
                    Some(GatewayDiagnostic::new("1")), Some(GatewayDiagnostic::new("100")),
                    Some(GatewayDiagnostic::new("Ambiguous approval")), None,
                    GatewayPaymentDescriptor::default(),
                ),
            ))),
        });
        let coordinator = Arc::new(TestCoordinator {
            pool: database.pool.clone(), events: Arc::new(Mutex::new(Vec::new())),
        });
        let service = SubscriptionBillingService::new(
            database.pool.clone(), Arc::new(UnusedOffers),
            Arc::new(StaticResolver {
                gateway: resolved_gateway(account, gateway.clone()), calls: AtomicUsize::new(0),
            }),
            Arc::new(PermitAdmission { calls: AtomicUsize::new(0) }), coordinator.clone(),
        ).with_required_gateway_account_mode(GatewayAccountMode::Test)
            .with_host_charge_targets(Arc::new(TestTargets));
        let scope = syrup_rail::BillingScopeId::new(account.billing_scope_id);
        let first = service.charge_host_target(ChargeHostTarget::new(
            scope, syrup_rail::SubscriberId::new(subscriber), HostChargeTargetId::new(target),
            GatewayConfigurationId::new(account.gateway_configuration_id),
            PaymentToken::new("tok_host_observation")?, IdempotencyKey::new("host-observation")?, None,
        )).await?;
        assert_eq!(first.status(), PaymentAttemptStatus::Unknown);
        let attempt_id = first.attempt().identity().attempt_id();
        let charge_rows = || sqlx::query_as::<_, (String, Option<String>, Option<String>, String)>(
            "SELECT gateway_approval_evidence, gateway_transaction_id, gateway_response_code, progression_state
             FROM billing_processor_charges WHERE attempt_id = $1 ORDER BY id",
        ).bind(attempt_id.as_uuid()).fetch_all(&database.pool);
        let before = charge_rows().await?;
        assert_eq!(before.len(), usize::from(identified));
        let decline = GatewayPaymentOutcome::new(
            GatewayPaymentStatus::Declined,
            ProcessorEvidence::new(
                syrup_rail::ProcessorApprovalEvidence::Absent, Some(transaction_id), None,
                Some(GatewayDiagnostic::new("2")), Some(GatewayDiagnostic::new("200")),
                Some(GatewayDiagnostic::new("Declined")), None,
                GatewayPaymentDescriptor::default(),
            ),
        );
        let after = apply_reconciled_host_charge_gateway_outcome(
            &database.pool, coordinator.as_ref(), &TestTargets, scope, attempt_id, &decline,
        ).await?;
        assert_eq!(after.status(), PaymentAttemptStatus::Declined);
        assert_eq!(after.attempt().state().processor_evidence().approval_evidence(),
            syrup_rail::ProcessorApprovalEvidence::Structured);
        assert_eq!(charge_rows().await?, before, "attempt risk must not manufacture or rewrite observations");
        let mut transaction = database.pool.begin().await?;
        let admission = host_charge_ledger_admission(&mut transaction,
            &HostChargeLedgerAdmissionQuery::new(scope, syrup_rail::SubscriberId::new(subscriber),
                HostChargeTargetId::new(target), HostChargeLedgerAdmissionMode::Release),
        ).await?;
        assert_eq!(admission, HostChargeLedgerAdmission::Unsafe);
        transaction.rollback().await?;
        assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 1);
        Ok::<(), Box<dyn Error>>(())
    }.await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}
