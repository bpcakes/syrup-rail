use super::*;

#[tokio::test]
async fn expiry_reason_cannot_create_a_reconcilable_or_host_charge_state()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("rail_chg_reason").await?;
    let result = async {
        let gateway = create_gateway_account(&database.pool, "test_gateway").await?;
        let subscription = insert_subscription_attempt(
            &database.pool, gateway, PaymentAttemptKind::SubscriptionRecovery, "reason-subscription",
        ).await?;
        let host = insert_host_charge_attempt(&database.pool, gateway, "reason-host").await?;
        let expiry = PaymentResolutionCode::SubscriptionApprovedPeriodExpired;
        for (attempt_id, order) in [
            (subscription.identity.attempt_id(), subscription.gateway_order_id),
            (host, GatewayOrderId::from_correlation("reason-host")?),
        ] {
            let mut transaction = database.pool.begin().await?;
            let ProcessorChargeObservationOutcome::Observed(charge) = observe_processor_charge_in_transaction(
                &mut transaction, attempt_id, &order, &approved_evidence(order.expose()),
                ProcessorChargeProgression::Pending,
            ).await? else { panic!("new charge expected") };
            let before = charge_by_id(&mut transaction, charge.id().into_uuid()).await?;
            assert!(matches!(apply_processor_charge_transition_in_transaction(
                &mut transaction, charge.id(), ChargeTransition::RequireReconciliation(Some(expiry)),
            ).await, Err(ProcessorChargeStoreError::InvalidState(_))));
            for destination in [ProcessorChargeProgression::Pending, ProcessorChargeProgression::ReconciliationRequired, ProcessorChargeProgression::Applied] {
                assert!(matches!(transition_processor_charge_in_transaction(
                    &mut transaction, charge.id(), &[ProcessorChargeProgression::Pending],
                    destination, Some(ProcessorChargeStateCode::PaymentResolution(expiry)),
                ).await, Err(ProcessorChargeStoreError::InvalidState(_))));
            }
            assert_eq!(charge_by_id(&mut transaction, charge.id().into_uuid()).await?, before);
            if attempt_id == host {
                for command in [ChargeTransition::ParkExpiredPeriod, ChargeTransition::RequireExternalReversal(Some(expiry))] {
                    assert!(matches!(apply_processor_charge_transition_in_transaction(
                        &mut transaction, charge.id(), command,
                    ).await, Err(ProcessorChargeStoreError::InvalidState(_))));
                }
                assert!(matches!(transition_processor_charge_in_transaction(
                    &mut transaction, charge.id(), &[ProcessorChargeProgression::Pending],
                    ProcessorChargeProgression::ExternalReversalRequired,
                    Some(ProcessorChargeStateCode::PaymentResolution(expiry)),
                ).await, Err(ProcessorChargeStoreError::InvalidState(_))));
            } else {
                let parked = apply_processor_charge_transition_in_transaction(
                    &mut transaction, charge.id(), ChargeTransition::ParkExpiredPeriod,
                ).await?;
                assert_eq!(parked.progression(), ProcessorChargeProgression::ExternalReversalRequired);
                sqlx::query("UPDATE billing_payment_attempts SET status = 'approved', resolved_at = clock_timestamp(), gateway_transaction_id = $2 WHERE id = $1")
                    .bind(attempt_id.as_uuid()).bind(order.expose()).execute(&mut *transaction).await?;
                // Neither a named application nor replacing the old reason can
                // recover authority after permanent period-expiry parking.
                assert!(apply_processor_charge_transition_in_transaction(
                    &mut transaction, charge.id(), ChargeTransition::MarkApplied,
                ).await.is_err());
                assert!(transition_processor_charge_in_transaction(
                    &mut transaction, charge.id(), &[ProcessorChargeProgression::ExternalReversalRequired],
                    ProcessorChargeProgression::Applied, None,
                ).await.is_err());
                assert_eq!(charge_by_id(&mut transaction, charge.id().into_uuid()).await?, parked);
            }
            transaction.rollback().await?;
        }
        Ok::<_, Box<dyn Error>>(())
    }.await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn compatibility_application_revalidates_reversal_review_ownership()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("rail_chg_resume").await?;
    let result = async {
        let gateway = create_gateway_account(&database.pool, "test_gateway").await?;
        let attempt_id = insert_host_charge_attempt(&database.pool, gateway, "resume-compat").await?;
        let order = GatewayOrderId::from_correlation("resume-compat")?;
        let evidence = approved_evidence("resume-primary");
        let mut transaction = database.pool.begin().await?;
        let ProcessorChargeObservationOutcome::Observed(charge) = observe_processor_charge_in_transaction(
            &mut transaction, attempt_id, &order, &evidence, ProcessorChargeProgression::ExternalReversalRequired,
        ).await? else { panic!("new charge expected") };
        for (status, transaction_id) in [("pending", None), ("approved", Some("different"))] {
            sqlx::query("UPDATE billing_payment_attempts SET status = $2, resolved_at = CASE WHEN $2 = 'approved' THEN clock_timestamp() ELSE NULL END, gateway_transaction_id = $3 WHERE id = $1")
                .bind(attempt_id.as_uuid()).bind(status).bind(transaction_id).execute(&mut *transaction).await?;
            assert!(transition_processor_charge_in_transaction(
                &mut transaction, charge.id(), &[ProcessorChargeProgression::ExternalReversalRequired],
                ProcessorChargeProgression::Applied, None,
            ).await.is_err());
        }
        sqlx::query("UPDATE billing_payment_attempts SET gateway_transaction_id = 'resume-primary' WHERE id = $1")
            .bind(attempt_id.as_uuid()).execute(&mut *transaction).await?;
        let ProcessorChargeObservationOutcome::Observed(additional) = observe_processor_charge_in_transaction(
            &mut transaction, attempt_id, &order, &approved_evidence("resume-additional"), ProcessorChargeProgression::ExternalReversalRequired,
        ).await? else { panic!("additional charge expected") };
        assert!(apply_processor_charge_transition_in_transaction(
            &mut transaction, additional.id(), ChargeTransition::MarkApplied,
        ).await.is_err());
        let applied = transition_processor_charge_in_transaction(
            &mut transaction, charge.id(), &[ProcessorChargeProgression::ExternalReversalRequired],
            ProcessorChargeProgression::Applied, None,
        ).await?;
        assert_eq!(applied.progression(), ProcessorChargeProgression::Applied);
        transaction.rollback().await?;
        Ok::<_, Box<dyn Error>>(())
    }.await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}
