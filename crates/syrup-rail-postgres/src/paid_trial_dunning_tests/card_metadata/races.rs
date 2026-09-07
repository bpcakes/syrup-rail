use super::*;

#[tokio::test]
async fn card_metadata_refresh_respects_method_and_account_locks() -> Result<(), Box<dyn Error>> {
    let db = TestDatabase::start("meta_locks").await?;
    let fixture = Fixture::new(&db.pool).await?;
    let provider = Arc::new(QueryGateway::new(Reply::observation(Some(metadata(
        "txn_metadata",
        None,
        "Visa",
        "1111",
        Some(10),
        Some(2029),
    )))));
    let resolver = fixture.resolver(provider.clone())?;
    let before = fixture.method_snapshot(&db.pool).await?;
    for lock in ["scrub", "approval", "account"] {
        let mut tx = db.pool.begin().await?;
        if lock == "scrub" {
            crate::deletion::lock_payment_method_scrub_domain(
                &mut tx,
                fixture.scope(),
                fixture.subscriber_id,
                &fixture.account.gateway_account_id,
            )
            .await?;
        } else if lock == "approval" {
            crate::enrollment_application::lock_payment_method_domain(
                &mut tx,
                fixture.subscriber_id,
                &fixture.account.gateway_account_id,
            )
            .await?;
        } else {
            sqlx::query("SELECT id FROM billing_gateway_accounts WHERE id = $1 FOR UPDATE")
                .bind(fixture.account.gateway_account_id)
                .execute(&mut *tx)
                .await?;
        }
        let error = refresh_payment_method_metadata(&db.pool, &resolver, fixture.command())
            .await
            .unwrap_err();
        assert!(
            matches!(error, RefreshError::Storage(sqlx::Error::Database(ref error)) if error.code().as_deref() == Some("55P03"))
        );
        assert_eq!(fixture.method_snapshot(&db.pool).await?, before);
        tx.rollback().await?;
    }
    assert_eq!(
        refresh_payment_method_metadata(&db.pool, &resolver, fixture.command()).await?,
        Outcome::Updated
    );
    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn canceled_card_metadata_query_and_failed_write_leave_no_projection()
-> Result<(), Box<dyn Error>> {
    let db = TestDatabase::start("meta_rollback").await?;
    let fixture = Fixture::new(&db.pool).await?;
    let before = fixture.method_snapshot(&db.pool).await?;
    let financial = fixture.financial_snapshot(&db.pool).await?;
    let provider = Arc::new(QueryGateway::new(Reply::Never));
    let resolver = fixture.resolver(provider.clone())?;
    let pool = db.pool.clone();
    let command = fixture.command();
    let pending =
        tokio::spawn(
            async move { refresh_payment_method_metadata(&pool, &resolver, command).await },
        );
    tokio::time::timeout(Duration::from_secs(3), provider.started.notified()).await?;
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    assert_eq!(fixture.method_snapshot(&db.pool).await?, before);
    assert_eq!(fixture.financial_snapshot(&db.pool).await?, financial);
    *provider.reply.lock().await = Reply::observation(Some(metadata(
        "txn_metadata",
        None,
        "Visa",
        "1111",
        Some(10),
        Some(2029),
    )));
    let resolver = fixture.resolver(provider)?;
    sqlx::raw_sql("CREATE FUNCTION host_reject_metadata() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'private fixture detail'; END $$; CREATE TRIGGER host_reject_metadata BEFORE UPDATE ON billing_payment_methods FOR EACH ROW EXECUTE FUNCTION host_reject_metadata()")
        .execute(&db.pool).await?;
    let error = refresh_payment_method_metadata(&db.pool, &resolver, fixture.command())
        .await
        .unwrap_err();
    assert!(
        matches!(&error, RefreshError::Storage(sqlx::Error::Database(cause))
        if cause.code().as_deref() == Some("P0001"))
    );
    assert!(!format!("{error:?} {error}").contains("private fixture detail"));
    assert_eq!(fixture.method_snapshot(&db.pool).await?, before);
    assert_eq!(fixture.financial_snapshot(&db.pool).await?, financial);
    sqlx::raw_sql("CREATE OR REPLACE FUNCTION host_reject_metadata() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NULL; END $$")
        .execute(&db.pool).await?;
    assert!(matches!(
        refresh_payment_method_metadata(&db.pool, &resolver, fixture.command()).await,
        Err(RefreshError::Storage(sqlx::Error::RowNotFound))
    ));
    assert_eq!(fixture.method_snapshot(&db.pool).await?, before);
    sqlx::query("DROP TRIGGER host_reject_metadata ON billing_payment_methods")
        .execute(&db.pool)
        .await?;
    assert_eq!(
        refresh_payment_method_metadata(&db.pool, &resolver, fixture.command()).await?,
        Outcome::Updated
    );
    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn card_metadata_scrub_and_replacement_during_query_cannot_restore_display()
-> Result<(), Box<dyn Error>> {
    let db = TestDatabase::start("metadata_races").await?;
    for renewed in [false, true] {
        for replacement in [None, Some("vault_replacement"), Some("vault_metadata")] {
            let mut fixture = Fixture::new(&db.pool).await?;
            let transaction_id = if renewed {
                workflows::approve_renewal(&db.pool, &mut fixture, None).await?;
                "txn_renewal"
            } else {
                "txn_metadata"
            };
            let mut provider = QueryGateway::new(Reply::observation(Some(metadata(
                transaction_id,
                None,
                "Visa",
                "1111",
                Some(10),
                Some(2029),
            ))));
            provider.expected_transaction = transaction_id;
            provider.block = true;
            let provider = Arc::new(provider);
            let resolver = fixture.resolver(provider.clone())?;
            let pool = db.pool.clone();
            let command = fixture.command();
            let pending = tokio::spawn(async move {
                refresh_payment_method_metadata(&pool, &resolver, command).await
            });
            tokio::time::timeout(Duration::from_secs(3), provider.started.notified()).await?;
            // Completing the competing canonical write while the provider is blocked
            // proves that refresh holds neither its transaction nor domain locks.
            tokio::time::timeout(Duration::from_secs(3), async {
                if let Some(reference) = replacement {
                    replace_method(&db.pool, &fixture, reference).await?;
                } else {
                    let mut tx = db.pool.begin().await?;
                    crate::scrub_subscriber_billing_data(
                        &mut tx,
                        syrup_rail::ScrubSubscriberBillingData::new(
                            fixture.scope(),
                            fixture.subscriber_id,
                        ),
                    )
                    .await?;
                    tx.commit().await?;
                }
                Ok::<_, Box<dyn Error>>(())
            })
            .await??;
            let financial = fixture.financial_snapshot(&db.pool).await?;
            let method = fixture.method_snapshot(&db.pool).await?;
            provider.release.notify_one();
            assert_eq!(pending.await??, Outcome::ChangedDuringQuery);
            assert_eq!(fixture.financial_snapshot(&db.pool).await?, financial);
            assert_eq!(fixture.method_snapshot(&db.pool).await?, method);
            let resolver = fixture.resolver(provider.clone())?;
            assert_eq!(
                refresh_payment_method_metadata(&db.pool, &resolver, fixture.command()).await?,
                Outcome::Ineligible
            );
            assert_eq!(provider.queries.load(Ordering::SeqCst), 1);
        }
    }
    db.cleanup().await?;
    Ok(())
}

pub(super) async fn replace_method(
    pool: &PgPool,
    fixture: &Fixture,
    reference: &str,
) -> Result<PaymentAttemptId, Box<dyn Error>> {
    replace_method_with_transaction(pool, fixture, reference, "txn_replacement").await
}

pub(super) async fn replace_method_with_transaction(
    pool: &PgPool,
    fixture: &Fixture,
    reference: &str,
    transaction_id: &str,
) -> Result<PaymentAttemptId, Box<dyn Error>> {
    use syrup_rail::{
        ReplaceSubscriptionPaymentMethod, SubscriptionPaymentMethodReplacementReservationOutcome,
    };
    let gateway = resolved_gateway(fixture.account)?;
    let command = ReplaceSubscriptionPaymentMethod::new(
        syrup_rail::SubscriptionPaymentContext::new(
            PaymentAttemptId::new(Uuid::now_v7()),
            fixture.scope(),
            fixture.subscriber_id,
            gateway.gateway_configuration_id(),
            IdempotencyKey::new(format!("metadata_replace_{}", fixture.plan_key.as_str()))?,
            PaymentToken::new("replacement_token")?,
            BillingContact::new(None, None, Some("subscriber@example.test".into()))?,
        ),
        fixture.portal_query().plan_key().clone(),
    );
    let mut tx = pool.begin().await?;
    let reservation = match crate::reserve_subscription_payment_method_replacement_in_transaction(
        &mut tx,
        &command,
        &gateway,
        GatewayAccountMode::Live,
    )
    .await?
    {
        SubscriptionPaymentMethodReplacementReservationOutcome::Reserved(reservation, _) => {
            *reservation
        }
        other => return Err(format!("unexpected replacement reservation: {other:?}").into()),
    };
    tx.commit().await?;
    assert!(matches!(
        crate::admit_subscription_payment_method_replacement(pool, &reservation).await?,
        crate::SubscriptionPaymentMethodReplacementAdmissionOutcome::Admitted(_)
    ));
    let payment = crate::apply_subscription_payment_method_replacement_gateway_outcome(
        pool,
        &fixture.coordinator,
        &reservation,
        &approved_outcome_with_reference(transaction_id, reference),
    )
    .await?;
    assert_eq!(payment.status(), syrup_rail::PaymentAttemptStatus::Approved);
    Ok(payment.attempt().identity().attempt_id())
}

#[tokio::test]
async fn concurrent_card_metadata_refresh_revalidates_and_retry_is_idempotent()
-> Result<(), Box<dyn Error>> {
    let db = TestDatabase::start("meta_concurrent").await?;
    let fixture = Fixture::new(&db.pool).await?;
    let financial = fixture.financial_snapshot(&db.pool).await?;
    let mut pending = Vec::new();
    // Wait for each exact query to start so both requests observed the empty
    // candidate before either provider response is allowed to complete.
    for _ in 0..2 {
        let mut provider = QueryGateway::new(Reply::observation(Some(metadata(
            "txn_metadata",
            None,
            "AMEX",
            "1111",
            Some(10),
            Some(2029),
        ))));
        provider.block = true;
        let provider = Arc::new(provider);
        let resolver = fixture.resolver(provider.clone())?;
        let pool = db.pool.clone();
        let command = fixture.command();
        let task = tokio::spawn(async move {
            refresh_payment_method_metadata(&pool, &resolver, command).await
        });
        tokio::time::timeout(Duration::from_secs(3), provider.started.notified()).await?;
        pending.push((provider, task));
    }
    let (first, first_task) = pending.remove(0);
    first.release.notify_one();
    assert_eq!(first_task.await??, Outcome::Updated);
    let method = fixture.method_snapshot(&db.pool).await?;
    let (second, second_task) = pending.remove(0);
    second.release.notify_one();
    assert_eq!(second_task.await??, Outcome::ChangedDuringQuery);
    let resolver = fixture.resolver(second.clone())?;
    assert_eq!(
        refresh_payment_method_metadata(&db.pool, &resolver, fixture.command()).await?,
        Outcome::Unchanged
    );
    assert_eq!(first.queries.load(Ordering::SeqCst), 1);
    assert_eq!(second.queries.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.method_snapshot(&db.pool).await?, method);
    assert_eq!(fixture.financial_snapshot(&db.pool).await?, financial);
    db.cleanup().await?;
    Ok(())
}
