use super::*;
use support::{
    active_grant_exists, attempt_identity_matches_requested_gateway,
    blocking_initial_attempt_exists, current_subscription_exists,
    enrollment_request_from_locked_terms, gateway_identity_matches_scope, initial_attempt_is_stale,
    insert_initial_attempt, map_discount_error, pending_attempt_matches_request,
    reject_locked_prepared_initial, reject_prepared_initial, replay_matches_command,
    replay_matches_reservation, unresolved_initial_charge_exists,
};

mod support;

/// Reserves or resumes one exact-plan initial enrollment inside the caller's transaction.
///
/// The payment token is deliberately absent from [`SubscriptionEnrollmentReservation`].
/// The transaction locks the plan aggregate, authoritative host offer, saved claim, attempt
/// rows, unresolved charge evidence, and exact gateway identity before inserting a token-free
/// pending attempt.
pub async fn reserve_subscription_enrollment_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    offers: &dyn crate::SubscriptionOfferStore,
    reservation: &SubscriptionEnrollmentReservation,
) -> Result<SubscriptionEnrollmentReservationOutcome, PaymentAttemptStoreError> {
    set_enrollment_timeouts(transaction).await?;
    let identity = reservation.identity();
    lock_subscription_aggregate(
        transaction,
        identity.subscriber_id(),
        reservation.plan_key(),
    )
    .await?;

    if let Some(existing) = payment_attempt_by_idempotency(
        transaction,
        identity.billing_scope_id(),
        identity.subscriber_id(),
        reservation.idempotency_key(),
        false,
    )
    .await?
    {
        if !replay_matches_reservation(&existing, reservation) {
            return Ok(SubscriptionEnrollmentReservationOutcome::IdempotencyConflict);
        }
        if initial_attempt_is_stale(transaction, existing.identity().attempt_id()).await? {
            lock_initial_attempt_rows(
                transaction,
                identity.billing_scope_id(),
                identity.subscriber_id(),
                reservation.plan_key(),
            )
            .await?;
            lock_initial_charge_rows(
                transaction,
                identity.billing_scope_id(),
                identity.subscriber_id(),
                reservation.plan_key(),
            )
            .await?;
            expire_stale_initial_attempts(
                transaction,
                identity.billing_scope_id(),
                identity.subscriber_id(),
                reservation.plan_key(),
            )
            .await?;
            let expired = payment_attempt_by_idempotency(
                transaction,
                identity.billing_scope_id(),
                identity.subscriber_id(),
                reservation.idempotency_key(),
                true,
            )
            .await?
            .ok_or_else(invalid_state)?;
            return Ok(SubscriptionEnrollmentReservationOutcome::Replay(expired));
        }
        if attempt_replay_phase(&existing) == AttemptReplayPhase::ReturnCanonical {
            return Ok(SubscriptionEnrollmentReservationOutcome::Replay(existing));
        }
    }

    let Some(offer) = offers
        .lock_enrollment_offer(
            transaction,
            crate::SubscriptionEnrollmentOfferContext::from_reservation(
                reservation,
                crate::SubscriptionEnrollmentOfferStage::Reservation,
            ),
        )
        .await?
    else {
        return Ok(SubscriptionEnrollmentReservationOutcome::Rejected(
            SubscriptionEnrollmentReservationRejection::EnrollmentTermsChanged,
        ));
    };
    if offer.plan_key() != reservation.plan_key() {
        return Ok(SubscriptionEnrollmentReservationOutcome::Rejected(
            SubscriptionEnrollmentReservationRejection::EnrollmentTermsChanged,
        ));
    }
    let saved_claim = crate::saved_subscription_discount_claim_in_transaction(
        transaction,
        identity.billing_scope_id(),
        identity.subscriber_id(),
        reservation.plan_key(),
    )
    .await
    .map_err(map_discount_error)?;

    lock_initial_attempt_rows(
        transaction,
        identity.billing_scope_id(),
        identity.subscriber_id(),
        reservation.plan_key(),
    )
    .await?;
    lock_initial_charge_rows(
        transaction,
        identity.billing_scope_id(),
        identity.subscriber_id(),
        reservation.plan_key(),
    )
    .await?;
    expire_stale_initial_attempts(
        transaction,
        identity.billing_scope_id(),
        identity.subscriber_id(),
        reservation.plan_key(),
    )
    .await?;

    if let Some(existing) = payment_attempt_by_idempotency(
        transaction,
        identity.billing_scope_id(),
        identity.subscriber_id(),
        reservation.idempotency_key(),
        true,
    )
    .await?
        && attempt_replay_phase(&existing) == AttemptReplayPhase::ReturnCanonical
    {
        return Ok(if replay_matches_reservation(&existing, reservation) {
            SubscriptionEnrollmentReservationOutcome::Replay(existing)
        } else {
            SubscriptionEnrollmentReservationOutcome::IdempotencyConflict
        });
    }

    if current_subscription_exists(
        transaction,
        identity.billing_scope_id(),
        identity.subscriber_id(),
        reservation.plan_key(),
    )
    .await?
    {
        return Ok(SubscriptionEnrollmentReservationOutcome::Rejected(
            SubscriptionEnrollmentReservationRejection::CurrentSubscription,
        ));
    }
    if active_grant_exists(
        transaction,
        identity.billing_scope_id(),
        identity.subscriber_id(),
        reservation.plan_key(),
    )
    .await?
    {
        return Ok(SubscriptionEnrollmentReservationOutcome::Rejected(
            SubscriptionEnrollmentReservationRejection::ActiveGrant,
        ));
    }
    if unresolved_initial_charge_exists(
        transaction,
        identity.billing_scope_id(),
        identity.subscriber_id(),
        reservation.plan_key(),
    )
    .await?
    {
        return Ok(SubscriptionEnrollmentReservationOutcome::Rejected(
            SubscriptionEnrollmentReservationRejection::UnresolvedProcessorCharge,
        ));
    }
    let Some(request) =
        enrollment_request_from_locked_terms(reservation, &offer, saved_claim.as_ref())
    else {
        return Ok(SubscriptionEnrollmentReservationOutcome::Rejected(
            SubscriptionEnrollmentReservationRejection::EnrollmentTermsChanged,
        ));
    };
    let expected_gateway =
        ExpectedGatewayIdentity::from_reservation(identity, reservation.provider_key());
    if !gateway_identity_matches_scope(transaction, &expected_gateway).await? {
        return Ok(SubscriptionEnrollmentReservationOutcome::Rejected(
            SubscriptionEnrollmentReservationRejection::GatewayConfigurationChanged,
        ));
    }

    if let Some(existing) = payment_attempt_by_idempotency(
        transaction,
        identity.billing_scope_id(),
        identity.subscriber_id(),
        reservation.idempotency_key(),
        true,
    )
    .await?
    {
        return Ok(
            if pending_attempt_matches_request(&existing, identity, &request) {
                SubscriptionEnrollmentReservationOutcome::Replay(existing)
            } else {
                SubscriptionEnrollmentReservationOutcome::IdempotencyConflict
            },
        );
    }
    if blocking_initial_attempt_exists(
        transaction,
        identity.billing_scope_id(),
        identity.subscriber_id(),
        reservation.plan_key(),
    )
    .await?
    {
        return Ok(SubscriptionEnrollmentReservationOutcome::Rejected(
            SubscriptionEnrollmentReservationRejection::AttemptInProgress,
        ));
    }

    let inserted = insert_initial_attempt(transaction, identity, &request).await?;
    if inserted {
        let attempt = payment_attempt_by_idempotency(
            transaction,
            identity.billing_scope_id(),
            identity.subscriber_id(),
            reservation.idempotency_key(),
            true,
        )
        .await?
        .ok_or_else(invalid_state)?;
        return Ok(SubscriptionEnrollmentReservationOutcome::Reserved(attempt));
    }
    let existing = payment_attempt_by_idempotency(
        transaction,
        identity.billing_scope_id(),
        identity.subscriber_id(),
        reservation.idempotency_key(),
        true,
    )
    .await?
    .ok_or_else(invalid_state)?;
    Ok(
        if pending_attempt_matches_request(&existing, identity, &request) {
            SubscriptionEnrollmentReservationOutcome::Replay(existing)
        } else {
            SubscriptionEnrollmentReservationOutcome::IdempotencyConflict
        },
    )
}

/// Resolves subscriber-wide enrollment idempotency before host admission.
///
/// Matching stale prepared work is expired at the exact database-clock
/// boundary and returned as a replay. This operation deliberately does not
/// lock an offer or inspect current subscription state: replay semantics are
/// determined by the historical request before any live policy or quota.
pub async fn preflight_subscription_enrollment_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    command: &syrup_rail::EnrollSubscription,
) -> Result<SubscriptionEnrollmentPreflightOutcome, PaymentAttemptStoreError> {
    set_enrollment_timeouts(transaction).await?;
    let Some(existing) = payment_attempt_by_idempotency(
        transaction,
        command.billing_scope_id(),
        command.subscriber_id(),
        command.idempotency_key(),
        false,
    )
    .await?
    else {
        return Ok(SubscriptionEnrollmentPreflightOutcome::Continue);
    };
    if !replay_matches_command(&existing, command) {
        return Ok(SubscriptionEnrollmentPreflightOutcome::IdempotencyConflict);
    }
    if attempt_replay_phase(&existing) == AttemptReplayPhase::ReturnCanonical
        && !(existing.status() == PaymentAttemptStatus::ReviewRequired
            && existing.state().timestamps().submitted_at().is_none())
    {
        return Ok(SubscriptionEnrollmentPreflightOutcome::Replay(Box::new(
            existing,
        )));
    }

    lock_subscription_aggregate(transaction, command.subscriber_id(), command.plan_key()).await?;
    let existing = payment_attempt_by_idempotency(
        transaction,
        command.billing_scope_id(),
        command.subscriber_id(),
        command.idempotency_key(),
        true,
    )
    .await?
    .ok_or_else(invalid_state)?;
    if !replay_matches_command(&existing, command) {
        return Ok(SubscriptionEnrollmentPreflightOutcome::IdempotencyConflict);
    }
    if !initial_attempt_is_stale(transaction, existing.identity().attempt_id()).await? {
        if attempt_replay_phase(&existing) == AttemptReplayPhase::ReturnCanonical {
            return Ok(SubscriptionEnrollmentPreflightOutcome::Replay(Box::new(
                existing,
            )));
        }
        return Ok(SubscriptionEnrollmentPreflightOutcome::Continue);
    }

    lock_initial_attempt_rows(
        transaction,
        command.billing_scope_id(),
        command.subscriber_id(),
        command.plan_key(),
    )
    .await?;
    lock_initial_charge_rows(
        transaction,
        command.billing_scope_id(),
        command.subscriber_id(),
        command.plan_key(),
    )
    .await?;
    expire_stale_initial_attempts(
        transaction,
        command.billing_scope_id(),
        command.subscriber_id(),
        command.plan_key(),
    )
    .await?;
    let expired = payment_attempt_by_idempotency(
        transaction,
        command.billing_scope_id(),
        command.subscriber_id(),
        command.idempotency_key(),
        true,
    )
    .await?
    .ok_or_else(invalid_state)?;
    Ok(SubscriptionEnrollmentPreflightOutcome::Replay(Box::new(
        expired,
    )))
}

/// Revalidates a prepared initial attempt and durably admits its one provider mutation.
///
/// No provider I/O may occur after this function returns `Admitted` and before the caller
/// invokes the sale. Every semantic rejection terminally fails the still-unsubmitted attempt.
pub async fn admit_subscription_enrollment_submission_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    offers: &dyn crate::SubscriptionOfferStore,
    reservation: &SubscriptionEnrollmentReservation,
) -> Result<SubscriptionEnrollmentSubmissionOutcome, PaymentAttemptStoreError> {
    set_enrollment_timeouts(transaction).await?;
    let identity = reservation.identity();
    lock_subscription_aggregate(
        transaction,
        identity.subscriber_id(),
        reservation.plan_key(),
    )
    .await?;

    let Some(offer) = offers
        .lock_enrollment_offer(
            transaction,
            crate::SubscriptionEnrollmentOfferContext::from_reservation(
                reservation,
                crate::SubscriptionEnrollmentOfferStage::SubmissionAdmission,
            ),
        )
        .await?
    else {
        return reject_prepared_initial(
            transaction,
            reservation,
            SubscriptionEnrollmentSubmissionRejection::EnrollmentTermsChanged,
            INITIAL_TERMS_CHANGED_TEXT,
        )
        .await;
    };
    let saved_claim = crate::saved_subscription_discount_claim_in_transaction(
        transaction,
        identity.billing_scope_id(),
        identity.subscriber_id(),
        reservation.plan_key(),
    )
    .await
    .map_err(map_discount_error)?;
    let attempt = payment_attempt_by_idempotency(
        transaction,
        identity.billing_scope_id(),
        identity.subscriber_id(),
        reservation.idempotency_key(),
        true,
    )
    .await?
    .ok_or_else(invalid_state)?;
    if !attempt_identity_matches_requested_gateway(&attempt, identity)
        || attempt.kind() != PaymentAttemptKind::SubscriptionInitial
        || attempt.request().target().plan_key() != Some(reservation.plan_key())
    {
        return Err(invalid_state());
    }
    if attempt.state().timestamps().submitted_at().is_some()
        || attempt.status() != PaymentAttemptStatus::Pending
    {
        return Ok(SubscriptionEnrollmentSubmissionOutcome::AlreadyAdmitted(
            attempt,
        ));
    }

    lock_initial_charge_rows(
        transaction,
        identity.billing_scope_id(),
        identity.subscriber_id(),
        reservation.plan_key(),
    )
    .await?;
    if current_subscription_exists(
        transaction,
        identity.billing_scope_id(),
        identity.subscriber_id(),
        reservation.plan_key(),
    )
    .await?
        || active_grant_exists(
            transaction,
            identity.billing_scope_id(),
            identity.subscriber_id(),
            reservation.plan_key(),
        )
        .await?
        || unresolved_initial_charge_exists(
            transaction,
            identity.billing_scope_id(),
            identity.subscriber_id(),
            reservation.plan_key(),
        )
        .await?
    {
        return reject_locked_prepared_initial(
            transaction,
            attempt,
            SubscriptionEnrollmentSubmissionRejection::BillingStateChanged,
            INITIAL_BILLING_STATE_CHANGED_TEXT,
        )
        .await;
    }
    let Some(expected_request) =
        enrollment_request_from_locked_terms(reservation, &offer, saved_claim.as_ref())
    else {
        return reject_locked_prepared_initial(
            transaction,
            attempt,
            SubscriptionEnrollmentSubmissionRejection::EnrollmentTermsChanged,
            INITIAL_TERMS_CHANGED_TEXT,
        )
        .await;
    };
    if !pending_attempt_matches_request(&attempt, identity, &expected_request) {
        return reject_locked_prepared_initial(
            transaction,
            attempt,
            SubscriptionEnrollmentSubmissionRejection::EnrollmentTermsChanged,
            INITIAL_TERMS_CHANGED_TEXT,
        )
        .await;
    }
    let expected_gateway =
        ExpectedGatewayIdentity::from_reservation(identity, reservation.provider_key());
    if !gateway_identity_matches_scope(transaction, &expected_gateway).await? {
        return reject_locked_prepared_initial(
            transaction,
            attempt,
            SubscriptionEnrollmentSubmissionRejection::GatewayConfigurationChanged,
            INITIAL_CONFIGURATION_CHANGED_TEXT,
        )
        .await;
    }

    let admitted = admit_prepared_attempt(transaction, &attempt).await?;
    Ok(SubscriptionEnrollmentSubmissionOutcome::Admitted(admitted))
}
