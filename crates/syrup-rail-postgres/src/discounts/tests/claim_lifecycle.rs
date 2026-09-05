#[tokio::test]
async fn discount_code_and_claim_policy_is_exact_plan_scoped_and_snapshot_preserving()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("sr_disc_life").await?;
    let result = async {
        create_offer_table(&database.pool).await?;
        let scope = BillingScopeId::new(Uuid::now_v7());
        let subscriber = SubscriberId::new(Uuid::now_v7());
        let plan_a = PlanKey::new("plan_a")?;
        let plan_b = PlanKey::new("plan_b")?;
        insert_offer(&database.pool, scope, &plan_a, 5_900).await?;
        insert_offer(&database.pool, scope, &plan_b, 9_900).await?;
        let usd = CurrencyCode::new("USD")?;

        let invalid_id = DiscountCodeId::new(Uuid::now_v7());
        let invalid = SubscriptionDiscountCodeCreation::new(
            invalid_id,
            scope,
            plan_a.clone(),
            SubscriptionDiscountCode::new("TOOLARGE")?,
            None,
            SubscriptionDiscountKind::AmountOffCents(PositiveDiscountCents::new(6_000)?),
            usd,
            SubscriptionDiscountDuration::Indefinite,
        )?;
        if !matches!(
            create_subscription_discount_code(&database.pool, &TestOfferStore, &invalid).await,
            Err(SubscriptionDiscountOperationError::InvalidConfiguration)
        ) {
            return Err(io::Error::other("invalid offer-relative terms were accepted").into());
        }
        let invalid_count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM billing_subscription_discount_codes WHERE id = $1",
        )
        .bind(invalid_id.as_uuid())
        .fetch_one(&database.pool)
        .await?;
        if invalid_count != 0 {
            return Err(io::Error::other("invalid code escaped its caller transaction").into());
        }

        let code_a_id = DiscountCodeId::new(Uuid::now_v7());
        let code = SubscriptionDiscountCode::new("SAVE25")?;
        let creation = SubscriptionDiscountCodeCreation::new(
            code_a_id,
            scope,
            plan_a.clone(),
            code.clone(),
            Some("  Launch offer  ".into()),
            SubscriptionDiscountKind::PercentOffBasisPoints(PercentOffBasisPoints::new(2_500)?),
            usd,
            SubscriptionDiscountDuration::Indefinite,
        )?;
        let created =
            create_subscription_discount_code(&database.pool, &TestOfferStore, &creation).await?;
        if created.label() != Some("Launch offer") {
            return Err(io::Error::other("created record lost canonical terms").into());
        }
        let created_quote = validate_subscription_discount_code(
            &database.pool,
            &TestOfferStore,
            scope,
            &plan_a,
            &code,
        )
        .await?
        .ok_or_else(|| io::Error::other("created code was not quoteable"))?;
        if created_quote.base_charge().cents() != 5_900
            || created_quote.discounted_charge().cents() != 4_425
        {
            return Err(io::Error::other("created code quote lost offer terms").into());
        }

        let first_claim_id = DiscountClaimId::new(Uuid::now_v7());
        let first_claim = SubscriptionDiscountClaim::new(
            first_claim_id,
            scope,
            subscriber,
            plan_a.clone(),
            code.clone(),
        );
        let first =
            claim_subscription_discount(&database.pool, &TestOfferStore, &first_claim).await?;
        let SubscriptionDiscountClaimOutcome::Saved(first) = first else {
            return Err(io::Error::other("first claim was not saved").into());
        };
        if first.snapshot().discounted_charge().cents() != 4_425 {
            return Err(io::Error::other("claim did not snapshot locked offer").into());
        }

        let update = SubscriptionDiscountCodeUpdate::new(
            code_a_id,
            scope,
            plan_a.clone(),
            Some("Changed terms".into()),
            SubscriptionDiscountCodeStatus::Active,
            SubscriptionDiscountKind::AmountOffCents(PositiveDiscountCents::new(900)?),
            usd,
            SubscriptionDiscountDuration::Indefinite,
        )?;
        update_subscription_discount_code(&database.pool, &TestOfferStore, &update)
            .await?
            .ok_or_else(|| io::Error::other("updated code disappeared"))?;

        let replay = SubscriptionDiscountClaim::new(
            DiscountClaimId::new(Uuid::now_v7()),
            scope,
            subscriber,
            plan_a.clone(),
            code.clone(),
        );
        let replay = claim_subscription_discount(&database.pool, &TestOfferStore, &replay).await?;
        let SubscriptionDiscountClaimOutcome::Existing(replay) = replay else {
            return Err(io::Error::other("same canonical code did not replay").into());
        };
        if replay.id() != first_claim_id
            || replay.snapshot().discounted_charge().cents() != 4_425
            || replay.snapshot().label() != Some("Launch offer")
        {
            return Err(io::Error::other("same-code replay reinterpreted the snapshot").into());
        }

        let plan_b_creation = SubscriptionDiscountCodeCreation::new(
            DiscountCodeId::new(Uuid::now_v7()),
            scope,
            plan_b.clone(),
            code.clone(),
            None,
            SubscriptionDiscountKind::AmountOffCents(PositiveDiscountCents::new(900)?),
            usd,
            SubscriptionDiscountDuration::Indefinite,
        )?;
        create_subscription_discount_code(&database.pool, &TestOfferStore, &plan_b_creation)
            .await?;
        let plan_b_claim = SubscriptionDiscountClaim::new(
            DiscountClaimId::new(Uuid::now_v7()),
            scope,
            subscriber,
            plan_b.clone(),
            code,
        );
        let plan_b_saved =
            claim_subscription_discount(&database.pool, &TestOfferStore, &plan_b_claim).await?;
        if !matches!(plan_b_saved, SubscriptionDiscountClaimOutcome::Saved(_)) {
            return Err(io::Error::other("another plan did not own an independent claim").into());
        }

        disable_subscription_discount_code(&database.pool, scope, &plan_a, code_a_id)
            .await?
            .ok_or_else(|| io::Error::other("disabled code disappeared"))?;
        if saved_subscription_discount_claim(&database.pool, scope, subscriber, &plan_a)
            .await?
            .is_some()
            || saved_subscription_discount_claim(&database.pool, scope, subscriber, &plan_b)
                .await?
                .is_none()
        {
            return Err(io::Error::other("disable crossed the exact plan boundary").into());
        }
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn claim_transitions_return_closed_lifecycle_states() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("sr_disc_trans").await?;
    let result = async {
        create_offer_table(&database.pool).await?;
        let scope = BillingScopeId::new(Uuid::now_v7());
        let subscriber = SubscriberId::new(Uuid::now_v7());
        let plan = PlanKey::new("plan_a")?;
        insert_offer(&database.pool, scope, &plan, 5_900).await?;
        let first_code = SubscriptionDiscountCode::new("FIRST10")?;
        let second_code = SubscriptionDiscountCode::new("SECOND10")?;
        for (id, code) in [
            (DiscountCodeId::new(Uuid::now_v7()), first_code.clone()),
            (DiscountCodeId::new(Uuid::now_v7()), second_code.clone()),
        ] {
            create_subscription_discount_code(
                &database.pool,
                &TestOfferStore,
                &SubscriptionDiscountCodeCreation::new(
                    id,
                    scope,
                    plan.clone(),
                    code,
                    None,
                    SubscriptionDiscountKind::AmountOffCents(PositiveDiscountCents::new(500)?),
                    CurrencyCode::new("USD")?,
                    SubscriptionDiscountDuration::Indefinite,
                )?,
            )
            .await?;
        }

        let first_id = DiscountClaimId::new(Uuid::now_v7());
        let first = claim_subscription_discount(
            &database.pool,
            &TestOfferStore,
            &SubscriptionDiscountClaim::new(first_id, scope, subscriber, plan.clone(), first_code),
        )
        .await?;
        let SubscriptionDiscountClaimOutcome::Saved(first) = first else {
            return Err(io::Error::other("first claim was not saved").into());
        };
        assert_eq!(first.state(), &SubscriptionDiscountClaimState::Saved);

        let second = claim_subscription_discount(
            &database.pool,
            &TestOfferStore,
            &SubscriptionDiscountClaim::new(
                DiscountClaimId::new(Uuid::now_v7()),
                scope,
                subscriber,
                plan.clone(),
                second_code,
            ),
        )
        .await?;
        let SubscriptionDiscountClaimOutcome::Saved(second) = second else {
            return Err(io::Error::other("second claim was not saved").into());
        };
        assert_eq!(second.state(), &SubscriptionDiscountClaimState::Saved);

        let row = sqlx::query(
            r#"
            SELECT id, billing_scope_id, subscriber_id, plan_key,
                discount_code_id, code_snapshot, label_snapshot, discount_kind,
                amount_off_cents, percent_off_bps, currency, duration,
                duration_months, base_amount_cents, discounted_amount_cents,
                status, claimed_at, applied_at, applied_subscription_id,
                applied_payment_attempt_id, superseded_at
            FROM billing_subscription_discount_claims
            WHERE id = $1
            "#,
        )
        .bind(first_id.as_uuid())
        .fetch_one(&database.pool)
        .await?;
        let superseded = claim_from_row(&row)?;
        assert!(matches!(
            superseded.state(),
            SubscriptionDiscountClaimState::Superseded { .. }
        ));
        assert_eq!(
            superseded.status(),
            syrup_rail::SubscriptionDiscountClaimStatus::Superseded
        );

        let cleared = clear_subscription_discount(&database.pool, scope, subscriber, &plan).await?;
        let SubscriptionDiscountClearOutcome::Cleared(cleared) = cleared else {
            return Err(io::Error::other("second saved claim was not cleared").into());
        };
        assert_eq!(cleared.state(), &SubscriptionDiscountClaimState::Expired);

        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn historical_code_records_remain_listable_and_disableable_after_cadence_drift()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("sr_disc_drift").await?;
    let result = async {
        create_offer_table(&database.pool).await?;
        let scope = BillingScopeId::new(Uuid::now_v7());
        let plan = PlanKey::new("monthly_plan")?;
        insert_offer(&database.pool, scope, &plan, 5_900).await?;
        let code_id = DiscountCodeId::new(Uuid::now_v7());
        let code = SubscriptionDiscountCode::new("MONTHS10")?;
        let creation = SubscriptionDiscountCodeCreation::new(
            code_id,
            scope,
            plan.clone(),
            code.clone(),
            Some("Historical monthly offer".into()),
            SubscriptionDiscountKind::AmountOffCents(PositiveDiscountCents::new(1_000)?),
            CurrencyCode::new("USD")?,
            SubscriptionDiscountDuration::LimitedMonths(LimitedDiscountMonths::new(3)?),
        )?;
        create_subscription_discount_code(&database.pool, &TestOfferStore, &creation).await?;

        sqlx::query(
            r#"
                UPDATE test_subscription_offers
                SET recurring_period_kind = 'fixed_days', recurring_period_count = 30
                WHERE billing_scope_id = $1 AND plan_key = $2
                "#,
        )
        .bind(scope.as_uuid())
        .bind(plan.as_str())
        .execute(&database.pool)
        .await?;

        assert!(matches!(
            validate_subscription_discount_code(
                &database.pool,
                &TestOfferStore,
                scope,
                &plan,
                &code,
            )
            .await,
            Err(SubscriptionDiscountOperationError::LimitedDiscountCadence)
        ));
        let listed = list_subscription_discount_codes(&database.pool, scope, &plan).await?;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id(), code_id);
        assert_eq!(listed[0].status(), SubscriptionDiscountCodeStatus::Active);

        let disabled_update = SubscriptionDiscountCodeUpdate::new(
            code_id,
            scope,
            plan.clone(),
            Some("Historical monthly offer".into()),
            SubscriptionDiscountCodeStatus::Disabled,
            SubscriptionDiscountKind::AmountOffCents(PositiveDiscountCents::new(1_000)?),
            CurrencyCode::new("USD")?,
            SubscriptionDiscountDuration::LimitedMonths(LimitedDiscountMonths::new(3)?),
        )?;
        let updated =
            update_subscription_discount_code(&database.pool, &TestOfferStore, &disabled_update)
                .await?
                .ok_or_else(|| io::Error::other("historical code disappeared while updating"))?;
        assert_eq!(updated.status(), SubscriptionDiscountCodeStatus::Disabled);

        let disabled = disable_subscription_discount_code(&database.pool, scope, &plan, code_id)
            .await?
            .ok_or_else(|| io::Error::other("historical code disappeared while disabling"))?;
        assert_eq!(disabled.status(), SubscriptionDiscountCodeStatus::Disabled);
        assert!(matches!(
            disabled.duration(),
            SubscriptionDiscountDuration::LimitedMonths(_)
        ));
        let listed = list_subscription_discount_codes(&database.pool, scope, &plan).await?;
        assert_eq!(listed, vec![disabled]);
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn clear_is_blocked_by_an_exact_plan_initial_attempt() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("sr_disc_clear").await?;
    let result = async {
        create_offer_table(&database.pool).await?;
        let gateway = create_gateway_account(&database.pool, "test_gateway").await?;
        let scope = BillingScopeId::new(gateway.billing_scope_id);
        let subscriber = SubscriberId::new(Uuid::now_v7());
        let plan = PlanKey::new("plan_a")?;
        insert_offer(&database.pool, scope, &plan, 5_900).await?;
        let creation = SubscriptionDiscountCodeCreation::new(
            DiscountCodeId::new(Uuid::now_v7()),
            scope,
            plan.clone(),
            SubscriptionDiscountCode::new("CLEAR10")?,
            None,
            SubscriptionDiscountKind::AmountOffCents(PositiveDiscountCents::new(500)?),
            CurrencyCode::new("USD")?,
            SubscriptionDiscountDuration::Indefinite,
        )?;
        create_subscription_discount_code(&database.pool, &TestOfferStore, &creation).await?;
        let claim = SubscriptionDiscountClaim::new(
            DiscountClaimId::new(Uuid::now_v7()),
            scope,
            subscriber,
            plan.clone(),
            SubscriptionDiscountCode::new("CLEAR10")?,
        );
        claim_subscription_discount(&database.pool, &TestOfferStore, &claim).await?;
        insert_pending_initial_attempt(
            &database.pool,
            gateway.billing_scope_id,
            subscriber.into_uuid(),
            plan.as_str(),
            gateway.gateway_account_id,
            gateway.gateway_configuration_id,
        )
        .await?;

        let blocked = clear_subscription_discount(&database.pool, scope, subscriber, &plan).await?;
        if blocked != SubscriptionDiscountClearOutcome::BlockedByInitialAttempt {
            return Err(io::Error::other("initial checkout did not block clear").into());
        }
        if saved_subscription_discount_claim(&database.pool, scope, subscriber, &plan)
            .await?
            .is_none()
        {
            return Err(io::Error::other("blocked clear expired the saved claim").into());
        }
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

async fn create_offer_table(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
            CREATE TABLE test_subscription_offers (
                billing_scope_id uuid NOT NULL,
                plan_key text NOT NULL,
                amount_cents integer NOT NULL,
                currency text NOT NULL,
                recurring_period_kind text NOT NULL DEFAULT 'calendar_months',
                recurring_period_count integer NOT NULL DEFAULT 1,
                PRIMARY KEY (billing_scope_id, plan_key)
            )
            "#,
    )
    .execute(pool)
    .await?;
    Ok(())
}

async fn insert_offer(
    pool: &PgPool,
    scope: BillingScopeId,
    plan: &PlanKey,
    amount_cents: i32,
) -> Result<(), sqlx::Error> {
    sqlx::query(
            "INSERT INTO test_subscription_offers (billing_scope_id, plan_key, amount_cents, currency) VALUES ($1, $2, $3, 'USD')",
        )
        .bind(scope.as_uuid())
        .bind(plan.as_str())
        .bind(amount_cents)
        .execute(pool)
        .await?;
    Ok(())
}

async fn insert_discount_subscription(
    pool: &PgPool,
    account: crate::test_support::GatewayAccountFixture,
    subscriber: SubscriberId,
    plan: &PlanKey,
    status: &str,
    period_end: chrono::DateTime<Utc>,
    updated_at: chrono::DateTime<Utc>,
) -> Result<Uuid, sqlx::Error> {
    let payment_method_id = Uuid::now_v7();
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
    .bind(subscriber.as_uuid())
    .bind(account.gateway_account_id)
    .bind(format!("discount-method-{payment_method_id}"))
    .execute(pool)
    .await?;

    let subscription_id = Uuid::now_v7();
    let canceled_at = (status == "canceled").then_some(updated_at);
    let unpaid_at = (status == "unpaid").then_some(updated_at);
    let next_payment_attempt_at = matches!(status, "active" | "past_due").then_some(period_end);
    sqlx::query(
        r#"
        INSERT INTO billing_subscriptions (
            required_gateway_account_mode,
            id, billing_scope_id, subscriber_id, plan_key, status,
            gateway_account_id, payment_method_id, amount_cents, currency,
            current_period_start_at, current_period_end_at, next_renewal_at,
            initial_transaction_id, canceled_at, created_at, updated_at,
            phase, recurring_period_kind, recurring_period_count,
            dunning_retry_delays_seconds, dunning_exhaustion,
            past_due_access, next_payment_attempt_at, unpaid_at
        ) VALUES (
            'live', $1, $2, $3, $4, $5, $6, $7, 1000, 'USD',
            $8, $9, $9, $10, $11, $12, $13, 'recurring',
            'calendar_months', 1, ARRAY[]::bigint[], 'remain_past_due',
            'suspend_immediately', $14, $15
        )
        "#,
    )
    .bind(subscription_id)
    .bind(account.billing_scope_id)
    .bind(subscriber.as_uuid())
    .bind(plan.as_str())
    .bind(status)
    .bind(account.gateway_account_id)
    .bind(payment_method_id)
    .bind(period_end - chrono::Duration::days(30))
    .bind(period_end)
    .bind(format!("discount-transaction-{subscription_id}"))
    .bind(canceled_at)
    .bind(updated_at - chrono::Duration::days(1))
    .bind(updated_at)
    .bind(next_payment_attempt_at)
    .bind(unpaid_at)
    .execute(pool)
    .await?;
    Ok(subscription_id)
}

async fn assert_row_update_times_out(
    pool: &PgPool,
    subscription_id: Uuid,
) -> Result<(), Box<dyn Error>> {
    let mut contender = pool.begin().await?;
    sqlx::query("SET LOCAL lock_timeout = '100ms'")
        .execute(&mut *contender)
        .await?;
    let error = sqlx::query(
        "UPDATE billing_subscriptions SET updated_at = clock_timestamp() WHERE id = $1",
    )
    .bind(subscription_id)
    .execute(&mut *contender)
    .await
    .expect_err("selected subscription row must remain locked");
    let code = error
        .as_database_error()
        .and_then(|database_error| database_error.code())
        .map(|code| code.into_owned());
    contender.rollback().await?;
    if code.as_deref() != Some("55P03") {
        return Err(
            io::Error::other(format!("expected selected-row lock timeout, got {error}")).into(),
        );
    }
    Ok(())
}

async fn insert_pending_initial_attempt(
    pool: &PgPool,
    scope: Uuid,
    subscriber: Uuid,
    plan: &str,
    gateway_account: Uuid,
    gateway_configuration: Uuid,
) -> Result<(), sqlx::Error> {
    let attempt = Uuid::now_v7();
    sqlx::query(
        r#"
            INSERT INTO billing_payment_attempts (
                required_gateway_account_mode,
                id, billing_scope_id, subscriber_id, plan_key, attempt_kind,
                status, idempotency_key, request_fingerprint, amount_cents,
                currency, gateway_account_id, gateway_configuration_id,
                gateway_order_id, subscription_initial_terms_version,
                subscription_initial_start_kind,
                subscription_initial_recurring_base_amount_cents,
                subscription_initial_recurring_period_kind,
                subscription_initial_recurring_period_count,
                subscription_initial_dunning_retry_delays_seconds,
                subscription_initial_dunning_exhaustion,
                subscription_initial_past_due_access
            ) VALUES (
                'live',
                $1, $2, $3, $4, 'subscription_initial', 'pending', $5, $6,
                100, 'USD', $7, $8, $9, 2, 'recurring_immediately', 100,
                'calendar_months', 1, ARRAY[]::bigint[],
                'remain_past_due', 'suspend_immediately'
            )
            "#,
    )
    .bind(attempt)
    .bind(scope)
    .bind(subscriber)
    .bind(plan)
    .bind(format!("discount-{}", attempt.simple()))
    .bind(format!("initial:{plan}:100:USD"))
    .bind(gateway_account)
    .bind(gateway_configuration)
    .bind(format!("discount-order-{}", attempt.simple()))
    .execute(pool)
    .await?;
    Ok(())
}
