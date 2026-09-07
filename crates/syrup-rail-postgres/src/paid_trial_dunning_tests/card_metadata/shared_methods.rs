use super::*;

#[tokio::test]
async fn card_metadata_latest_approval_can_repair_a_method_still_used_by_another_plan()
-> Result<(), Box<dyn Error>> {
    let db = TestDatabase::start("meta_two_plans").await?;
    for remove_last_reference in [false, true] {
        let first = Fixture::new(&db.pool).await?;
        let second = approve_shared_plan(&db.pool, &first).await?;
        assert_eq!(first.method_id, second.method_id);
        races::replace_method_with_transaction(
            &db.pool,
            &second,
            "vault_second_replacement",
            "txn_second_replacement",
        )
        .await?;
        let status: String =
            sqlx::query_scalar("SELECT status FROM billing_payment_methods WHERE id = $1")
                .bind(first.method_id)
                .fetch_one(&db.pool)
                .await?;
        assert_eq!(
            status, "active",
            "the first plan still uses the shared method"
        );
        let mut provider = QueryGateway::new(Reply::observation(Some(metadata(
            "txn_shared_plan",
            None,
            "Master Card",
            "2222",
            Some(12),
            Some(2030),
        ))));
        provider.expected_transaction = "txn_shared_plan";
        let provider = Arc::new(provider);
        let resolver = first.resolver(provider.clone())?;
        assert_eq!(
            refresh_payment_method_metadata(&db.pool, &resolver, first.command()).await?,
            Outcome::Ineligible
        );
        assert_eq!(
            provider.queries.load(Ordering::SeqCst),
            0,
            "older approval must never refill potentially different card evidence"
        );

        if remove_last_reference {
            let mut blocked = QueryGateway::new(Reply::observation(Some(metadata(
                "txn_shared_plan",
                None,
                "Master Card",
                "2222",
                Some(12),
                Some(2030),
            ))));
            blocked.expected_transaction = "txn_shared_plan";
            blocked.block = true;
            let blocked = Arc::new(blocked);
            let resolver = second.resolver(blocked.clone())?;
            let pool = db.pool.clone();
            let command = second.command();
            let pending = tokio::spawn(async move {
                refresh_payment_method_metadata(&pool, &resolver, command).await
            });
            tokio::time::timeout(Duration::from_secs(3), blocked.started.notified()).await?;
            races::replace_method(&db.pool, &first, "vault_first_replacement").await?;
            let financial = first.financial_snapshot(&db.pool).await?;
            let method = first.method_snapshot(&db.pool).await?;
            blocked.release.notify_one();
            assert_eq!(pending.await??, Outcome::ChangedDuringQuery);
            assert_eq!(first.financial_snapshot(&db.pool).await?, financial);
            assert_eq!(first.method_snapshot(&db.pool).await?, method);
        } else {
            let financial = first.financial_snapshot(&db.pool).await?;
            let method_identity = first.method_identity(&db.pool).await?;
            assert_eq!(
                refresh_payment_method_metadata(&db.pool, &resolver, second.command()).await?,
                Outcome::Updated
            );
            assert_eq!(provider.queries.load(Ordering::SeqCst), 1);
            let portal = subscription_billing_portal(&db.pool, &first.portal_query()).await?;
            let display = portal.payment_method_display().unwrap();
            assert_eq!(display.card_brand(), Some(PaymentCardBrand::Mastercard));
            assert_eq!(display.card_last_four(), Some("2222"));
            assert_eq!(display.card_expiration_month(), Some(12));
            assert_eq!(display.card_expiration_year(), Some(2030));
            let other_portal =
                subscription_billing_portal(&db.pool, &second.portal_query()).await?;
            assert!(
                other_portal.payment_method_display().is_none(),
                "the second plan's new method is untouched"
            );
            assert_eq!(first.financial_snapshot(&db.pool).await?, financial);
            assert_eq!(first.method_identity(&db.pool).await?, method_identity);
        }
    }
    db.cleanup().await?;
    Ok(())
}

async fn approve_shared_plan(pool: &PgPool, first: &Fixture) -> Result<Fixture, Box<dyn Error>> {
    let original = paid_trial_offer()?;
    let plan_key = PlanKey::new("second_plan")?;
    let offer = syrup_rail::SubscriptionOffer::new(
        plan_key.clone(),
        original.recurring(),
        original.start(),
        original.renewal_failure().clone(),
    )?;
    let gateway = resolved_gateway(first.account)?;
    let payment = approve_enrollment(
        pool,
        &StaticOfferStore {
            offer: offer.clone(),
        },
        &gateway,
        &first.coordinator,
        first.account,
        first.subscriber_id,
        "metadata_second_plan",
        SubscriptionEnrollmentExpectedTerms::full_price(offer),
        "txn_shared_plan",
        "vault_metadata",
    )
    .await?;
    let subscription_id = payment.subscription().expect("second plan approved").id();
    let method_id: Uuid =
        sqlx::query_scalar("SELECT payment_method_id FROM billing_subscriptions WHERE id = $1")
            .bind(subscription_id.as_uuid())
            .fetch_one(pool)
            .await?;
    Ok(Fixture {
        account: first.account,
        subscriber_id: first.subscriber_id,
        attempt_id: payment.attempt().identity().attempt_id(),
        method_id,
        subscription_id,
        plan_key,
        coordinator: first.coordinator.clone(),
    })
}

#[tokio::test]
async fn card_metadata_accepts_a_provider_reference_with_an_erased_prefix()
-> Result<(), Box<dyn Error>> {
    let db = TestDatabase::start("meta_ref_prefix").await?;
    let fixture = Fixture::new_with_reference(&db.pool, "erased:provider_reference").await?;
    let before = fixture.financial_snapshot(&db.pool).await?;
    let resolver = fixture.resolver(Arc::new(QueryGateway::new(Reply::observation(Some(
        metadata(
            "txn_metadata",
            Some("erased:provider_reference"),
            "Visa",
            "1111",
            Some(10),
            Some(2029),
        ),
    )))))?;
    assert_eq!(
        refresh_payment_method_metadata(&db.pool, &resolver, fixture.command()).await?,
        Outcome::Updated
    );
    assert_eq!(fixture.financial_snapshot(&db.pool).await?, before);
    db.cleanup().await?;
    Ok(())
}
