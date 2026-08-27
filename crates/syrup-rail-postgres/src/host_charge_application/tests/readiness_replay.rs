use std::collections::VecDeque;

use super::*;
use crate::{GatewayMutationCooldownScope, SubscriptionBillingServiceError};

struct FinalModeCheckGateway {
    readiness: Mutex<VecDeque<Result<GatewayAccountMode, GatewayError>>>,
    sale_calls: AtomicUsize,
    sale_error: Mutex<Option<GatewayNotSubmittedError>>,
    sale_outcome: Mutex<Option<GatewayPaymentOutcome>>,
}

struct ConcurrentResolutionRateLimitedGateway {
    pool: PgPool,
    billing_scope_id: Uuid,
    subscriber_id: Uuid,
    readiness_calls: AtomicUsize,
}

struct ConcurrentConfigurationNotSubmittedGateway {
    pool: PgPool,
    billing_scope_id: Uuid,
    subscriber_id: Uuid,
}

#[async_trait]
impl PaymentGateway for ConcurrentResolutionRateLimitedGateway {
    async fn account_mode(&self) -> Result<GatewayAccountMode, GatewayError> {
        if self.readiness_calls.fetch_add(1, Ordering::SeqCst) == 0 {
            return Ok(GatewayAccountMode::Live);
        }
        sqlx::query(
            r#"
            UPDATE billing_payment_attempts
            SET status = 'failed', gateway_response_text = 'concurrently resolved',
                gateway_condition = 'failed', resolved_at = clock_timestamp(),
                updated_at = clock_timestamp()
            WHERE billing_scope_id = $1 AND subscriber_id = $2
                AND attempt_kind = 'host_charge' AND status = 'pending'
            "#,
        )
        .bind(self.billing_scope_id)
        .bind(self.subscriber_id)
        .execute(&self.pool)
        .await
        .expect("concurrent terminal resolution must succeed");
        Err(GatewayError::RateLimited(GatewayDiagnostic::new(
            "host mode query throttled after concurrent resolution",
        )))
    }

    async fn sale(
        &self,
        _request: GatewaySaleRequest,
    ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
        panic!("rate-limited readiness must prevent provider submission")
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

#[async_trait]
impl PaymentGateway for ConcurrentConfigurationNotSubmittedGateway {
    async fn account_mode(&self) -> Result<GatewayAccountMode, GatewayError> {
        Ok(GatewayAccountMode::Live)
    }

    async fn sale(
        &self,
        _request: GatewaySaleRequest,
    ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
        sqlx::query(
            r#"
            UPDATE billing_payment_attempts
            SET status = 'failed',
                resolution_code = 'gateway_configuration_before_submission',
                gateway_response_text = 'concurrently resolved',
                gateway_condition = 'failed', resolved_at = clock_timestamp(),
                updated_at = clock_timestamp()
            WHERE billing_scope_id = $1 AND subscriber_id = $2
                AND attempt_kind = 'host_charge' AND status = 'pending'
            "#,
        )
        .bind(self.billing_scope_id)
        .bind(self.subscriber_id)
        .execute(&self.pool)
        .await
        .expect("concurrent terminal resolution must succeed");
        Err(GatewayMutationError::NotSubmitted(
            GatewayNotSubmittedError::Configuration(GatewayDiagnostic::new(
                "configuration changed after concurrent resolution",
            )),
        ))
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

#[async_trait]
impl PaymentGateway for FinalModeCheckGateway {
    async fn account_mode(&self) -> Result<GatewayAccountMode, GatewayError> {
        self.readiness
            .lock()
            .await
            .pop_front()
            .expect("scripted account-mode result")
    }

    async fn sale(
        &self,
        _request: GatewaySaleRequest,
    ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
        self.sale_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(error) = self.sale_error.lock().await.take() {
            return Err(GatewayMutationError::NotSubmitted(error));
        }
        Ok(self
            .sale_outcome
            .lock()
            .await
            .take()
            .expect("a successful sale needs one scripted outcome"))
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

struct RecordingTargets {
    preflight_calls: AtomicUsize,
    admission_calls: AtomicUsize,
    release_transitions: AtomicUsize,
}

#[async_trait]
impl HostChargeTargetStore for RecordingTargets {
    async fn preflight_target(
        &self,
        connection: &mut PgConnection,
        reservation: &HostChargeTargetReservation,
    ) -> Result<HostChargeReservationDecision, HostChargeTargetError> {
        self.preflight_calls.fetch_add(1, Ordering::SeqCst);
        TestTargets.preflight_target(connection, reservation).await
    }

    async fn reserve_target(
        &self,
        connection: &mut PgConnection,
        reservation: &HostChargeTargetReservation,
    ) -> Result<HostChargeReservationDecision, HostChargeTargetError> {
        TestTargets.reserve_target(connection, reservation).await
    }

    async fn ensure_submission_admitted(
        &self,
        connection: &mut PgConnection,
        admission: &HostChargeSubmissionAdmission,
    ) -> Result<HostChargeSubmissionDecision, HostChargeTargetError> {
        self.admission_calls.fetch_add(1, Ordering::SeqCst);
        TestTargets
            .ensure_submission_admitted(connection, admission)
            .await
    }

    async fn apply_transition(
        &self,
        connection: &mut PgConnection,
        transition: HostChargeTargetTransition,
    ) -> Result<HostChargeTargetTransitionOutcome, HostChargeTargetError> {
        if transition.kind() == HostChargeTargetTransitionKind::ReleasedBeforeSubmission {
            self.release_transitions.fetch_add(1, Ordering::SeqCst);
        }
        TestTargets.apply_transition(connection, transition).await
    }
}

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
async fn active_cooldown_releases_an_already_reserved_host_target() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("host_cd_reserved").await?;
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
            PaymentToken::new("tok_host_reserved_cooldown")?,
            IdempotencyKey::new("host-reserved-cooldown")?,
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
            GatewayAccountMode::Live,
        )?;
        let mut transaction = database.pool.begin().await?;
        assert!(matches!(
            reserve_host_charge_in_transaction(&mut transaction, &TestTargets, &reservation)
                .await?,
            HostChargeReservationOutcome::Reserved(_)
        ));
        transaction.commit().await?;
        sqlx::query(
            "UPDATE billing_gateway_provider_rate_limits \
             SET rate_limited_until = clock_timestamp() + interval '1 minute' \
             WHERE provider_key = 'nmi'",
        )
        .execute(&database.pool)
        .await?;

        let resolver = Arc::new(StaticResolver {
            gateway: resolved,
            calls: AtomicUsize::new(0),
        });
        let targets = Arc::new(RecordingTargets {
            preflight_calls: AtomicUsize::new(0),
            admission_calls: AtomicUsize::new(0),
            release_transitions: AtomicUsize::new(0),
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
        .with_host_charge_targets(targets.clone());

        let error = service
            .charge_host_target(command.clone())
            .await
            .expect_err("the active provider cooldown must resolve reserved work");
        assert!(matches!(
            error,
            SubscriptionBillingServiceError::GatewayMutationCooldown {
                scope: GatewayMutationCooldownScope::Provider
            }
        ));
        let replay = service.charge_host_target(command).await?;
        assert_eq!(replay.attempt().identity().attempt_id(), attempt_id);
        assert_eq!(replay.attempt().status(), PaymentAttemptStatus::Failed);
        assert_eq!(
            replay.attempt().state().resolution_code(),
            Some(PaymentResolutionCode::GatewayProviderRateLimitedBeforeSubmission)
        );
        assert_eq!(targets.release_transitions.load(Ordering::SeqCst), 1);
        assert_eq!(targets.preflight_calls.load(Ordering::SeqCst), 1);
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 0);
        assert_eq!(gateway.readiness_calls.load(Ordering::SeqCst), 0);
        assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 0);
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup?;
    Ok(())
}

#[tokio::test]
async fn unchanged_target_release_cannot_terminalize_attempt() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("host_rel_refuse").await?;
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

        let resolved = resolved_gateway(
            account,
            Arc::new(ConfigurationReadinessGateway {
                readiness_calls: AtomicUsize::new(0),
                sale_calls: AtomicUsize::new(0),
            }),
        );
        let command = ChargeHostTarget::new(
            syrup_rail::BillingScopeId::new(account.billing_scope_id),
            syrup_rail::SubscriberId::new(subscriber_id),
            HostChargeTargetId::new(target_id),
            GatewayConfigurationId::new(account.gateway_configuration_id),
            PaymentToken::new("tok_host_release_refused")?,
            IdempotencyKey::new("host-release-refused")?,
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
            GatewayAccountMode::Live,
        )?;
        let mut transaction = database.pool.begin().await?;
        assert!(matches!(
            reserve_host_charge_in_transaction(&mut transaction, &TestTargets, &reservation)
                .await?,
            HostChargeReservationOutcome::Reserved(_)
        ));
        transaction.commit().await?;

        let error = resolve_host_charge_before_submission(
            &database.pool,
            &RefusingTransitionTargets,
            &reservation,
            resolved.provider_key(),
            GatewayDiagnostic::new("release refused"),
            PaymentResolutionCode::GatewayConfigurationBeforeSubmission,
            HostChargeBeforeSubmissionResolution::prepared(),
        )
        .await
        .expect_err("an unchanged target release must abort attempt resolution");
        assert!(matches!(error, HostChargeApplicationError::InvalidState(_)));

        let attempt_state: (String, Option<String>, Option<DateTime<Utc>>) = sqlx::query_as(
            "SELECT status, resolution_code, resolved_at FROM billing_payment_attempts WHERE id = $1",
        )
        .bind(attempt_id.as_uuid())
        .fetch_one(&database.pool)
        .await?;
        assert_eq!(attempt_state, ("pending".to_owned(), None, None));
        let target_state: String =
            sqlx::query_scalar("SELECT status FROM host_charge_targets WHERE id = $1")
                .bind(target_id)
                .fetch_one(&database.pool)
                .await?;
        assert_eq!(target_state, "pending");

        sqlx::query(
            r#"
            UPDATE billing_payment_attempts
            SET status = 'failed',
                resolution_code = 'gateway_configuration_before_submission',
                gateway_condition = 'failed',
                gateway_response_text = 'concurrent canonical result',
                resolved_at = clock_timestamp(),
                updated_at = clock_timestamp()
            WHERE id = $1
            "#,
        )
        .bind(attempt_id.as_uuid())
        .execute(&database.pool)
        .await?;
        let concurrent = resolve_host_charge_before_submission(
            &database.pool,
            &RefusingTransitionTargets,
            &reservation,
            resolved.provider_key(),
            GatewayDiagnostic::new("release refused after concurrent resolution"),
            PaymentResolutionCode::GatewayConfigurationBeforeSubmission,
            HostChargeBeforeSubmissionResolution::prepared(),
        )
        .await?;
        assert_eq!(concurrent.attempt().status(), PaymentAttemptStatus::Failed);
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup?;
    Ok(())
}

#[tokio::test]
async fn unreserved_mode_mismatch_precedes_admission_and_reservation() -> Result<(), Box<dyn Error>>
{
    let database = TestDatabase::start("host_ready_new").await?;
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

        let gateway = Arc::new(ScriptedGateway {
            account_mode: GatewayAccountMode::Test,
            sale_calls: AtomicUsize::new(0),
            outcome: Mutex::new(None),
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
            PaymentToken::new("tok_host_ready_new")?,
            IdempotencyKey::new("host-ready-new")?,
            None,
        );

        assert!(matches!(
            service.charge_host_target(command).await,
            Err(SubscriptionBillingServiceError::GatewayReadiness(
                GatewayError::Configuration(_)
            ))
        ));
        assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 0);
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
        assert_eq!(admission.calls.load(Ordering::SeqCst), 0);
        let attempts: i64 = sqlx::query_scalar("SELECT count(*) FROM billing_payment_attempts")
            .fetch_one(&database.pool)
            .await?;
        assert_eq!(attempts, 0);
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
            GatewayAccountMode::Live,
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
        let targets = Arc::new(RecordingTargets {
            preflight_calls: AtomicUsize::new(0),
            admission_calls: AtomicUsize::new(0),
            release_transitions: AtomicUsize::new(0),
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
        .with_host_charge_targets(targets.clone());

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
        assert_eq!(targets.release_transitions.load(Ordering::SeqCst), 1);
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup?;
    Ok(())
}

#[tokio::test]
async fn prepared_replay_rate_limit_resolves_attempt_and_extends_provider_cooldown()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("host_rate_rpl").await?;
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
        let resolved = resolved_gateway(account, gateway.clone());
        let command = ChargeHostTarget::new(
            syrup_rail::BillingScopeId::new(account.billing_scope_id),
            syrup_rail::SubscriberId::new(subscriber_id),
            HostChargeTargetId::new(target_id),
            GatewayConfigurationId::new(account.gateway_configuration_id),
            PaymentToken::new("tok_host_rate_replay")?,
            IdempotencyKey::new("host-rate-replay")?,
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
            GatewayAccountMode::Live,
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
        let targets = Arc::new(RecordingTargets {
            preflight_calls: AtomicUsize::new(0),
            admission_calls: AtomicUsize::new(0),
            release_transitions: AtomicUsize::new(0),
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
        .with_host_charge_targets(targets.clone());

        let error = service
            .charge_host_target(command.clone())
            .await
            .expect_err("prepared readiness throttle must remain typed");
        assert!(matches!(
            error,
            SubscriptionBillingServiceError::GatewayMutationCooldown {
                scope: GatewayMutationCooldownScope::Provider
            }
        ));
        let replay = service.charge_host_target(command).await?;
        assert_eq!(replay.attempt().identity().attempt_id(), attempt_id);
        assert_eq!(replay.attempt().status(), PaymentAttemptStatus::Failed);
        assert_eq!(
            replay.attempt().state().resolution_code(),
            Some(PaymentResolutionCode::GatewayProviderRateLimitedBeforeSubmission)
        );
        assert!(
            replay
                .attempt()
                .state()
                .timestamps()
                .submitted_at()
                .is_none()
        );
        let provider_cooldown_is_active: bool = sqlx::query_scalar(
            "SELECT rate_limited_until > clock_timestamp() \
             FROM billing_gateway_provider_rate_limits WHERE provider_key = 'nmi'",
        )
        .fetch_one(&database.pool)
        .await?;
        assert!(provider_cooldown_is_active);
        assert_eq!(gateway.readiness_calls.load(Ordering::SeqCst), 1);
        assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 0);
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
        assert_eq!(admission.calls.load(Ordering::SeqCst), 0);
        assert_eq!(targets.release_transitions.load(Ordering::SeqCst), 1);
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup?;
    Ok(())
}

#[tokio::test]
async fn final_mode_mismatch_releases_the_host_target_with_the_terminal_attempt()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("host_final_mode").await?;
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

        let gateway = Arc::new(FinalModeCheckGateway {
            readiness: Mutex::new(VecDeque::from([
                Ok(GatewayAccountMode::Live),
                Ok(GatewayAccountMode::Test),
            ])),
            sale_calls: AtomicUsize::new(0),
            sale_error: Mutex::new(None),
            sale_outcome: Mutex::new(None),
        });
        let resolver = Arc::new(StaticResolver {
            gateway: resolved_gateway(account, gateway.clone()),
            calls: AtomicUsize::new(0),
        });
        let admission = Arc::new(PermitAdmission {
            calls: AtomicUsize::new(0),
        });
        let targets = Arc::new(RecordingTargets {
            preflight_calls: AtomicUsize::new(0),
            admission_calls: AtomicUsize::new(0),
            release_transitions: AtomicUsize::new(0),
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
        .with_host_charge_targets(targets.clone());
        let command = ChargeHostTarget::new(
            syrup_rail::BillingScopeId::new(account.billing_scope_id),
            syrup_rail::SubscriberId::new(subscriber_id),
            HostChargeTargetId::new(target_id),
            GatewayConfigurationId::new(account.gateway_configuration_id),
            PaymentToken::new("tok_host_final_mode")?,
            IdempotencyKey::new("host-final-mode")?,
            None,
        );

        let error = service
            .charge_host_target(command)
            .await
            .expect_err("the final mode mismatch must stop before sale");
        let SubscriptionBillingServiceError::GatewayNotSubmitted(
            GatewayNotSubmittedError::AccountModeMismatch {
                required: GatewayAccountMode::Live,
                observed: GatewayAccountMode::Test,
                detail,
            },
        ) = error
        else {
            return Err("expected a live-required/test-observed mode mismatch".into());
        };
        assert_eq!(
            detail.expose(),
            "Payment was not submitted because the payment processor account mode did not match this deployment."
        );
        let attempt: (String, Option<String>, Option<DateTime<Utc>>) = sqlx::query_as(
            "SELECT status, resolution_code, submitted_at FROM billing_payment_attempts \
             WHERE billing_scope_id = $1 AND subscriber_id = $2",
        )
        .bind(account.billing_scope_id)
        .bind(subscriber_id)
        .fetch_one(&database.pool)
        .await?;
        assert_eq!(
            attempt,
            (
                "failed".to_owned(),
                Some("gateway_live_readiness_failed_before_submission".to_owned()),
                None,
            )
        );
        assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 0);
        assert_eq!(admission.calls.load(Ordering::SeqCst), 1);
        assert_eq!(targets.release_transitions.load(Ordering::SeqCst), 1);
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup?;
    Ok(())
}

#[tokio::test]
async fn final_unavailable_mode_query_restores_host_charge_for_same_key_retry()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("host_final_retry").await?;
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

        let gateway = Arc::new(FinalModeCheckGateway {
            readiness: Mutex::new(VecDeque::from([
                Ok(GatewayAccountMode::Live),
                Err(GatewayError::Unavailable(GatewayDiagnostic::new(
                    "final host mode query unavailable",
                ))),
                Ok(GatewayAccountMode::Live),
                Ok(GatewayAccountMode::Live),
            ])),
            sale_calls: AtomicUsize::new(0),
            sale_error: Mutex::new(None),
            sale_outcome: Mutex::new(Some(approved_outcome("host_final_retry_txn"))),
        });
        let targets = Arc::new(RecordingTargets {
            preflight_calls: AtomicUsize::new(0),
            admission_calls: AtomicUsize::new(0),
            release_transitions: AtomicUsize::new(0),
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
            Arc::new(TestCoordinator {
                pool: database.pool.clone(),
                events: Arc::new(Mutex::new(Vec::new())),
            }),
        )
        .with_host_charge_targets(targets.clone());
        let command = ChargeHostTarget::new(
            syrup_rail::BillingScopeId::new(account.billing_scope_id),
            syrup_rail::SubscriberId::new(subscriber_id),
            HostChargeTargetId::new(target_id),
            GatewayConfigurationId::new(account.gateway_configuration_id),
            PaymentToken::new("tok_host_final_retry")?,
            IdempotencyKey::new("host-final-retry")?,
            None,
        );

        let error = service
            .charge_host_target(command.clone())
            .await
            .expect_err("the final unavailable query must remain retryable");
        assert!(matches!(
            error,
            SubscriptionBillingServiceError::GatewayNotSubmitted(
                GatewayNotSubmittedError::AccountModeVerification(GatewayError::Unavailable(_))
            )
        ));
        let prepared: (String, Option<String>, Option<DateTime<Utc>>) = sqlx::query_as(
            "SELECT status, resolution_code, submitted_at FROM billing_payment_attempts \
             WHERE billing_scope_id = $1 AND subscriber_id = $2",
        )
        .bind(account.billing_scope_id)
        .bind(subscriber_id)
        .fetch_one(&database.pool)
        .await?;
        assert_eq!(prepared, ("pending".to_owned(), None, None));
        assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 0);
        assert_eq!(targets.admission_calls.load(Ordering::SeqCst), 1);
        assert_eq!(targets.release_transitions.load(Ordering::SeqCst), 0);

        sqlx::query("UPDATE host_charge_targets SET amount_cents = 1300 WHERE id = $1")
            .bind(target_id)
            .execute(&database.pool)
            .await?;
        let conflict = service
            .charge_host_target(command.clone())
            .await
            .expect_err("changed same-key target economics must conflict before gateway I/O");
        assert!(matches!(
            conflict,
            SubscriptionBillingServiceError::IdempotencyConflict
        ));
        assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 0);

        sqlx::query("UPDATE host_charge_targets SET amount_cents = 1250 WHERE id = $1")
            .bind(target_id)
            .execute(&database.pool)
            .await?;
        let payment = service.charge_host_target(command).await?;
        assert_eq!(payment.attempt().status(), PaymentAttemptStatus::Approved);
        assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 1);
        assert_eq!(targets.admission_calls.load(Ordering::SeqCst), 2);
        assert_eq!(targets.release_transitions.load(Ordering::SeqCst), 0);
        assert_eq!(targets.preflight_calls.load(Ordering::SeqCst), 3);
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup?;
    Ok(())
}

#[tokio::test]
async fn final_mode_query_rate_limit_persists_provider_cooldown_and_releases_target()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("host_mode_rate").await?;
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

        let gateway = Arc::new(FinalModeCheckGateway {
            readiness: Mutex::new(VecDeque::from([
                Ok(GatewayAccountMode::Live),
                Err(GatewayError::RateLimited(GatewayDiagnostic::new(
                    "host mode query throttled",
                ))),
            ])),
            sale_calls: AtomicUsize::new(0),
            sale_error: Mutex::new(None),
            sale_outcome: Mutex::new(None),
        });
        let admission = Arc::new(PermitAdmission {
            calls: AtomicUsize::new(0),
        });
        let targets = Arc::new(RecordingTargets {
            preflight_calls: AtomicUsize::new(0),
            admission_calls: AtomicUsize::new(0),
            release_transitions: AtomicUsize::new(0),
        });
        let service = SubscriptionBillingService::new(
            database.pool.clone(),
            Arc::new(UnusedOffers),
            Arc::new(StaticResolver {
                gateway: resolved_gateway(account, gateway.clone()),
                calls: AtomicUsize::new(0),
            }),
            admission.clone(),
            Arc::new(TestCoordinator {
                pool: database.pool.clone(),
                events: Arc::new(Mutex::new(Vec::new())),
            }),
        )
        .with_host_charge_targets(targets.clone());
        let command = ChargeHostTarget::new(
            syrup_rail::BillingScopeId::new(account.billing_scope_id),
            syrup_rail::SubscriberId::new(subscriber_id),
            HostChargeTargetId::new(target_id),
            GatewayConfigurationId::new(account.gateway_configuration_id),
            PaymentToken::new("tok_host_final_mode_rate")?,
            IdempotencyKey::new("host-final-mode-rate")?,
            None,
        );

        let error = service
            .charge_host_target(command)
            .await
            .expect_err("the final account-mode throttle must remain typed");
        assert!(matches!(
            error,
            SubscriptionBillingServiceError::GatewayNotSubmitted(
                GatewayNotSubmittedError::AccountModeVerification(GatewayError::RateLimited(_))
            )
        ));
        let attempt: (String, Option<String>, Option<DateTime<Utc>>) = sqlx::query_as(
            "SELECT status, resolution_code, submitted_at FROM billing_payment_attempts \
             WHERE billing_scope_id = $1 AND subscriber_id = $2",
        )
        .bind(account.billing_scope_id)
        .bind(subscriber_id)
        .fetch_one(&database.pool)
        .await?;
        assert_eq!(
            attempt,
            (
                "failed".to_owned(),
                Some("gateway_provider_rate_limited_before_submission".to_owned()),
                None,
            )
        );
        let provider_cooldown_is_active: bool = sqlx::query_scalar(
            "SELECT rate_limited_until > clock_timestamp() \
             FROM billing_gateway_provider_rate_limits WHERE provider_key = 'nmi'",
        )
        .fetch_one(&database.pool)
        .await?;
        assert!(provider_cooldown_is_active);
        let account_cooldown_is_active: bool = sqlx::query_scalar(
            "SELECT COALESCE(mutation_rate_limited_until > clock_timestamp(), FALSE) \
             FROM billing_gateway_accounts WHERE id = $1",
        )
        .bind(account.gateway_account_id)
        .fetch_one(&database.pool)
        .await?;
        assert!(!account_cooldown_is_active);
        assert!(gateway.readiness.lock().await.is_empty());
        assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 0);
        assert_eq!(admission.calls.load(Ordering::SeqCst), 1);
        assert_eq!(targets.release_transitions.load(Ordering::SeqCst), 1);
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup?;
    Ok(())
}

#[tokio::test]
async fn concurrent_same_code_resolution_returns_canonical_payment() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("host_conc_same").await?;
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

        let gateway = Arc::new(ConcurrentConfigurationNotSubmittedGateway {
            pool: database.pool.clone(),
            billing_scope_id: account.billing_scope_id,
            subscriber_id,
        });
        let service = SubscriptionBillingService::new(
            database.pool.clone(),
            Arc::new(UnusedOffers),
            Arc::new(StaticResolver {
                gateway: resolved_gateway(account, gateway),
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
        let payment = service
            .charge_host_target(ChargeHostTarget::new(
                syrup_rail::BillingScopeId::new(account.billing_scope_id),
                syrup_rail::SubscriberId::new(subscriber_id),
                HostChargeTargetId::new(target_id),
                GatewayConfigurationId::new(account.gateway_configuration_id),
                PaymentToken::new("tok_host_concurrent_same")?,
                IdempotencyKey::new("host-concurrent-same")?,
                None,
            ))
            .await?;
        assert_eq!(payment.attempt().status(), PaymentAttemptStatus::Failed);
        assert_eq!(
            payment.attempt().state().resolution_code(),
            Some(PaymentResolutionCode::GatewayConfigurationBeforeSubmission)
        );
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup?;
    Ok(())
}

#[tokio::test]
async fn concurrent_terminal_resolution_cannot_roll_back_provider_cooldown()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("host_conc_rate").await?;
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

        let gateway = Arc::new(ConcurrentResolutionRateLimitedGateway {
            pool: database.pool.clone(),
            billing_scope_id: account.billing_scope_id,
            subscriber_id,
            readiness_calls: AtomicUsize::new(0),
        });
        let service = SubscriptionBillingService::new(
            database.pool.clone(),
            Arc::new(UnusedOffers),
            Arc::new(StaticResolver {
                gateway: resolved_gateway(account, gateway),
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
        let payment = service
            .charge_host_target(ChargeHostTarget::new(
                syrup_rail::BillingScopeId::new(account.billing_scope_id),
                syrup_rail::SubscriberId::new(subscriber_id),
                HostChargeTargetId::new(target_id),
                GatewayConfigurationId::new(account.gateway_configuration_id),
                PaymentToken::new("tok_host_concurrent_rate")?,
                IdempotencyKey::new("host-concurrent-rate")?,
                None,
            ))
            .await?;
        assert_eq!(payment.attempt().status(), PaymentAttemptStatus::Failed);
        assert_eq!(
            payment.attempt().state().resolution_code(),
            None,
            "the concurrent canonical resolution must win"
        );
        let provider_cooldown_is_active: bool = sqlx::query_scalar(
            "SELECT rate_limited_until > clock_timestamp() \
             FROM billing_gateway_provider_rate_limits WHERE provider_key = 'nmi'",
        )
        .fetch_one(&database.pool)
        .await?;
        assert!(provider_cooldown_is_active);
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup?;
    Ok(())
}

#[tokio::test]
async fn final_mutation_rate_limit_persists_account_cooldown_and_releases_target()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("host_final_rate").await?;
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

        let gateway = Arc::new(FinalModeCheckGateway {
            readiness: Mutex::new(VecDeque::from([
                Ok(GatewayAccountMode::Live),
                Ok(GatewayAccountMode::Live),
            ])),
            sale_calls: AtomicUsize::new(0),
            sale_error: Mutex::new(Some(GatewayNotSubmittedError::RateLimited(
                GatewayDiagnostic::new("host mutation throttled"),
            ))),
            sale_outcome: Mutex::new(None),
        });
        let resolver = Arc::new(StaticResolver {
            gateway: resolved_gateway(account, gateway.clone()),
            calls: AtomicUsize::new(0),
        });
        let admission = Arc::new(PermitAdmission {
            calls: AtomicUsize::new(0),
        });
        let targets = Arc::new(RecordingTargets {
            preflight_calls: AtomicUsize::new(0),
            admission_calls: AtomicUsize::new(0),
            release_transitions: AtomicUsize::new(0),
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
        .with_host_charge_targets(targets.clone());
        let command = ChargeHostTarget::new(
            syrup_rail::BillingScopeId::new(account.billing_scope_id),
            syrup_rail::SubscriberId::new(subscriber_id),
            HostChargeTargetId::new(target_id),
            GatewayConfigurationId::new(account.gateway_configuration_id),
            PaymentToken::new("tok_host_final_rate")?,
            IdempotencyKey::new("host-final-rate")?,
            None,
        );

        let error = service
            .charge_host_target(command)
            .await
            .expect_err("the provider throttle must remain typed");
        assert!(matches!(
            error,
            SubscriptionBillingServiceError::GatewayNotSubmitted(
                GatewayNotSubmittedError::RateLimited(_)
            )
        ));
        let attempt: (String, Option<String>, Option<DateTime<Utc>>) = sqlx::query_as(
            "SELECT status, resolution_code, submitted_at FROM billing_payment_attempts \
             WHERE billing_scope_id = $1 AND subscriber_id = $2",
        )
        .bind(account.billing_scope_id)
        .bind(subscriber_id)
        .fetch_one(&database.pool)
        .await?;
        assert_eq!(
            attempt,
            (
                "failed".to_owned(),
                Some("gateway_account_rate_limited_before_submission".to_owned()),
                None,
            )
        );
        let account_cooldown_is_active: bool = sqlx::query_scalar(
            "SELECT mutation_rate_limited_until > clock_timestamp() \
             FROM billing_gateway_accounts WHERE id = $1",
        )
        .bind(account.gateway_account_id)
        .fetch_one(&database.pool)
        .await?;
        assert!(account_cooldown_is_active);
        assert!(gateway.readiness.lock().await.is_empty());
        assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 1);
        assert_eq!(admission.calls.load(Ordering::SeqCst), 1);
        assert_eq!(targets.release_transitions.load(Ordering::SeqCst), 1);
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup?;
    Ok(())
}
