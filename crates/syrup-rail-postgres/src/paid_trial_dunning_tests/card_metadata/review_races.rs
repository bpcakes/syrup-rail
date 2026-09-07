use super::*;

#[tokio::test]
async fn card_metadata_configuration_and_timestamp_changes_return_changed_during_query()
-> Result<(), Box<dyn Error>> {
    let db = TestDatabase::start("meta_rotation").await?;
    for rotate in [false, true] {
        let fixture = Fixture::new(&db.pool).await?;
        let mut provider = QueryGateway::new(Reply::observation(Some(metadata(
            "txn_metadata",
            None,
            "Visa",
            "1111",
            Some(10),
            Some(2029),
        ))));
        provider.block = true;
        let provider = Arc::new(provider);
        let resolver = fixture.resolver(provider.clone())?;
        let pool = db.pool.clone();
        let command = fixture.command();
        let pending = tokio::spawn(async move {
            refresh_payment_method_metadata(&pool, &resolver, command).await
        });
        tokio::time::timeout(Duration::from_secs(3), provider.started.notified()).await?;
        if rotate {
            let activation = syrup_rail::GatewayConfigurationActivation::new(
                fixture.scope(),
                GatewayAccountId::new(fixture.account.gateway_account_id),
                GatewayProviderKey::new("nmi")?,
                GatewayConfigurationId::new(fixture.account.gateway_configuration_id),
                GatewayConfigurationId::new(Uuid::now_v7()),
            );
            let mut tx = db.pool.begin().await?;
            assert_eq!(
                crate::activate_gateway_configuration(&mut tx, &activation).await?,
                syrup_rail::GatewayConfigurationActivationOutcome::Activated
            );
            tx.commit().await?;
        } else {
            sqlx::query(
                "UPDATE billing_subscriptions SET updated_at = clock_timestamp() WHERE id = $1",
            )
            .bind(fixture.subscription_id.as_uuid())
            .execute(&db.pool)
            .await?;
        }
        let financial = fixture.financial_snapshot(&db.pool).await?;
        let before = fixture.method_snapshot(&db.pool).await?;
        provider.release.notify_one();
        assert_eq!(pending.await??, Outcome::ChangedDuringQuery);
        assert_eq!(fixture.method_snapshot(&db.pool).await?, before);
        assert_eq!(fixture.financial_snapshot(&db.pool).await?, financial);
        assert_eq!(provider.queries.load(Ordering::SeqCst), 1);
    }
    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn card_metadata_reused_vault_approval_invalidates_old_display_and_can_be_refreshed()
-> Result<(), Box<dyn Error>> {
    let db = TestDatabase::start("meta_reapproval").await?;
    let mut fixture = Fixture::new(&db.pool).await?;
    let provider = Arc::new(QueryGateway::new(Reply::observation(Some(metadata(
        "txn_metadata",
        None,
        "Visa",
        "1111",
        Some(10),
        Some(2029),
    )))));
    let resolver = fixture.resolver(provider)?;
    assert_eq!(
        refresh_payment_method_metadata(&db.pool, &resolver, fixture.command()).await?,
        Outcome::Updated
    );
    let old_command = fixture.command();
    fixture.attempt_id = races::replace_method(&db.pool, &fixture, "vault_metadata").await?;
    let portal = subscription_billing_portal(&db.pool, &fixture.portal_query()).await?;
    assert!(
        portal.payment_method_display().is_none(),
        "a reused vault reference cannot prove the previous card is still current"
    );
    let financial = fixture.financial_snapshot(&db.pool).await?;
    let mut provider = QueryGateway::new(Reply::observation(Some(metadata(
        "txn_replacement",
        None,
        "Master Card",
        "2222",
        Some(12),
        Some(2030),
    ))));
    provider.expected_transaction = "txn_replacement";
    let provider = Arc::new(provider);
    let resolver = fixture.resolver(provider.clone())?;
    assert_eq!(
        refresh_payment_method_metadata(&db.pool, &resolver, old_command).await?,
        Outcome::Ineligible
    );
    assert_eq!(provider.queries.load(Ordering::SeqCst), 0);
    assert_eq!(
        refresh_payment_method_metadata(&db.pool, &resolver, fixture.command()).await?,
        Outcome::Updated
    );
    let portal = subscription_billing_portal(&db.pool, &fixture.portal_query()).await?;
    let display = portal.payment_method_display().unwrap();
    assert_eq!(display.card_brand(), Some(PaymentCardBrand::Mastercard));
    assert_eq!(display.card_last_four(), Some("2222"));
    assert_eq!(display.card_expiration_month(), Some(12));
    assert_eq!(display.card_expiration_year(), Some(2030));
    assert_eq!(fixture.financial_snapshot(&db.pool).await?, financial);
    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn card_metadata_canceled_write_transaction_rolls_back_and_releases_connection()
-> Result<(), Box<dyn Error>> {
    let db = TestDatabase::start("meta_write_abort").await?;
    let fixture = Fixture::new(&db.pool).await?;
    let before = fixture.method_snapshot(&db.pool).await?;
    let financial = fixture.financial_snapshot(&db.pool).await?;
    let options = (*db.pool.connect_options())
        .clone()
        .application_name("metadata_write_abort");
    let single = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await?;
    let provider = Arc::new(QueryGateway::new(Reply::observation(Some(metadata(
        "txn_metadata",
        None,
        "Visa",
        "1111",
        Some(10),
        Some(2029),
    )))));
    let resolver = fixture.resolver(provider.clone())?;
    let pool = single.clone();
    let command = fixture.command();
    let written = Arc::new(Notify::new());
    let mut pending = tokio::spawn(
        crate::payment_method_metadata::PAUSE_AFTER_WRITE.scope(written.clone(), async move {
            refresh_payment_method_metadata(&pool, &resolver, command).await
        }),
    );
    tokio::select! {
        ready = tokio::time::timeout(Duration::from_secs(3), written.notified()) => ready?,
        outcome = &mut pending => panic!("refresh completed before the uncommitted write pause: {outcome:?}"),
    }
    assert_eq!(provider.queries.load(Ordering::SeqCst), 1);
    assert!(
        !pending.is_finished(),
        "refresh must own an open write transaction"
    );
    assert_eq!(fixture.method_snapshot(&db.pool).await?, before);
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    // Reusing the only connection forces queued SQLx rollback to complete.
    tokio::time::timeout(
        Duration::from_secs(3),
        sqlx::query("SELECT 1").execute(&single),
    )
    .await??;
    assert_eq!(fixture.method_snapshot(&db.pool).await?, before);
    assert_eq!(fixture.financial_snapshot(&db.pool).await?, financial);
    let resolver = fixture.resolver(provider)?;
    assert_eq!(
        refresh_payment_method_metadata(&single, &resolver, fixture.command()).await?,
        Outcome::Updated
    );
    single.close().await;
    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn card_metadata_canceled_inflight_statement_rolls_back_and_releases_connection()
-> Result<(), Box<dyn Error>> {
    let db = TestDatabase::start("meta_query_abort").await?;
    let fixture = Fixture::new(&db.pool).await?;
    let before = fixture.method_snapshot(&db.pool).await?;
    let financial = fixture.financial_snapshot(&db.pool).await?;
    let options = (*db.pool.connect_options())
        .clone()
        .application_name("metadata_query_abort");
    let single = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await?;
    let mut blocker = db.pool.begin().await?;
    sqlx::query("SELECT id FROM billing_gateway_accounts WHERE id = $1 FOR UPDATE")
        .bind(fixture.account.gateway_account_id)
        .execute(&mut *blocker)
        .await?;
    let provider = Arc::new(QueryGateway::new(Reply::observation(Some(metadata(
        "txn_metadata",
        None,
        "Visa",
        "1111",
        Some(10),
        Some(2029),
    )))));
    let resolver = fixture.resolver(provider.clone())?;
    let pool = single.clone();
    let command = fixture.command();
    // Extend only this test transaction's deadlines beyond the 3 s observation
    // window, so the usual 250 ms lock timeout cannot substitute for cancellation.
    let pending = tokio::spawn(
        crate::payment_method_metadata::EXTEND_WRITE_WAIT.scope((), async move {
            refresh_payment_method_metadata(&pool, &resolver, command).await
        }),
    );
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let waiting: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE application_name = 'metadata_query_abort' AND datname = current_database() AND wait_event_type = 'Lock')")
                .fetch_one(&db.pool).await?;
            if waiting { return Ok::<_, sqlx::Error>(()); }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }).await??;
    assert_eq!(provider.queries.load(Ordering::SeqCst), 1);
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    blocker.rollback().await?;
    // Reusing the only connection forces queued SQLx rollback to complete.
    tokio::time::timeout(
        Duration::from_secs(3),
        sqlx::query("SELECT 1").execute(&single),
    )
    .await??;
    assert_eq!(fixture.method_snapshot(&db.pool).await?, before);
    assert_eq!(fixture.financial_snapshot(&db.pool).await?, financial);
    let resolver = fixture.resolver(provider)?;
    assert_eq!(
        refresh_payment_method_metadata(&single, &resolver, fixture.command()).await?,
        Outcome::Updated
    );
    single.close().await;
    db.cleanup().await?;
    Ok(())
}
