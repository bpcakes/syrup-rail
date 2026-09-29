//! Read-only payment-attempt diagnostics against real PostgreSQL storage.

use std::{future::Future, pin::Pin, time::Duration};

use syrup_rail::{
    GatewayDiagnosticOperation, GatewayDiagnosticsCompleteness, GatewayDiagnosticsSource,
    GatewayDiagnosticsUnavailableReason as Reason, GatewayTransactionDiagnostics,
    GatewayTransactionDiagnosticsObservation, GatewayTransactionDiagnosticsRequest,
    ScrubSubscriberBillingData,
};

use super::billing_address::{
    enroll_command, make_renewal_due, named_contact, replacement_command, service,
};
use super::*;
use crate::{
    GatewayMutationCooldownScope, PaymentAttemptDiagnosticEligibility as Eligibility,
    PaymentAttemptDiagnosticIneligibility as Ineligibility, PaymentAttemptDiagnosticTarget,
    PaymentAttemptDiagnosticsError, PaymentAttemptDiagnosticsOutcome as Outcome,
    payment_attempt_diagnostic_eligibility, query_payment_attempt_diagnostics,
    scrub_subscriber_billing_data,
};

type Hook = Pin<Box<dyn Future<Output = ()> + Send>>;

const DEADLINE: Duration = Duration::from_secs(15);

/// A provider that answers only diagnostic queries, optionally running a hook
/// while the query is in flight.
struct DiagnosticsGateway {
    calls: AtomicUsize,
    requests: Mutex<Vec<GatewayTransactionDiagnosticsRequest>>,
    during: Mutex<Option<Hook>>,
    delay: Duration,
    result: Mutex<Option<Result<GatewayTransactionDiagnostics, GatewayError>>>,
}

impl DiagnosticsGateway {
    fn new(result: Result<GatewayTransactionDiagnostics, GatewayError>) -> Arc<Self> {
        Self::with(result, Duration::ZERO, None)
    }

    fn with(
        result: Result<GatewayTransactionDiagnostics, GatewayError>,
        delay: Duration,
        during: Option<Hook>,
    ) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            requests: Mutex::new(Vec::new()),
            during: Mutex::new(during),
            delay,
            result: Mutex::new(Some(result)),
        })
    }
}

#[async_trait]
impl PaymentGateway for DiagnosticsGateway {
    async fn account_mode(&self) -> Result<GatewayAccountMode, GatewayError> {
        Ok(GatewayAccountMode::Live)
    }

    async fn sale(
        &self,
        _request: GatewaySaleRequest,
    ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
        panic!("diagnostics must never submit a payment")
    }

    async fn store_payment_method(
        &self,
        _request: GatewayStorePaymentMethodRequest,
    ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
        panic!("diagnostics must never store a payment method")
    }

    async fn query_transaction(
        &self,
        _request: GatewayQueryRequest,
    ) -> Result<Option<GatewayPaymentOutcome>, GatewayError> {
        panic!("diagnostics use their own query")
    }

    async fn query_transaction_reports(
        &self,
        _request: GatewayTransactionReportRequest,
    ) -> Result<Vec<GatewayTransactionReport>, GatewayError> {
        panic!("diagnostics never read reports")
    }

    async fn query_transaction_diagnostics(
        &self,
        request: GatewayTransactionDiagnosticsRequest,
    ) -> Result<GatewayTransactionDiagnostics, GatewayError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.requests.lock().await.push(request);
        let hook = self.during.lock().await.take();
        if let Some(hook) = hook {
            hook.await;
        }
        tokio::time::sleep(self.delay).await;
        self.result
            .lock()
            .await
            .take()
            .expect("one scripted diagnostic result")
    }
}

fn observed(operation: GatewayDiagnosticOperation) -> GatewayTransactionDiagnostics {
    GatewayTransactionDiagnostics::Observed(Box::new(
        GatewayTransactionDiagnosticsObservation::new(
            operation,
            GatewayDiagnosticsSource::new("test_query_api").unwrap(),
        )
        .with_gateway_response_code(Some("253"))
        .with_processor_response_code(Some("59"))
        .with_avs_response(Some("0")),
    ))
}

fn resolver_for(
    account: crate::test_support::GatewayAccountFixture,
    gateway: &Arc<DiagnosticsGateway>,
) -> StaticResolver {
    StaticResolver {
        gateway: scripted_resolved_gateway(account, Arc::clone(gateway)),
        calls: AtomicUsize::new(0),
    }
}

fn target(
    fixture: &ApplicationFixture,
    attempt_id: PaymentAttemptId,
) -> PaymentAttemptDiagnosticTarget {
    PaymentAttemptDiagnosticTarget::new(
        fixture.command.billing_scope_id(),
        fixture.command.subscriber_id(),
        fixture.command.plan_key().clone(),
        attempt_id,
    )
}

async fn diagnose(
    fixture: &ApplicationFixture,
    gateway: &Arc<DiagnosticsGateway>,
    attempt_id: PaymentAttemptId,
    deadline: Duration,
) -> Result<Outcome, PaymentAttemptDiagnosticsError> {
    query_payment_attempt_diagnostics(
        &fixture.database.pool,
        &resolver_for(fixture.gateway_account, gateway),
        target(fixture, attempt_id),
        deadline,
    )
    .await
}

/// Enrolls with the given provider outcome and returns the initial attempt.
async fn enroll(
    fixture: &ApplicationFixture,
    key: &str,
    outcome: GatewayPaymentOutcome,
) -> Result<SubscriptionEnrollmentPaymentResult, Box<dyn Error>> {
    let gateway = Arc::new(ScriptedGateway::new(Ok(outcome)));
    Ok(service(fixture, &gateway)
        .enroll(enroll_command(fixture, key, named_contact(None)))
        .await?)
}

fn declined(transaction_id: &str) -> GatewayPaymentOutcome {
    GatewayPaymentOutcome::new(
        GatewayPaymentStatus::Declined,
        ProcessorEvidence::new(
            Some(GatewayTransactionId::new(transaction_id).unwrap()),
            None,
            Some(GatewayDiagnostic::new("2")),
            Some(GatewayDiagnostic::new("253")),
            Some(GatewayDiagnostic::new("Declined")),
            Some(GatewayDiagnostic::new("declined")),
            GatewayPaymentDescriptor::default(),
        ),
    )
}

async fn canonical_rows(pool: &sqlx::PgPool) -> Result<Vec<String>, sqlx::Error> {
    let mut rows = Vec::new();
    for table in [
        "billing_payment_attempts",
        "billing_payment_methods",
        "billing_subscriptions",
        "billing_processor_charges",
        "billing_gateway_accounts",
        "billing_gateway_provider_rate_limits",
    ] {
        rows.push(
            sqlx::query_scalar(&format!(
                "SELECT COALESCE(jsonb_agg(to_jsonb(rows) ORDER BY to_jsonb(rows)::text), '[]'::jsonb)::text FROM public.{table} AS rows"
            ))
            .fetch_one(pool)
            .await?,
        );
    }
    Ok(rows)
}

async fn database_now(pool: &sqlx::PgPool) -> Result<DateTime<Utc>, sqlx::Error> {
    sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(pool)
        .await
}

async fn provider_cooldown_remaining(pool: &sqlx::PgPool) -> Result<f64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXTRACT(EPOCH FROM rate_limited_until - clock_timestamp())::float8 FROM billing_gateway_provider_rate_limits WHERE provider_key = 'nmi'",
    )
    .fetch_one(pool)
    .await
}

#[tokio::test]
async fn diagnostics_observe_an_owned_attempt_without_writes_at_database_time()
-> Result<(), Box<dyn Error>> {
    let fixture = enrollment_fixture("diag_observe", false, false, false).await?;
    let initial = enroll(
        &fixture,
        "diag-observe",
        approved_outcome("txn_diag_initial"),
    )
    .await?;
    let attempt_id = initial.attempt().identity().attempt_id();
    let before_rows = canonical_rows(&fixture.database.pool).await?;
    let gateway = DiagnosticsGateway::new(Ok(observed(GatewayDiagnosticOperation::Sale)));

    let before = database_now(&fixture.database.pool).await?;
    let outcome = diagnose(&fixture, &gateway, attempt_id, DEADLINE).await?;
    let after = database_now(&fixture.database.pool).await?;
    assert_eq!(outcome.as_str(), "observed");
    let Outcome::Observed(diagnostics) = outcome else {
        panic!("owned approved attempt should be observed");
    };
    assert_eq!(diagnostics.target(), &target(&fixture, attempt_id));
    assert_eq!(
        diagnostics.attempt_kind(),
        PaymentAttemptKind::SubscriptionInitial
    );
    assert_eq!(
        diagnostics.gateway_account_id().into_uuid(),
        fixture.gateway_account.gateway_account_id
    );
    assert_eq!(
        diagnostics.gateway_configuration_id().into_uuid(),
        fixture.gateway_account.gateway_configuration_id
    );
    assert_eq!(diagnostics.provider_key().as_str(), "nmi");
    assert_eq!(diagnostics.action_type(), GatewayDiagnosticOperation::Sale);
    assert_eq!(
        diagnostics
            .gateway_response_code()
            .map(GatewayDiagnostic::expose),
        Some("253")
    );
    assert_eq!(
        diagnostics
            .processor_response_code()
            .map(GatewayDiagnostic::expose),
        Some("59")
    );
    assert_eq!(
        diagnostics.avs_response().map(GatewayDiagnostic::expose),
        Some("0")
    );
    assert_eq!(diagnostics.csc_response(), None);
    assert_eq!(diagnostics.source().as_str(), "test_query_api");
    assert_eq!(
        diagnostics.completeness(),
        GatewayDiagnosticsCompleteness::Partial
    );
    assert!(before <= diagnostics.observed_at() && diagnostics.observed_at() <= after);
    assert!(!format!("{diagnostics:?}").contains("253"));

    let requests = gateway.requests.lock().await;
    let [request] = requests.as_slice() else {
        panic!("exactly one provider query");
    };
    assert_eq!(request.transaction_id().expose(), "txn_diag_initial");
    assert_eq!(request.operation(), GatewayDiagnosticOperation::Sale);
    assert_eq!(request.amount(), initial.attempt().request().amount());
    assert_eq!(
        request.expected_order_id(),
        Some(initial.attempt().request().gateway_order_id())
    );
    drop(requests);
    assert_eq!(canonical_rows(&fixture.database.pool).await?, before_rows);
    fixture.cleanup().await
}

#[tokio::test]
async fn eligibility_follows_kind_status_ownership_and_precedence() -> Result<(), Box<dyn Error>> {
    let fixture = enrollment_fixture("diag_eligible", false, false, false).await?;
    let pool = fixture.database.pool.clone();
    let initial = enroll(
        &fixture,
        "diag-eligible",
        approved_outcome("txn_eligible_initial"),
    )
    .await?;
    let initial_id = initial.attempt().identity().attempt_id();
    let subscription_id = initial.subscription().expect("subscription").id();

    // Replacement verifies with the zero-amount validate operation.
    let replacement_gateway = Arc::new(ScriptedGateway::for_stored_method(Ok(
        approved_outcome_with_reference(Some("txn_eligible_validate"), "vault_eligible_new"),
    )));
    let replacement = service(&fixture, &replacement_gateway)
        .replace_payment_method(replacement_command(
            &fixture,
            "diag-replacement",
            named_contact(None),
        ))
        .await?;
    let replacement_id = replacement.attempt().identity().attempt_id();
    let validate_gateway =
        DiagnosticsGateway::new(Ok(observed(GatewayDiagnosticOperation::Validate)));
    assert!(matches!(
        diagnose(&fixture, &validate_gateway, replacement_id, DEADLINE).await?,
        Outcome::Observed(_)
    ));
    {
        let requests = validate_gateway.requests.lock().await;
        assert_eq!(
            requests[0].operation(),
            GatewayDiagnosticOperation::Validate
        );
        assert_eq!(requests[0].amount().cents(), 0);
    }

    // Renewal and recovery attempts are diagnosable too.
    let period_start_at = make_renewal_due(&pool, subscription_id).await?;
    let renewal_gateway = Arc::new(ScriptedGateway::new(Ok(declined("txn_eligible_renewal"))));
    let SubscriptionRenewalOutcome::Payment(renewal) = service(&fixture, &renewal_gateway)
        .renew(ChargeRenewal::new(
            fixture.command.billing_scope_id(),
            subscription_id,
            period_start_at,
        ))
        .await?
    else {
        panic!("due renewal should submit");
    };
    let renewal_id = renewal.attempt().identity().attempt_id();
    let single = |attempt_id: PaymentAttemptId| {
        let pool = pool.clone();
        let scope = fixture.command.billing_scope_id();
        let subscriber = fixture.command.subscriber_id();
        let plan = fixture.command.plan_key().clone();
        async move {
            payment_attempt_diagnostic_eligibility(&pool, scope, subscriber, &plan, &[attempt_id])
                .await
                .map(|items| {
                    items
                        .into_iter()
                        .map(|item| item.eligibility())
                        .collect::<Vec<_>>()
                })
        }
    };
    // Every submitted, non-pending status with a transaction ID is eligible;
    // pending is not, and a missing transaction ID is reported after pending.
    // Run this before recovery: one period holds one in-flight or approved
    // renewal or recovery.
    for status in [
        "approved",
        "failed",
        "unknown",
        "review_required",
        "declined",
    ] {
        sqlx::query(
            "UPDATE billing_payment_attempts SET status = $2, resolved_at = COALESCE(resolved_at, now()) WHERE id = $1",
        )
        .bind(renewal_id.as_uuid())
        .bind(status)
        .execute(&pool)
        .await?;
        assert_eq!(
            single(renewal_id).await?,
            vec![Eligibility::Eligible],
            "{status}"
        );
    }
    sqlx::query(
        "UPDATE billing_payment_attempts SET status = 'pending', gateway_transaction_id = NULL WHERE id = $1",
    )
    .bind(renewal_id.as_uuid())
    .execute(&pool)
    .await?;
    assert_eq!(
        single(renewal_id).await?,
        vec![Eligibility::Ineligible(Ineligibility::Pending)],
        "pending takes precedence over a missing transaction ID"
    );
    sqlx::query("UPDATE billing_payment_attempts SET status = 'unknown' WHERE id = $1")
        .bind(renewal_id.as_uuid())
        .execute(&pool)
        .await?;
    assert_eq!(
        single(renewal_id).await?,
        vec![Eligibility::Ineligible(Ineligibility::NoTransactionId)]
    );
    // Leave a terminal failure without a transaction ID so recovery may run.
    sqlx::query("UPDATE billing_payment_attempts SET status = 'failed' WHERE id = $1")
        .bind(renewal_id.as_uuid())
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE billing_subscriptions SET status = 'past_due', next_payment_attempt_at = next_renewal_at WHERE id = $1",
    )
    .bind(subscription_id.as_uuid())
    .execute(&pool)
    .await?;
    let recovery_gateway = Arc::new(ScriptedGateway::new(Ok(approved_outcome_with_reference(
        Some("txn_eligible_recovery"),
        "vault_eligible_recovery",
    ))));
    let recovery = service(&fixture, &recovery_gateway)
        .recover(RecoverSubscriptionPayment::new(
            syrup_rail::SubscriptionPaymentContext::new(
                PaymentAttemptId::new(Uuid::now_v7()),
                fixture.command.billing_scope_id(),
                fixture.command.subscriber_id(),
                fixture.command.gateway_configuration_id(),
                IdempotencyKey::new("diag-recovery")?,
                PaymentToken::new("opaque-diag-recovery")?,
                named_contact(None),
            ),
            fixture.command.plan_key().clone(),
        ))
        .await?;
    let recovery_id = recovery.attempt().identity().attempt_id();

    // A prepared, unsubmitted replacement is not submitted even though it is
    // also pending.
    let prepared = replacement_command(&fixture, "diag-prepared", named_contact(None));
    let mut transaction = pool.begin().await?;
    let SubscriptionPaymentMethodReplacementReservationOutcome::Reserved(_, prepared_attempt) =
        reserve_subscription_payment_method_replacement_in_transaction(
            &mut transaction,
            &prepared,
            &scripted_resolved_gateway(
                fixture.gateway_account,
                Arc::new(ScriptedGateway::new(Ok(approved_outcome("txn_unused")))),
            ),
            GatewayAccountMode::Live,
        )
        .await?
    else {
        panic!("replacement should reserve");
    };
    transaction.commit().await?;
    let prepared_id = prepared_attempt.identity().attempt_id();

    // A host charge of the same subscriber has no plan.
    let host_charge_id = PaymentAttemptId::new(Uuid::now_v7());
    let target_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO billing_payment_attempts (
            id, billing_scope_id, subscriber_id, host_charge_target_id, attempt_kind,
            status, idempotency_key, request_fingerprint, amount_cents, currency,
            gateway_account_id, gateway_configuration_id, gateway_order_id,
            gateway_transaction_id, submitted_at, resolved_at,
            required_gateway_account_mode
        ) VALUES (
            $1, $2, $3, $4, 'host_charge', 'approved', $5, $6, 500, 'USD', $7, $8,
            $9, 'txn_host_charge', now(), now(), 'live'
        )
        "#,
    )
    .bind(host_charge_id.as_uuid())
    .bind(fixture.gateway_account.billing_scope_id)
    .bind(fixture.command.subscriber_id().as_uuid())
    .bind(target_id)
    .bind(format!("diag-host-{}", target_id.simple()))
    .bind(format!("host_charge:{target_id}:500:USD"))
    .bind(fixture.gateway_account.gateway_account_id)
    .bind(fixture.gateway_account.gateway_configuration_id)
    .bind(format!("diag-host-order-{}", target_id.simple()))
    .execute(&pool)
    .await?;

    let unknown_id = PaymentAttemptId::new(Uuid::now_v7());
    let eligibility = |ids: Vec<PaymentAttemptId>| {
        let pool = pool.clone();
        let scope = fixture.command.billing_scope_id();
        let subscriber = fixture.command.subscriber_id();
        let plan = fixture.command.plan_key().clone();
        async move {
            payment_attempt_diagnostic_eligibility(&pool, scope, subscriber, &plan, &ids)
                .await
                .map(|items| {
                    items
                        .into_iter()
                        .map(|item| (item.attempt_id(), item.eligibility()))
                        .collect::<Vec<_>>()
                })
        }
    };
    assert_eq!(
        eligibility(vec![
            unknown_id,
            initial_id,
            replacement_id,
            renewal_id,
            recovery_id,
            prepared_id,
            host_charge_id,
            initial_id,
        ])
        .await?,
        vec![
            (initial_id, Eligibility::Eligible),
            (replacement_id, Eligibility::Eligible),
            (
                renewal_id,
                Eligibility::Ineligible(Ineligibility::NoTransactionId),
            ),
            (recovery_id, Eligibility::Eligible),
            (
                prepared_id,
                Eligibility::Ineligible(Ineligibility::NotSubmitted)
            ),
            (
                host_charge_id,
                Eligibility::Ineligible(Ineligibility::UnsupportedKind)
            ),
        ],
        "unowned IDs are omitted and duplicates reported once in request order"
    );

    // Unpaid collection history stays diagnosable.
    sqlx::query(
        "UPDATE billing_subscriptions SET status = 'unpaid', unpaid_at = now(), next_payment_attempt_at = NULL WHERE id = $1",
    )
    .bind(subscription_id.as_uuid())
    .execute(&pool)
    .await?;
    assert_eq!(
        eligibility(vec![initial_id]).await?,
        vec![(initial_id, Eligibility::Eligible)]
    );
    let unpaid_gateway = DiagnosticsGateway::new(Ok(observed(GatewayDiagnosticOperation::Sale)));
    assert!(matches!(
        diagnose(&fixture, &unpaid_gateway, initial_id, DEADLINE).await?,
        Outcome::Observed(_)
    ));

    // Ownership and eligibility stop before any provider I/O.
    let untouched = DiagnosticsGateway::new(Ok(observed(GatewayDiagnosticOperation::Sale)));
    assert_eq!(
        diagnose(&fixture, &untouched, unknown_id, DEADLINE).await?,
        Outcome::NotFound
    );
    assert_eq!(
        query_payment_attempt_diagnostics(
            &pool,
            &resolver_for(fixture.gateway_account, &untouched),
            PaymentAttemptDiagnosticTarget::new(
                fixture.command.billing_scope_id(),
                fixture.command.subscriber_id(),
                syrup_rail::PlanKey::new("other_plan")?,
                initial_id,
            ),
            DEADLINE,
        )
        .await?,
        Outcome::NotFound
    );
    assert_eq!(
        query_payment_attempt_diagnostics(
            &pool,
            &resolver_for(fixture.gateway_account, &untouched),
            PaymentAttemptDiagnosticTarget::new(
                fixture.command.billing_scope_id(),
                SubscriberId::new(Uuid::now_v7()),
                fixture.command.plan_key().clone(),
                initial_id,
            ),
            DEADLINE,
        )
        .await?,
        Outcome::NotFound
    );
    for (attempt_id, reason) in [
        (prepared_id, Ineligibility::NotSubmitted),
        (host_charge_id, Ineligibility::UnsupportedKind),
        (renewal_id, Ineligibility::NoTransactionId),
    ] {
        assert_eq!(
            diagnose(&fixture, &untouched, attempt_id, DEADLINE).await?,
            Outcome::Ineligible(reason)
        );
    }
    assert_eq!(untouched.calls.load(Ordering::SeqCst), 0);
    assert!(matches!(
        eligibility(vec![initial_id; 101]).await,
        Err(PaymentAttemptDiagnosticsError::TooManyAttempts)
    ));
    fixture.cleanup().await
}

#[tokio::test]
async fn cooldowns_and_short_deadlines_stop_before_provider_io() -> Result<(), Box<dyn Error>> {
    let fixture = enrollment_fixture("diag_stop", false, false, false).await?;
    let pool = &fixture.database.pool;
    let initial = enroll(&fixture, "diag-stop", approved_outcome("txn_stop_initial")).await?;
    let attempt_id = initial.attempt().identity().attempt_id();
    let gateway = DiagnosticsGateway::new(Ok(observed(GatewayDiagnosticOperation::Sale)));

    for deadline in [Duration::from_millis(500), Duration::from_millis(3_500)] {
        assert_eq!(
            diagnose(&fixture, &gateway, attempt_id, deadline).await?,
            Outcome::TimedOut
        );
    }
    sqlx::query(
        "UPDATE billing_gateway_provider_rate_limits SET rate_limited_until = clock_timestamp() + interval '1 minute' WHERE provider_key = 'nmi'",
    )
    .execute(pool)
    .await?;
    assert_eq!(
        diagnose(&fixture, &gateway, attempt_id, DEADLINE).await?,
        Outcome::CooldownActive(GatewayMutationCooldownScope::Provider)
    );
    sqlx::query(
        "UPDATE billing_gateway_provider_rate_limits SET rate_limited_until = '-infinity' WHERE provider_key = 'nmi'",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "UPDATE billing_gateway_accounts SET mutation_rate_limited_until = clock_timestamp() + interval '1 minute' WHERE id = $1",
    )
    .bind(fixture.gateway_account.gateway_account_id)
    .execute(pool)
    .await?;
    assert_eq!(
        diagnose(&fixture, &gateway, attempt_id, DEADLINE).await?,
        Outcome::CooldownActive(GatewayMutationCooldownScope::Account)
    );
    assert_eq!(gateway.calls.load(Ordering::SeqCst), 0);
    fixture.cleanup().await
}

#[tokio::test]
async fn rate_limited_diagnostics_extend_the_provider_cooldown_at_the_deadline()
-> Result<(), Box<dyn Error>> {
    let fixture = enrollment_fixture("diag_rate", false, false, false).await?;
    let pool = fixture.database.pool.clone();
    let initial = enroll(&fixture, "diag-rate", approved_outcome("txn_rate_initial")).await?;
    let attempt_id = initial.attempt().identity().attempt_id();

    // The query uses nearly all of its 1.3-second budget, so the cooldown
    // write happens after the caller's I/O deadline and must still complete.
    let gateway = DiagnosticsGateway::with(
        Err(GatewayError::RateLimited(GatewayDiagnostic::new(
            "HTTP 429",
        ))),
        Duration::from_millis(1_200),
        None,
    );
    let started = std::time::Instant::now();
    assert_eq!(
        diagnose(&fixture, &gateway, attempt_id, Duration::from_millis(4_300)).await?,
        Outcome::RateLimited
    );
    assert!(started.elapsed() >= Duration::from_millis(1_200));
    assert!(provider_cooldown_remaining(&pool).await? > 50.0);

    // A cooldown that cannot be recorded is an error, not a silent outcome.
    sqlx::query(
        "UPDATE billing_gateway_provider_rate_limits SET rate_limited_until = '-infinity' WHERE provider_key = 'nmi'",
    )
    .execute(&pool)
    .await?;
    sqlx::raw_sql(
        r#"
        CREATE FUNCTION test_reject_provider_cooldown() RETURNS trigger
        LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected cooldown failure'; END; $$;
        CREATE TRIGGER test_reject_provider_cooldown
        BEFORE UPDATE ON billing_gateway_provider_rate_limits
        FOR EACH ROW EXECUTE FUNCTION test_reject_provider_cooldown();
        "#,
    )
    .execute(&pool)
    .await?;
    let failing = DiagnosticsGateway::new(Err(GatewayError::RateLimited(GatewayDiagnostic::new(
        "HTTP 429",
    ))));
    assert!(matches!(
        diagnose(&fixture, &failing, attempt_id, DEADLINE).await,
        Err(PaymentAttemptDiagnosticsError::RateLimitCooldownPersistenceFailed { .. })
    ));
    fixture.cleanup().await
}

/// Runs a rate-limited diagnostic query while a renewal is between its
/// readiness check and reservation, as a busy host could.
struct RacingDiagnosticsGateway {
    pool: sqlx::PgPool,
    diagnostics: Arc<DiagnosticsGateway>,
    account: crate::test_support::GatewayAccountFixture,
    target: PaymentAttemptDiagnosticTarget,
    sale_calls: AtomicUsize,
}

#[async_trait]
impl PaymentGateway for RacingDiagnosticsGateway {
    async fn account_mode(&self) -> Result<GatewayAccountMode, GatewayError> {
        let outcome = query_payment_attempt_diagnostics(
            &self.pool,
            &resolver_for(self.account, &self.diagnostics),
            self.target.clone(),
            DEADLINE,
        )
        .await
        .expect("diagnostic query should complete");
        assert_eq!(outcome, Outcome::RateLimited);
        Ok(GatewayAccountMode::Live)
    }

    async fn sale(
        &self,
        _request: GatewaySaleRequest,
    ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
        self.sale_calls.fetch_add(1, Ordering::SeqCst);
        panic!("the renewal must stop at the provider cooldown")
    }

    async fn store_payment_method(
        &self,
        _request: GatewayStorePaymentMethodRequest,
    ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
        panic!("renewal never stores a payment method")
    }

    async fn query_transaction(
        &self,
        _request: GatewayQueryRequest,
    ) -> Result<Option<GatewayPaymentOutcome>, GatewayError> {
        panic!("renewal does not query")
    }

    async fn query_transaction_reports(
        &self,
        _request: GatewayTransactionReportRequest,
    ) -> Result<Vec<GatewayTransactionReport>, GatewayError> {
        panic!("renewal does not read reports")
    }
}

#[tokio::test]
async fn rate_limited_diagnostics_fail_a_racing_renewal_before_submission()
-> Result<(), Box<dyn Error>> {
    let fixture = enrollment_fixture("diag_race", false, false, false).await?;
    let pool = fixture.database.pool.clone();
    let initial = enroll(&fixture, "diag-race", approved_outcome("txn_race_initial")).await?;
    let subscription_id = initial.subscription().expect("subscription").id();
    let period_start_at = make_renewal_due(&pool, subscription_id).await?;
    let racing = Arc::new(RacingDiagnosticsGateway {
        pool: pool.clone(),
        diagnostics: DiagnosticsGateway::new(Err(GatewayError::RateLimited(
            GatewayDiagnostic::new("HTTP 429"),
        ))),
        account: fixture.gateway_account,
        target: target(&fixture, initial.attempt().identity().attempt_id()),
        sale_calls: AtomicUsize::new(0),
    });
    let renewal_service = SubscriptionBillingService::new(
        pool.clone(),
        Arc::new(TestOfferStore),
        Arc::new(StaticResolver {
            gateway: scripted_resolved_gateway(fixture.gateway_account, Arc::clone(&racing)),
            calls: AtomicUsize::new(0),
        }),
        Arc::new(PermitAdmission {
            calls: AtomicUsize::new(0),
        }),
        Arc::new(fixture.coordinator.clone()),
    );
    let outcome = renewal_service
        .renew(ChargeRenewal::new(
            fixture.command.billing_scope_id(),
            subscription_id,
            period_start_at,
        ))
        .await?;
    assert!(matches!(outcome, SubscriptionRenewalOutcome::Noop));
    assert_eq!(racing.sale_calls.load(Ordering::SeqCst), 0);
    let (status, resolution_code): (String, Option<String>) = sqlx::query_as(
        "SELECT status, resolution_code FROM billing_payment_attempts WHERE subscription_id = $1 AND attempt_kind = 'subscription_renewal'",
    )
    .bind(subscription_id.as_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(status, "failed");
    assert_eq!(
        resolution_code.as_deref(),
        Some("gateway_provider_rate_limited_before_submission")
    );
    let subscription_status: String =
        sqlx::query_scalar("SELECT status FROM billing_subscriptions WHERE id = $1")
            .bind(subscription_id.as_uuid())
            .fetch_one(&pool)
            .await?;
    assert_eq!(
        subscription_status, "active",
        "the readiness failure is not past due"
    );
    fixture.cleanup().await
}

#[tokio::test]
async fn configuration_rotation_before_and_during_the_query() -> Result<(), Box<dyn Error>> {
    let fixture = enrollment_fixture("diag_rotate", false, false, false).await?;
    let pool = fixture.database.pool.clone();
    let initial = enroll(
        &fixture,
        "diag-rotate",
        approved_outcome("txn_rotate_initial"),
    )
    .await?;
    let attempt_id = initial.attempt().identity().attempt_id();

    // Rotation before the call: the current canonical configuration is
    // queried, never the attempt's historical one.
    let rotated = crate::test_support::GatewayAccountFixture {
        gateway_configuration_id: Uuid::now_v7(),
        ..fixture.gateway_account
    };
    sqlx::query("UPDATE billing_gateway_accounts SET gateway_configuration_id = $2 WHERE id = $1")
        .bind(rotated.gateway_account_id)
        .bind(rotated.gateway_configuration_id)
        .execute(&pool)
        .await?;
    let stale = DiagnosticsGateway::new(Ok(observed(GatewayDiagnosticOperation::Sale)));
    assert_eq!(
        diagnose(&fixture, &stale, attempt_id, DEADLINE).await?,
        Outcome::ConfigurationChanged,
        "a resolver bound to the historical configuration is not used"
    );
    assert_eq!(stale.calls.load(Ordering::SeqCst), 0);
    let current = DiagnosticsGateway::new(Ok(observed(GatewayDiagnosticOperation::Sale)));
    let Outcome::Observed(diagnostics) = query_payment_attempt_diagnostics(
        &pool,
        &resolver_for(rotated, &current),
        target(&fixture, attempt_id),
        DEADLINE,
    )
    .await?
    else {
        panic!("the current configuration should be queried");
    };
    assert_eq!(
        diagnostics.gateway_configuration_id().into_uuid(),
        rotated.gateway_configuration_id
    );
    assert_ne!(
        diagnostics.gateway_configuration_id(),
        initial.attempt().identity().gateway_configuration_id()
    );

    // Rotation during the query invalidates the observation.
    let hook_pool = pool.clone();
    let account_id = rotated.gateway_account_id;
    let during = DiagnosticsGateway::with(
        Ok(observed(GatewayDiagnosticOperation::Sale)),
        Duration::ZERO,
        Some(Box::pin(async move {
            sqlx::query(
                "UPDATE billing_gateway_accounts SET gateway_configuration_id = $2 WHERE id = $1",
            )
            .bind(account_id)
            .bind(Uuid::now_v7())
            .execute(&hook_pool)
            .await
            .expect("rotation during query");
        })),
    );
    assert_eq!(
        query_payment_attempt_diagnostics(
            &pool,
            &resolver_for(rotated, &during),
            target(&fixture, attempt_id),
            DEADLINE,
        )
        .await?,
        Outcome::ConfigurationChanged
    );
    fixture.cleanup().await
}

#[tokio::test]
async fn late_approval_rewrites_during_the_query_are_target_changes() -> Result<(), Box<dyn Error>>
{
    let fixture = enrollment_fixture("diag_target", false, false, false).await?;
    let pool = fixture.database.pool.clone();
    let declined_enrollment =
        enroll(&fixture, "diag-target", declined("txn_target_declined")).await?;
    assert_eq!(
        declined_enrollment.attempt().status(),
        PaymentAttemptStatus::Declined
    );
    let attempt_id = declined_enrollment.attempt().identity().attempt_id();

    for rewrite in [
        "UPDATE billing_payment_attempts SET status = 'review_required' WHERE id = $1",
        "UPDATE billing_payment_attempts SET status = 'declined', gateway_transaction_id = 'txn_target_rewritten' WHERE id = $1",
    ] {
        let hook_pool = pool.clone();
        let gateway = DiagnosticsGateway::with(
            Ok(observed(GatewayDiagnosticOperation::Sale)),
            Duration::ZERO,
            Some(Box::pin(async move {
                sqlx::query(rewrite)
                    .bind(attempt_id.as_uuid())
                    .execute(&hook_pool)
                    .await
                    .expect("late-approval rewrite during query");
            })),
        );
        assert_eq!(
            diagnose(&fixture, &gateway, attempt_id, DEADLINE).await?,
            Outcome::TargetChanged,
            "{rewrite}"
        );
    }
    fixture.cleanup().await
}

#[tokio::test]
async fn diagnostics_leave_a_concurrent_scrub_intact() -> Result<(), Box<dyn Error>> {
    let fixture = enrollment_fixture("diag_scrub", false, false, false).await?;
    let pool = fixture.database.pool.clone();
    let initial = enroll(
        &fixture,
        "diag-scrub",
        approved_outcome("txn_scrub_initial"),
    )
    .await?;
    let attempt_id = initial.attempt().identity().attempt_id();
    let scrub_pool = pool.clone();
    let scrub_command = ScrubSubscriberBillingData::new(
        fixture.command.billing_scope_id(),
        fixture.command.subscriber_id(),
    );
    // The scrub commits while the provider query is in flight; diagnostics
    // hold no database lock during I/O and write nothing afterwards.
    let gateway = DiagnosticsGateway::with(
        Ok(observed(GatewayDiagnosticOperation::Sale)),
        Duration::ZERO,
        Some(Box::pin(async move {
            let mut transaction = scrub_pool.begin().await.expect("scrub transaction");
            scrub_subscriber_billing_data(&mut transaction, scrub_command)
                .await
                .expect("scrub during query");
            transaction.commit().await.expect("scrub commit");
        })),
    );
    let before = canonical_rows(&pool).await?;
    let outcome = diagnose(&fixture, &gateway, attempt_id, DEADLINE).await?;
    // Scrubbed attempts keep their transaction ID and status, so the query
    // still completes; hosts must not diagnose scrubbed subscribers.
    assert!(matches!(outcome, Outcome::Observed(_)));
    let after = canonical_rows(&pool).await?;
    assert_ne!(after, before, "the scrub committed");
    let scrubbed: (Option<String>, Option<String>, String, Option<String>) = sqlx::query_as(
        "SELECT billing_first_name, billing_email, status, gateway_transaction_id FROM billing_payment_attempts WHERE id = $1",
    )
    .bind(attempt_id.as_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        scrubbed,
        (
            None,
            None,
            "approved".to_owned(),
            Some("txn_scrub_initial".to_owned())
        )
    );
    fixture.cleanup().await
}

#[tokio::test]
async fn provider_results_and_failures_map_to_typed_outcomes() -> Result<(), Box<dyn Error>> {
    let fixture = enrollment_fixture("diag_map", false, false, false).await?;
    let initial = enroll(&fixture, "diag-map", approved_outcome("txn_map_initial")).await?;
    let attempt_id = initial.attempt().identity().attempt_id();
    for (result, expected) in [
        (
            Ok(GatewayTransactionDiagnostics::Unsupported),
            Outcome::Unsupported,
        ),
        (
            Ok(GatewayTransactionDiagnostics::NotFound),
            Outcome::ProviderTransactionNotFound,
        ),
        (
            Ok(GatewayTransactionDiagnostics::Unavailable(
                Reason::AmbiguousAction,
            )),
            Outcome::Unavailable(Reason::AmbiguousAction),
        ),
        (
            Err(GatewayError::Unavailable(GatewayDiagnostic::new("timeout"))),
            Outcome::Unavailable(Reason::ProviderUnavailable),
        ),
        (
            Err(GatewayError::Malformed(GatewayDiagnostic::new("bad xml"))),
            Outcome::Unavailable(Reason::MalformedResponse),
        ),
    ] {
        let gateway = DiagnosticsGateway::new(result);
        assert_eq!(
            diagnose(&fixture, &gateway, attempt_id, DEADLINE).await?,
            expected
        );
    }

    for (outcome, expected) in [
        (Outcome::Ineligible(Ineligibility::Pending), "ineligible"),
        (Outcome::NotFound, "not_found"),
        (
            Outcome::ProviderTransactionNotFound,
            "provider_transaction_not_found",
        ),
        (Outcome::Unsupported, "unsupported"),
        (
            Outcome::CooldownActive(GatewayMutationCooldownScope::Provider),
            "cooldown_active",
        ),
        (Outcome::RateLimited, "rate_limited"),
        (Outcome::TimedOut, "timed_out"),
        (Outcome::ConfigurationChanged, "configuration_changed"),
        (Outcome::TargetChanged, "target_changed"),
        (
            Outcome::Unavailable(Reason::NoMatchingAction),
            "unavailable",
        ),
    ] {
        assert_eq!(outcome.as_str(), expected);
    }
    for (reason, expected) in [
        (Ineligibility::UnsupportedKind, "unsupported_kind"),
        (Ineligibility::NotSubmitted, "not_submitted"),
        (Ineligibility::Pending, "pending"),
        (Ineligibility::NoTransactionId, "no_transaction_id"),
    ] {
        assert_eq!(reason.as_str(), expected);
        assert_eq!(Eligibility::Ineligible(reason).as_str(), expected);
    }
    assert_eq!(Eligibility::Eligible.as_str(), "eligible");
    fixture.cleanup().await
}
