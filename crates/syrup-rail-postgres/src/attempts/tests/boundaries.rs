use std::{io, time::Duration as StdDuration};

use super::*;

#[tokio::test]
async fn active_grant_blocks_only_its_exact_plan() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("enroll_grant").await?;
    install_host_offers(&database).await?;
    let account = create_gateway_account(&database.pool, "nmi").await?;
    set_offer(&database, account.billing_scope_id, "basic", 1_000).await?;
    set_offer(&database, account.billing_scope_id, "premium", 2_000).await?;
    let subscriber_id = Uuid::now_v7();
    sqlx::query(
        r#"
            INSERT INTO billing_subscription_grants (
                id, billing_scope_id, subscriber_id, plan_key, grant_kind,
                reason, starts_at, ends_at, granted_by_actor_id
            ) VALUES (
                $1, $2, $3, 'basic', 'promotion', 'launch',
                clock_timestamp() - interval '1 minute',
                clock_timestamp() + interval '1 day', $4
            )
            "#,
    )
    .bind(Uuid::now_v7())
    .bind(account.billing_scope_id)
    .bind(subscriber_id)
    .bind(Uuid::now_v7())
    .execute(&database.pool)
    .await?;
    let gateway = resolved_gateway(account);

    let basic = SubscriptionEnrollmentReservation::from_command(
        &enrollment_command(
            account,
            subscriber_id,
            Uuid::now_v7(),
            "basic-grant",
            full_price("basic", 1_000),
        ),
        &gateway,
    )?;
    let mut transaction = database.pool.begin().await?;
    assert_eq!(
        reserve_subscription_enrollment_in_transaction(&mut transaction, &TestOfferStore, &basic,)
            .await?,
        SubscriptionEnrollmentReservationOutcome::Rejected(
            SubscriptionEnrollmentReservationRejection::ActiveGrant,
        )
    );
    transaction.rollback().await?;

    let premium = SubscriptionEnrollmentReservation::from_command(
        &enrollment_command(
            account,
            subscriber_id,
            Uuid::now_v7(),
            "premium-with-basic-grant",
            full_price("premium", 2_000),
        ),
        &gateway,
    )?;
    let mut transaction = database.pool.begin().await?;
    assert!(matches!(
            reserve_subscription_enrollment_in_transaction(
                &mut transaction,
                &TestOfferStore,
                &premium,
            )
            .await?,
            SubscriptionEnrollmentReservationOutcome::Reserved(_)
        ));
    transaction.commit().await?;
    database.cleanup().await
}

#[tokio::test]
async fn gateway_configuration_rotation_rejects_prepared_submission() -> Result<(), Box<dyn Error>>
{
    let database = TestDatabase::start("enroll_rotate").await?;
    install_host_offers(&database).await?;
    let account = create_gateway_account(&database.pool, "nmi").await?;
    let plan_key = "base_subscription";
    set_offer(&database, account.billing_scope_id, plan_key, 1_000).await?;
    let gateway = resolved_gateway(account);
    let reservation = SubscriptionEnrollmentReservation::from_command(
        &enrollment_command(
            account,
            Uuid::now_v7(),
            Uuid::now_v7(),
            "rotated-configuration",
            full_price(plan_key, 1_000),
        ),
        &gateway,
    )?;
    let mut transaction = database.pool.begin().await?;
    assert!(matches!(
        reserve_subscription_enrollment_in_transaction(
            &mut transaction,
            &TestOfferStore,
            &reservation,
        )
        .await?,
        SubscriptionEnrollmentReservationOutcome::Reserved(_)
    ));
    transaction.commit().await?;

    sqlx::query(
            "UPDATE billing_gateway_accounts SET gateway_configuration_id = $2, updated_at = clock_timestamp() WHERE id = $1",
        )
        .bind(account.gateway_account_id)
        .bind(Uuid::now_v7())
        .execute(&database.pool)
        .await?;
    let mut transaction = database.pool.begin().await?;
    let rejected = admit_subscription_enrollment_submission_in_transaction(
        &mut transaction,
        &TestOfferStore,
        &reservation,
    )
    .await?;
    assert!(matches!(
        rejected,
        SubscriptionEnrollmentSubmissionOutcome::Rejected {
            ref attempt,
            reason: SubscriptionEnrollmentSubmissionRejection::GatewayConfigurationChanged,
        } if attempt.status() == PaymentAttemptStatus::Failed
            && attempt.state().timestamps().submitted_at().is_none()
    ));
    transaction.commit().await?;
    database.cleanup().await
}

#[tokio::test]
async fn loaders_preserve_exact_scope_and_redact_durable_values() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("attempt_owner").await?;
    let account = create_gateway_account(&database.pool, "nmi").await?;
    let attempt_id = Uuid::now_v7();
    let subscriber_id = Uuid::now_v7();
    sqlx::query(
        r#"
            INSERT INTO billing_payment_attempts (
                id, billing_scope_id, subscriber_id, host_charge_target_id,
                attempt_kind, status, idempotency_key, request_fingerprint,
                amount_cents, currency, gateway_account_id,
                gateway_configuration_id, gateway_order_id,
                gateway_transaction_id, gateway_payment_method_reference,
                gateway_response, gateway_response_code, gateway_response_text,
                gateway_condition, billing_first_name, billing_last_name,
                billing_email
            ) VALUES (
                $1, $2, $3, $4, 'host_charge', 'pending', $5, $6,
                1000, 'USD', $7, $8, $9, $10, $11, $12, $13, $14,
                $15, $16, $17, $18
            )
            "#,
    )
    .bind(attempt_id)
    .bind(account.billing_scope_id)
    .bind(subscriber_id)
    .bind(Uuid::now_v7())
    .bind("idempotency-secret")
    .bind("fingerprint-secret")
    .bind(account.gateway_account_id)
    .bind(account.gateway_configuration_id)
    .bind("order-secret")
    .bind("transaction-secret")
    .bind("method-secret")
    .bind("response-secret")
    .bind("code-secret")
    .bind("text-secret")
    .bind("condition-secret")
    .bind("Sensitive")
    .bind("Name")
    .bind("secret@example.test")
    .execute(&database.pool)
    .await?;

    let mut transaction = database.pool.begin().await?;
    assert!(
        find_payment_attempt_by_id_in_transaction(
            &mut transaction,
            BillingScopeId::new(Uuid::now_v7()),
            PaymentAttemptId::new(attempt_id),
        )
        .await?
        .is_none()
    );
    let idempotency_key = IdempotencyKey::new("idempotency-secret")?;
    assert!(
        find_payment_attempt_by_idempotency(
            &mut transaction,
            BillingScopeId::new(Uuid::now_v7()),
            SubscriberId::new(subscriber_id),
            &idempotency_key,
        )
        .await?
        .is_none()
    );
    assert!(
        find_payment_attempt_by_idempotency(
            &mut transaction,
            BillingScopeId::new(account.billing_scope_id),
            SubscriberId::new(Uuid::now_v7()),
            &idempotency_key,
        )
        .await?
        .is_none()
    );
    let found = find_payment_attempt_by_idempotency(
        &mut transaction,
        BillingScopeId::new(account.billing_scope_id),
        SubscriberId::new(subscriber_id),
        &idempotency_key,
    )
    .await?
    .expect("exact owner row should load without a lock");
    assert_eq!(found.identity().attempt_id().as_uuid(), &attempt_id);
    let attempt = lock_payment_attempt_by_idempotency_in_transaction(
        &mut transaction,
        BillingScopeId::new(account.billing_scope_id),
        SubscriberId::new(subscriber_id),
        &idempotency_key,
    )
    .await?
    .expect("exact owner row should load");
    assert_eq!(attempt.identity().attempt_id().as_uuid(), &attempt_id);
    assert_eq!(attempt.kind(), PaymentAttemptKind::HostCharge);
    assert_eq!(attempt.request().amount().cents(), 1_000);
    assert_eq!(
        attempt
            .state()
            .processor_evidence()
            .transaction_id()
            .expect("transaction ID")
            .expose(),
        "transaction-secret"
    );
    let debug = format!("{attempt:?}");
    for secret in [
        "idempotency-secret",
        "fingerprint-secret",
        "order-secret",
        "transaction-secret",
        "method-secret",
        "response-secret",
        "code-secret",
        "text-secret",
        "condition-secret",
        "Sensitive Name",
        "secret@example.test",
    ] {
        assert!(!debug.contains(secret), "debug leaked {secret}");
    }
    transaction.rollback().await?;
    database.cleanup().await
}

#[tokio::test]
async fn idempotency_find_completes_while_explicit_lock_times_out_on_a_held_row()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("attempt_lockmode").await?;
    let result = async {
        let account = create_gateway_account(&database.pool, "nmi").await?;
        let attempt_id = Uuid::now_v7();
        let subscriber_id = Uuid::now_v7();
        let idempotency_key = IdempotencyKey::new("idempotency-lock-mode")?;
        sqlx::query(
            r#"
                INSERT INTO billing_payment_attempts (
                    id, billing_scope_id, subscriber_id, host_charge_target_id,
                    attempt_kind, status, idempotency_key, request_fingerprint,
                    amount_cents, currency, gateway_account_id,
                    gateway_configuration_id, gateway_order_id
                ) VALUES (
                    $1, $2, $3, $4, 'host_charge', 'pending', $5, $6,
                    1000, 'USD', $7, $8, $9
                )
                "#,
        )
        .bind(attempt_id)
        .bind(account.billing_scope_id)
        .bind(subscriber_id)
        .bind(Uuid::now_v7())
        .bind(idempotency_key.expose())
        .bind("fingerprint-lock-mode")
        .bind(account.gateway_account_id)
        .bind(account.gateway_configuration_id)
        .bind("order-lock-mode")
        .execute(&database.pool)
        .await?;

        let billing_scope_id = BillingScopeId::new(account.billing_scope_id);
        let subscriber_id = SubscriberId::new(subscriber_id);
        let mut holder = database.pool.begin().await?;
        let held = lock_payment_attempt_by_idempotency_in_transaction(
            &mut holder,
            billing_scope_id,
            subscriber_id,
            &idempotency_key,
        )
        .await?
        .expect("holder must lock the idempotency row");
        assert_eq!(held.identity().attempt_id().as_uuid(), &attempt_id);

        let mut finder = database.pool.begin().await?;
        let found = tokio::time::timeout(
            StdDuration::from_secs(1),
            find_payment_attempt_by_idempotency(
                &mut finder,
                billing_scope_id,
                subscriber_id,
                &idempotency_key,
            ),
        )
        .await
        .map_err(|_| io::Error::other("unlocked idempotency find waited on the row lock"))??
        .expect("find must see the exact owner row while another transaction holds its lock");
        assert_eq!(found.identity().attempt_id().as_uuid(), &attempt_id);
        finder.rollback().await?;

        let mut contender = database.pool.begin().await?;
        sqlx::query("SET LOCAL lock_timeout = '100ms'")
            .execute(&mut *contender)
            .await?;
        let error = tokio::time::timeout(
            StdDuration::from_secs(1),
            lock_payment_attempt_by_idempotency(
                &mut contender,
                billing_scope_id,
                subscriber_id,
                &idempotency_key,
            ),
        )
        .await
        .map_err(|_| io::Error::other("explicit idempotency lock did not time out"))?
        .expect_err("explicit idempotency lock must wait for the held row");
        let PaymentAttemptStoreError::Sql(error) = error else {
            return Err(io::Error::other(format!(
                "expected a PostgreSQL lock timeout, got {error}"
            ))
            .into());
        };
        let code = error
            .as_database_error()
            .and_then(|database_error| database_error.code())
            .map(|code| code.into_owned());
        if code.as_deref() != Some("55P03") {
            return Err(
                io::Error::other(format!("expected PostgreSQL lock timeout, got {error}")).into(),
            );
        }
        contender.rollback().await?;
        holder.rollback().await?;
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn renewal_and_recovery_share_one_lossless_insert_codec() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("attempt_codec").await?;
    let account = create_gateway_account(&database.pool, "nmi").await?;
    let base_time: DateTime<Utc> =
        sqlx::query_scalar("SELECT date_trunc('microseconds', clock_timestamp())")
            .fetch_one(&database.pool)
            .await?;
    let currency = CurrencyCode::new("USD")?;

    for (position, kind, expected_status) in [
        (
            1,
            PaymentAttemptKind::SubscriptionRenewal,
            SubscriptionStatus::Active,
        ),
        (
            2,
            PaymentAttemptKind::SubscriptionRecovery,
            SubscriptionStatus::PastDue,
        ),
    ] {
        let subscriber_id = Uuid::now_v7();
        let subscription_id = Uuid::now_v7();
        let target_method_id = Uuid::now_v7();
        let expected_method_id = Uuid::now_v7();
        for (method_id, reference) in [
            (target_method_id, format!("target-method-{position}")),
            (expected_method_id, format!("expected-method-{position}")),
        ] {
            sqlx::query(
                r#"
                INSERT INTO billing_payment_methods (
                    id, billing_scope_id, subscriber_id, gateway_account_id,
                    gateway_payment_method_reference, status
                ) VALUES ($1, $2, $3, $4, $5, 'active')
                "#,
            )
            .bind(method_id)
            .bind(account.billing_scope_id)
            .bind(subscriber_id)
            .bind(account.gateway_account_id)
            .bind(reference)
            .execute(&database.pool)
            .await?;
        }

        let plan_key = PlanKey::new(format!("codec_plan_{position}"))?;
        let initial_transaction = format!("initial-transaction-{position}");
        let period_start = base_time + Duration::days(i64::from(position));
        let period_end = period_start + Duration::days(30);
        sqlx::query(
            r#"
            INSERT INTO billing_subscriptions (
                id, billing_scope_id, subscriber_id, plan_key, status,
                gateway_account_id, payment_method_id, amount_cents, currency,
                current_period_start_at, current_period_end_at, next_renewal_at,
                initial_transaction_id, phase, recurring_period_kind,
                recurring_period_count, dunning_retry_delays_seconds,
                dunning_exhaustion, past_due_access, next_payment_attempt_at
            ) VALUES (
                $1, $2, $3, $4, $5, $6, $7, $8, 'USD',
                $9, $10, $10, $11, 'recurring', 'calendar_months', 1,
                ARRAY[]::bigint[], 'remain_past_due', 'suspend_immediately', $10
            )
            "#,
        )
        .bind(subscription_id)
        .bind(account.billing_scope_id)
        .bind(subscriber_id)
        .bind(plan_key.as_str())
        .bind(expected_status.as_str())
        .bind(account.gateway_account_id)
        .bind(expected_method_id)
        .bind(1_000 + position)
        .bind(period_start - Duration::days(30))
        .bind(period_start)
        .bind(&initial_transaction)
        .execute(&database.pool)
        .await?;

        let attempt_id = PaymentAttemptId::new(Uuid::now_v7());
        let identity = PaymentAttemptIdentity::new(
            attempt_id,
            BillingScopeId::new(account.billing_scope_id),
            SubscriberId::new(subscriber_id),
            GatewayAccountId::new(account.gateway_account_id),
            GatewayConfigurationId::new(account.gateway_configuration_id),
        );
        let expected_state = SubscriptionPaymentStateSnapshot::new(
            SubscriptionId::new(subscription_id),
            PaymentMethodId::new(expected_method_id),
            GatewayTransactionId::new(initial_transaction.clone())?,
            expected_status,
        )?;
        let period = BillingPeriod::new(period_start, period_end)?;
        let target = match kind {
            PaymentAttemptKind::SubscriptionRenewal => PaymentAttemptTarget::SubscriptionRenewal {
                plan_key: plan_key.clone(),
                payment_method_id: PaymentMethodId::new(target_method_id),
                period,
                expected_state,
            },
            PaymentAttemptKind::SubscriptionRecovery => {
                PaymentAttemptTarget::SubscriptionRecovery {
                    plan_key: plan_key.clone(),
                    payment_method_id: PaymentMethodId::new(target_method_id),
                    period,
                    expected_state,
                }
            }
            _ => unreachable!(),
        };
        let idempotency_key = IdempotencyKey::new(format!("codec-key-{position}"))?;
        let fingerprint = PaymentAttemptFingerprint::new(format!("codec-fingerprint-{position}"))?;
        let amount = Money::new(1_000 + position, currency)?;
        let gateway_order_id = GatewayOrderId::from_generated_attempt(
            format!("codec-{position}-{}", attempt_id.as_uuid().simple()),
            attempt_id,
        )?;
        let billing_contact = BillingContactSnapshot::from_parts(
            Some(format!("First{position}")),
            Some(format!("Last{position}")),
            Some(format!("codec{position}@example.test")),
        );
        let request = PaymentAttemptRequest::from_persisted_parts(
            target,
            idempotency_key.clone(),
            fingerprint.clone(),
            amount,
            gateway_order_id,
            billing_contact.clone(),
        );

        let mut transaction = database.pool.begin().await?;
        assert!(insert_subscription_charge_attempt(&mut transaction, identity, &request).await?);
        assert!(
            !insert_subscription_charge_attempt(&mut transaction, identity, &request).await?,
            "ON CONFLICT must report that the duplicate was not inserted"
        );
        transaction.commit().await?;

        let row = sqlx::query("SELECT * FROM billing_payment_attempts WHERE id = $1")
            .bind(attempt_id.as_uuid())
            .fetch_one(&database.pool)
            .await?;
        assert_eq!(row.try_get::<Uuid, _>("id")?, attempt_id.into_uuid());
        assert_eq!(
            row.try_get::<Uuid, _>("billing_scope_id")?,
            account.billing_scope_id
        );
        assert_eq!(row.try_get::<Uuid, _>("subscriber_id")?, subscriber_id);
        assert_eq!(row.try_get::<String, _>("plan_key")?, plan_key.as_str());
        assert_eq!(row.try_get::<Uuid, _>("subscription_id")?, subscription_id);
        assert_eq!(
            row.try_get::<Uuid, _>("payment_method_id")?,
            target_method_id
        );
        assert_eq!(row.try_get::<String, _>("attempt_kind")?, kind.as_str());
        assert_eq!(row.try_get::<String, _>("status")?, "pending");
        assert_eq!(
            row.try_get::<String, _>("idempotency_key")?,
            idempotency_key.expose()
        );
        assert_eq!(
            row.try_get::<String, _>("request_fingerprint")?,
            fingerprint.expose()
        );
        assert_eq!(row.try_get::<i32, _>("amount_cents")?, amount.cents());
        assert_eq!(row.try_get::<String, _>("currency")?, "USD");
        assert_eq!(
            row.try_get::<DateTime<Utc>, _>("billing_period_start_at")?,
            period_start
        );
        assert_eq!(
            row.try_get::<DateTime<Utc>, _>("billing_period_end_at")?,
            period_end
        );
        assert_eq!(
            row.try_get::<Uuid, _>("gateway_account_id")?,
            account.gateway_account_id
        );
        assert_eq!(
            row.try_get::<Uuid, _>("gateway_configuration_id")?,
            account.gateway_configuration_id
        );
        assert_eq!(
            row.try_get::<String, _>("gateway_order_id")?,
            request.gateway_order_id().expose()
        );
        assert_eq!(
            row.try_get::<Option<String>, _>("billing_first_name")?
                .as_deref(),
            billing_contact.first_name()
        );
        assert_eq!(
            row.try_get::<Option<String>, _>("billing_last_name")?
                .as_deref(),
            billing_contact.last_name()
        );
        assert_eq!(
            row.try_get::<Option<String>, _>("billing_email")?
                .as_deref(),
            billing_contact.email()
        );
        assert_eq!(
            row.try_get::<Uuid, _>("subscription_expected_payment_method_id")?,
            expected_method_id
        );
        assert_eq!(
            row.try_get::<String, _>("subscription_expected_initial_transaction_id")?,
            initial_transaction
        );
        assert_eq!(
            row.try_get::<String, _>("subscription_expected_status")?,
            expected_status.as_str()
        );

        let mut transaction = database.pool.begin().await?;
        let loaded = find_payment_attempt_by_id_in_transaction(
            &mut transaction,
            identity.billing_scope_id(),
            attempt_id,
        )
        .await?
        .expect("inserted subscription charge must hydrate");
        transaction.rollback().await?;
        assert_eq!(loaded.kind(), kind);
        assert_eq!(
            loaded.request().target().payment_method_id(),
            Some(PaymentMethodId::new(target_method_id))
        );
        assert_eq!(
            loaded
                .request()
                .target()
                .subscription_payment_state_snapshot()
                .expect("subscription snapshot")
                .payment_method_id(),
            PaymentMethodId::new(expected_method_id)
        );
    }

    database.cleanup().await
}

#[tokio::test]
async fn recovery_keeps_related_and_expected_payment_methods_distinct() -> Result<(), Box<dyn Error>>
{
    let database = TestDatabase::start("attempt_recovery").await?;
    let account = create_gateway_account(&database.pool, "nmi").await?;
    let subscriber_id = Uuid::now_v7();
    let expected_method_id = Uuid::now_v7();
    let related_method_id = Uuid::now_v7();
    for (method_id, reference) in [
        (expected_method_id, "vault-expected"),
        (related_method_id, "vault-related"),
    ] {
        sqlx::query(
            r#"
                INSERT INTO billing_payment_methods (
                    id, billing_scope_id, subscriber_id, gateway_account_id,
                    gateway_payment_method_reference, status
                ) VALUES ($1, $2, $3, $4, $5, 'active')
                "#,
        )
        .bind(method_id)
        .bind(account.billing_scope_id)
        .bind(subscriber_id)
        .bind(account.gateway_account_id)
        .bind(reference)
        .execute(&database.pool)
        .await?;
    }
    let subscription_id = Uuid::now_v7();
    let period_start = Utc::now();
    let period_end = period_start + Duration::days(30);
    sqlx::query(
        r#"
            INSERT INTO billing_subscriptions (
                id, billing_scope_id, subscriber_id, plan_key, status,
                gateway_account_id, payment_method_id, amount_cents, currency,
                current_period_start_at, current_period_end_at, next_renewal_at,
                initial_transaction_id, phase, recurring_period_kind,
                recurring_period_count, dunning_retry_delays_seconds,
                dunning_exhaustion, past_due_access, next_payment_attempt_at
            ) VALUES (
                $1, $2, $3, 'premium', 'active', $4, $5, 1000, 'USD',
                $6, $7, $7, 'txn-initial', 'recurring', 'calendar_months', 1,
                ARRAY[]::bigint[], 'remain_past_due', 'suspend_immediately', $7
            )
            "#,
    )
    .bind(subscription_id)
    .bind(account.billing_scope_id)
    .bind(subscriber_id)
    .bind(account.gateway_account_id)
    .bind(related_method_id)
    .bind(period_start)
    .bind(period_end)
    .execute(&database.pool)
    .await?;

    let attempt_id = Uuid::now_v7();
    let charge_start = period_end;
    let charge_end = charge_start + Duration::days(30);
    let order_id = format!("sr_recovery_{}", attempt_id.simple());
    sqlx::query(
        r#"
            INSERT INTO billing_payment_attempts (
                id, billing_scope_id, subscriber_id, plan_key,
                subscription_id, payment_method_id, attempt_kind, status,
                idempotency_key, request_fingerprint, amount_cents, currency,
                billing_period_start_at, billing_period_end_at,
                gateway_account_id, gateway_configuration_id, gateway_order_id,
                gateway_transaction_id, submitted_at, resolved_at,
                subscription_expected_payment_method_id,
                subscription_expected_initial_transaction_id,
                subscription_expected_status
            ) VALUES (
                $1, $2, $3, 'premium', $4, $5,
                'subscription_recovery', 'approved', $6, $7, 1000, 'USD',
                $8, $9, $10, $11, $12, 'txn-recovery', now(), now(),
                $13, 'txn-initial', 'past_due'
            )
            "#,
    )
    .bind(attempt_id)
    .bind(account.billing_scope_id)
    .bind(subscriber_id)
    .bind(subscription_id)
    .bind(related_method_id)
    .bind("recovery-key")
    .bind("recovery-fingerprint")
    .bind(charge_start)
    .bind(charge_end)
    .bind(account.gateway_account_id)
    .bind(account.gateway_configuration_id)
    .bind(order_id)
    .bind(expected_method_id)
    .execute(&database.pool)
    .await?;

    let mut transaction = database.pool.begin().await?;
    let attempt = find_payment_attempt_by_id_in_transaction(
        &mut transaction,
        BillingScopeId::new(account.billing_scope_id),
        PaymentAttemptId::new(attempt_id),
    )
    .await?
    .expect("recovery row should load");
    let target = attempt.request().target();
    assert_eq!(
        target.payment_method_id().unwrap().as_uuid(),
        &related_method_id
    );
    assert_eq!(
        target.subscription_id().unwrap().as_uuid(),
        &subscription_id
    );
    assert_eq!(
        target
            .subscription_payment_state_snapshot()
            .expect("expected state")
            .payment_method_id()
            .as_uuid(),
        &expected_method_id
    );
    assert_eq!(
        target
            .subscription_payment_state_snapshot()
            .expect("expected state")
            .status(),
        SubscriptionStatus::PastDue
    );
    transaction.rollback().await?;
    database.cleanup().await
}
