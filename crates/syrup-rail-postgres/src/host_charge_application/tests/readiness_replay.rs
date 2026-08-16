use super::*;
use crate::{GatewayMutationCooldownScope, SubscriptionBillingServiceError};

struct ConfigurationReadinessGateway {
    readiness_calls: AtomicUsize,
    sale_calls: AtomicUsize,
}

#[async_trait]
impl PaymentGateway for ConfigurationReadinessGateway {
    async fn account_mode(&self) -> Result<GatewayAccountMode, GatewayError> {
        self.readiness_calls.fetch_add(1, Ordering::SeqCst);
        Err(GatewayError::Configuration(GatewayDiagnostic::new(
            "invalid merchant configuration",
        )))
    }

    async fn sale(
        &self,
        _request: GatewaySaleRequest,
    ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
        self.sale_calls.fetch_add(1, Ordering::SeqCst);
        panic!("deterministic readiness failure must prevent provider submission")
    }

    async fn store_payment_method(
        &self,
        _request: GatewayStorePaymentMethodRequest,
    ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
        panic!("host charge must not store a payment method")
    }

    async fn query_transaction(
        &self,
        _request: GatewayQueryRequest,
    ) -> Result<Option<GatewayPaymentOutcome>, GatewayError> {
        panic!("foreground host charge must not query")
    }

    async fn query_transaction_reports(
        &self,
        _request: GatewayTransactionReportRequest,
    ) -> Result<Vec<GatewayTransactionReport>, GatewayError> {
        panic!("foreground host charge must not query reports")
    }
}

async fn assert_cooldown_stops_before_gateway_resolution(
    database_name: &str,
    expected_scope: GatewayMutationCooldownScope,
) -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start(database_name).await?;
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
        match expected_scope {
            GatewayMutationCooldownScope::Account => {
                sqlx::query(
                    r#"
                    UPDATE billing_gateway_accounts
                    SET mutation_rate_limited_until = clock_timestamp() + interval '1 minute'
                    WHERE id = $1
                    "#,
                )
                .bind(account.gateway_account_id)
                .execute(&database.pool)
                .await?;
            }
            GatewayMutationCooldownScope::Provider => {
                sqlx::query(
                    r#"
                    UPDATE billing_gateway_provider_rate_limits
                    SET rate_limited_until = clock_timestamp() + interval '1 minute'
                    WHERE provider_key = 'nmi'
                    "#,
                )
                .execute(&database.pool)
                .await?;
            }
        }
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

        let gateway = Arc::new(ConfigurationReadinessGateway {
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
            resolver.clone(),
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
            PaymentToken::new("tok_host_cooldown")?,
            IdempotencyKey::new(format!("host-{database_name}"))?,
            None,
        );

        let error = service
            .charge_host_target(command)
            .await
            .expect_err("durable cooldown must reject before gateway resolution");
        let SubscriptionBillingServiceError::GatewayMutationCooldown { scope } = error else {
            panic!("expected typed gateway mutation cooldown, got {error:?}");
        };
        assert_eq!(scope, expected_scope);
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 0);
        assert_eq!(gateway.readiness_calls.load(Ordering::SeqCst), 0);
        assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 0);
        assert_eq!(admission.calls.load(Ordering::SeqCst), 0);
        let attempts: i64 = sqlx::query_scalar("SELECT count(*) FROM billing_payment_attempts")
            .fetch_one(&database.pool)
            .await?;
        assert_eq!(attempts, 0);
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup?;
    Ok(())
}

#[tokio::test]
async fn account_cooldown_stops_host_charge_before_gateway_resolution() -> Result<(), Box<dyn Error>>
{
    assert_cooldown_stops_before_gateway_resolution(
        "host_cd_account",
        GatewayMutationCooldownScope::Account,
    )
    .await
}

#[tokio::test]
async fn provider_cooldown_stops_host_charge_before_gateway_resolution()
-> Result<(), Box<dyn Error>> {
    assert_cooldown_stops_before_gateway_resolution(
        "host_cd_provider",
        GatewayMutationCooldownScope::Provider,
    )
    .await
}

#[tokio::test]
async fn prepared_replay_resolves_deterministic_readiness_before_returning_error()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("host_ready_rpl").await?;
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

        let gateway = Arc::new(ConfigurationReadinessGateway {
            readiness_calls: AtomicUsize::new(0),
            sale_calls: AtomicUsize::new(0),
        });
        let resolved = resolved_gateway(account, gateway.clone());
        let command = ChargeHostTarget::new(
            syrup_rail::BillingScopeId::new(account.billing_scope_id),
            syrup_rail::SubscriberId::new(subscriber_id),
            HostChargeTargetId::new(target_id),
            GatewayConfigurationId::new(account.gateway_configuration_id),
            PaymentToken::new("tok_host_ready_replay")?,
            IdempotencyKey::new("host-ready-replay")?,
            None,
        );
        let attempt_id = PaymentAttemptId::new(Uuid::now_v7());
        let reservation = HostChargeReservation::from_command(
            &command,
            syrup_rail::HostChargeTargetSnapshot::new(
                HostChargeTargetId::new(target_id),
                ChargeAmount::new(1250, CurrencyCode::new("USD")?)?,
            ),
            &resolved,
            attempt_id,
        )?;
        let mut transaction = database.pool.begin().await?;
        assert!(matches!(
            reserve_host_charge_in_transaction(&mut transaction, &TestTargets, &reservation)
                .await?,
            HostChargeReservationOutcome::Reserved(_)
        ));
        transaction.commit().await?;

        let resolver = Arc::new(StaticResolver {
            gateway: resolved,
            calls: AtomicUsize::new(0),
        });
        let admission = Arc::new(PermitAdmission {
            calls: AtomicUsize::new(0),
        });
        let service = SubscriptionBillingService::new(
            database.pool.clone(),
            Arc::new(UnusedOffers),
            resolver.clone(),
            admission.clone(),
            Arc::new(TestCoordinator {
                pool: database.pool.clone(),
                events: Arc::new(Mutex::new(Vec::new())),
            }),
        )
        .with_host_charge_targets(Arc::new(TestTargets));

        let error = service
            .charge_host_target(command.clone())
            .await
            .expect_err("configuration readiness must retain its typed error");
        assert!(matches!(
            error,
            SubscriptionBillingServiceError::GatewayReadiness(GatewayError::Configuration(_))
        ));
        let state: (String, Option<String>, Option<DateTime<Utc>>) = sqlx::query_as(
            "SELECT status, resolution_code, submitted_at \
             FROM billing_payment_attempts WHERE id = $1",
        )
        .bind(attempt_id.as_uuid())
        .fetch_one(&database.pool)
        .await?;
        assert_eq!(
            state,
            (
                "failed".to_owned(),
                Some("gateway_configuration_before_submission".to_owned()),
                None,
            )
        );

        let replay = service.charge_host_target(command).await?;
        assert_eq!(replay.attempt().identity().attempt_id(), attempt_id);
        assert_eq!(replay.attempt().status(), PaymentAttemptStatus::Failed);
        assert_eq!(
            replay.attempt().state().resolution_code(),
            Some(PaymentResolutionCode::GatewayConfigurationBeforeSubmission)
        );
        assert_eq!(gateway.readiness_calls.load(Ordering::SeqCst), 1);
        assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 0);
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
        assert_eq!(admission.calls.load(Ordering::SeqCst), 0);
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup?;
    Ok(())
}
