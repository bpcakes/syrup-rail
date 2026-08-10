use super::*;

pub(super) async fn apply_approved_outcome(
    coordinator: &dyn BillingTransactionCoordinator,
    reservation: &SubscriptionEnrollmentReservation,
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
    let application = apply_approved_on_connection(
        transaction.connection(),
        subject_state,
        reservation,
        evidence,
    )
    .await;
    finalize_approved_application(transaction, application).await
}

async fn apply_approved_on_connection(
    connection: &mut PgConnection,
    subject_state: BillingTransactionSubjectState,
    reservation: &SubscriptionEnrollmentReservation,
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
    let attempt =
        lock_expected_reservation_attempt(connection, OutcomeReservation::Initial(reservation))
            .await?;

    if attempt.status() == PaymentAttemptStatus::Approved {
        let subscription = load_applied_subscription(connection, &attempt).await?;
        let Some(subscription) = subscription else {
            return Err(SubscriptionEnrollmentApplicationError::InvalidState(
                INVALID_APPLICATION_STATE,
            ));
        };
        observe_processor_charge(
            connection,
            &attempt,
            evidence,
            ProcessorChargeProgression::Applied,
        )
        .await?;
        return Ok((
            SubscriptionEnrollmentPaymentResult::new(attempt, Some(subscription)),
            None,
        ));
    }

    if subject_state != BillingTransactionSubjectState::LiveRecipient {
        return Err(SubscriptionEnrollmentApplicationError::InvalidState(
            "a new subscription enrollment event requires a live recipient",
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
        let parked = park_locked_attempt(
            connection,
            &attempt,
            evidence,
            None,
            TERMINAL_APPROVAL_RACE_TEXT,
        )
        .await?;
        if let ObservedCharge::Owned(charge) = observation {
            transition_charge(
                connection,
                charge.id,
                ProcessorChargeProgression::ExternalReversalRequired,
                None,
            )
            .await?;
        }
        return Ok((SubscriptionEnrollmentPaymentResult::new(parked, None), None));
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
        return Ok((SubscriptionEnrollmentPaymentResult::new(parked, None), None));
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
        return Ok((SubscriptionEnrollmentPaymentResult::new(parked, None), None));
    }

    if current_subscription_exists(connection, reservation).await? {
        transition_charge(
            connection,
            charge.id,
            ProcessorChargeProgression::ExternalReversalRequired,
            Some(PaymentResolutionCode::SubscriptionInitialCurrentSubscriptionConflict),
        )
        .await?;
        let parked = park_locked_attempt(
            connection,
            &attempt,
            evidence,
            Some(PaymentResolutionCode::SubscriptionInitialCurrentSubscriptionConflict),
            CURRENT_SUBSCRIPTION_CONFLICT_TEXT,
        )
        .await?;
        return Ok((SubscriptionEnrollmentPaymentResult::new(parked, None), None));
    }
    if active_grant_exists(connection, reservation).await? {
        transition_charge(
            connection,
            charge.id,
            ProcessorChargeProgression::ExternalReversalRequired,
            Some(PaymentResolutionCode::SubscriptionInitialCurrentGrantConflict),
        )
        .await?;
        let parked = park_locked_attempt(
            connection,
            &attempt,
            evidence,
            Some(PaymentResolutionCode::SubscriptionInitialCurrentGrantConflict),
            CURRENT_GRANT_CONFLICT_TEXT,
        )
        .await?;
        return Ok((SubscriptionEnrollmentPaymentResult::new(parked, None), None));
    }

    let transaction_id =
        evidence
            .transaction_id()
            .ok_or(SubscriptionEnrollmentApplicationError::InvalidState(
                INVALID_APPLICATION_STATE,
            ))?;
    let method_id = upsert_payment_method(connection, &attempt, evidence).await?;
    let period_start_at = attempt.state().timestamps().submitted_or_created_at();
    // Initial application intentionally retains its historical partial
    // reservation match, so the locked attempt remains the sole authority for
    // accepted financial and lifecycle terms.
    let durable_reservation = SubscriptionEnrollmentReservation::from_attempt(
        &attempt,
        reservation.provider_key().clone(),
    )
    .map_err(|_| SubscriptionEnrollmentApplicationError::InvalidState(INVALID_APPLICATION_STATE))?;
    let activation = durable_reservation.expected_terms().activation_projection();
    let period =
        next_billing_period(period_start_at, activation.initial_period_rule()).map_err(|_| {
            SubscriptionEnrollmentApplicationError::InvalidState(INVALID_APPLICATION_STATE)
        })?;
    let subscription_id = SubscriptionId::new(Uuid::now_v7());
    insert_subscription(
        connection,
        &attempt,
        subscription_id,
        method_id,
        activation.recurring_charge_after_initial().cents(),
        activation.phase(),
        &period,
        transaction_id,
    )
    .await?;
    apply_initial_discount(
        connection,
        &attempt,
        subscription_id,
        period_start_at,
        activation.discount_periods_applied(),
    )
    .await?;
    mark_attempt_approved(connection, &attempt, evidence, subscription_id, method_id).await?;
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
    let subscription = load_subscription(connection, identity.billing_scope_id(), subscription_id)
        .await?
        .ok_or(SubscriptionEnrollmentApplicationError::InvalidState(
            INVALID_APPLICATION_STATE,
        ))?;
    let event = BillingEvent::SubscriptionStarted {
        attempt_id: identity.attempt_id(),
        subscription_id,
        plan_key: reservation.plan_key().clone(),
        charge: syrup_rail::ChargeAmount::new(
            attempt.request().amount().cents(),
            attempt.request().amount().currency(),
        )
        .map_err(|_| {
            SubscriptionEnrollmentApplicationError::InvalidState(INVALID_APPLICATION_STATE)
        })?,
        period,
        phase: activation.phase(),
    };
    Ok((
        SubscriptionEnrollmentPaymentResult::new(attempt, Some(subscription)),
        Some(event),
    ))
}

async fn current_subscription_exists(
    connection: &mut PgConnection,
    reservation: &SubscriptionEnrollmentReservation,
) -> Result<bool, sqlx::Error> {
    let identity = reservation.identity();
    let rows = sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT id FROM billing_subscriptions
        WHERE billing_scope_id = $1 AND subscriber_id = $2 AND plan_key = $3
            AND (
                status IN ('active', 'past_due')
                OR (status = 'canceled' AND current_period_end_at > clock_timestamp())
            )
        ORDER BY updated_at DESC, id DESC
        FOR NO KEY UPDATE
        "#,
    )
    .bind(identity.billing_scope_id().as_uuid())
    .bind(identity.subscriber_id().as_uuid())
    .bind(reservation.plan_key().as_str())
    .fetch_all(connection)
    .await?;
    Ok(!rows.is_empty())
}

async fn active_grant_exists(
    connection: &mut PgConnection,
    reservation: &SubscriptionEnrollmentReservation,
) -> Result<bool, sqlx::Error> {
    let identity = reservation.identity();
    let rows = sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT id FROM billing_subscription_grants
        WHERE billing_scope_id = $1 AND subscriber_id = $2 AND plan_key = $3
            AND revoked_at IS NULL
            AND starts_at <= clock_timestamp() AND ends_at > clock_timestamp()
        ORDER BY ends_at DESC, id DESC
        FOR UPDATE
        "#,
    )
    .bind(identity.billing_scope_id().as_uuid())
    .bind(identity.subscriber_id().as_uuid())
    .bind(reservation.plan_key().as_str())
    .fetch_all(connection)
    .await?;
    Ok(!rows.is_empty())
}

#[allow(clippy::too_many_arguments)]
async fn insert_subscription(
    connection: &mut PgConnection,
    attempt: &PaymentAttempt,
    subscription_id: SubscriptionId,
    method_id: PaymentMethodId,
    recurring_amount_cents: i32,
    phase: SubscriptionPhase,
    period: &BillingPeriod,
    transaction_id: &GatewayTransactionId,
) -> Result<(), sqlx::Error> {
    let identity = attempt.identity();
    let plan_key = attempt
        .request()
        .target()
        .plan_key()
        .expect("validated initial plan");
    let offer = attempt
        .request()
        .target()
        .enrollment_offer()
        .expect("validated initial offer");
    let trial = offer.start().paid_trial();
    let retry_delays = offer
        .renewal_failure()
        .schedule()
        .retry_delays()
        .iter()
        .map(|delay| i64::from(delay.seconds().get()))
        .collect::<Vec<_>>();
    sqlx::query(
        r#"
        INSERT INTO billing_subscriptions (
            id, billing_scope_id, subscriber_id, plan_key, status,
            gateway_account_id, payment_method_id, amount_cents, currency,
            current_period_start_at, current_period_end_at, next_renewal_at,
            initial_transaction_id, phase, recurring_period_kind,
            recurring_period_count, trial_amount_cents, trial_period_kind,
            trial_period_count, dunning_retry_delays_seconds,
            dunning_exhaustion, past_due_access, next_payment_attempt_at
        ) VALUES (
            $1, $2, $3, $4, 'active', $5, $6, $7, $8, $9, $10, $10,
            $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $10
        )
        "#,
    )
    .bind(subscription_id.as_uuid())
    .bind(identity.billing_scope_id().as_uuid())
    .bind(identity.subscriber_id().as_uuid())
    .bind(plan_key.as_str())
    .bind(identity.gateway_account_id().as_uuid())
    .bind(method_id.as_uuid())
    .bind(recurring_amount_cents)
    .bind(attempt.request().amount().currency().as_str())
    .bind(period.start_at())
    .bind(period.end_at())
    .bind(transaction_id.expose())
    .bind(phase.as_str())
    .bind(offer.recurring().period().as_str())
    .bind(i32::from(offer.recurring().period().count().get()))
    .bind(trial.map(|trial| trial.charge().cents()))
    .bind(trial.map(|trial| trial.period().as_str()))
    .bind(trial.map(|trial| i32::from(trial.period().count().get())))
    .bind(retry_delays)
    .bind(offer.renewal_failure().exhaustion().as_str())
    .bind(offer.renewal_failure().past_due_access().as_str())
    .execute(connection)
    .await?;
    Ok(())
}

async fn apply_initial_discount(
    connection: &mut PgConnection,
    attempt: &PaymentAttempt,
    subscription_id: SubscriptionId,
    applied_at: DateTime<Utc>,
    discount_periods_applied: u8,
) -> Result<(), SubscriptionEnrollmentApplicationError> {
    let Some(discount) = attempt.request().target().enrollment_discount() else {
        return Ok(());
    };
    let identity = attempt.identity();
    let snapshot = discount.snapshot();
    let (amount_off_cents, percent_off_bps) = match snapshot.kind() {
        SubscriptionDiscountKind::AmountOffCents(value) => (Some(value.get()), None),
        SubscriptionDiscountKind::PercentOffBasisPoints(value) => {
            (None, Some(i32::from(value.get())))
        }
    };
    let periods_applied = i32::from(discount_periods_applied);
    let (duration_months, periods_total, status, completed_at) = match snapshot.duration() {
        SubscriptionDiscountDuration::Indefinite => (None, None, "active", None),
        SubscriptionDiscountDuration::LimitedMonths(months) => {
            let months = i32::from(months.get());
            if periods_applied == months {
                (Some(months), Some(months), "completed", Some(applied_at))
            } else {
                (Some(months), Some(months), "active", None)
            }
        }
    };
    sqlx::query(
        r#"
        INSERT INTO billing_subscription_discounts (
            subscription_id, billing_scope_id, subscriber_id, plan_key,
            discount_claim_id, discount_code_id, code_snapshot, label_snapshot,
            discount_kind, amount_off_cents, percent_off_bps, currency, duration,
            duration_months, base_amount_cents, discounted_amount_cents,
            periods_total, periods_applied, status, applied_at, completed_at
        ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
            $14, $15, $16, $17, $18, $19, $20, $21
        )
        "#,
    )
    .bind(subscription_id.as_uuid())
    .bind(identity.billing_scope_id().as_uuid())
    .bind(identity.subscriber_id().as_uuid())
    .bind(
        attempt
            .request()
            .target()
            .plan_key()
            .expect("validated plan")
            .as_str(),
    )
    .bind(discount.claim_id().as_uuid())
    .bind(discount.code_id().as_uuid())
    .bind(snapshot.code().as_str())
    .bind(snapshot.label())
    .bind(snapshot.kind().as_str())
    .bind(amount_off_cents)
    .bind(percent_off_bps)
    .bind(snapshot.currency().as_str())
    .bind(snapshot.duration().as_str())
    .bind(duration_months)
    .bind(snapshot.base_charge().cents())
    .bind(snapshot.discounted_charge().cents())
    .bind(periods_total)
    .bind(periods_applied)
    .bind(status)
    .bind(applied_at)
    .bind(completed_at)
    .execute(&mut *connection)
    .await?;

    let updated = sqlx::query(
        r#"
        UPDATE billing_subscription_discount_claims
        SET status = 'applied', applied_subscription_id = $5,
            applied_payment_attempt_id = $6, applied_at = $7
        WHERE id = $1 AND billing_scope_id = $2 AND subscriber_id = $3
            AND plan_key = $4 AND status IN ('saved', 'expired')
            AND applied_subscription_id IS NULL
            AND applied_payment_attempt_id IS NULL
        "#,
    )
    .bind(discount.claim_id().as_uuid())
    .bind(identity.billing_scope_id().as_uuid())
    .bind(identity.subscriber_id().as_uuid())
    .bind(
        attempt
            .request()
            .target()
            .plan_key()
            .expect("validated plan")
            .as_str(),
    )
    .bind(subscription_id.as_uuid())
    .bind(identity.attempt_id().as_uuid())
    .bind(applied_at)
    .execute(connection)
    .await?;
    if updated.rows_affected() != 1 {
        return Err(SubscriptionEnrollmentApplicationError::InvalidState(
            INVALID_APPLICATION_STATE,
        ));
    }
    Ok(())
}
