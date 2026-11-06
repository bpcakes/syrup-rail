use super::*;

#[tokio::test]
async fn parked_host_approval_recovers_without_another_sale() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("rail_host_resume").await?;
    let result = async {
        sqlx::raw_sql(
            r#"
            CREATE TABLE host_charge_targets (
                id uuid PRIMARY KEY, billing_scope_id uuid NOT NULL,
                subscriber_id uuid NOT NULL, status text NOT NULL,
                amount_cents integer NOT NULL, currency text NOT NULL,
                paid_at timestamptz
            );
            CREATE SEQUENCE application_failures;
            CREATE FUNCTION fail_first_applications() RETURNS trigger LANGUAGE plpgsql AS $$
            BEGIN
                IF NEW.status = 'paid' AND nextval('application_failures') <= 3 THEN
                    RAISE EXCEPTION 'transient application failure' USING ERRCODE = '40001';
                END IF;
                RETURN NEW;
            END;
            $$;
            CREATE TRIGGER application_failure BEFORE UPDATE ON host_charge_targets
                FOR EACH ROW EXECUTE FUNCTION fail_first_applications();
            "#,
        )
        .execute(&database.pool)
        .await?;
        let account = create_gateway_account(&database.pool, "nmi").await?;
        let subscriber_id = Uuid::now_v7();
        let target_id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO host_charge_targets VALUES ($1, $2, $3, 'pending', 1250, 'USD', NULL)",
        )
        .bind(target_id)
        .bind(account.billing_scope_id)
        .bind(subscriber_id)
        .execute(&database.pool)
        .await?;
        let approval = approved_outcome("host_recoverable_approval");
        let gateway = Arc::new(ScriptedGateway {
            account_mode: GatewayAccountMode::Test,
            sale_calls: AtomicUsize::new(0),
            outcome: Mutex::new(Some(approval.clone())),
        });
        let events = Arc::new(Mutex::new(Vec::new()));
        let coordinator = Arc::new(TestCoordinator {
            pool: database.pool.clone(),
            events: Arc::clone(&events),
        });
        let service = SubscriptionBillingService::new(
            database.pool.clone(),
            Arc::new(UnusedOffers),
            Arc::new(StaticResolver {
                gateway: resolved_gateway(account, gateway.clone()),
                calls: AtomicUsize::new(0),
            }),
            Arc::new(PermitAdmission {
                calls: AtomicUsize::new(0),
            }),
            coordinator.clone(),
        )
        .with_required_gateway_account_mode(GatewayAccountMode::Test)
        .with_host_charge_targets(Arc::new(TestTargets));
        let parked = service
            .charge_host_target(ChargeHostTarget::new(
                syrup_rail::BillingScopeId::new(account.billing_scope_id),
                syrup_rail::SubscriberId::new(subscriber_id),
                HostChargeTargetId::new(target_id),
                GatewayConfigurationId::new(account.gateway_configuration_id),
                PaymentToken::new("host_recovery_token")?,
                IdempotencyKey::new("host_recovery")?,
                None,
            ))
            .await?;
        assert_eq!(parked.status(), PaymentAttemptStatus::ReviewRequired);
        let attempt_id = parked.attempt().identity().attempt_id();
        let state: (String, String) = sqlx::query_as(
            "SELECT targets.status, charges.progression_state FROM host_charge_targets targets \
             JOIN billing_processor_charges charges ON charges.host_charge_target_id = targets.id \
             WHERE charges.attempt_id = $1",
        )
        .bind(attempt_id.as_uuid())
        .fetch_one(&database.pool)
        .await?;
        assert_eq!(
            state,
            ("pending".into(), "external_reversal_required".into())
        );
        assert!(events.lock().await.is_empty());
        assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 1);
        let failures: i64 = sqlx::query_scalar("SELECT last_value FROM application_failures")
            .fetch_one(&database.pool)
            .await?;
        assert_eq!(failures, APPROVED_APPLICATION_ATTEMPTS as i64);

        // Storage recovers; reconciliation uses the same approval, never a sale.
        for _ in 0..2 {
            let applied = apply_reconciled_host_charge_gateway_outcome(
                &database.pool,
                coordinator.as_ref(),
                &TestTargets,
                syrup_rail::BillingScopeId::new(account.billing_scope_id),
                attempt_id,
                &approval,
            )
            .await?;
            assert_eq!(applied.status(), PaymentAttemptStatus::Approved);
        }
        let state: (String, String, String, bool) = sqlx::query_as(
            "SELECT targets.status, attempts.status, charges.progression_state, \
                    targets.paid_at IS NOT NULL AND charges.applied_at IS NOT NULL \
             FROM billing_payment_attempts attempts \
             JOIN host_charge_targets targets ON targets.id = attempts.host_charge_target_id \
             JOIN billing_processor_charges charges ON charges.attempt_id = attempts.id \
             WHERE attempts.id = $1",
        )
        .bind(attempt_id.as_uuid())
        .fetch_one(&database.pool)
        .await?;
        assert_eq!(
            state,
            ("paid".into(), "approved".into(), "applied".into(), true)
        );
        assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 1);
        let events = events.lock().await;
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].semantic_key(),
            BillingEventKey::HostChargePaid(HostChargeTargetId::new(target_id))
        );
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}
