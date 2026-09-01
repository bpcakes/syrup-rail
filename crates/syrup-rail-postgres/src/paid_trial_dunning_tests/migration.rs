use super::*;

use syrup_rail::{
    PaymentAttemptFingerprint, PaymentMethodId, SubscriptionId, SubscriptionRecoveryReservation,
    SubscriptionRecoveryReservationRejection,
};

use crate::schema_contract::{
    V1_TO_V2_UPGRADE_SQL, assert_v2_conforms, assert_v3_conforms, assert_v4_conforms,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LegacyRecoveryState {
    Prepared,
    SubmittedPending,
    Unknown,
    ReviewRequired,
}

impl LegacyRecoveryState {
    const fn label(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::SubmittedPending => "submitted_pending",
            Self::Unknown => "unknown",
            Self::ReviewRequired => "review_required",
        }
    }

    const fn status(self) -> &'static str {
        match self {
            Self::Prepared | Self::SubmittedPending => "pending",
            Self::Unknown => "unknown",
            Self::ReviewRequired => "review_required",
        }
    }

    const fn submitted(self) -> bool {
        !matches!(self, Self::Prepared)
    }
}

struct LegacyRecoveryFixture {
    state: LegacyRecoveryState,
    subscriber_id: SubscriberId,
    subscription_id: SubscriptionId,
    attempt_id: PaymentAttemptId,
    period_start_at: DateTime<Utc>,
}

#[tokio::test]
async fn v1_active_recovery_authority_survives_v2_v3_and_v4_cutovers() -> Result<(), Box<dyn Error>>
{
    let database = TestDatabase::start_v1("pt_v1_recovery").await?;
    let result = async {
        let account = create_gateway_account(&database.pool, "nmi").await?;
        let gateway = resolved_gateway(account)?;
        let states = [
            LegacyRecoveryState::Prepared,
            LegacyRecoveryState::SubmittedPending,
            LegacyRecoveryState::Unknown,
            LegacyRecoveryState::ReviewRequired,
        ];
        let mut fixtures = Vec::with_capacity(states.len());
        for state in states {
            fixtures.push(insert_v1_active_recovery(&database.pool, account, state).await?);
        }

        let mut transaction = database.pool.begin().await?;
        sqlx::raw_sql(V1_TO_V2_UPGRADE_SQL)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        assert_v2_conforms(&database.pool).await?;
        database.upgrade_v2_to_v3().await?;
        assert_v3_conforms(&database.pool).await?;
        database.upgrade_v3_to_v4().await?;
        assert_v4_conforms(&database.pool).await?;

        let events = Arc::new(Mutex::new(Vec::new()));
        let coordinator = TestCoordinator {
            pool: database.pool.clone(),
            events: Arc::clone(&events),
        };

        for fixture in &fixtures {
            let transaction_id = format!("legacy_recovery_{}", fixture.state.label());
            let payment_method_reference = format!("vault_{}", fixture.state.label());
            let payment = if fixture.state == LegacyRecoveryState::Prepared {
                let mut transaction = database.pool.begin().await?;
                let attempt = find_payment_attempt_by_id_in_transaction(
                    &mut transaction,
                    BillingScopeId::new(account.billing_scope_id),
                    fixture.attempt_id,
                )
                .await?
                .expect("upgraded prepared recovery remains durable");
                transaction.rollback().await?;
                let reservation = SubscriptionRecoveryReservation::from_attempt(
                    &attempt,
                    GatewayProviderKey::new("nmi")?,
                )?;
                assert!(matches!(
                    admit_subscription_recovery_submission(&database.pool, &reservation).await?,
                    SubscriptionRecoveryAdmissionOutcome::Admitted(_)
                ));
                apply_subscription_recovery_gateway_outcome(
                    &database.pool,
                    &coordinator,
                    &reservation,
                    &approved_outcome_with_reference(&transaction_id, &payment_method_reference),
                )
                .await?
            } else {
                apply_reconciled_subscription_recovery_gateway_outcome(
                    &database.pool,
                    &coordinator,
                    BillingScopeId::new(account.billing_scope_id),
                    fixture.attempt_id,
                    &approved_outcome_with_reference(&transaction_id, &payment_method_reference),
                )
                .await?
            };

            assert_eq!(
                payment.attempt().status(),
                syrup_rail::PaymentAttemptStatus::Approved,
                "legacy {:?} recovery did not resolve",
                fixture.state,
            );
            let subscription = payment
                .subscription()
                .expect("approved legacy recovery advances the subscription");
            assert_eq!(subscription.id(), fixture.subscription_id);
            assert_eq!(subscription.status(), SubscriptionStatus::Active);
            assert_eq!(
                subscription.current_period().start_at(),
                &fixture.period_start_at,
            );
            assert_eq!(
                subscription.next_payment_attempt_at(),
                Some(subscription.current_period().end_at()),
            );
        }

        assert_eq!(events.lock().await.len(), fixtures.len());

        let first = &fixtures[0];
        let fresh_command = RecoverSubscriptionPayment::new(
            syrup_rail::SubscriptionPaymentContext::new(
                PaymentAttemptId::new(Uuid::now_v7()),
                BillingScopeId::new(account.billing_scope_id),
                first.subscriber_id,
                GatewayConfigurationId::new(account.gateway_configuration_id),
                IdempotencyKey::new("fresh-active-recovery-after-cutover")?,
                PaymentToken::new("opaque-fresh-recovery-token")?,
                BillingContact::new(None, None, Some("fresh@example.test".to_owned()))?,
            ),
            PlanKey::new("base_subscription")?,
        );
        let mut transaction = database.pool.begin().await?;
        let fresh = reserve_subscription_recovery_in_transaction(
            &mut transaction,
            &fresh_command,
            &gateway,
            GatewayAccountMode::Live,
        )
        .await?;
        transaction.rollback().await?;
        assert_eq!(
            fresh,
            syrup_rail::SubscriptionRecoveryReservationOutcome::Rejected(
                SubscriptionRecoveryReservationRejection::SubscriptionNotFound,
            ),
            "honoring durable v1 authority must not reopen active recovery",
        );

        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

async fn insert_v1_active_recovery(
    pool: &PgPool,
    account: GatewayAccountFixture,
    state: LegacyRecoveryState,
) -> Result<LegacyRecoveryFixture, Box<dyn Error>> {
    let subscriber_id = SubscriberId::new(Uuid::now_v7());
    let payment_method_id = PaymentMethodId::new(Uuid::now_v7());
    let subscription_id = SubscriptionId::new(Uuid::now_v7());
    let attempt_id = PaymentAttemptId::new(Uuid::now_v7());
    let label = state.label();

    sqlx::query(
        r#"
        INSERT INTO billing_payment_methods (
            id, billing_scope_id, subscriber_id, gateway_account_id,
            gateway_payment_method_reference, status
        ) VALUES ($1, $2, $3, $4, $5, 'active')
        "#,
    )
    .bind(payment_method_id.as_uuid())
    .bind(account.billing_scope_id)
    .bind(subscriber_id.as_uuid())
    .bind(account.gateway_account_id)
    .bind(format!("legacy_vault_{label}"))
    .execute(pool)
    .await?;

    let period_start_at: DateTime<Utc> =
        sqlx::query_scalar("SELECT clock_timestamp() - interval '1 second'")
            .fetch_one(pool)
            .await?;
    let period = syrup_rail::next_billing_period(
        period_start_at,
        SubscriptionPeriodRule::calendar_months(1)?,
    )?;
    let initial_transaction_id = format!("legacy_initial_{label}");
    sqlx::query(
        r#"
        INSERT INTO billing_subscriptions (
            id, billing_scope_id, subscriber_id, plan_key, status,
            gateway_account_id, payment_method_id, amount_cents, currency,
            current_period_start_at, current_period_end_at, next_renewal_at,
            initial_transaction_id
        ) VALUES (
            $1, $2, $3, 'base_subscription', 'active', $4, $5, 100, 'USD',
            $6 - interval '30 days', $6, $6, $7
        )
        "#,
    )
    .bind(subscription_id.as_uuid())
    .bind(account.billing_scope_id)
    .bind(subscriber_id.as_uuid())
    .bind(account.gateway_account_id)
    .bind(payment_method_id.as_uuid())
    .bind(period_start_at)
    .bind(&initial_transaction_id)
    .execute(pool)
    .await?;

    let fingerprint = PaymentAttemptFingerprint::for_subscription_recovery(
        &PlanKey::new("base_subscription")?,
        subscription_id,
        payment_method_id,
        period_start_at,
        ChargeAmount::new(100, CurrencyCode::new("USD")?)?.money(),
    );
    let order_value = format!("legacy-recovery-{label}");
    let order_id = GatewayOrderId::from_correlation(&order_value)?;
    sqlx::query(
        r#"
        INSERT INTO billing_payment_attempts (
            id, billing_scope_id, subscriber_id, plan_key,
            subscription_id, payment_method_id, attempt_kind, status,
            idempotency_key, request_fingerprint, amount_cents, currency,
            billing_period_start_at, billing_period_end_at,
            gateway_account_id, gateway_configuration_id, gateway_order_id,
            submitted_at, subscription_expected_payment_method_id,
            subscription_expected_initial_transaction_id,
            subscription_expected_status
        ) VALUES (
            $1, $2, $3, 'base_subscription', $4, $5,
            'subscription_recovery', $6, $7, $8, 100, 'USD', $9, $10,
            $11, $12, $13,
            CASE WHEN $14 THEN clock_timestamp() ELSE NULL END,
            $5, $15, 'active'
        )
        "#,
    )
    .bind(attempt_id.as_uuid())
    .bind(account.billing_scope_id)
    .bind(subscriber_id.as_uuid())
    .bind(subscription_id.as_uuid())
    .bind(payment_method_id.as_uuid())
    .bind(state.status())
    .bind(format!("legacy-recovery-{label}"))
    .bind(fingerprint.expose())
    .bind(period.start_at())
    .bind(period.end_at())
    .bind(account.gateway_account_id)
    .bind(account.gateway_configuration_id)
    .bind(order_id.expose())
    .bind(state.submitted())
    .bind(&initial_transaction_id)
    .execute(pool)
    .await?;

    Ok(LegacyRecoveryFixture {
        state,
        subscriber_id,
        subscription_id,
        attempt_id,
        period_start_at,
    })
}
