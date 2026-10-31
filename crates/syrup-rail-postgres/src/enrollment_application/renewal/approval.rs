use super::*;

pub(super) async fn apply_renewal_approved_outcome(
    coordinator: &dyn BillingTransactionCoordinator,
    reservation: &SubscriptionRenewalReservation,
    approved_evidence: &ApprovedProcessorEvidence,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    let identity = reservation.identity();
    let mut transaction = coordinator
        .begin(
            BillingEventSubject::new(identity.billing_scope_id(), identity.subscriber_id()),
            BILLING_LOCK_TIMEOUT,
        )
        .await?;
    let subject_state = transaction.subject_state();
    let expiry_policy = coordinator.subscription_period_expiry_policy();
    let application = apply_renewal_approved_on_connection(
        transaction.connection(),
        subject_state,
        reservation,
        approved_evidence,
        expiry_policy,
    )
    .await;
    finalize_approved_application(transaction, application).await
}

async fn apply_renewal_approved_on_connection(
    connection: &mut PgConnection,
    subject_state: BillingTransactionSubjectState,
    reservation: &SubscriptionRenewalReservation,
    approved_evidence: &ApprovedProcessorEvidence,
    expiry_policy: syrup_rail::SubscriptionPeriodExpiryPolicy,
) -> Result<
    (SubscriptionEnrollmentPaymentResult, Option<BillingEvent>),
    SubscriptionEnrollmentApplicationError,
> {
    let evidence = approved_evidence.evidence();
    set_application_timeouts(connection).await?;
    let identity = reservation.identity();
    lock_payment_method_domain(
        connection,
        identity.subscriber_id(),
        identity.gateway_account_id().as_uuid(),
    )
    .await?;
    lock_subscription_aggregate(connection, identity.subscriber_id(), reservation.plan_key())
        .await?;
    let attempt =
        lock_expected_reservation_attempt(connection, OutcomeReservation::Renewal(reservation))
            .await?;

    let (conflicting_payment, conflict_diagnostics) =
        stop_conflicting_subscription_approval(connection, &attempt, evidence).await?;
    if let Some(payment) = conflicting_payment {
        return Ok((payment, None));
    }
    if attempt.status() == PaymentAttemptStatus::Approved {
        let subscription = load_applied_subscription(connection, &attempt)
            .await?
            .ok_or(SubscriptionEnrollmentApplicationError::InvalidState(
                INVALID_APPLICATION_STATE,
            ))?;
        observe_processor_charge(
            connection,
            &attempt,
            evidence,
            ProcessorChargeProgression::Applied,
        )
        .await?;
        return Ok((
            SubscriptionEnrollmentPaymentResult::applied(attempt, subscription)?,
            None,
        ));
    }
    if is_parked_expired_period_approval(&attempt) {
        return observe_parked_expired_period_approval(connection, attempt, evidence).await;
    }
    if subject_state != BillingTransactionSubjectState::LiveRecipient {
        return Err(SubscriptionEnrollmentApplicationError::InvalidState(
            "a subscription renewal event requires a live recipient",
        ));
    }
    if attempt.status().is_terminal() {
        let observation = observe_processor_charge(
            connection,
            &attempt,
            evidence,
            ProcessorChargeProgression::ExternalReversalRequired,
        )
        .await?;
        if let ObservedCharge::Owned(charge) = &observation
            && charge_is_externally_reversed(connection, charge.id).await?
        {
            let payment = payment_result_for_attempt(connection, attempt)
                .await?
                .with_observation_diagnostics(conflict_diagnostics);
            return Ok((payment, None));
        }
        if let ObservedCharge::Owned(charge) = observation {
            transition_charge(
                connection,
                charge.id,
                ProcessorChargeProgression::ExternalReversalRequired,
                None,
            )
            .await?;
        }
        // The terminal attempt keeps its failure history; an approval for an
        // expired period still gives its primary charge the typed reason.
        park_observed_approval_if_period_expired(
            connection,
            &attempt,
            &observation,
            evidence,
            expiry_policy
                .rejects_expired_periods()
                .then_some(*reservation.period().end_at()),
        )
        .await?;
        let payment = SubscriptionEnrollmentPaymentResult::confirmation_pending(
            attempt,
            approved_evidence.clone(),
        )?
        .with_observation_diagnostics(conflict_diagnostics);
        return Ok((payment, None));
    }
    let observation = observe_processor_charge(
        connection,
        &attempt,
        evidence,
        ProcessorChargeProgression::Pending,
    )
    .await?;
    let ObservedCharge::Owned(charge) = observation else {
        let parked = park_locked_attempt(
            connection,
            &attempt,
            evidence,
            None,
            "The approved gateway transaction is already owned by another payment attempt.",
        )
        .await?;
        return Ok((
            SubscriptionEnrollmentPaymentResult::not_applied(parked)?,
            None,
        ));
    };
    if charge.role == ProcessorChargeRole::Additional {
        transition_charge(
            connection,
            charge.id,
            ProcessorChargeProgression::ExternalReversalRequired,
            None,
        )
        .await?;
        let parked = park_locked_attempt(
            connection,
            &attempt,
            evidence,
            None,
            "An additional approved charge requires manual reversal review.",
        )
        .await?;
        return Ok((
            SubscriptionEnrollmentPaymentResult::not_applied(parked)?,
            None,
        ));
    }
    if !renewal_subscription_matches(connection, reservation).await? {
        transition_charge(
            connection,
            charge.id,
            ProcessorChargeProgression::ExternalReversalRequired,
            Some(PaymentResolutionCode::SubscriptionApprovedRenewalStaleState),
        )
        .await?;
        let parked = park_locked_attempt(
            connection,
            &attempt,
            evidence,
            Some(PaymentResolutionCode::SubscriptionApprovedRenewalStaleState),
            RENEWAL_STALE_STATE_TEXT,
        )
        .await?;
        return Ok((
            SubscriptionEnrollmentPaymentResult::not_applied(parked)?,
            None,
        ));
    }
    if expiry_policy.rejects_expired_periods()
        && crate::period_expiry::period_has_expired_at_database_time(
            connection,
            *reservation.period().end_at(),
        )
        .await?
    {
        return park_expired_period_approval(connection, &attempt, charge.id, evidence).await;
    }
    let expected = reservation.expected_state();
    let updated = sqlx::query(
        r#"
        UPDATE billing_subscriptions
        SET status = 'active', phase = 'recurring', current_period_start_at = $2,
            current_period_end_at = $3, next_renewal_at = $3,
            next_payment_attempt_at = $3,
            updated_at = clock_timestamp()
        WHERE id = $1 AND billing_scope_id = $4 AND subscriber_id = $5
            AND gateway_account_id = $6 AND plan_key = $7
            AND status = $8 AND status IN ('active', 'past_due')
            AND payment_method_id = $9 AND initial_transaction_id = $10
            AND amount_cents = $11 AND currency = $12
            AND next_renewal_at = $2
        "#,
    )
    .bind(reservation.subscription_id().as_uuid())
    .bind(reservation.period().start_at())
    .bind(reservation.period().end_at())
    .bind(identity.billing_scope_id().as_uuid())
    .bind(identity.subscriber_id().as_uuid())
    .bind(identity.gateway_account_id().as_uuid())
    .bind(reservation.plan_key().as_str())
    .bind(expected.status().as_str())
    .bind(expected.payment_method_id().as_uuid())
    .bind(expected.initial_transaction_id().expose())
    .bind(reservation.request().amount().cents())
    .bind(reservation.request().amount().currency().as_str())
    .execute(&mut *connection)
    .await?;
    if updated.rows_affected() != 1 {
        return Err(SubscriptionEnrollmentApplicationError::InvalidState(
            INVALID_APPLICATION_STATE,
        ));
    }
    advance_subscription_discount_after_successful_charge(
        connection,
        reservation.subscription_id(),
        reservation.plan_key(),
    )
    .await?;
    mark_attempt_approved(
        connection,
        &attempt,
        evidence,
        reservation.subscription_id(),
        expected.payment_method_id(),
    )
    .await?;
    transition_charge(
        connection,
        charge.id,
        ProcessorChargeProgression::Applied,
        None,
    )
    .await?;
    let attempt = find_payment_attempt_by_id_on_connection(
        connection,
        identity.billing_scope_id(),
        identity.attempt_id(),
    )
    .await?
    .ok_or(SubscriptionEnrollmentApplicationError::InvalidState(
        INVALID_APPLICATION_STATE,
    ))?;
    let subscription = load_subscription(
        connection,
        identity.billing_scope_id(),
        reservation.subscription_id(),
    )
    .await?
    .ok_or(SubscriptionEnrollmentApplicationError::InvalidState(
        INVALID_APPLICATION_STATE,
    ))?;
    let event = BillingEvent::SubscriptionRenewed {
        attempt_id: identity.attempt_id(),
        subscription_id: reservation.subscription_id(),
        plan_key: reservation.plan_key().clone(),
        charge: syrup_rail::ChargeAmount::try_from(attempt.request().amount()).map_err(|_| {
            SubscriptionEnrollmentApplicationError::InvalidState(INVALID_APPLICATION_STATE)
        })?,
        period: reservation.period().clone(),
    };
    Ok((
        SubscriptionEnrollmentPaymentResult::applied(attempt, subscription)?,
        Some(event),
    ))
}
