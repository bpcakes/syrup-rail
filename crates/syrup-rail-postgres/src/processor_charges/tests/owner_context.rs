use super::*;

#[tokio::test]
async fn host_charge_rejects_subscription_reasons_through_both_apis() -> Result<(), Box<dyn Error>>
{
    let database = TestDatabase::start("chg_owner_kind").await?;
    let result = async {
        let gateway = create_gateway_account(&database.pool, "test_gateway").await?;
        let attempt = insert_host_charge_attempt(&database.pool, gateway, "owner-order").await?;
        let mut transaction = database.pool.begin().await?;
        let ProcessorChargeObservationOutcome::Observed(charge) =
            observe_processor_charge_in_transaction(
                &mut transaction,
                attempt,
                &GatewayOrderId::from_correlation("owner-order")?,
                &approved_evidence("owner-transaction"),
                ProcessorChargeProgression::Pending,
            )
            .await?
        else {
            panic!("new charge expected")
        };
        for reason in [
            ChargeReversalReason::InitialSubscriptionConflict,
            ChargeReversalReason::InitialGrantConflict,
            ChargeReversalReason::RenewalStaleState,
            ChargeReversalReason::RecoveryStaleState,
        ] {
            assert_rejected_by_both_apis(
                &mut transaction,
                charge.id(),
                ChargeTransition::RequireExternalReversal(Some(reason)),
                ProcessorChargeProgression::ExternalReversalRequired,
                reason.code(),
            )
            .await?;
        }
        for reason in [
            ChargeReconciliationReason::PaymentMethodSubscriptionIneligible,
            ChargeReconciliationReason::PaymentMethodStaleState,
        ] {
            assert_rejected_by_both_apis(
                &mut transaction,
                charge.id(),
                ChargeTransition::RequireReconciliation(Some(reason)),
                ProcessorChargeProgression::ReconciliationRequired,
                reason.code(),
            )
            .await?;
        }
        assert_rejected_by_both_apis(
            &mut transaction,
            charge.id(),
            ChargeTransition::ParkExpiredPeriod,
            ProcessorChargeProgression::ExternalReversalRequired,
            PaymentResolutionCode::SubscriptionApprovedPeriodExpired,
        )
        .await?;
        let retained = charge_by_id(&mut transaction, charge.id().into_uuid()).await?;
        assert_eq!(retained.progression(), ProcessorChargeProgression::Pending);
        assert_eq!(retained.state_code(), None);
        transaction.rollback().await?;
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn expiry_rejects_additional_and_approved_subscription_charges() -> Result<(), Box<dyn Error>>
{
    let database = TestDatabase::start("chg_exp_owner").await?;
    let result = async {
        let gateway = create_gateway_account(&database.pool, "test_gateway").await?;
        let fixture = insert_subscription_attempt(
            &database.pool, gateway, PaymentAttemptKind::SubscriptionRecovery, "expiry-owner-order",
        ).await?;
        let attempt = fixture.identity.attempt_id();
        let mut transaction = database.pool.begin().await?;
        let mut charges = Vec::new();
        for id in ["expiry-primary", "expiry-additional"] {
            let ProcessorChargeObservationOutcome::Observed(charge) = observe_processor_charge_in_transaction(
                &mut transaction, attempt, &fixture.gateway_order_id, &approved_evidence(id),
                ProcessorChargeProgression::Pending,
            ).await? else { panic!("new charge expected") };
            charges.push(charge);
        }
        assert_rejected_by_both_apis(
            &mut transaction, charges[1].id(), ChargeTransition::ParkExpiredPeriod,
            ProcessorChargeProgression::ExternalReversalRequired,
            PaymentResolutionCode::SubscriptionApprovedPeriodExpired,
        ).await?;
        assert_rejected_by_both_apis(
            &mut transaction, charges[0].id(), ChargeTransition::RequireExternalReversal(Some(ChargeReversalReason::HostStaleState)),
            ProcessorChargeProgression::ExternalReversalRequired,
            PaymentResolutionCode::HostChargeApprovedStaleState,
        ).await?;
        sqlx::query("UPDATE billing_payment_attempts SET status = 'approved', resolved_at = clock_timestamp(), gateway_transaction_id = 'expiry-primary' WHERE id = $1")
            .bind(attempt.as_uuid()).execute(&mut *transaction).await?;
        assert_rejected_by_both_apis(
            &mut transaction, charges[0].id(), ChargeTransition::ParkExpiredPeriod,
            ProcessorChargeProgression::ExternalReversalRequired,
            PaymentResolutionCode::SubscriptionApprovedPeriodExpired,
        ).await?;
        let applied = apply_processor_charge_transition_in_transaction(
            &mut transaction, charges[0].id(), ChargeTransition::MarkApplied,
        ).await?;
        assert_eq!(applied.progression(), ProcessorChargeProgression::Applied);
        let reversal = apply_processor_charge_transition_in_transaction(
            &mut transaction, charges[1].id(), ChargeTransition::RequireExternalReversal(None),
        ).await?;
        assert_eq!(reversal.progression(), ProcessorChargeProgression::ExternalReversalRequired);
        transaction.rollback().await?;
        Ok::<_, Box<dyn Error>>(())
    }.await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

async fn assert_rejected_by_both_apis(
    connection: &mut PgConnection,
    charge: ProcessorChargeId,
    transition: ChargeTransition,
    destination: ProcessorChargeProgression,
    code: PaymentResolutionCode,
) -> Result<(), Box<dyn Error>> {
    let current = charge_by_id(connection, charge.into_uuid()).await?;
    assert!(matches!(
        apply_processor_charge_transition_in_transaction(connection, charge, transition).await,
        Err(ProcessorChargeStoreError::InvalidState(_))
    ));
    assert!(matches!(
        transition_processor_charge_in_transaction(
            connection,
            charge,
            &[current.progression()],
            destination,
            Some(ProcessorChargeStateCode::PaymentResolution(code)),
        )
        .await,
        Err(ProcessorChargeStoreError::InvalidState(_))
    ));
    assert_eq!(charge_by_id(connection, charge.into_uuid()).await?, current);
    Ok(())
}
