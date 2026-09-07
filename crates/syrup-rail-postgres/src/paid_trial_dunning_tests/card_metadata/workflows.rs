use super::*;

#[tokio::test]
async fn card_metadata_refreshes_approved_replacement_renewal_and_recovery_attempts()
-> Result<(), Box<dyn Error>> {
    let db = TestDatabase::start("meta_workflows").await?;
    for workflow in [
        "replacement",
        "renewal",
        "renewal_without_reference",
        "recovery",
    ] {
        let mut fixture = Fixture::new(&db.pool).await?;
        let old_command = fixture.command();
        let gateway = resolved_gateway(fixture.account)?;
        let transaction_id = match workflow {
            "replacement" => {
                fixture.attempt_id =
                    races::replace_method(&db.pool, &fixture, "vault_replacement").await?;
                "txn_replacement"
            }
            "renewal" | "renewal_without_reference" => {
                approve_renewal(
                    &db.pool,
                    &mut fixture,
                    (workflow == "renewal").then_some("vault_metadata"),
                )
                .await?;
                "txn_renewal"
            }
            "recovery" => {
                let due_at = make_trial_due(&db.pool, fixture.subscription_id).await?;
                decline_due_renewal(
                    &db.pool,
                    &gateway,
                    &fixture.coordinator,
                    fixture.scope(),
                    fixture.subscription_id,
                    due_at,
                    "txn_declined",
                )
                .await?;
                let command = RecoverSubscriptionPayment::new(
                    syrup_rail::SubscriptionPaymentContext::new(
                        PaymentAttemptId::new(Uuid::now_v7()),
                        fixture.scope(),
                        fixture.subscriber_id,
                        gateway.gateway_configuration_id(),
                        IdempotencyKey::new("metadata_recovery")?,
                        PaymentToken::new("recovery_token")?,
                        BillingContact::new(None, None, Some("subscriber@example.test".into()))?,
                    ),
                    fixture.portal_query().plan_key().clone(),
                );
                let reservation = reserve_and_admit_recovery(&db.pool, &gateway, &command).await?;
                let payment = apply_subscription_recovery_gateway_outcome(
                    &db.pool,
                    &fixture.coordinator,
                    &reservation,
                    &approved_outcome_with_reference("txn_recovery", "vault_recovery"),
                )
                .await?;
                assert_eq!(payment.status(), syrup_rail::PaymentAttemptStatus::Approved);
                fixture.attempt_id = payment.attempt().identity().attempt_id();
                "txn_recovery"
            }
            _ => unreachable!(),
        };
        fixture.method_id =
            sqlx::query_scalar("SELECT payment_method_id FROM billing_subscriptions WHERE id = $1")
                .bind(fixture.subscription_id.as_uuid())
                .fetch_one(&db.pool)
                .await?;
        let financial = fixture.financial_snapshot(&db.pool).await?;
        let identity = fixture.method_identity(&db.pool).await?;
        let event_count = fixture.coordinator.events.lock().await.len();
        let mut provider = QueryGateway::new(Reply::observation(Some(metadata(
            transaction_id,
            None,
            "UnionPay",
            "1111",
            Some(10),
            Some(2029),
        ))));
        provider.expected_transaction = transaction_id;
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
        assert_eq!(display.card_brand(), Some(PaymentCardBrand::UnionPay));
        assert_eq!(display.card_last_four(), Some("1111"));
        assert_eq!(display.card_expiration_month(), Some(10));
        assert_eq!(display.card_expiration_year(), Some(2029));
        assert_eq!(fixture.financial_snapshot(&db.pool).await?, financial);
        assert_eq!(fixture.method_identity(&db.pool).await?, identity);
        assert_eq!(fixture.coordinator.events.lock().await.len(), event_count);
        assert_eq!(provider.queries.load(Ordering::SeqCst), 1);
    }
    db.cleanup().await?;
    Ok(())
}

pub(super) async fn approve_renewal(
    pool: &PgPool,
    fixture: &mut Fixture,
    reference: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    let gateway = resolved_gateway(fixture.account)?;
    let due_at = make_trial_due(pool, fixture.subscription_id).await?;
    let reservation = reserve_and_admit_renewal(
        pool,
        &gateway,
        ChargeRenewal::new(fixture.scope(), fixture.subscription_id, due_at),
    )
    .await?;
    let outcome = GatewayPaymentOutcome::new(
        GatewayPaymentStatus::Approved,
        ProcessorEvidence::new(
            Some(GatewayTransactionId::new("txn_renewal")?),
            reference
                .map(GatewayPaymentMethodReference::new)
                .transpose()?,
            None,
            None,
            None,
            None,
            GatewayPaymentDescriptor::default(),
        ),
    );
    let payment = apply_subscription_renewal_gateway_outcome(
        pool,
        &fixture.coordinator,
        &reservation,
        &outcome,
    )
    .await?;
    assert_eq!(payment.status(), syrup_rail::PaymentAttemptStatus::Approved);
    fixture.attempt_id = payment.attempt().identity().attempt_id();
    let stored: Option<String> = sqlx::query_scalar(
        "SELECT gateway_payment_method_reference FROM billing_payment_attempts WHERE id = $1",
    )
    .bind(fixture.attempt_id.as_uuid())
    .fetch_one(pool)
    .await?;
    assert_eq!(stored.as_deref(), reference);
    Ok(())
}

#[tokio::test]
async fn card_metadata_rejects_declined_renewal_and_conflicting_durable_reference()
-> Result<(), Box<dyn Error>> {
    let db = TestDatabase::start("meta_admission").await?;
    for approved in [false, true] {
        let mut fixture = Fixture::new(&db.pool).await?;
        if approved {
            approve_renewal(&db.pool, &mut fixture, Some("conflicting_vault")).await?;
        } else {
            let gateway = resolved_gateway(fixture.account)?;
            let due_at = make_trial_due(&db.pool, fixture.subscription_id).await?;
            let (payment, _) = decline_due_renewal(
                &db.pool,
                &gateway,
                &fixture.coordinator,
                fixture.scope(),
                fixture.subscription_id,
                due_at,
                "txn_declined",
            )
            .await?;
            fixture.attempt_id = payment.attempt().identity().attempt_id();
        }
        let financial = fixture.financial_snapshot(&db.pool).await?;
        let method = fixture.method_snapshot(&db.pool).await?;
        let provider = Arc::new(QueryGateway::new(Reply::observation(None)));
        let resolver = fixture.resolver(provider.clone())?;
        assert_eq!(
            refresh_payment_method_metadata(&db.pool, &resolver, fixture.command()).await?,
            Outcome::Ineligible
        );
        assert_eq!(provider.queries.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.financial_snapshot(&db.pool).await?, financial);
        assert_eq!(fixture.method_snapshot(&db.pool).await?, method);
    }
    db.cleanup().await?;
    Ok(())
}
