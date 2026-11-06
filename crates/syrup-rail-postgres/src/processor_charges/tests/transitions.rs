use super::*;

#[tokio::test]
async fn charge_application_requires_owning_approval_and_cannot_be_reopened()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("rail_chg_edges").await?;
    let result = async {
        let gateway = create_gateway_account(&database.pool, "test_gateway").await?;
        let attempt_id = insert_host_charge_attempt(&database.pool, gateway, "edge-order").await?;
        let order = GatewayOrderId::from_correlation("edge-order")?;
        let evidence = approved_evidence("edge-transaction");
        let mut transaction = database.pool.begin().await?;
        for forbidden in [ProcessorChargeProgression::Applied, ProcessorChargeProgression::ExternallyReversed] {
            assert!(observe_processor_charge_in_transaction(
                &mut transaction, attempt_id, &order, &evidence, forbidden,
            ).await.is_err());
        }
        let ProcessorChargeObservationOutcome::Observed(charge) = observe_processor_charge_in_transaction(
            &mut transaction, attempt_id, &order, &evidence, ProcessorChargeProgression::Pending,
        ).await? else { panic!("new charge expected") };
        assert_eq!(charge_by_id(&mut transaction, charge.id().into_uuid()).await?.progression(), ProcessorChargeProgression::Pending);
        let parked = apply_processor_charge_transition_in_transaction(
            &mut transaction, charge.id(), ProcessorChargeTransition::RequireExternalReversal(Some(ChargeReversalReason::HostStaleState)),
        ).await?;
        assert_eq!(parked.state_code(), Some(ProcessorChargeStateCode::PaymentResolution(PaymentResolutionCode::HostChargeApprovedStaleState)));
        assert!(apply_processor_charge_transition_in_transaction(
            &mut transaction, charge.id(), ProcessorChargeTransition::MarkApplied,
        ).await.is_err());
        assert!(complete_attested_reversal(&mut transaction, charge.id()).await.is_err());
        sqlx::query("UPDATE billing_payment_attempts SET status = 'approved', resolved_at = clock_timestamp(), gateway_transaction_id = 'different-transaction' WHERE id = $1")
            .bind(attempt_id.as_uuid()).execute(&mut *transaction).await?;
        assert!(apply_processor_charge_transition_in_transaction(
            &mut transaction, charge.id(), ProcessorChargeTransition::MarkApplied,
        ).await.is_err());
        sqlx::query("UPDATE billing_payment_attempts SET gateway_transaction_id = 'edge-transaction' WHERE id = $1")
            .bind(attempt_id.as_uuid()).execute(&mut *transaction).await?;
        let applied = apply_processor_charge_transition_in_transaction(
            &mut transaction, charge.id(), ProcessorChargeTransition::MarkApplied,
        ).await?;
        assert_eq!(applied.progression(), ProcessorChargeProgression::Applied);
        assert_eq!(applied.state_code(), parked.state_code());
        for transition in [
            ProcessorChargeTransition::RequireReconciliation(None),
            ProcessorChargeTransition::ReconcileConflictingApproval,
            ProcessorChargeTransition::RequireExternalReversal(None),
        ] {
            assert!(apply_processor_charge_transition_in_transaction(
                &mut transaction, charge.id(), transition,
            ).await.is_err());
        }
        transaction.rollback().await?;
        Ok::<_, Box<dyn Error>>(())
    }.await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn expired_approval_cannot_lose_its_reversal_disposition() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("rail_chg_expiry").await?;
    let result = async {
        let gateway = create_gateway_account(&database.pool, "test_gateway").await?;
        let fixture = insert_subscription_attempt(
            &database.pool,
            gateway,
            PaymentAttemptKind::SubscriptionRecovery,
            "expiry-order",
        )
        .await?;
        let attempt_id = fixture.identity.attempt_id();
        sqlx::query("UPDATE billing_payment_attempts SET billing_period_start_at = clock_timestamp() - interval '40 days', billing_period_end_at = clock_timestamp() - interval '10 days' WHERE id = $1")
            .bind(attempt_id.as_uuid()).execute(&database.pool).await?;
        let mut transaction = database.pool.begin().await?;
        let ProcessorChargeObservationOutcome::Observed(charge) =
            observe_processor_charge_in_transaction(
                &mut transaction,
                attempt_id,
                &GatewayOrderId::from_correlation("expiry-order")?,
                &approved_evidence("expiry-transaction"),
                ProcessorChargeProgression::ExternalReversalRequired,
            )
            .await?
        else {
            panic!("new charge expected")
        };
        // A conflicting approval can be reconciled while the attempt is unresolved.
        apply_charge_transition(
            &mut transaction,
            charge.id(),
            ChargeTransition::ReconcileConflictingApproval,
        )
        .await?;
        apply_charge_transition(
            &mut transaction,
            charge.id(),
            ChargeTransition::ParkExpiredPeriod,
        )
        .await?;
        assert!(
            apply_charge_transition(
                &mut transaction,
                charge.id(),
                ChargeTransition::ReconcileConflictingApproval,
            )
            .await
            .is_err()
        );
        assert!(
            complete_attested_reversal(&mut transaction, charge.id())
                .await
                .is_err()
        );
        let retained = charge_by_id(&mut transaction, charge.id().into_uuid()).await?;
        assert_eq!(
            retained.progression(),
            ProcessorChargeProgression::ExternalReversalRequired
        );
        assert_eq!(
            retained.state_code(),
            Some(ProcessorChargeStateCode::PaymentResolution(
                PaymentResolutionCode::SubscriptionApprovedPeriodExpired
            ))
        );
        sqlx::query("UPDATE billing_payment_attempts SET status = 'approved', resolved_at = clock_timestamp(), gateway_transaction_id = 'expiry-transaction' WHERE id = $1")
            .bind(attempt_id.as_uuid()).execute(&mut *transaction).await?;
        assert!(apply_charge_transition(
            &mut transaction, charge.id(), ChargeTransition::MarkApplied,
        ).await.is_err());
        transaction.rollback().await?;
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}
