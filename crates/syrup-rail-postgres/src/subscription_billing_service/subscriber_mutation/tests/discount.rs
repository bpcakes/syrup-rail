use super::*;

#[tokio::test]
async fn discount_service_facade_admits_claim_and_clear_without_provider_or_host_events()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("srv_mut_discount").await?;
    let result = async {
        install_host_boundary(&database.pool).await?;
        let account = create_gateway_account(&database.pool, "test_gateway").await?;
        let scope = BillingScopeId::new(account.billing_scope_id);
        let subscriber = SubscriberId::new(Uuid::now_v7());
        let plan = PlanKey::new("discount_plan")?;
        let offers = Arc::new(CountingOfferStore::new(immediate_offer(
            plan.clone(),
            ChargeAmount::new(5_900, CurrencyCode::new("USD")?)?,
        )));
        let code = SubscriptionDiscountCode::new("SAVE10")?;
        create_subscription_discount_code(
            &database.pool,
            offers.as_ref(),
            &SubscriptionDiscountCodeCreation::new(
                DiscountCodeId::new(Uuid::now_v7()),
                scope,
                plan.clone(),
                code.clone(),
                None,
                SubscriptionDiscountKind::AmountOffCents(PositiveDiscountCents::new(500)?),
                CurrencyCode::new("USD")?,
                SubscriptionDiscountDuration::Indefinite,
            )?,
        )
        .await?;
        offers.calls.store(0, Ordering::SeqCst);
        let admission = Arc::new(RecordingAdmission::allowed());
        let resolver = Arc::new(CountingResolver {
            calls: AtomicUsize::new(0),
        });
        let coordinator = Arc::new(TestCoordinator::new(database.pool.clone(), false));
        let service = SubscriptionBillingService::new(
            database.pool.clone(),
            offers.clone(),
            resolver.clone(),
            admission.clone(),
            coordinator.clone(),
        );

        assert!(matches!(
            service
                .claim_discount(SubscriptionDiscountClaim::new(
                    DiscountClaimId::new(Uuid::now_v7()),
                    scope,
                    subscriber,
                    plan.clone(),
                    code.clone(),
                ))
                .await?,
            SubscriptionDiscountClaimOutcome::Saved(_)
        ));
        assert!(matches!(
            service
                .claim_discount(SubscriptionDiscountClaim::new(
                    DiscountClaimId::new(Uuid::now_v7()),
                    scope,
                    subscriber,
                    plan.clone(),
                    code.clone(),
                ))
                .await?,
            SubscriptionDiscountClaimOutcome::Existing(_)
        ));
        let offer_calls_before_clear = offers.calls.load(Ordering::SeqCst);
        assert!(matches!(
            service
                .clear_discount(ClearSubscriptionDiscount::new(
                    scope,
                    subscriber,
                    plan.clone(),
                ))
                .await?,
            SubscriptionDiscountClearOutcome::Cleared(_)
        ));
        assert_eq!(
            offers.calls.load(Ordering::SeqCst),
            offer_calls_before_clear,
            "clear must not lock an offer"
        );
        assert_eq!(
            service
                .clear_discount(ClearSubscriptionDiscount::new(
                    scope,
                    subscriber,
                    plan.clone(),
                ))
                .await?,
            SubscriptionDiscountClearOutcome::NotFound
        );

        assert!(matches!(
            service
                .claim_discount(SubscriptionDiscountClaim::new(
                    DiscountClaimId::new(Uuid::now_v7()),
                    scope,
                    subscriber,
                    plan.clone(),
                    code,
                ))
                .await?,
            SubscriptionDiscountClaimOutcome::Saved(_)
        ));
        insert_pending_initial_attempt(&database.pool, account, subscriber, &plan).await?;
        assert_eq!(
            service
                .clear_discount(ClearSubscriptionDiscount::new(
                    scope,
                    subscriber,
                    plan.clone(),
                ))
                .await?,
            SubscriptionDiscountClearOutcome::BlockedByInitialAttempt
        );
        assert!(
            saved_subscription_discount_claim(&database.pool, scope, subscriber, &plan)
                .await?
                .is_some()
        );
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 0);
        assert_eq!(coordinator.begins.load(Ordering::SeqCst), 0);
        assert_eq!(outbox_count(&database.pool).await?, 0);
        assert_eq!(
            admission
                .commands
                .lock()
                .await
                .iter()
                .map(EndUserMutationCommand::operation)
                .collect::<Vec<_>>(),
            vec![
                EndUserMutationOperation::SubscriptionDiscountClaim,
                EndUserMutationOperation::SubscriptionDiscountClaim,
                EndUserMutationOperation::SubscriptionDiscountClear,
                EndUserMutationOperation::SubscriptionDiscountClear,
                EndUserMutationOperation::SubscriptionDiscountClaim,
                EndUserMutationOperation::SubscriptionDiscountClear,
            ]
        );
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn denied_discount_mutations_leave_claims_untouched_without_host_or_provider_work()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("srv_mut_disc_no").await?;
    let result = async {
        let account = create_gateway_account(&database.pool, "test_gateway").await?;
        let scope = BillingScopeId::new(account.billing_scope_id);
        let subscriber = SubscriberId::new(Uuid::now_v7());
        let plan = PlanKey::new("denied_discount")?;
        let offers = Arc::new(CountingOfferStore::new(immediate_offer(
            plan.clone(),
            ChargeAmount::new(5_900, CurrencyCode::new("USD")?)?,
        )));
        let code = SubscriptionDiscountCode::new("DENY10")?;
        create_subscription_discount_code(
            &database.pool,
            offers.as_ref(),
            &SubscriptionDiscountCodeCreation::new(
                DiscountCodeId::new(Uuid::now_v7()),
                scope,
                plan.clone(),
                code.clone(),
                None,
                SubscriptionDiscountKind::AmountOffCents(PositiveDiscountCents::new(500)?),
                CurrencyCode::new("USD")?,
                SubscriptionDiscountDuration::Indefinite,
            )?,
        )
        .await?;
        let saved = crate::claim_subscription_discount(
            &database.pool,
            offers.as_ref(),
            &SubscriptionDiscountClaim::new(
                DiscountClaimId::new(Uuid::now_v7()),
                scope,
                subscriber,
                plan.clone(),
                code.clone(),
            ),
        )
        .await?;
        assert!(matches!(saved, SubscriptionDiscountClaimOutcome::Saved(_)));
        offers.calls.store(0, Ordering::SeqCst);
        let admission = Arc::new(RecordingAdmission::denied());
        let resolver = Arc::new(CountingResolver {
            calls: AtomicUsize::new(0),
        });
        let coordinator = Arc::new(TestCoordinator::new(database.pool.clone(), false));
        let service = SubscriptionBillingService::new(
            database.pool.clone(),
            offers.clone(),
            resolver.clone(),
            admission.clone(),
            coordinator.clone(),
        );

        assert!(matches!(
            service
                .claim_discount(SubscriptionDiscountClaim::new(
                    DiscountClaimId::new(Uuid::now_v7()),
                    scope,
                    subscriber,
                    plan.clone(),
                    code,
                ))
                .await,
            Err(SubscriptionBillingServiceError::AdmissionDenied { .. })
        ));
        assert!(matches!(
            service
                .clear_discount(ClearSubscriptionDiscount::new(
                    scope,
                    subscriber,
                    plan.clone()
                ))
                .await,
            Err(SubscriptionBillingServiceError::AdmissionDenied { .. })
        ));
        assert!(
            saved_subscription_discount_claim(&database.pool, scope, subscriber, &plan)
                .await?
                .is_some()
        );
        assert_eq!(offers.calls.load(Ordering::SeqCst), 0);
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 0);
        assert_eq!(coordinator.begins.load(Ordering::SeqCst), 0);
        assert_eq!(admission.commands.lock().await.len(), 2);
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}
