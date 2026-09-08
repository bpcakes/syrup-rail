use super::*;

async fn assert_default_timeouts(pool: &PgPool) -> Result<i32, Box<dyn Error>> {
    let (pid, lock, statement): (i32, String, String) = tokio::time::timeout(
        Duration::from_secs(3),
        sqlx::query_as(
            "SELECT pg_backend_pid(), current_setting('lock_timeout'), \
             current_setting('statement_timeout')",
        )
        .fetch_one(pool),
    )
    .await??;
    assert_eq!((lock.as_str(), statement.as_str()), ("0", "0"));
    Ok(pid)
}

#[tokio::test]
async fn card_metadata_initial_reads_time_out_before_io_and_restore_connection()
-> Result<(), Box<dyn Error>> {
    let db = TestDatabase::start("meta_read_limits").await?;
    let fixture = Fixture::new(&db.pool).await?;
    let financial = fixture.financial_snapshot(&db.pool).await?;
    let method = fixture.method_snapshot(&db.pool).await?;
    let single = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_with((*db.pool.connect_options()).clone())
        .await?;
    sqlx::query("SELECT set_config('lock_timeout', '0', false), set_config('statement_timeout', '0', false)")
        .execute(&single).await?;
    let pid = assert_default_timeouts(&single).await?;
    let provider = Arc::new(QueryGateway::new(Reply::observation(None)));
    let resolver = fixture.resolver(provider.clone())?;
    // The method table blocks the candidate read; the provider table is used
    // only by the subsequent cooldown read. Acquire each lock before refresh.
    for lock in [
        "LOCK TABLE billing_payment_methods IN ACCESS EXCLUSIVE MODE",
        "LOCK TABLE billing_gateway_provider_rate_limits IN ACCESS EXCLUSIVE MODE",
    ] {
        let mut blocker = db.pool.begin().await?;
        sqlx::query(lock).execute(&mut *blocker).await?;
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            refresh_payment_method_metadata(&single, &resolver, fixture.command()),
        )
        .await
        .expect("the database lock timeout must fire while the blocker still holds its lock");
        assert!(
            matches!(result, Err(RefreshError::Storage(sqlx::Error::Database(cause)))
            if cause.code().as_deref() == Some("55P03"))
        );
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 0);
        assert_eq!(provider.queries.load(Ordering::SeqCst), 0);
        blocker.rollback().await?;
        // One connection proves SQLx completed rollback and restored settings,
        // rather than hiding a leaked transaction behind another pool checkout.
        assert_eq!(assert_default_timeouts(&single).await?, pid);
        assert_eq!(fixture.financial_snapshot(&db.pool).await?, financial);
        assert_eq!(fixture.method_snapshot(&db.pool).await?, method);
    }
    assert_eq!(
        refresh_payment_method_metadata(&single, &resolver, fixture.command()).await?,
        Outcome::NotFound
    );
    assert_eq!(provider.queries.load(Ordering::SeqCst), 1);
    assert_eq!(assert_default_timeouts(&single).await?, pid);
    single.close().await;
    db.cleanup().await
}

struct PoolCheckingResolver {
    pool: PgPool,
    inner: CountingResolver,
}

#[async_trait]
impl GatewayResolver for PoolCheckingResolver {
    async fn resolve(
        &self,
        scope: BillingScopeId,
        account: GatewayAccountId,
        configuration: GatewayConfigurationId,
        provider: GatewayProviderKey,
    ) -> Result<ResolvedGateway, GatewayResolutionError> {
        assert_default_timeouts(&self.pool)
            .await
            .expect("the read transaction must release the only connection before host resolution");
        self.inner
            .resolve(scope, account, configuration, provider)
            .await
    }
}

#[tokio::test]
async fn card_metadata_releases_read_transaction_before_resolution_and_provider_io()
-> Result<(), Box<dyn Error>> {
    let db = TestDatabase::start("meta_read_free").await?;
    let fixture = Fixture::new(&db.pool).await?;
    let single = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_with((*db.pool.connect_options()).clone())
        .await?;
    sqlx::query("SELECT set_config('lock_timeout', '0', false), set_config('statement_timeout', '0', false)")
        .execute(&single).await?;
    let pid = assert_default_timeouts(&single).await?;
    let mut provider = QueryGateway::new(Reply::observation(None));
    provider.block = true;
    let provider = Arc::new(provider);
    let resolver = PoolCheckingResolver {
        pool: single.clone(),
        inner: fixture.resolver(provider.clone())?,
    };
    let pool = single.clone();
    let command = fixture.command();
    let mut pending =
        tokio::spawn(
            async move { refresh_payment_method_metadata(&pool, &resolver, command).await },
        );
    tokio::select! {
        ready = tokio::time::timeout(Duration::from_secs(5), provider.started.notified()) => ready?,
        result = &mut pending => panic!("refresh ended before the provider query: {result:?}"),
    }
    assert_eq!(assert_default_timeouts(&single).await?, pid);
    // A DDL-strength lock also proves no read transaction retained table locks.
    let mut lock = db.pool.begin().await?;
    sqlx::query("LOCK TABLE billing_payment_methods IN ACCESS EXCLUSIVE MODE NOWAIT")
        .execute(&mut *lock)
        .await?;
    lock.rollback().await?;
    provider.release.notify_one();
    assert_eq!(pending.await??, Outcome::NotFound);
    single.close().await;
    db.cleanup().await
}
