use super::*;
use crate::{
    PaymentMethodMetadataRefreshError as RefreshError,
    PaymentMethodMetadataRefreshOutcome as Outcome, RefreshPaymentMethodMetadata,
    refresh_payment_method_metadata, subscription_billing_portal,
};
use syrup_rail::{PaymentCardBrand, SubscriptionBillingPortalQuery};
use tokio::sync::Notify;

mod boundaries;
mod fixture;
mod operational;
mod races;
mod review_races;
mod shared_methods;
mod workflows;
use fixture::*;

#[tokio::test]
async fn approved_empty_card_metadata_refresh_populates_portal_without_financial_changes()
-> Result<(), Box<dyn Error>> {
    let db = TestDatabase::start("metadata_portal").await?;
    let fixture = Fixture::new(&db.pool).await?;
    let before = fixture.financial_snapshot(&db.pool).await?;
    let method_before = fixture.method_identity(&db.pool).await?;
    let portal = subscription_billing_portal(&db.pool, &fixture.portal_query()).await?;
    assert!(portal.payment_method_display().is_none());
    assert!(portal.entitlement().permits_product_access());
    let events_before = fixture.coordinator.events.lock().await.len();

    let provider = Arc::new(QueryGateway::new(Reply::observation(Some(metadata(
        "txn_metadata",
        Some("vault_metadata"),
        "Visa",
        "1111",
        Some(10),
        Some(2029),
    )))));
    let resolver = fixture.resolver(provider.clone())?;
    let service = SubscriptionBillingService::new(
        db.pool.clone(),
        Arc::new(StaticOfferStore {
            offer: paid_trial_offer()?,
        }),
        Arc::new(resolver),
        Arc::new(PermitAdmission),
        Arc::new(fixture.coordinator.clone()),
    );
    assert_eq!(
        service
            .refresh_payment_method_metadata(fixture.command())
            .await?,
        Outcome::Updated
    );
    let portal = subscription_billing_portal(&db.pool, &fixture.portal_query()).await?;
    let display = portal
        .payment_method_display()
        .expect("query fills saved method display");
    assert_eq!(display.card_brand(), Some(PaymentCardBrand::Visa));
    assert_eq!(display.card_last_four(), Some("1111"));
    assert_eq!(display.card_expiration_month(), Some(10));
    assert_eq!(display.card_expiration_year(), Some(2029));
    assert!(portal.entitlement().permits_product_access());
    assert_eq!(fixture.financial_snapshot(&db.pool).await?, before);
    assert_eq!(fixture.method_identity(&db.pool).await?, method_before);
    assert_eq!(fixture.coordinator.events.lock().await.len(), events_before);
    assert_eq!(
        service
            .refresh_payment_method_metadata(fixture.command())
            .await?,
        Outcome::Unchanged
    );
    assert_eq!(provider.queries.load(Ordering::SeqCst), 1);
    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn card_metadata_missing_failed_and_conflicting_queries_allow_later_refresh()
-> Result<(), Box<dyn Error>> {
    let db = TestDatabase::start("meta_failures").await?;
    let cases = [
        (Reply::observation(None), Some(Outcome::NotFound)),
        (
            Reply::observation(Some(approved_outcome_with_reference(
                "txn_metadata",
                "vault_metadata",
            ))),
            Some(Outcome::Unchanged),
        ),
        (Reply::Malformed, None),
        (Reply::Unavailable, None),
        (Reply::Never, None),
        (
            Reply::observation(Some(metadata(
                "wrong_transaction",
                None,
                "Visa",
                "1111",
                Some(10),
                Some(2029),
            ))),
            Some(Outcome::EvidenceRejected),
        ),
        (
            Reply::observation(Some(metadata(
                "txn_metadata",
                Some("wrong_vault"),
                "Visa",
                "1111",
                Some(10),
                Some(2029),
            ))),
            Some(Outcome::EvidenceRejected),
        ),
        (
            Reply::observation(Some(declined_outcome("txn_metadata"))),
            Some(Outcome::EvidenceRejected),
        ),
    ];
    for (reply, expected) in cases {
        let expects_timeout = matches!(&reply, Reply::Never);
        let fixture = Fixture::new(&db.pool).await?;
        let before = fixture.financial_snapshot(&db.pool).await?;
        let method = fixture.method_snapshot(&db.pool).await?;
        let provider = Arc::new(QueryGateway::new(reply));
        let resolver = fixture.resolver(provider.clone())?;
        let result = tokio::time::timeout(
            Duration::from_secs(12),
            refresh_payment_method_metadata(&db.pool, &resolver, fixture.command()),
        )
        .await
        .expect("refresh must finish within its provider timeout and local query budget");
        match expected {
            Some(expected) => assert_eq!(result?, expected),
            None if expects_timeout => assert!(matches!(result, Err(RefreshError::QueryTimedOut))),
            None => assert!(matches!(result, Err(RefreshError::Query(_)))),
        }
        assert_eq!(provider.queries.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.financial_snapshot(&db.pool).await?, before);
        assert_eq!(fixture.method_snapshot(&db.pool).await?, method);
        // Query responses need not repeat vault linkage when exact transaction
        // correlation and the durable attempt/method linkage are available.
        *provider.reply.lock().await = Reply::observation(Some(metadata(
            "txn_metadata",
            None,
            "Visa",
            "1111",
            Some(10),
            Some(2029),
        )));
        assert_eq!(
            refresh_payment_method_metadata(&db.pool, &resolver, fixture.command()).await?,
            Outcome::Updated
        );
        assert_eq!(provider.queries.load(Ordering::SeqCst), 2);
        assert_eq!(fixture.financial_snapshot(&db.pool).await?, before);
    }
    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn card_metadata_refresh_preserves_existing_fields_and_rejects_conflicts()
-> Result<(), Box<dyn Error>> {
    let db = TestDatabase::start("metadata_merge").await?;
    let fixture = Fixture::new(&db.pool).await?;
    sqlx::query("UPDATE billing_payment_methods SET card_brand = 'VISA', card_last4 = '1111', card_exp_month = 10 WHERE id = $1")
        .bind(fixture.method_id).execute(&db.pool).await?;
    let before = fixture.method_snapshot(&db.pool).await?;
    let provider = Arc::new(QueryGateway::new(Reply::observation(None)));
    let resolver = fixture.resolver(provider.clone())?;
    for (brand, last4, month) in [
        ("Mastercard", "1111", 10),
        ("Visa", "2222", 10),
        ("Visa", "1111", 11),
    ] {
        *provider.reply.lock().await = Reply::observation(Some(metadata(
            "txn_metadata",
            None,
            brand,
            last4,
            Some(month),
            Some(2029),
        )));
        assert_eq!(
            refresh_payment_method_metadata(&db.pool, &resolver, fixture.command()).await?,
            Outcome::EvidenceRejected
        );
        assert_eq!(fixture.method_snapshot(&db.pool).await?, before);
    }
    // Invalid expiry and raw PAN are discarded by core descriptor validation.
    *provider.reply.lock().await = Reply::observation(Some(metadata(
        "txn_metadata",
        None,
        "Visa",
        "4111111111111111",
        Some(13),
        Some(1999),
    )));
    assert_eq!(
        refresh_payment_method_metadata(&db.pool, &resolver, fixture.command()).await?,
        Outcome::Unchanged
    );
    assert_eq!(fixture.method_snapshot(&db.pool).await?, before);
    *provider.reply.lock().await = Reply::observation(Some(metadata(
        "txn_metadata",
        None,
        "Visa",
        "1111",
        Some(10),
        Some(2029),
    )));
    assert_eq!(
        refresh_payment_method_metadata(&db.pool, &resolver, fixture.command()).await?,
        Outcome::Updated
    );
    let fields: (String, String, i16, i16) = sqlx::query_as("SELECT card_brand, card_last4, card_exp_month, card_exp_year FROM billing_payment_methods WHERE id = $1").bind(fixture.method_id).fetch_one(&db.pool).await?;
    assert_eq!(fields, ("VISA".into(), "1111".into(), 10, 2029));
    db.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn card_metadata_refresh_rejects_wrong_owner_and_resolved_identity_before_io()
-> Result<(), Box<dyn Error>> {
    let db = TestDatabase::start("metadata_owner").await?;
    let fixture = Fixture::new(&db.pool).await?;
    let before = fixture.financial_snapshot(&db.pool).await?;
    let provider = Arc::new(QueryGateway::new(Reply::observation(None)));
    let resolver = fixture.resolver(provider.clone())?;
    for command in [
        RefreshPaymentMethodMetadata::new(
            BillingScopeId::new(Uuid::now_v7()),
            fixture.subscriber_id,
            fixture.attempt_id,
        ),
        RefreshPaymentMethodMetadata::new(
            fixture.scope(),
            SubscriberId::new(Uuid::now_v7()),
            fixture.attempt_id,
        ),
        RefreshPaymentMethodMetadata::new(
            fixture.scope(),
            fixture.subscriber_id,
            PaymentAttemptId::new(Uuid::now_v7()),
        ),
    ] {
        assert_eq!(
            refresh_payment_method_metadata(&db.pool, &resolver, command).await?,
            Outcome::Ineligible
        );
    }
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 0);
    for mismatch in 0..4 {
        let correct = fixture.resolver(provider.clone())?.gateway;
        let wrong = ResolvedGateway::new(
            if mismatch == 0 {
                BillingScopeId::new(Uuid::now_v7())
            } else {
                correct.billing_scope_id()
            },
            if mismatch == 1 {
                GatewayAccountId::new(Uuid::now_v7())
            } else {
                correct.gateway_account_id()
            },
            if mismatch == 2 {
                GatewayConfigurationId::new(Uuid::now_v7())
            } else {
                correct.gateway_configuration_id()
            },
            if mismatch == 3 {
                GatewayProviderKey::new("other")?
            } else {
                correct.provider_key().clone()
            },
            correct.lifecycle_query_policy().clone(),
            correct.mutation_reference_factory(),
            provider.clone(),
        );
        let resolver = UncheckedResolver(wrong);
        assert!(matches!(
            refresh_payment_method_metadata(&db.pool, &resolver, fixture.command()).await,
            Err(RefreshError::GatewayIdentityMismatch)
        ));
    }
    assert_eq!(provider.queries.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.financial_snapshot(&db.pool).await?, before);
    db.cleanup().await?;
    Ok(())
}
