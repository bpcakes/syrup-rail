use super::*;

pub(super) async fn apply_payment_method_replacement_approved_outcome(
    coordinator: &dyn BillingTransactionCoordinator,
    reservation: &SubscriptionPaymentMethodReplacement,
    evidence: &ProcessorEvidence,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    let identity = reservation.identity();
    let mut transaction = coordinator
        .begin(
            BillingEventSubject::new(identity.billing_scope_id(), identity.subscriber_id()),
            BILLING_LOCK_TIMEOUT,
        )
        .await?;
    let subject_state = transaction.subject_state();
    let application = apply_payment_method_replacement_approved_on_connection(
        transaction.connection(),
        subject_state,
        reservation,
        evidence,
    )
    .await;
    finalize_approved_application(transaction, application).await
}

async fn apply_payment_method_replacement_approved_on_connection(
    connection: &mut PgConnection,
    subject_state: BillingTransactionSubjectState,
    reservation: &SubscriptionPaymentMethodReplacement,
    evidence: &ProcessorEvidence,
) -> Result<
    (SubscriptionEnrollmentPaymentResult, Option<BillingEvent>),
    SubscriptionEnrollmentApplicationError,
> {
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
    let attempt = lock_expected_reservation_attempt(
        connection,
        OutcomeReservation::PaymentMethodReplacement(reservation),
    )
    .await?;
    if attempt.status() == PaymentAttemptStatus::Approved {
        let subscription = load_applied_subscription(connection, &attempt)
            .await?
            .ok_or(SubscriptionEnrollmentApplicationError::InvalidState(
                INVALID_APPLICATION_STATE,
            ))?;
        let progression =
            if attempt.state().processor_evidence().transaction_id() == evidence.transaction_id() {
                ProcessorChargeProgression::Applied
            } else {
                ProcessorChargeProgression::ReconciliationRequired
            };
        observe_processor_charge(connection, &attempt, evidence, progression).await?;
        return Ok((
            SubscriptionEnrollmentPaymentResult::applied(attempt, subscription)?,
            None,
        ));
    }
    if subject_state != BillingTransactionSubjectState::LiveRecipient {
        return Err(SubscriptionEnrollmentApplicationError::InvalidState(
            "a payment method replacement event requires a live recipient",
        ));
    }
    if attempt.status().is_terminal() {
        observe_processor_charge(
            connection,
            &attempt,
            evidence,
            ProcessorChargeProgression::ReconciliationRequired,
        )
        .await?;
        return Ok((
            SubscriptionEnrollmentPaymentResult::confirmation_pending(attempt, evidence.clone())?,
            None,
        ));
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
            ProcessorChargeProgression::ReconciliationRequired,
            None,
        )
        .await?;
        let parked = park_locked_attempt(
            connection,
            &attempt,
            evidence,
            None,
            "An additional approved stored-method result requires manual review.",
        )
        .await?;
        return Ok((
            SubscriptionEnrollmentPaymentResult::not_applied(parked)?,
            None,
        ));
    }
    let transaction_id =
        evidence
            .transaction_id()
            .ok_or(SubscriptionEnrollmentApplicationError::InvalidState(
                INVALID_APPLICATION_STATE,
            ))?;
    let method_id = upsert_payment_method(connection, &attempt, evidence).await?;
    let expected = reservation.expected_state();
    let row = sqlx::query(
        r#"
        SELECT status, payment_method_id, initial_transaction_id
        FROM billing_subscriptions
        WHERE id = $1 AND billing_scope_id = $2 AND subscriber_id = $3
            AND gateway_account_id = $4 AND plan_key = $5
        FOR UPDATE
        "#,
    )
    .bind(reservation.subscription_id().as_uuid())
    .bind(identity.billing_scope_id().as_uuid())
    .bind(identity.subscriber_id().as_uuid())
    .bind(identity.gateway_account_id().as_uuid())
    .bind(reservation.plan_key().as_str())
    .fetch_optional(&mut *connection)
    .await?;
    let Some(row) = row else {
        transition_charge(
            connection,
            charge.id,
            ProcessorChargeProgression::ReconciliationRequired,
            Some(PaymentResolutionCode::SubscriptionApprovedPaymentMethodUpdateSubscriptionIneligible),
        )
        .await?;
        let parked = park_locked_attempt(
            connection,
            &attempt,
            evidence,
            Some(PaymentResolutionCode::SubscriptionApprovedPaymentMethodUpdateSubscriptionIneligible),
            PAYMENT_METHOD_REPLACEMENT_STALE_STATE_TEXT,
        )
        .await?;
        return Ok((
            SubscriptionEnrollmentPaymentResult::not_applied(parked)?,
            None,
        ));
    };
    let status: String = row.try_get("status")?;
    let current_method_id = PaymentMethodId::new(row.try_get("payment_method_id")?);
    let current_transaction_id: String = row.try_get("initial_transaction_id")?;
    let expected_state = status == "active" || status == "past_due";
    let baseline_matches = current_method_id == expected.payment_method_id()
        && current_transaction_id == expected.expected_initial_transaction_id().expose();
    let exact_replay =
        current_method_id == method_id && current_transaction_id == transaction_id.expose();
    if !expected_state || (!baseline_matches && !exact_replay) {
        let code = if expected_state {
            PaymentResolutionCode::SubscriptionApprovedPaymentMethodUpdateStaleState
        } else {
            PaymentResolutionCode::SubscriptionApprovedPaymentMethodUpdateSubscriptionIneligible
        };
        transition_charge(
            connection,
            charge.id,
            ProcessorChargeProgression::ReconciliationRequired,
            Some(code),
        )
        .await?;
        let parked = park_locked_attempt(
            connection,
            &attempt,
            evidence,
            Some(code),
            PAYMENT_METHOD_REPLACEMENT_STALE_STATE_TEXT,
        )
        .await?;
        return Ok((
            SubscriptionEnrollmentPaymentResult::not_applied(parked)?,
            None,
        ));
    }
    if baseline_matches {
        let updated = sqlx::query(
            r#"
            UPDATE billing_subscriptions
            SET payment_method_id = $2, initial_transaction_id = $3,
                updated_at = clock_timestamp()
            WHERE id = $1 AND billing_scope_id = $4 AND subscriber_id = $5
                AND gateway_account_id = $6 AND plan_key = $7
                AND status IN ('active', 'past_due')
                AND payment_method_id = $8 AND initial_transaction_id = $9
            "#,
        )
        .bind(reservation.subscription_id().as_uuid())
        .bind(method_id.as_uuid())
        .bind(transaction_id.expose())
        .bind(identity.billing_scope_id().as_uuid())
        .bind(identity.subscriber_id().as_uuid())
        .bind(identity.gateway_account_id().as_uuid())
        .bind(reservation.plan_key().as_str())
        .bind(expected.payment_method_id().as_uuid())
        .bind(expected.expected_initial_transaction_id().expose())
        .execute(&mut *connection)
        .await?;
        if updated.rows_affected() != 1 {
            return Err(SubscriptionEnrollmentApplicationError::InvalidState(
                INVALID_APPLICATION_STATE,
            ));
        }
    }
    mark_attempt_approved(
        connection,
        &attempt,
        evidence,
        reservation.subscription_id(),
        method_id,
    )
    .await?;
    disable_payment_method_if_unreferenced(connection, expected.payment_method_id()).await?;
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
    let descriptor = evidence.descriptor();
    let card = descriptor
        .canonical_card_brand()
        .zip(descriptor.card_last_four().cloned())
        .map(|(brand, last_four)| PaymentCardDisplay::new(brand, last_four));
    let event = BillingEvent::PaymentMethodChanged {
        attempt_id: identity.attempt_id(),
        subscription_id: reservation.subscription_id(),
        plan_key: reservation.plan_key().clone(),
        card,
    };
    Ok((
        SubscriptionEnrollmentPaymentResult::applied(attempt, subscription)?,
        Some(event),
    ))
}
