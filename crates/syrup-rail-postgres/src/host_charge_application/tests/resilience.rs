use super::*;

#[tokio::test]
async fn unreserved_rate_limit_avoids_attempt_and_extends_provider_cooldown()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("rail_host_rate").await?;
    let result = async {
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

        let gateway = Arc::new(RateLimitedAfterReservationGateway {
            readiness_calls: AtomicUsize::new(0),
            sale_calls: AtomicUsize::new(0),
        });
        let resolver = Arc::new(StaticResolver {
            gateway: resolved_gateway(account, gateway.clone()),
            calls: AtomicUsize::new(0),
        });
        let admission = Arc::new(PermitAdmission {
            calls: AtomicUsize::new(0),
        });
        let service = SubscriptionBillingService::new(
            database.pool.clone(),
            Arc::new(UnusedOffers),
            resolver,
            admission.clone(),
            Arc::new(TestCoordinator {
                pool: database.pool.clone(),
                events: Arc::new(Mutex::new(Vec::new())),
            }),
        )
        .with_host_charge_targets(Arc::new(TestTargets));
        let command = ChargeHostTarget::new(
            syrup_rail::BillingScopeId::new(account.billing_scope_id),
            syrup_rail::SubscriberId::new(subscriber_id),
            HostChargeTargetId::new(target_id),
            GatewayConfigurationId::new(account.gateway_configuration_id),
            PaymentToken::new("tok_host_throttled")?,
            IdempotencyKey::new("host-throttled")?,
            None,
        );

        let error = service
            .charge_host_target(command)
            .await
            .expect_err("provider throttle must be reported");
        assert!(matches!(
            error,
            crate::SubscriptionBillingServiceError::GatewayMutationCooldown {
                scope: crate::GatewayMutationCooldownScope::Provider
            }
        ));
        assert_eq!(gateway.readiness_calls.load(Ordering::SeqCst), 1);
        assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 0);

        let attempt_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM billing_payment_attempts \
             WHERE billing_scope_id = $1 AND subscriber_id = $2",
        )
        .bind(account.billing_scope_id)
        .bind(subscriber_id)
        .fetch_one(&database.pool)
        .await?;
        assert_eq!(attempt_count, 0);
        assert_eq!(admission.calls.load(Ordering::SeqCst), 0);
        let cooldown_is_active: bool = sqlx::query_scalar(
            "SELECT rate_limited_until > clock_timestamp() FROM billing_gateway_provider_rate_limits WHERE provider_key = 'nmi'",
        )
        .fetch_one(&database.pool)
        .await?;
        assert!(cooldown_is_active);
        let target_status: String =
            sqlx::query_scalar("SELECT status FROM host_charge_targets WHERE id = $1")
                .bind(target_id)
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
async fn terminal_approval_race_does_not_mark_host_target_paid() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("rail_host_race").await?;
    let result = async {
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

        let gateway = Arc::new(TerminalRaceGateway {
            pool: database.pool.clone(),
            sale_calls: AtomicUsize::new(0),
        });
        let resolver = Arc::new(StaticResolver {
            gateway: resolved_gateway(account, gateway.clone()),
            calls: AtomicUsize::new(0),
        });
        let events = Arc::new(Mutex::new(Vec::new()));
        let service = SubscriptionBillingService::new(
            database.pool.clone(),
            Arc::new(UnusedOffers),
            resolver,
            Arc::new(PermitAdmission {
                calls: AtomicUsize::new(0),
            }),
            Arc::new(TestCoordinator {
                pool: database.pool.clone(),
                events: Arc::clone(&events),
            }),
        )
        .with_host_charge_targets(Arc::new(TestTargets));
        let payment = service
            .charge_host_target(ChargeHostTarget::new(
                syrup_rail::BillingScopeId::new(account.billing_scope_id),
                syrup_rail::SubscriberId::new(subscriber_id),
                HostChargeTargetId::new(target_id),
                GatewayConfigurationId::new(account.gateway_configuration_id),
                PaymentToken::new("tok_host_race")?,
                IdempotencyKey::new("host-race")?,
                None,
            ))
            .await?;

        assert_eq!(payment.status(), PaymentAttemptStatus::Unknown);
        assert_eq!(payment.attempt().status(), PaymentAttemptStatus::Failed);
        assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 1);
        assert!(events.lock().await.is_empty());
        let target_status: String =
            sqlx::query_scalar("SELECT status FROM host_charge_targets WHERE id = $1")
                .bind(target_id)
                .fetch_one(&database.pool)
                .await?;
        assert_eq!(target_status, "pending");
        let charge_state: String = sqlx::query_scalar(
            "SELECT progression_state FROM billing_processor_charges WHERE attempt_id = $1",
        )
        .bind(payment.attempt().identity().attempt_id().as_uuid())
        .fetch_one(&database.pool)
        .await?;
        assert_eq!(charge_state, "pending");
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup?;
    Ok(())
}

#[tokio::test]
async fn matching_retry_resumes_prepared_attempt_without_readiness_loser_overwrite()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("rail_host_resume").await?;
    let result = async {
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

        let (readiness_started_tx, readiness_started_rx) = oneshot::channel();
        let (sale_started_tx, sale_started_rx) = oneshot::channel();
        let gateway = Arc::new(RacingPreparedRetryGateway {
            readiness_calls: AtomicUsize::new(0),
            sale_calls: AtomicUsize::new(0),
            blocked_readiness_started: Mutex::new(Some(readiness_started_tx)),
            sale_started: Mutex::new(Some(sale_started_tx)),
            release_readiness: Notify::new(),
            release_sale: Notify::new(),
        });
        let command = ChargeHostTarget::new(
            syrup_rail::BillingScopeId::new(account.billing_scope_id),
            syrup_rail::SubscriberId::new(subscriber_id),
            HostChargeTargetId::new(target_id),
            GatewayConfigurationId::new(account.gateway_configuration_id),
            PaymentToken::new("tok_host_resume")?,
            IdempotencyKey::new("host-resume")?,
            None,
        );
        let resolved = resolved_gateway(account, gateway.clone());
        let reservation = HostChargeReservation::from_command(
            &command,
            syrup_rail::HostChargeTargetSnapshot::new(
                HostChargeTargetId::new(target_id),
                ChargeAmount::new(1250, CurrencyCode::new("USD")?)?,
            ),
            &resolved,
            PaymentAttemptId::new(Uuid::now_v7()),
            GatewayAccountMode::Live,
        )?;
        let mut transaction = database.pool.begin().await?;
        assert!(matches!(
            reserve_host_charge_in_transaction(&mut transaction, &TestTargets, &reservation,)
                .await?,
            HostChargeReservationOutcome::Reserved(_)
        ));
        transaction.commit().await?;
        let service = SubscriptionBillingService::new(
            database.pool.clone(),
            Arc::new(UnusedOffers),
            Arc::new(StaticResolver {
                gateway: resolved,
                calls: AtomicUsize::new(0),
            }),
            Arc::new(PermitAdmission {
                calls: AtomicUsize::new(0),
            }),
            Arc::new(TestCoordinator {
                pool: database.pool.clone(),
                events: Arc::new(Mutex::new(Vec::new())),
            }),
        )
        .with_host_charge_targets(Arc::new(TestTargets));

        let first_service = service.clone();
        let first_command = command.clone();
        let first =
            tokio::spawn(async move { first_service.charge_host_target(first_command).await });
        readiness_started_rx.await?;
        let retry = ChargeHostTarget::new(
            command.billing_scope_id(),
            command.subscriber_id(),
            command.target_id(),
            command.gateway_configuration_id(),
            PaymentToken::new("tok_host_resume_retry")?,
            command.idempotency_key().clone(),
            command.billing_contact().cloned(),
        );
        let second_service = service.clone();
        let second = tokio::spawn(async move { second_service.charge_host_target(retry).await });
        sale_started_rx.await?;
        gateway.release_readiness.notify_one();
        let first = first.await??;
        assert_eq!(first.status(), PaymentAttemptStatus::Pending);
        assert!(
            first
                .attempt()
                .state()
                .timestamps()
                .submitted_at()
                .is_some()
        );
        gateway.release_sale.notify_one();
        let second = second.await??;
        assert_eq!(second.status(), PaymentAttemptStatus::Approved);
        assert_eq!(first.attempt().identity(), second.attempt().identity());
        assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 1);
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup?;
    Ok(())
}
