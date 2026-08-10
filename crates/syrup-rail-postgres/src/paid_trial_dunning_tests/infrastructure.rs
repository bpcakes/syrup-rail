use super::*;
use syrup_rail::PaymentResolutionCode;

#[tokio::test]
async fn infrastructure_failure_is_paced_for_twenty_four_hours_without_consuming_dunning()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("pt_infra").await?;
    let account = create_gateway_account(&database.pool, "nmi").await?;
    let gateway = resolved_gateway(account)?;
    let subscriber_id = SubscriberId::new(Uuid::now_v7());
    let payment_method_id = Uuid::now_v7();
    let subscription_id = syrup_rail::SubscriptionId::new(Uuid::now_v7());
    sqlx::query(
        r#"
        INSERT INTO billing_payment_methods (
            id, billing_scope_id, subscriber_id, gateway_account_id,
            gateway_payment_method_reference, status
        ) VALUES ($1, $2, $3, $4, $5, 'active')
        "#,
    )
    .bind(payment_method_id)
    .bind(account.billing_scope_id)
    .bind(subscriber_id.as_uuid())
    .bind(account.gateway_account_id)
    .bind(format!("vault_{}", payment_method_id.simple()))
    .execute(&database.pool)
    .await?;
    let due_at: DateTime<Utc> = sqlx::query_scalar(
        r#"
        WITH clock AS MATERIALIZED (SELECT clock_timestamp() AS observed_at)
        INSERT INTO billing_subscriptions (
            id, billing_scope_id, subscriber_id, plan_key, status,
            gateway_account_id, payment_method_id, amount_cents, currency,
            current_period_start_at, current_period_end_at, next_renewal_at,
            initial_transaction_id, phase, recurring_period_kind,
            recurring_period_count, dunning_retry_delays_seconds,
            dunning_exhaustion, past_due_access, next_payment_attempt_at
        ) SELECT
            $1, $2, $3, 'identity_pro', 'active', $4, $5, 2900, 'USD',
            observed_at - interval '1 month' - interval '1 hour',
            observed_at - interval '1 hour', observed_at - interval '1 hour',
            $6, 'recurring', 'calendar_months', 1, ARRAY[60]::bigint[],
            'mark_unpaid', 'continue_until_dunning_exhausted',
            observed_at - interval '1 hour'
        FROM clock
        RETURNING next_renewal_at
        "#,
    )
    .bind(subscription_id.as_uuid())
    .bind(account.billing_scope_id)
    .bind(subscriber_id.as_uuid())
    .bind(account.gateway_account_id)
    .bind(payment_method_id)
    .bind(format!("initial_{}", subscription_id.as_uuid().simple()))
    .fetch_one(&database.pool)
    .await?;
    let attempt_id = Uuid::now_v7();
    sqlx::query(
        r#"
        WITH clock AS MATERIALIZED (SELECT clock_timestamp() AS observed_at)
        INSERT INTO billing_payment_attempts (
            id, billing_scope_id, subscriber_id, plan_key, subscription_id,
            payment_method_id, attempt_kind, status, idempotency_key,
            request_fingerprint, amount_cents, currency,
            billing_period_start_at, billing_period_end_at,
            gateway_account_id, gateway_configuration_id, gateway_order_id,
            resolution_code, resolved_at, created_at, updated_at,
            subscription_expected_payment_method_id,
            subscription_expected_initial_transaction_id,
            subscription_expected_status
        ) SELECT
            $1, $2, $3, 'identity_pro', $4, $5, 'subscription_renewal',
            'failed', $6, $7, 2900, 'USD', $8, $8 + interval '1 month',
            $9, $10, $11, $12, observed_at,
            observed_at - interval '25 hours', observed_at,
            $5, subscriptions.initial_transaction_id, 'active'
        FROM clock
        JOIN billing_subscriptions AS subscriptions ON subscriptions.id = $4
        "#,
    )
    .bind(attempt_id)
    .bind(account.billing_scope_id)
    .bind(subscriber_id.as_uuid())
    .bind(subscription_id.as_uuid())
    .bind(payment_method_id)
    .bind(format!("infra_{}", attempt_id.simple()))
    .bind(format!("infra_fingerprint_{}", attempt_id.simple()))
    .bind(due_at)
    .bind(account.gateway_account_id)
    .bind(account.gateway_configuration_id)
    .bind(format!("infra_order_{}", attempt_id.simple()))
    .bind(PaymentResolutionCode::GatewayLiveReadinessFailedBeforeSubmission.as_str())
    .execute(&database.pool)
    .await?;

    let mut transaction = database.pool.begin().await?;
    let state =
        crate::renewal_attempt_state(&mut transaction, subscription_id, due_at, None).await?;
    transaction.rollback().await?;
    assert_eq!(state.automatic_infrastructure_attempt_count, 1);
    assert!(state.last_automatic_infrastructure_failure_at.is_some());
    assert!(due_renewals(&database.pool).await?.is_empty());

    let resolver = Arc::new(CountingResolver {
        gateway,
        calls: AtomicUsize::new(0),
    });
    let service = SubscriptionBillingService::new(
        database.pool.clone(),
        Arc::new(StaticOfferStore {
            offer: paid_trial_offer()?,
        }),
        resolver.clone(),
        Arc::new(PermitAdmission),
        Arc::new(TestCoordinator {
            pool: database.pool.clone(),
            events: Arc::new(Mutex::new(Vec::new())),
        }),
    );
    let command = ChargeRenewal::new(
        BillingScopeId::new(account.billing_scope_id),
        subscription_id,
        due_at,
    );
    assert!(matches!(
        service.renew(command).await?,
        SubscriptionRenewalOutcome::Noop
    ));
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 0);

    sqlx::query(
        r#"
        UPDATE billing_payment_attempts
        SET resolved_at = clock_timestamp() - interval '24 hours',
            updated_at = clock_timestamp()
        WHERE id = $1
        "#,
    )
    .bind(attempt_id)
    .execute(&database.pool)
    .await?;
    let due = due_renewals(&database.pool).await?;
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].subscription_id(), subscription_id);
    let status: String =
        sqlx::query_scalar("SELECT status FROM billing_subscriptions WHERE id = $1")
            .bind(subscription_id.as_uuid())
            .fetch_one(&database.pool)
            .await?;
    assert_eq!(status, "active");

    database.cleanup().await
}
