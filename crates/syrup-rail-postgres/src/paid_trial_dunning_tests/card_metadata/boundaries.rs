use super::*;

#[tokio::test]
async fn card_metadata_accepts_exact_unknown_lifecycle_observation_without_reapplying_payment()
-> Result<(), Box<dyn Error>> {
    let db = TestDatabase::start("meta_lifecycle").await?;
    let fixture = Fixture::new(&db.pool).await?;
    let financial = fixture.financial_snapshot(&db.pool).await?;
    let method = fixture.method_identity(&db.pool).await?;
    let (_, evidence, _) = metadata("txn_metadata", None, "Visa", "1111", Some(10), Some(2029))
        .into_parts_with_diagnostics();
    let observation = GatewayPaymentOutcome::new(GatewayPaymentStatus::Unknown, evidence);
    let resolver = fixture.resolver(Arc::new(QueryGateway::new(Reply::observation(Some(
        observation,
    )))))?;
    assert_eq!(
        refresh_payment_method_metadata(&db.pool, &resolver, fixture.command()).await?,
        Outcome::Updated
    );
    let portal = subscription_billing_portal(&db.pool, &fixture.portal_query()).await?;
    assert_eq!(
        portal.payment_method_display().unwrap().card_last_four(),
        Some("1111")
    );
    assert_eq!(fixture.financial_snapshot(&db.pool).await?, financial);
    assert_eq!(fixture.method_identity(&db.pool).await?, method);
    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn card_metadata_unknown_observed_brand_is_bounded_evidence_and_safe_display()
-> Result<(), Box<dyn Error>> {
    let db = TestDatabase::start("meta_other_brand").await?;
    let fixture = Fixture::new(&db.pool).await?;
    let raw = "Future regional scheme";
    let financial = fixture.financial_snapshot(&db.pool).await?;
    let provider = Arc::new(QueryGateway::new(Reply::observation(Some(metadata(
        "txn_metadata",
        None,
        raw,
        "",
        None,
        None,
    )))));
    let resolver = fixture.resolver(provider.clone())?;
    assert_eq!(
        refresh_payment_method_metadata(&db.pool, &resolver, fixture.command()).await?,
        Outcome::Updated
    );
    let portal = subscription_billing_portal(&db.pool, &fixture.portal_query()).await?;
    assert_eq!(
        portal.payment_method_display().unwrap().card_brand(),
        Some(PaymentCardBrand::Other)
    );
    let stored: String =
        sqlx::query_scalar("SELECT card_brand FROM billing_payment_methods WHERE id = $1")
            .bind(fixture.method_id)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(stored, raw);
    *provider.reply.lock().await = Reply::observation(Some(metadata(
        "txn_metadata",
        None,
        raw,
        "1111",
        Some(10),
        Some(2029),
    )));
    assert_eq!(
        refresh_payment_method_metadata(&db.pool, &resolver, fixture.command()).await?,
        Outcome::Updated
    );
    assert_eq!(fixture.financial_snapshot(&db.pool).await?, financial);
    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn card_metadata_brands_survive_partial_refresh_and_portal_projection()
-> Result<(), Box<dyn Error>> {
    let db = TestDatabase::start("metadata_brands").await?;
    for (raw, expected) in [
        ("AMEX", PaymentCardBrand::AmericanExpress),
        ("Visa", PaymentCardBrand::Visa),
        ("Master Card", PaymentCardBrand::Mastercard),
        ("Discover", PaymentCardBrand::Discover),
        ("JCB", PaymentCardBrand::Jcb),
        ("Diners Club", PaymentCardBrand::DinersClub),
        ("UnionPay", PaymentCardBrand::UnionPay),
        ("Maestro", PaymentCardBrand::Maestro),
    ] {
        let fixture = Fixture::new(&db.pool).await?;
        let financial = fixture.financial_snapshot(&db.pool).await?;
        let identity = fixture.method_identity(&db.pool).await?;
        let provider = Arc::new(QueryGateway::new(Reply::observation(Some(metadata(
            "txn_metadata",
            None,
            raw,
            "",
            None,
            None,
        )))));
        let resolver = fixture.resolver(provider.clone())?;
        assert_eq!(
            refresh_payment_method_metadata(&db.pool, &resolver, fixture.command()).await?,
            Outcome::Updated
        );
        let portal = subscription_billing_portal(&db.pool, &fixture.portal_query()).await?;
        assert_eq!(
            portal.payment_method_display().unwrap().card_brand(),
            Some(expected)
        );
        let stored: String =
            sqlx::query_scalar("SELECT card_brand FROM billing_payment_methods WHERE id = $1")
                .bind(fixture.method_id)
                .fetch_one(&db.pool)
                .await?;
        assert_eq!(stored, raw, "retain the same evidence format as approval");
        *provider.reply.lock().await = Reply::observation(Some(metadata(
            "txn_metadata",
            None,
            &raw.to_ascii_lowercase(),
            "1111",
            Some(10),
            Some(2029),
        )));
        assert_eq!(
            refresh_payment_method_metadata(&db.pool, &resolver, fixture.command()).await?,
            Outcome::Updated
        );
        let portal = subscription_billing_portal(&db.pool, &fixture.portal_query()).await?;
        let display = portal.payment_method_display().unwrap();
        assert_eq!(display.card_brand(), Some(expected));
        assert_eq!(display.card_last_four(), Some("1111"));
        assert_eq!(display.card_expiration_month(), Some(10));
        assert_eq!(display.card_expiration_year(), Some(2029));
        assert_eq!(
            refresh_payment_method_metadata(&db.pool, &resolver, fixture.command()).await?,
            Outcome::Unchanged
        );
        assert_eq!(provider.queries.load(Ordering::SeqCst), 2);
        assert_eq!(fixture.financial_snapshot(&db.pool).await?, financial);
        assert_eq!(fixture.method_identity(&db.pool).await?, identity);
    }
    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn card_metadata_unknown_stored_brand_does_not_block_missing_fields()
-> Result<(), Box<dyn Error>> {
    let db = TestDatabase::start("metadata_unknown").await?;
    let fixture = Fixture::new(&db.pool).await?;
    sqlx::query("UPDATE billing_payment_methods SET card_brand = 'unrecognized provider scheme' WHERE id = $1")
        .bind(fixture.method_id).execute(&db.pool).await?;
    let financial = fixture.financial_snapshot(&db.pool).await?;
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
    let portal = subscription_billing_portal(&db.pool, &fixture.portal_query()).await?;
    let display = portal.payment_method_display().unwrap();
    assert_eq!(display.card_brand(), Some(PaymentCardBrand::Other));
    assert_eq!(display.card_last_four(), Some("1111"));
    assert_eq!(display.card_expiration_month(), Some(10));
    assert_eq!(display.card_expiration_year(), Some(2029));
    let stored: String =
        sqlx::query_scalar("SELECT card_brand FROM billing_payment_methods WHERE id = $1")
            .bind(fixture.method_id)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(stored, "unrecognized provider scheme");
    assert_eq!(fixture.financial_snapshot(&db.pool).await?, financial);
    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn card_metadata_diagnostics_allow_only_absent_optional_vault_reference()
-> Result<(), Box<dyn Error>> {
    let db = TestDatabase::start("meta_diagnostics").await?;
    for diagnostic in GatewayPaymentDiagnostic::ALL {
        let fixture = Fixture::new(&db.pool).await?;
        let before = fixture.method_snapshot(&db.pool).await?;
        let financial = fixture.financial_snapshot(&db.pool).await?;
        let observation = metadata("txn_metadata", None, "Visa", "1111", Some(10), Some(2029))
            .with_diagnostics(vec![*diagnostic]);
        let resolver = fixture.resolver(Arc::new(QueryGateway::new(Reply::observation(Some(
            observation,
        )))))?;
        let outcome =
            refresh_payment_method_metadata(&db.pool, &resolver, fixture.command()).await?;
        if *diagnostic == GatewayPaymentDiagnostic::MissingPaymentMethodReference {
            assert_eq!(outcome, Outcome::Updated);
            let portal = subscription_billing_portal(&db.pool, &fixture.portal_query()).await?;
            assert_eq!(
                portal.payment_method_display().unwrap().card_last_four(),
                Some("1111")
            );
        } else {
            assert_eq!(outcome, Outcome::EvidenceRejected);
            assert_eq!(fixture.method_snapshot(&db.pool).await?, before);
        }
        assert_eq!(fixture.financial_snapshot(&db.pool).await?, financial);
    }
    db.cleanup().await?;
    Ok(())
}
