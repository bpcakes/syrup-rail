use super::*;
use crate::test_support::assert_attempt_billing_address;

#[tokio::test]
async fn recovery_persists_billing_address_and_rejects_changed_address_replay()
-> Result<(), Box<dyn Error>> {
    let fixture = enrollment_fixture("address_recovery", false, false, false).await?;
    let result = async {
        let (subscription_id, _) = enroll_with(
            &fixture,
            "address-recovery-initial",
            named_contact(None),
            "txn_address_initial",
            "vault_address_initial",
        )
        .await?;
        make_renewal_due(&fixture.database.pool, subscription_id).await?;
        sqlx::query("UPDATE billing_subscriptions SET status = 'past_due' WHERE id = $1")
            .bind(subscription_id.as_uuid())
            .execute(&fixture.database.pool)
            .await?;

        let home = address("10 Recovery St");
        let command = |address| {
            RecoverSubscriptionPayment::new(
                context(&fixture, "address-recovery", named_contact(Some(address))),
                fixture.command.plan_key().clone(),
            )
        };
        let original = command(home.clone());
        let gateway = Arc::new(ScriptedGateway::new(Ok(approved_outcome_with_reference(
            Some("txn_address_recovery"),
            "vault_address_recovery",
        ))));
        let resolved = scripted_resolved_gateway(fixture.gateway_account, Arc::clone(&gateway));
        let service = service(&fixture, &gateway);
        let mut transaction = fixture.database.pool.begin().await?;
        let reserved = match reserve_subscription_recovery_in_transaction(
            &mut transaction,
            &original,
            &resolved,
            GatewayAccountMode::Live,
        )
        .await?
        {
            SubscriptionRecoveryReservationOutcome::Reserved(_, attempt) => *attempt,
            other => panic!("expected addressed recovery reservation, got {other:?}"),
        };
        transaction.commit().await?;
        let loaded = assert_attempt_billing_address(
            &fixture.database.pool,
            original.billing_scope_id(),
            original.attempt_id(),
            &home,
        )
        .await?;
        assert_eq!(loaded, reserved);
        assert!(loaded.state().timestamps().submitted_at().is_none());

        let same = command(home.clone());
        assert_ne!(same.attempt_id(), original.attempt_id());
        let mut transaction = fixture.database.pool.begin().await?;
        let replay = reserve_subscription_recovery_in_transaction(
            &mut transaction,
            &same,
            &resolved,
            GatewayAccountMode::Live,
        )
        .await?;
        transaction.commit().await?;
        assert!(
            matches!(replay, SubscriptionRecoveryReservationOutcome::Replay(attempt)
            if *attempt == loaded)
        );

        let changed = command(home.clone().with_city(Some("Cambridge".to_owned()))?);
        assert!(matches!(
            service.recover(changed).await,
            Err(SubscriptionBillingServiceError::IdempotencyConflict)
        ));
        assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 0);
        assert_eq!(gateway.account_mode_calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            assert_attempt_billing_address(
                &fixture.database.pool,
                original.billing_scope_id(),
                original.attempt_id(),
                &home,
            )
            .await?,
            loaded
        );

        let applied = service.recover(same.clone()).await?;
        assert_eq!(
            applied.attempt().identity().attempt_id(),
            original.attempt_id()
        );
        assert_eq!(applied.attempt().status(), PaymentAttemptStatus::Approved);
        assert_eq!(
            applied.attempt().request().billing_contact().address(),
            Some(&home)
        );
        assert_eq!(service.recover(same).await?.attempt(), applied.attempt());
        assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 1);
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    fixture.cleanup().await?;
    result
}
