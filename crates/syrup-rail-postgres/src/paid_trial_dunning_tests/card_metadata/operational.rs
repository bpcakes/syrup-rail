use super::*;

#[tokio::test]
async fn card_metadata_expiry_components_fill_independently_and_reject_conflicts()
-> Result<(), Box<dyn Error>> {
    let db = TestDatabase::start("meta_expiry").await?;
    for (first_month, first_year) in [(Some(10), None), (None, Some(2029))] {
        let fixture = Fixture::new(&db.pool).await?;
        let financial = fixture.financial_snapshot(&db.pool).await?;
        let provider = Arc::new(QueryGateway::new(Reply::observation(Some(metadata(
            "txn_metadata",
            None,
            "",
            "",
            first_month,
            first_year,
        )))));
        let resolver = fixture.resolver(provider.clone())?;
        assert_eq!(
            refresh_payment_method_metadata(&db.pool, &resolver, fixture.command()).await?,
            Outcome::Updated
        );
        let fields: (Option<i16>, Option<i16>) = sqlx::query_as(
            "SELECT card_exp_month, card_exp_year FROM billing_payment_methods WHERE id = $1",
        )
        .bind(fixture.method_id)
        .fetch_one(&db.pool)
        .await?;
        assert_eq!(fields, (first_month, first_year));
        let before = fixture.method_snapshot(&db.pool).await?;
        *provider.reply.lock().await = Reply::observation(Some(metadata(
            "txn_metadata",
            None,
            "",
            "",
            Some(11),
            Some(2030),
        )));
        assert_eq!(
            refresh_payment_method_metadata(&db.pool, &resolver, fixture.command()).await?,
            Outcome::EvidenceRejected
        );
        assert_eq!(fixture.method_snapshot(&db.pool).await?, before);
        *provider.reply.lock().await = Reply::observation(Some(metadata(
            "txn_metadata",
            None,
            "",
            "",
            Some(10),
            Some(2029),
        )));
        assert_eq!(
            refresh_payment_method_metadata(&db.pool, &resolver, fixture.command()).await?,
            Outcome::Updated
        );
        let portal = subscription_billing_portal(&db.pool, &fixture.portal_query()).await?;
        let display = portal.payment_method_display().unwrap();
        assert_eq!(display.card_expiration_month(), Some(10));
        assert_eq!(display.card_expiration_year(), Some(2029));
        assert_eq!(display.card_brand(), None);
        assert_eq!(fixture.financial_snapshot(&db.pool).await?, financial);
    }
    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn card_metadata_honors_account_and_provider_cooldowns_before_io()
-> Result<(), Box<dyn Error>> {
    let db = TestDatabase::start("meta_cooldowns").await?;
    let fixture = Fixture::new(&db.pool).await?;
    let financial = fixture.financial_snapshot(&db.pool).await?;
    let provider = Arc::new(QueryGateway::new(Reply::observation(Some(metadata(
        "txn_metadata",
        None,
        "Visa",
        "1111",
        Some(10),
        Some(2029),
    )))));
    let resolver = fixture.resolver(provider.clone())?;
    for account in [true, false] {
        sqlx::query("UPDATE billing_gateway_accounts SET mutation_rate_limited_until = CASE WHEN $2 THEN clock_timestamp() + interval '1 hour' ELSE NULL END WHERE id = $1")
            .bind(fixture.account.gateway_account_id).bind(account).execute(&db.pool).await?;
        sqlx::query("UPDATE billing_gateway_provider_rate_limits SET rate_limited_until = CASE WHEN $1 THEN '-infinity'::timestamptz ELSE clock_timestamp() + interval '1 hour' END WHERE provider_key = 'nmi'")
            .bind(account).execute(&db.pool).await?;
        let expected_scope = if account {
            crate::GatewayMutationCooldownScope::Account
        } else {
            crate::GatewayMutationCooldownScope::Provider
        };
        assert!(
            matches!(refresh_payment_method_metadata(&db.pool, &resolver, fixture.command()).await,
            Err(RefreshError::CooldownActive { scope }) if scope == expected_scope)
        );
        assert_eq!(provider.queries.load(Ordering::SeqCst), 0);
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 0);
    }
    sqlx::query("UPDATE billing_gateway_provider_rate_limits SET rate_limited_until = '-infinity' WHERE provider_key = 'nmi'")
        .execute(&db.pool).await?;
    assert_eq!(
        refresh_payment_method_metadata(&db.pool, &resolver, fixture.command()).await?,
        Outcome::Updated
    );
    assert_eq!(provider.queries.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.financial_snapshot(&db.pool).await?, financial);
    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn card_metadata_query_throttle_persists_shared_provider_cooldown()
-> Result<(), Box<dyn Error>> {
    let db = TestDatabase::start("meta_throttle").await?;
    let fixture = Fixture::new(&db.pool).await?;
    let financial = fixture.financial_snapshot(&db.pool).await?;
    let before = fixture.method_snapshot(&db.pool).await?;
    let provider = Arc::new(QueryGateway::new(Reply::RateLimited));
    let resolver = fixture.resolver(provider.clone())?;
    assert!(matches!(
        refresh_payment_method_metadata(&db.pool, &resolver, fixture.command()).await,
        Err(RefreshError::Query(GatewayError::RateLimited(_)))
    ));
    let active: bool = sqlx::query_scalar("SELECT rate_limited_until > clock_timestamp() FROM billing_gateway_provider_rate_limits WHERE provider_key = 'nmi'")
        .fetch_one(&db.pool).await?;
    assert!(
        active,
        "query throttling must be visible to financial admission"
    );
    let other = Fixture::new(&db.pool).await?;
    let other_provider = Arc::new(QueryGateway::new(Reply::Unavailable));
    let other_resolver = other.resolver(other_provider.clone())?;
    assert!(matches!(
        refresh_payment_method_metadata(&db.pool, &other_resolver, other.command()).await,
        Err(RefreshError::CooldownActive {
            scope: crate::GatewayMutationCooldownScope::Provider
        })
    ));
    assert_eq!(other_provider.queries.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.method_snapshot(&db.pool).await?, before);
    assert_eq!(fixture.financial_snapshot(&db.pool).await?, financial);
    assert_eq!(provider.queries.load(Ordering::SeqCst), 1);
    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn card_metadata_failed_cooldown_write_preserves_throttle_and_storage_errors()
-> Result<(), Box<dyn Error>> {
    let db = TestDatabase::start("meta_limit_error").await?;
    let fixture = Fixture::new(&db.pool).await?;
    let financial = fixture.financial_snapshot(&db.pool).await?;
    let method = fixture.method_snapshot(&db.pool).await?;
    let provider = Arc::new(QueryGateway::new(Reply::RateLimited));
    let resolver = fixture.resolver(provider.clone())?;
    sqlx::raw_sql("CREATE FUNCTION host_reject_cooldown() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'private cooldown detail'; END $$; CREATE TRIGGER host_reject_cooldown BEFORE UPDATE ON billing_gateway_provider_rate_limits FOR EACH ROW EXECUTE FUNCTION host_reject_cooldown()")
        .execute(&db.pool).await?;
    let error = refresh_payment_method_metadata(&db.pool, &resolver, fixture.command())
        .await
        .unwrap_err();
    assert!(
        matches!(&error, RefreshError::RateLimitCooldownPersistenceFailed {
        query: GatewayError::RateLimited(_), storage: sqlx::Error::Database(cause),
    } if cause.code().as_deref() == Some("P0001"))
    );
    assert!(!format!("{error:?} {error}").contains("private cooldown detail"));
    assert_eq!(provider.queries.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.financial_snapshot(&db.pool).await?, financial);
    assert_eq!(fixture.method_snapshot(&db.pool).await?, method);
    db.cleanup().await?;
    Ok(())
}
