//! Retain late and conflicting approval evidence without applying host effects.
use super::*;

pub(super) async fn observe_conflicting_host_charge_approval(
    coordinator: &dyn BillingTransactionCoordinator,
    reservation: &HostChargeReservation,
    evidence: &ProcessorEvidence,
) -> Result<HostChargePaymentResult, HostChargeApplicationError> {
    let identity = reservation.identity();
    let mut transaction = coordinator
        .begin(
            BillingEventSubject::new(identity.billing_scope_id(), identity.subscriber_id()),
            BILLING_LOCK_TIMEOUT,
        )
        .await?;
    let connection = transaction.connection();
    set_application_timeouts(connection).await?;
    let attempt = lock_expected_host_charge(connection, reservation).await?;
    let diagnostics = processor_identity_conflict_diagnostics(&attempt, evidence);
    if diagnostics.is_empty() {
        transaction.rollback().await?;
        return Ok(HostChargePaymentResult::new(attempt)?);
    }
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
    let observation = observe_processor_charge(connection, &attempt, evidence, progression).await?;
    if progression == ApprovalObservation::ReviewReversal && evidence.transaction_id().is_some() {
        promote_conflicting_charge_to_external_reversal(connection, observation).await?;
    }
    transaction.commit().await?;
    Ok(append_host_observation_diagnostics(
        HostChargePaymentResult::new(attempt)?,
        &diagnostics,
    ))
}

pub(super) async fn observe_terminal_host_charge_approval(
    coordinator: &dyn BillingTransactionCoordinator,
    reservation: &HostChargeReservation,
    terminal_attempt: &PaymentAttempt,
    approved_evidence: &ApprovedProcessorEvidence,
) -> Result<HostChargePaymentResult, HostChargeApplicationError> {
    let evidence = approved_evidence.evidence();
    let identity = reservation.identity();
    let mut transaction = coordinator
        .begin(
            BillingEventSubject::new(identity.billing_scope_id(), identity.subscriber_id()),
            BILLING_LOCK_TIMEOUT,
        )
        .await?;
    let connection = transaction.connection();
    set_application_timeouts(connection).await?;
    if matches!(
        observe_processor_charge(
            connection,
            terminal_attempt,
            evidence,
            ApprovalObservation::AwaitApplication,
        )
        .await?,
        ObservedCharge::OwnedByOtherAttempt
    ) {
        let _ = transaction.rollback().await;
        return Err(HostChargeApplicationError::InvalidState(
            "the approved gateway transaction belongs to another payment attempt",
        ));
    }
    let locked = lock_expected_host_charge(connection, reservation).await?;
    if !locked.status().is_terminal() || locked.status() == PaymentAttemptStatus::Approved {
        let _ = transaction.rollback().await;
        return Err(HostChargeApplicationError::InvalidState(
            INVALID_HOST_CHARGE_STATE,
        ));
    }
    transaction.commit().await?;
    Ok(HostChargePaymentResult::confirmation_pending(
        locked,
        approved_evidence.clone(),
    )?)
}
