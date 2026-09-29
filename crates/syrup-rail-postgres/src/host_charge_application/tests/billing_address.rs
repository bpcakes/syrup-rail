use super::*;
use crate::{
    SubscriptionBillingServiceError,
    test_support::{assert_attempt_billing_address, billing_address},
};

#[tokio::test]
async fn host_charge_persists_billing_address_and_rejects_changed_address_replay()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("address_host_charge").await?;
    let result = async {
        sqlx::query(
            "CREATE TABLE host_charge_targets (id uuid PRIMARY KEY, \
             billing_scope_id uuid NOT NULL, subscriber_id uuid NOT NULL, \
             status text NOT NULL, amount_cents integer NOT NULL, currency text NOT NULL, \
             paid_at timestamptz)",
        )
        .execute(&database.pool)
        .await?;
        let account = create_gateway_account(&database.pool, "nmi").await?;
        let subscriber_id = Uuid::now_v7();
        let target_id = HostChargeTargetId::new(Uuid::now_v7());
        sqlx::query(
            "INSERT INTO host_charge_targets VALUES ($1, $2, $3, 'pending', 1250, 'USD', NULL)",
        )
        .bind(target_id.as_uuid())
        .bind(account.billing_scope_id)
        .bind(subscriber_id)
        .execute(&database.pool)
        .await?;

        let home = billing_address("20 Host St");
        let command = |address| {
            ChargeHostTarget::new(
                syrup_rail::BillingScopeId::new(account.billing_scope_id),
                syrup_rail::SubscriberId::new(subscriber_id),
                target_id,
                GatewayConfigurationId::new(account.gateway_configuration_id),
                PaymentToken::new("opaque-host-address").unwrap(),
                IdempotencyKey::new("host-address").unwrap(),
                Some(BillingContact::from_address(address)),
            )
        };
        let original = command(home.clone());
        let gateway = Arc::new(ScriptedGateway {
            account_mode: GatewayAccountMode::Live,
            sale_calls: AtomicUsize::new(0),
            outcome: Mutex::new(Some(approved_outcome("txn_host_address"))),
        });
        let resolver = Arc::new(StaticResolver {
            gateway: resolved_gateway(account, gateway.clone()),
            calls: AtomicUsize::new(0),
        });
        let service = SubscriptionBillingService::new(
            database.pool.clone(),
            Arc::new(UnusedOffers),
            resolver.clone(),
            Arc::new(PermitAdmission {
                calls: AtomicUsize::new(0),
            }),
            Arc::new(TestCoordinator {
                pool: database.pool.clone(),
                events: Arc::new(Mutex::new(Vec::new())),
            }),
        )
        .with_host_charge_targets(Arc::new(TestTargets));
        let attempt_id = PaymentAttemptId::new(Uuid::now_v7());
        let reservation = HostChargeReservation::from_command(
            &original,
            syrup_rail::HostChargeTargetSnapshot::new(
                target_id,
                ChargeAmount::new(1250, CurrencyCode::new("USD")?)?,
            ),
            &resolver.gateway,
            attempt_id,
            GatewayAccountMode::Live,
        )?;
        let mut transaction = database.pool.begin().await?;
        let reserved =
            match reserve_host_charge_in_transaction(&mut transaction, &TestTargets, &reservation)
                .await?
            {
                HostChargeReservationOutcome::Reserved(attempt) => attempt,
                other => panic!("expected addressed host reservation, got {other:?}"),
            };
        transaction.commit().await?;
        let loaded = assert_attempt_billing_address(
            &database.pool,
            original.billing_scope_id(),
            attempt_id,
            &home,
        )
        .await?;
        assert_eq!(loaded, reserved);
        assert!(loaded.state().timestamps().submitted_at().is_none());

        let mut transaction = database.pool.begin().await?;
        let replay =
            reserve_host_charge_in_transaction(&mut transaction, &TestTargets, &reservation)
                .await?;
        transaction.commit().await?;
        assert!(
            matches!(replay, HostChargeReservationOutcome::Replay(attempt)
            if attempt == loaded)
        );

        let changed = command(home.clone().with_city(Some("Cambridge".to_owned()))?);
        assert!(matches!(
            service.charge_host_target(changed).await,
            Err(SubscriptionBillingServiceError::IdempotencyConflict)
        ));
        assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 0);
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            assert_attempt_billing_address(
                &database.pool,
                original.billing_scope_id(),
                attempt_id,
                &home,
            )
            .await?,
            loaded
        );

        let applied = service.charge_host_target(original.clone()).await?;
        assert_eq!(applied.attempt().identity().attempt_id(), attempt_id);
        assert_eq!(applied.status(), PaymentAttemptStatus::Approved);
        assert_eq!(
            applied.attempt().request().billing_contact().address(),
            Some(&home)
        );
        assert_eq!(
            service.charge_host_target(original).await?.attempt(),
            applied.attempt()
        );
        assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 1);
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    database.cleanup().await?;
    result
}
