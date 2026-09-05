use std::error::Error;

use syrup_rail::{
    BillingScopeId, CurrencyCode, GatewayAccountId, GatewayAccountMode, GatewayConfigurationId,
    GatewayDiagnostic, GatewayPaymentDescriptor, GatewayTransactionId, OperatorReviewPageLimit,
    PlanKey, SubscriberId,
};

use super::*;
use crate::test_support::{GatewayAccountFixture, TestDatabase, create_gateway_account};

async fn insert_host_charge_attempt(
    pool: &PgPool,
    gateway: GatewayAccountFixture,
    gateway_order_id: &str,
) -> Result<PaymentAttemptId, sqlx::Error> {
    let attempt_id = PaymentAttemptId::new(Uuid::now_v7());
    let host_charge_target_id = Uuid::now_v7();
    sqlx::query(
        r#"
            INSERT INTO billing_payment_attempts (
                required_gateway_account_mode,
                id, billing_scope_id, subscriber_id, host_charge_target_id,
                attempt_kind, status, idempotency_key, request_fingerprint,
                amount_cents, currency, gateway_account_id,
                gateway_configuration_id, gateway_order_id
            ) VALUES (
                'live',
                $1, $2, $3, $4, 'host_charge', 'pending', $5, $6,
                100, 'USD', $7, $8, $9
            )
            "#,
    )
    .bind(attempt_id.as_uuid())
    .bind(gateway.billing_scope_id)
    .bind(Uuid::now_v7())
    .bind(host_charge_target_id)
    .bind(format!("charge-test-{attempt_id}"))
    .bind(format!("host_charge:{host_charge_target_id}:100:USD"))
    .bind(gateway.gateway_account_id)
    .bind(gateway.gateway_configuration_id)
    .bind(gateway_order_id)
    .execute(pool)
    .await?;
    Ok(attempt_id)
}

fn approved_evidence(transaction_id: &str) -> ProcessorEvidence {
    ProcessorEvidence::new(
        syrup_rail::ProcessorApprovalEvidence::Structured,
        Some(GatewayTransactionId::new(transaction_id).unwrap()),
        None,
        Some(GatewayDiagnostic::new("1")),
        Some(GatewayDiagnostic::new("100")),
        Some(GatewayDiagnostic::new("Approved")),
        Some(GatewayDiagnostic::new("complete")),
        GatewayPaymentDescriptor::default(),
    )
}

#[tokio::test]
async fn malformed_charge_maps_through_each_consumer_error_boundary() -> Result<(), Box<dyn Error>>
{
    let database = TestDatabase::start("rail_chg_codec").await?;
    let result = async {
        let gateway = create_gateway_account(&database.pool, "test_gateway").await?;
        let attempt_id =
            insert_host_charge_attempt(&database.pool, gateway, "malformed-charge-codec-order")
                .await?;
        let charge_id = ProcessorChargeId::new(Uuid::now_v7());
        sqlx::query(
            r#"
                INSERT INTO billing_processor_charges (
                    id, attempt_id, billing_scope_id, gateway_account_id,
                    gateway_order_id, gateway_transaction_id, charge_role,
                    progression_state, state_code, external_reversal_required_at,
                    attempt_kind, host_charge_target_id, amount_cents, currency
                )
                SELECT $1, id, billing_scope_id, gateway_account_id,
                    gateway_order_id, 'txn-malformed-charge-codec', 'primary',
                    'external_reversal_required', 'future_processor_charge_state',
                    clock_timestamp(), attempt_kind, host_charge_target_id,
                    amount_cents, currency
                FROM billing_payment_attempts
                WHERE id = $2
                "#,
        )
        .bind(charge_id.as_uuid())
        .bind(attempt_id.as_uuid())
        .execute(&database.pool)
        .await?;

        let mut transaction = database.pool.begin().await?;
        let charge_error = charge_by_id(&mut transaction, charge_id.into_uuid())
            .await
            .expect_err("charge storage must reject an unknown persisted state code");
        transaction.rollback().await?;
        assert!(matches!(
            &charge_error,
            ProcessorChargeStoreError::InvalidState("canonical operator review state is invalid")
        ));
        assert_eq!(
            charge_error.to_string(),
            "canonical operator review state is invalid"
        );

        let operator_error = crate::processor_charge_review_page(
            &database.pool,
            OperatorReviewPageLimit::new(1)?,
            None,
        )
        .await
        .expect_err("operator review must reject an unknown persisted state code");
        assert!(matches!(
            &operator_error,
            crate::OperatorReviewError::InvalidState("canonical operator review state is invalid")
        ));
        assert_eq!(
            operator_error.to_string(),
            "canonical operator review state is invalid"
        );
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

struct SubscriptionAttemptFixture {
    identity: PaymentAttemptIdentity,
    plan_key: PlanKey,
    gateway_order_id: GatewayOrderId,
}

async fn insert_subscription_attempt(
    pool: &PgPool,
    gateway: GatewayAccountFixture,
    attempt_kind: PaymentAttemptKind,
    gateway_order: &str,
) -> Result<SubscriptionAttemptFixture, Box<dyn Error>> {
    assert!(matches!(
        attempt_kind,
        PaymentAttemptKind::SubscriptionRecovery
            | PaymentAttemptKind::SubscriptionPaymentMethodUpdate
    ));
    let attempt_id = PaymentAttemptId::new(Uuid::now_v7());
    let subscriber_id = SubscriberId::new(Uuid::now_v7());
    let payment_method_id = Uuid::now_v7();
    let subscription_id = Uuid::now_v7();
    let plan_key = PlanKey::new("fallback-plan")?;
    let gateway_order_id = GatewayOrderId::from_correlation(gateway_order)?;
    sqlx::query(
        r#"
            WITH input AS (
                SELECT
                    $1::uuid AS attempt_id,
                    $2::uuid AS billing_scope_id,
                    $3::uuid AS subscriber_id,
                    $4::uuid AS gateway_account_id,
                    $5::uuid AS gateway_configuration_id,
                    $6::text AS plan_key,
                    $7::text AS gateway_order_id,
                    $8::text AS attempt_kind,
                    $9::uuid AS payment_method_id,
                    $10::uuid AS subscription_id,
                    clock_timestamp() AS period_start_at
            ), payment_method AS (
                INSERT INTO billing_payment_methods (
                    id, billing_scope_id, subscriber_id, gateway_account_id,
                    gateway_payment_method_reference, status
                )
                SELECT payment_method_id, billing_scope_id, subscriber_id,
                    gateway_account_id, 'method-' || payment_method_id::text, 'active'
                FROM input
                RETURNING id
            ), subscription AS (
                INSERT INTO billing_subscriptions (
            required_gateway_account_mode,
                    id, billing_scope_id, subscriber_id, plan_key, status,
                    gateway_account_id, payment_method_id, amount_cents, currency,
                    current_period_start_at, current_period_end_at, next_renewal_at,
                    initial_transaction_id, phase, recurring_period_kind,
                    recurring_period_count, dunning_retry_delays_seconds,
                    dunning_exhaustion, past_due_access, next_payment_attempt_at
                )
                SELECT 'live', subscription_id, billing_scope_id, subscriber_id, plan_key, 'active',
                    gateway_account_id, payment_method_id, 100, 'USD', period_start_at,
                    period_start_at + interval '30 days',
                    period_start_at + interval '30 days',
                    'initial-' || subscription_id::text, 'recurring',
                    'calendar_months', 1, ARRAY[]::bigint[],
                    'remain_past_due', 'suspend_immediately',
                    period_start_at + interval '30 days'
                FROM input CROSS JOIN payment_method
                RETURNING id
            )
            INSERT INTO billing_payment_attempts (
                required_gateway_account_mode,
                id, billing_scope_id, subscriber_id, plan_key, subscription_id,
                payment_method_id, attempt_kind, status, idempotency_key,
                request_fingerprint, amount_cents, currency, billing_period_start_at,
                billing_period_end_at, gateway_account_id, gateway_configuration_id,
                gateway_order_id, payment_method_update_expected_payment_method_id,
                payment_method_update_expected_initial_transaction_id,
                subscription_expected_payment_method_id,
                subscription_expected_initial_transaction_id, subscription_expected_status
            )
            SELECT 'live', attempt_id, billing_scope_id, subscriber_id, plan_key, subscription_id,
                payment_method_id, attempt_kind, 'pending', 'fallback-' || attempt_id::text,
                attempt_kind || ':' || subscription_id::text,
                CASE WHEN attempt_kind = 'subscription_payment_method_update' THEN 0 ELSE 100 END,
                'USD',
                CASE WHEN attempt_kind = 'subscription_recovery' THEN period_start_at END,
                CASE WHEN attempt_kind = 'subscription_recovery'
                    THEN period_start_at + interval '30 days' END,
                gateway_account_id, gateway_configuration_id, gateway_order_id,
                CASE WHEN attempt_kind = 'subscription_payment_method_update'
                    THEN payment_method_id END,
                CASE WHEN attempt_kind = 'subscription_payment_method_update'
                    THEN 'initial-' || subscription_id::text END,
                CASE WHEN attempt_kind = 'subscription_recovery' THEN payment_method_id END,
                CASE WHEN attempt_kind = 'subscription_recovery'
                    THEN 'initial-' || subscription_id::text END,
                CASE WHEN attempt_kind = 'subscription_recovery' THEN 'active' END
            FROM input CROSS JOIN subscription
            "#,
    )
    .bind(attempt_id.as_uuid())
    .bind(gateway.billing_scope_id)
    .bind(subscriber_id.as_uuid())
    .bind(gateway.gateway_account_id)
    .bind(gateway.gateway_configuration_id)
    .bind(plan_key.as_str())
    .bind(gateway_order_id.expose())
    .bind(attempt_kind.as_str())
    .bind(payment_method_id)
    .bind(subscription_id)
    .execute(pool)
    .await?;

    Ok(SubscriptionAttemptFixture {
        identity: PaymentAttemptIdentity::new(
            attempt_id,
            BillingScopeId::new(gateway.billing_scope_id),
            subscriber_id,
            GatewayAccountId::new(gateway.gateway_account_id),
            GatewayConfigurationId::new(gateway.gateway_configuration_id),
            GatewayAccountMode::Live,
        ),
        plan_key,
        gateway_order_id,
    })
}

fn lock_free_terms(
    fixture: &SubscriptionAttemptFixture,
    attempt_kind: PaymentAttemptKind,
) -> LockFreeApprovedEvidenceTerms<'_> {
    let amount_cents = if attempt_kind == PaymentAttemptKind::SubscriptionPaymentMethodUpdate {
        0
    } else {
        100
    };
    LockFreeApprovedEvidenceTerms::subscription(
        fixture.identity,
        attempt_kind,
        &fixture.plan_key,
        &fixture.gateway_order_id,
        amount_cents,
        CurrencyCode::new("USD").expect("test currency"),
    )
}

#[tokio::test]
async fn lock_free_subscription_evidence_preserves_replay_and_ownership()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("rail_lock_fb").await?;
    let result = async {
            let gateway = create_gateway_account(&database.pool, "test_gateway").await?;
            let recovery = insert_subscription_attempt(
                &database.pool,
                gateway,
                PaymentAttemptKind::SubscriptionRecovery,
                "recovery-fallback-order",
            )
            .await?;
            let evidence = approved_evidence("txn_recovery_fallback");
            assert_eq!(
                persist_approved_evidence_without_attempt_lock(
                    &database.pool,
                    lock_free_terms(&recovery, PaymentAttemptKind::SubscriptionRecovery),
                    &evidence,
                )
                .await?,
                LockFreeApprovedEvidenceOutcome::Persisted
            );
            assert_eq!(
                persist_approved_evidence_without_attempt_lock(
                    &database.pool,
                    lock_free_terms(&recovery, PaymentAttemptKind::SubscriptionRecovery),
                    &evidence,
                )
                .await?,
                LockFreeApprovedEvidenceOutcome::ExactReplay
            );
            let classification_drift = evidence
                .clone()
                .with_approval_evidence(syrup_rail::ProcessorApprovalEvidence::TextOnly);
            assert_eq!(
                persist_approved_evidence_without_attempt_lock(
                    &database.pool,
                    lock_free_terms(&recovery, PaymentAttemptKind::SubscriptionRecovery),
                    &classification_drift,
                )
                .await?,
                LockFreeApprovedEvidenceOutcome::NotDurable
            );
            let owner = insert_subscription_attempt(
                &database.pool,
                gateway,
                PaymentAttemptKind::SubscriptionPaymentMethodUpdate,
                "replacement-owner-order",
            )
            .await?;
            let contender = insert_subscription_attempt(
                &database.pool,
                gateway,
                PaymentAttemptKind::SubscriptionPaymentMethodUpdate,
                "replacement-contender-order",
            )
            .await?;
            let evidence = approved_evidence("txn_replacement_owner");
            assert_eq!(
                persist_approved_evidence_without_attempt_lock(
                    &database.pool,
                    lock_free_terms(
                        &owner,
                        PaymentAttemptKind::SubscriptionPaymentMethodUpdate,
                    ),
                    &evidence,
                )
                .await?,
                LockFreeApprovedEvidenceOutcome::Persisted
            );
            assert_eq!(
                persist_approved_evidence_without_attempt_lock(
                    &database.pool,
                    lock_free_terms(
                        &contender,
                        PaymentAttemptKind::SubscriptionPaymentMethodUpdate,
                    ),
                    &evidence,
                )
                .await?,
                LockFreeApprovedEvidenceOutcome::OwnedByOtherAttempt
            );
            let dimensions: (String, Option<Uuid>, i32, String, String) = sqlx::query_as(
                "SELECT attempt_kind, host_charge_target_id, amount_cents, currency, progression_state FROM billing_processor_charges WHERE attempt_id = $1",
            )
            .bind(owner.identity.attempt_id().as_uuid())
            .fetch_one(&database.pool)
            .await?;
            assert_eq!(
                dimensions,
                (
                    "subscription_payment_method_update".to_owned(),
                    None,
                    0,
                    "USD".to_owned(),
                    "pending".to_owned(),
                )
            );
            let contender_charges: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM billing_processor_charges WHERE attempt_id = $1",
            )
            .bind(contender.identity.attempt_id().as_uuid())
            .fetch_one(&database.pool)
            .await?;
            assert_eq!(contender_charges, 0);
            Ok::<_, Box<dyn Error>>(())
        }
        .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn compensating_store_retries_transient_database_failures() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("rail_chg_retry").await?;
    let result = async {
        let gateway = create_gateway_account(&database.pool, "test_gateway").await?;
        let attempt_id = insert_host_charge_attempt(&database.pool, gateway, "retry-order").await?;
        sqlx::raw_sql(
            r#"
                CREATE SEQUENCE processor_charge_retry_sequence;
                CREATE FUNCTION fail_first_processor_charge_writes()
                RETURNS trigger LANGUAGE plpgsql AS $$
                BEGIN
                    IF nextval('processor_charge_retry_sequence') <= 2 THEN
                        RAISE EXCEPTION 'transient test failure' USING ERRCODE = '40001';
                    END IF;
                    RETURN NEW;
                END;
                $$;
                CREATE TRIGGER fail_first_processor_charge_writes
                BEFORE INSERT ON billing_processor_charges
                FOR EACH ROW EXECUTE FUNCTION fail_first_processor_charge_writes();
                "#,
        )
        .execute(&database.pool)
        .await?;

        let order = GatewayOrderId::from_correlation("retry-order")?;
        let outcome = store_compensating_processor_charge(
            &database.pool,
            attempt_id,
            &order,
            &approved_evidence("txn_retry"),
        )
        .await?;
        assert_eq!(outcome, CompensatingProcessorChargeOutcome::Observed);
        let attempts: i64 =
            sqlx::query_scalar("SELECT last_value::bigint FROM processor_charge_retry_sequence")
                .fetch_one(&database.pool)
                .await?;
        assert_eq!(attempts, 3);
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM billing_processor_charges WHERE attempt_id = $1",
        )
        .bind(attempt_id.as_uuid())
        .fetch_one(&database.pool)
        .await?;
        assert_eq!(count, 1);
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn transaction_observation_preserves_exact_replay_and_rejects_drift()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("rail_charge_tx").await?;
    let result = async {
        let gateway = create_gateway_account(&database.pool, "test_gateway").await?;
        let attempt_id = insert_host_charge_attempt(&database.pool, gateway, "tx-order").await?;
        let order = GatewayOrderId::from_correlation("tx-order")?;
        let evidence = approved_evidence("txn_transactional");
        let mut transaction = database.pool.begin().await?;
        let observed = observe_processor_charge_in_transaction(
            &mut transaction,
            attempt_id,
            &order,
            &evidence,
            ProcessorChargeProgression::ReconciliationRequired,
        )
        .await?;
        assert!(matches!(
            observed,
            ProcessorChargeObservationOutcome::Observed(_)
        ));
        let replay = observe_processor_charge_in_transaction(
            &mut transaction,
            attempt_id,
            &order,
            &evidence,
            ProcessorChargeProgression::ReconciliationRequired,
        )
        .await?;
        assert!(matches!(
            replay,
            ProcessorChargeObservationOutcome::ExactReplay(_)
        ));
        let classification_drift = evidence
            .clone()
            .with_approval_evidence(syrup_rail::ProcessorApprovalEvidence::TextOnly);
        let drift = observe_processor_charge_in_transaction(
            &mut transaction,
            attempt_id,
            &order,
            &classification_drift,
            ProcessorChargeProgression::ReconciliationRequired,
        )
        .await;
        assert!(matches!(
            drift,
            Err(ProcessorChargeStoreError::InvalidState(
                "processor charge replay evidence changed"
            ))
        ));
        let mut changed = approved_evidence("txn_transactional");
        changed = ProcessorEvidence::new(
            syrup_rail::ProcessorApprovalEvidence::Structured,
            changed.transaction_id().cloned(),
            changed.payment_method_reference().cloned(),
            changed.response().cloned(),
            changed.response_code().cloned(),
            Some(GatewayDiagnostic::new("Changed")),
            changed.condition().cloned(),
            changed.descriptor().clone(),
        );
        let drift = observe_processor_charge_in_transaction(
            &mut transaction,
            attempt_id,
            &order,
            &changed,
            ProcessorChargeProgression::ReconciliationRequired,
        )
        .await;
        assert!(matches!(
            drift,
            Err(ProcessorChargeStoreError::InvalidState(
                "processor charge replay evidence changed"
            ))
        ));
        transaction.rollback().await?;
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn transactionless_charge_identification_requires_matching_approval_classification()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("rail_chg_class").await?;
    let result = async {
        let gateway = create_gateway_account(&database.pool, "test_gateway").await?;
        let attempt_id = insert_host_charge_attempt(&database.pool, gateway, "class-order").await?;
        let order = GatewayOrderId::from_correlation("class-order")?;
        let mut transactionless = approved_evidence("unused_transaction");
        transactionless = ProcessorEvidence::new(
            transactionless.approval_evidence(),
            None,
            transactionless.payment_method_reference().cloned(),
            transactionless.response().cloned(),
            transactionless.response_code().cloned(),
            transactionless.response_text().cloned(),
            transactionless.condition().cloned(),
            transactionless.descriptor().clone(),
        );
        let mut transaction = database.pool.begin().await?;
        observe_processor_charge_in_transaction(
            &mut transaction,
            attempt_id,
            &order,
            &transactionless,
            ProcessorChargeProgression::ReconciliationRequired,
        )
        .await?;

        let classification_drift = approved_evidence("txn_classification_drift")
            .with_approval_evidence(syrup_rail::ProcessorApprovalEvidence::TextOnly);
        observe_processor_charge_in_transaction(
            &mut transaction,
            attempt_id,
            &order,
            &classification_drift,
            ProcessorChargeProgression::ReconciliationRequired,
        )
        .await?;

        let charges: Vec<(Option<String>, String)> = sqlx::query_as(
            "SELECT gateway_transaction_id, gateway_approval_evidence \
             FROM billing_processor_charges WHERE attempt_id = $1 \
             ORDER BY gateway_transaction_id NULLS FIRST",
        )
        .bind(attempt_id.as_uuid())
        .fetch_all(&mut *transaction)
        .await?;
        assert_eq!(
            charges,
            vec![
                (None, "structured".to_owned()),
                (
                    Some("txn_classification_drift".to_owned()),
                    "text_only".to_owned()
                ),
            ]
        );
        transaction.rollback().await?;
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[test]
fn charge_retry_policy_excludes_pool_and_non_database_errors() {
    for (codes, expected) in [
        (&["40001", "40P01", "55P03", "57014"][..], true),
        (
            &["00000", "08006", "23505", "23514", "42P01", "XX000"][..],
            false,
        ),
    ] {
        for &code in codes {
            assert_eq!(
                is_transient(&ProcessorChargeStoreError::Sql(
                    crate::test_support::sqlstate_error(code)
                )),
                expected,
                "direct {code}"
            );
            assert_eq!(
                is_transient(&ProcessorChargeStoreError::Attempt(
                    PaymentAttemptStoreError::Sql(crate::test_support::sqlstate_error(code))
                )),
                expected,
                "nested {code}"
            );
        }
    }
    for error in [sqlx::Error::PoolTimedOut, sqlx::Error::RowNotFound] {
        assert!(!is_transient(&ProcessorChargeStoreError::Sql(error)));
    }
    assert!(!is_transient(&ProcessorChargeStoreError::Attempt(
        PaymentAttemptStoreError::Sql(sqlx::Error::PoolTimedOut),
    )));
}

#[test]
fn role_decoders_share_labels_and_preserve_error_context() {
    use crate::processor_charge_persistence::{self, ProcessorChargePersistenceError};

    for (label, role) in [
        ("primary", ProcessorChargeRole::Primary),
        ("additional", ProcessorChargeRole::Additional),
    ] {
        assert_eq!(parse_role(label).unwrap(), role);
        assert_eq!(
            processor_charge_persistence::parse_role(label).unwrap(),
            role
        );
    }
    for label in ["", "Primary", " primary", "unknown"] {
        assert!(matches!(
            parse_role(label),
            Err(ProcessorChargeStoreError::InvalidState(
                "canonical processor charge state is invalid"
            ))
        ));
        assert!(matches!(
            processor_charge_persistence::parse_role(label),
            Err(ProcessorChargePersistenceError::InvalidState(
                "canonical operator review state is invalid"
            ))
        ));
    }
}
