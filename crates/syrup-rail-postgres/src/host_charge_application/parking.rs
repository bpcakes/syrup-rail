use super::*;

pub(super) async fn park_host_charge_approved(
    pool: &PgPool,
    reservation: &HostChargeReservation,
    evidence: &ProcessorEvidence,
) -> Result<HostChargePaymentResult, HostChargeApplicationError> {
    let mut transaction = pool.begin().await?;
    set_application_timeouts(&mut transaction).await?;
    let attempt = lock_expected_host_charge(&mut transaction, reservation).await?;
    let diagnostics = processor_identity_conflict_diagnostics(&attempt, evidence);
    if !diagnostics.is_empty() {
        if attempt.status() == PaymentAttemptStatus::Approved
            && same_processor_transaction(&attempt, evidence)
        {
            transaction.commit().await?;
            return Ok(append_host_observation_diagnostics(
                HostChargePaymentResult::new(attempt)?,
                &diagnostics,
            ));
        }
        let progression = if attempt.status().is_resolvable() {
            ApprovalObservation::Reconcile
        } else {
            ApprovalObservation::ReviewReversal
        };
        let observation =
            observe_processor_charge(&mut transaction, &attempt, evidence, progression).await?;
        if progression == ApprovalObservation::ReviewReversal {
            promote_conflicting_charge_to_external_reversal(&mut transaction, observation).await?;
        } else if let ObservedCharge::Owned(charge) = observation {
            transition_charge(
                &mut transaction,
                charge.id,
                ChargeTransition::ReconcileConflictingApproval,
            )
            .await?;
        }
        transaction.commit().await?;
        return Ok(append_host_observation_diagnostics(
            HostChargePaymentResult::new(attempt)?,
            &diagnostics,
        ));
    }
    if attempt.status() == PaymentAttemptStatus::Approved {
        observe_processor_charge(
            &mut transaction,
            &attempt,
            evidence,
            ApprovalObservation::for_approved_attempt(&attempt, evidence),
        )
        .await?;
        transaction.commit().await?;
        return Ok(append_host_observation_diagnostics(
            HostChargePaymentResult::new(attempt)?,
            &diagnostics,
        ));
    }
    let progression = if evidence.transaction_id().is_some() {
        ApprovalObservation::ReviewReversal
    } else {
        ApprovalObservation::Reconcile
    };
    let observation =
        observe_processor_charge(&mut transaction, &attempt, evidence, progression).await?;
    if let ObservedCharge::Owned(charge) = observation {
        let transition = if evidence.transaction_id().is_some() {
            ChargeTransition::RequireExternalReversal(None)
        } else {
            ChargeTransition::RequireReconciliation(None)
        };
        transition_charge(&mut transaction, charge.id, transition).await?;
    }
    let parked = park_locked_attempt(
        &mut transaction,
        &attempt,
        evidence,
        None,
        APPROVED_STORAGE_FAILURE_TEXT,
    )
    .await?;
    transaction.commit().await?;
    Ok(HostChargePaymentResult::new(parked)?)
}

pub(super) async fn durably_park_host_charge_approved(
    pool: &PgPool,
    reservation: &HostChargeReservation,
    approved_evidence: &ApprovedProcessorEvidence,
) -> Result<HostChargePaymentResult, HostChargeApplicationError> {
    let evidence = approved_evidence.evidence();
    if let Ok(payment) = park_host_charge_approved(pool, reservation, evidence).await {
        return Ok(payment);
    }
    crate::store_compensating_processor_charge(
        pool,
        reservation.identity().attempt_id(),
        reservation.request().gateway_order_id(),
        evidence,
    )
    .await?;
    let mut transaction = pool.begin().await?;
    let attempt = crate::find_payment_attempt_by_id_in_transaction(
        &mut transaction,
        reservation.identity().billing_scope_id(),
        reservation.identity().attempt_id(),
    )
    .await?
    .ok_or(HostChargeApplicationError::InvalidState(
        INVALID_HOST_CHARGE_STATE,
    ))?;
    transaction.commit().await?;
    if attempt.status() == PaymentAttemptStatus::Approved {
        let diagnostics = processor_identity_conflict_diagnostics(&attempt, evidence);
        return Ok(append_host_observation_diagnostics(
            HostChargePaymentResult::new(attempt)?,
            &diagnostics,
        ));
    }
    Ok(HostChargePaymentResult::confirmation_pending(
        attempt,
        approved_evidence.clone(),
    )?)
}
