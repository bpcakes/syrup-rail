use super::*;

pub(super) async fn observe_recovery_approved_evidence_with_retry(
    pool: &PgPool,
    reservation: &SubscriptionRecoveryReservation,
    evidence: &ProcessorEvidence,
    expiry_policy: syrup_rail::SubscriptionPeriodExpiryPolicy,
) -> Result<(), SubscriptionEnrollmentApplicationError> {
    for attempt_index in 0..APPROVED_EVIDENCE_WRITE_ATTEMPTS {
        let result = async {
            let mut transaction = pool.begin().await?;
            set_application_timeouts(&mut transaction).await?;
            let attempt = lock_expected_reservation_attempt(
                &mut transaction,
                OutcomeReservation::Recovery(reservation),
            )
            .await?;
            let observation = observe_processor_charge(
                &mut transaction,
                &attempt,
                evidence,
                ProcessorChargeProgression::Pending,
            )
            .await?;
            park_observed_approval_if_period_expired(
                &mut transaction,
                &attempt,
                &observation,
                evidence,
                expiry_policy
                    .rejects_expired_periods()
                    .then_some(*reservation.period().end_at()),
            )
            .await?;
            transaction.commit().await?;
            Ok::<(), SubscriptionEnrollmentApplicationError>(())
        }
        .await;
        match result {
            Ok(()) => return Ok(()),
            Err(error)
                if is_retryable_evidence_error(&error)
                    && attempt_index + 1 < APPROVED_EVIDENCE_WRITE_ATTEMPTS =>
            {
                tokio::time::sleep(APPROVED_EVIDENCE_RETRY_DELAY).await;
            }
            Err(error) if is_retryable_evidence_error(&error) => break,
            Err(error) => return Err(error),
        }
    }
    let expired_period = expiry_policy.rejects_expired_periods()
        && crate::period_expiry::period_has_expired_on_pool(pool, *reservation.period().end_at())
            .await?;
    persist_approved_evidence_without_attempt_lock(
        pool,
        LockFreeApprovedEvidenceTerms::recovery(reservation),
        evidence,
        if expired_period {
            crate::processor_charges::LockFreeApprovalPolicy::ReverseExpiredPeriod
        } else {
            crate::processor_charges::LockFreeApprovalPolicy::Reconcile
        },
    )
    .await
}
