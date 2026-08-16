use super::*;

fn recovery_attempt_matches_command(
    attempt: &PaymentAttempt,
    command: &syrup_rail::RecoverSubscriptionPayment,
) -> bool {
    let identity = attempt.identity();
    let PaymentAttemptTarget::SubscriptionRecovery {
        plan_key,
        period,
        expected_state,
        ..
    } = attempt.request().target()
    else {
        return false;
    };
    identity.billing_scope_id() == command.billing_scope_id()
        && identity.subscriber_id() == command.subscriber_id()
        && identity.gateway_configuration_id() == command.gateway_configuration_id()
        && plan_key == command.plan_key()
        && attempt.request().billing_contact()
            == &BillingContactSnapshot::from_billing_contact(command.billing_contact())
        && attempt
            .request()
            .fingerprint()
            .matches_subscription_recovery(
                plan_key,
                expected_state.subscription_id(),
                expected_state.payment_method_id(),
                *period.start_at(),
                attempt.request().amount(),
            )
}

async fn recovery_attempt_matches_replay_context(
    transaction: &mut Transaction<'_, Postgres>,
    attempt: &PaymentAttempt,
) -> Result<bool, sqlx::Error> {
    let PaymentAttemptTarget::SubscriptionRecovery {
        plan_key,
        period,
        expected_state,
        ..
    } = attempt.request().target()
    else {
        return Ok(false);
    };
    let identity = attempt.identity();
    sqlx::query_scalar(
        r#"
        SELECT EXISTS (
            SELECT 1 FROM billing_subscriptions
            WHERE id = $1 AND billing_scope_id = $2 AND subscriber_id = $3
                AND plan_key = $4
                AND (
                    (
                        status IN ('active', 'past_due')
                        AND next_renewal_at = $5
                    )
                    OR (
                        $6 = 'approved'
                        AND next_renewal_at > clock_timestamp()
                        AND current_period_start_at = $5
                    )
                )
        )
        "#,
    )
    .bind(expected_state.subscription_id().as_uuid())
    .bind(identity.billing_scope_id().as_uuid())
    .bind(identity.subscriber_id().as_uuid())
    .bind(plan_key.as_str())
    .bind(period.start_at())
    .bind(attempt.status().as_str())
    .fetch_one(&mut **transaction)
    .await
}

async fn recovery_attempt_for_replay(
    transaction: &mut Transaction<'_, Postgres>,
    attempt: PaymentAttempt,
) -> Result<Option<PaymentAttempt>, PaymentAttemptStoreError> {
    if attempt_replay_disposition(&attempt) == AttemptReplayDisposition::ReturnCanonical {
        return Ok(Some(attempt));
    }

    let attempt = expire_stale_recovery_context_and_reload(transaction, &attempt).await?;
    if attempt_replay_disposition(&attempt) == AttemptReplayDisposition::ReturnCanonical
        || recovery_attempt_matches_replay_context(transaction, &attempt).await?
    {
        Ok(Some(attempt))
    } else {
        Ok(None)
    }
}

fn recovery_attempt_matches_reservation(
    attempt: &PaymentAttempt,
    reservation: &SubscriptionRecoveryReservation,
) -> bool {
    recovery_attempt_belongs_to_reservation(attempt, reservation)
        && attempt.request() == reservation.request()
}

fn recovery_attempt_belongs_to_reservation(
    attempt: &PaymentAttempt,
    reservation: &SubscriptionRecoveryReservation,
) -> bool {
    attempt.identity() == reservation.identity()
        && attempt.kind() == PaymentAttemptKind::SubscriptionRecovery
        && attempt.request().idempotency_key() == reservation.request().idempotency_key()
        && attempt.request().gateway_order_id() == reservation.request().gateway_order_id()
}

async fn expire_stale_recovery_context_and_reload(
    transaction: &mut Transaction<'_, Postgres>,
    attempt: &PaymentAttempt,
) -> Result<PaymentAttempt, PaymentAttemptStoreError> {
    let Some(subscription_id) = attempt.request().target().subscription_id() else {
        return Err(invalid_state());
    };
    fail_stale_unsubmitted_subscription_charges(transaction, subscription_id).await?;
    find_payment_attempt_by_id_in_transaction(
        transaction,
        attempt.identity().billing_scope_id(),
        attempt.identity().attempt_id(),
    )
    .await?
    .ok_or_else(invalid_state)
}

fn locked_recovery_terms_from_row(
    row: &PgRow,
    subscription_id: SubscriptionId,
    gateway_account_id: GatewayAccountId,
) -> Result<SubscriptionRecoveryLockedTerms, PaymentAttemptStoreError> {
    let payment_method_id = PaymentMethodId::new(row.try_get("payment_method_id")?);
    let period_start_at: DateTime<Utc> = row.try_get("next_renewal_at")?;
    let recurring_period_kind: String = row.try_get("recurring_period_kind")?;
    let recurring_period_count: i32 = row.try_get("recurring_period_count")?;
    let recurring_period = subscription_period_rule_from_scalars(
        SubscriptionPeriodRuleScalars::new(&recurring_period_kind, recurring_period_count),
    )
    .map_err(map_subscription_persistence_error)?;
    let period = syrup_rail::next_billing_period(period_start_at, recurring_period)
        .map_err(|_| invalid_state())?;
    let currency =
        CurrencyCode::new(&row.try_get::<String, _>("currency")?).map_err(|_| invalid_state())?;
    let charge =
        ChargeAmount::new(row.try_get("amount_cents")?, currency).map_err(|_| invalid_state())?;
    let initial_transaction_id =
        GatewayTransactionId::new(row.try_get::<String, _>("initial_transaction_id")?)
            .map_err(|_| invalid_state())?;
    let status = row
        .try_get::<String, _>("status")?
        .parse::<SubscriptionStatus>()
        .map_err(|_| invalid_state())?;
    let expected_state = locked_subscription_payment_state(
        subscription_id,
        payment_method_id,
        initial_transaction_id,
        status,
    )?;
    Ok(SubscriptionRecoveryLockedTerms::new(
        gateway_account_id,
        expected_state,
        period,
        charge,
    ))
}

async fn recovery_subscription_state_matches(
    transaction: &mut Transaction<'_, Postgres>,
    reservation: &SubscriptionRecoveryReservation,
) -> Result<bool, sqlx::Error> {
    let identity = reservation.identity();
    let expected = reservation.expected_state();
    sqlx::query_scalar(
        r#"
        SELECT EXISTS (
            SELECT 1 FROM billing_subscriptions
            WHERE id = $1 AND billing_scope_id = $2 AND subscriber_id = $3
                AND gateway_account_id = $4 AND plan_key = $5
                -- New v2 recoveries are reserved only from past_due, but a
                -- durable v1 attempt may have snapshotted active authority.
                AND status = $6 AND status IN ('active', 'past_due')
                AND payment_method_id = $7 AND initial_transaction_id = $8
                AND next_renewal_at = $9
        )
        "#,
    )
    .bind(reservation.subscription_id().as_uuid())
    .bind(identity.billing_scope_id().as_uuid())
    .bind(identity.subscriber_id().as_uuid())
    .bind(identity.gateway_account_id().as_uuid())
    .bind(reservation.plan_key().as_str())
    .bind(expected.status().as_str())
    .bind(expected.payment_method_id().as_uuid())
    .bind(expected.initial_transaction_id().expose())
    .bind(reservation.period().start_at())
    .fetch_one(&mut **transaction)
    .await
}

async fn insert_recovery_attempt(
    transaction: &mut Transaction<'_, Postgres>,
    reservation: &SubscriptionRecoveryReservation,
) -> Result<bool, sqlx::Error> {
    let identity = reservation.identity();
    let request = reservation.request();
    let expected = reservation.expected_state();
    let result = sqlx::query(
        r#"
        INSERT INTO billing_payment_attempts (
            id, billing_scope_id, subscriber_id, plan_key, subscription_id,
            payment_method_id, attempt_kind, status, idempotency_key,
            request_fingerprint, amount_cents, currency,
            billing_period_start_at, billing_period_end_at,
            gateway_account_id, gateway_configuration_id, gateway_order_id,
            billing_name, billing_email,
            subscription_expected_payment_method_id,
            subscription_expected_initial_transaction_id,
            subscription_expected_status
        ) VALUES (
            $1, $2, $3, $4, $5, $6, 'subscription_recovery', 'pending',
            $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17,
            $18, $19, $20
        )
        ON CONFLICT DO NOTHING
        "#,
    )
    .bind(identity.attempt_id().as_uuid())
    .bind(identity.billing_scope_id().as_uuid())
    .bind(identity.subscriber_id().as_uuid())
    .bind(reservation.plan_key().as_str())
    .bind(reservation.subscription_id().as_uuid())
    .bind(expected.payment_method_id().as_uuid())
    .bind(request.idempotency_key().expose())
    .bind(request.fingerprint().expose())
    .bind(request.amount().cents())
    .bind(request.amount().currency().as_str())
    .bind(reservation.period().start_at())
    .bind(reservation.period().end_at())
    .bind(identity.gateway_account_id().as_uuid())
    .bind(identity.gateway_configuration_id().as_uuid())
    .bind(request.gateway_order_id().expose())
    .bind(request.billing_contact().name())
    .bind(request.billing_contact().email())
    .bind(expected.payment_method_id().as_uuid())
    .bind(expected.initial_transaction_id().expose())
    .bind(expected.status().as_str())
    .execute(&mut **transaction)
    .await?;
    Ok(result.rows_affected() == 1)
}

async fn reject_locked_recovery(
    transaction: &mut Transaction<'_, Postgres>,
    attempt: PaymentAttempt,
    reason: SubscriptionRecoverySubmissionRejection,
    message: &'static str,
) -> Result<SubscriptionRecoverySubmissionOutcome, PaymentAttemptStoreError> {
    let resolution_code = match reason {
        SubscriptionRecoverySubmissionRejection::BillingStateChanged => {
            PaymentResolutionCode::SubscriptionRenewalRetryStateChangedBeforeCharge
        }
        SubscriptionRecoverySubmissionRejection::GatewayConfigurationChanged => {
            PaymentResolutionCode::GatewayConfigurationBeforeSubmission
        }
    };
    let attempt = reject_prepared_attempt(transaction, &attempt, resolution_code, message).await?;
    Ok(SubscriptionRecoverySubmissionOutcome::Rejected { attempt, reason })
}

/// Resolves subscriber-wide recovery idempotency before host admission.
///
/// A recovery command deliberately contains no amount or period. Matching is
/// therefore against the immutable canonical target already stored on the
/// attempt, plus the command's owner, plan, and gateway configuration.
pub async fn preflight_subscription_recovery_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    command: &syrup_rail::RecoverSubscriptionPayment,
) -> Result<SubscriptionRecoveryPreflightOutcome, PaymentAttemptStoreError> {
    set_enrollment_timeouts(transaction).await?;
    match preflight_existing_attempt(
        transaction,
        command.billing_scope_id(),
        command.subscriber_id(),
        command.idempotency_key(),
        |attempt| recovery_attempt_matches_command(attempt, command),
    )
    .await?
    {
        ExistingAttemptPreflight::Continue => {
            return Ok(SubscriptionRecoveryPreflightOutcome::Continue);
        }
        ExistingAttemptPreflight::IdempotencyConflict => {
            return Ok(SubscriptionRecoveryPreflightOutcome::IdempotencyConflict);
        }
        ExistingAttemptPreflight::ReplayCanonical(existing) => {
            return Ok(SubscriptionRecoveryPreflightOutcome::Replay(existing));
        }
        ExistingAttemptPreflight::RequiresLockedContext => {}
    }
    lock_subscription_aggregate(transaction, command.subscriber_id(), command.plan_key()).await?;
    let Some(existing) = payment_attempt_by_idempotency(
        transaction,
        command.billing_scope_id(),
        command.subscriber_id(),
        command.idempotency_key(),
        true,
    )
    .await?
    else {
        return Err(invalid_state());
    };
    if !recovery_attempt_matches_command(&existing, command) {
        return Ok(SubscriptionRecoveryPreflightOutcome::IdempotencyConflict);
    }
    Ok(
        match recovery_attempt_for_replay(transaction, existing).await? {
            Some(existing) => SubscriptionRecoveryPreflightOutcome::Replay(Box::new(existing)),
            None => SubscriptionRecoveryPreflightOutcome::IdempotencyConflict,
        },
    )
}

/// Locks the canonical subscription, derives the exact due-period request, and
/// inserts a token-free recovery attempt in one transaction.
pub async fn reserve_subscription_recovery_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    command: &syrup_rail::RecoverSubscriptionPayment,
    gateway: &syrup_rail::ResolvedGateway,
) -> Result<SubscriptionRecoveryReservationOutcome, PaymentAttemptStoreError> {
    set_enrollment_timeouts(transaction).await?;
    lock_subscription_aggregate(transaction, command.subscriber_id(), command.plan_key()).await?;

    if let Some(existing) = payment_attempt_by_idempotency(
        transaction,
        command.billing_scope_id(),
        command.subscriber_id(),
        command.idempotency_key(),
        true,
    )
    .await?
    {
        if !recovery_attempt_matches_command(&existing, command) {
            return Ok(SubscriptionRecoveryReservationOutcome::IdempotencyConflict);
        }
        return Ok(
            match recovery_attempt_for_replay(transaction, existing).await? {
                Some(existing) => {
                    SubscriptionRecoveryReservationOutcome::Replay(Box::new(existing))
                }
                None => SubscriptionRecoveryReservationOutcome::IdempotencyConflict,
            },
        );
    }

    let row = sqlx::query(
        r#"
        SELECT id, gateway_account_id, payment_method_id, amount_cents, currency,
            next_renewal_at, initial_transaction_id, status,
            recurring_period_kind, recurring_period_count,
            next_renewal_at <= clock_timestamp() AS is_due
        FROM billing_subscriptions
        WHERE billing_scope_id = $1 AND subscriber_id = $2 AND plan_key = $3
            AND status = 'past_due'
        ORDER BY updated_at DESC, id DESC
        LIMIT 1
        FOR UPDATE
        "#,
    )
    .bind(command.billing_scope_id().as_uuid())
    .bind(command.subscriber_id().as_uuid())
    .bind(command.plan_key().as_str())
    .fetch_optional(&mut **transaction)
    .await?;
    let Some(row) = row else {
        return Ok(SubscriptionRecoveryReservationOutcome::Rejected(
            SubscriptionRecoveryReservationRejection::SubscriptionNotFound,
        ));
    };
    if !row.try_get::<bool, _>("is_due")? {
        return Ok(SubscriptionRecoveryReservationOutcome::Rejected(
            SubscriptionRecoveryReservationRejection::PaymentNotDue,
        ));
    }

    let subscription_id = SubscriptionId::new(row.try_get("id")?);
    fail_stale_unsubmitted_payment_method_updates(transaction, subscription_id).await?;
    fail_stale_unsubmitted_subscription_charges(transaction, subscription_id).await?;
    let gateway_account_id = GatewayAccountId::new(row.try_get("gateway_account_id")?);
    let expected_gateway = ExpectedGatewayIdentity::for_gateway(
        command.billing_scope_id(),
        command.gateway_configuration_id(),
        gateway,
    );
    if gateway_account_id != expected_gateway.gateway_account_id()
        || !gateway_identity_matches_account(transaction, &expected_gateway).await?
    {
        return Ok(SubscriptionRecoveryReservationOutcome::Rejected(
            SubscriptionRecoveryReservationRejection::GatewayConfigurationChanged,
        ));
    }
    if blocking_subscription_charge_attempt_exists(transaction, subscription_id).await? {
        return Ok(SubscriptionRecoveryReservationOutcome::Rejected(
            SubscriptionRecoveryReservationRejection::AttemptInProgress,
        ));
    }
    if blocking_payment_method_update_exists(transaction, subscription_id).await? {
        return Ok(SubscriptionRecoveryReservationOutcome::Rejected(
            SubscriptionRecoveryReservationRejection::PaymentMethodUpdateInProgress,
        ));
    }

    let terms = locked_recovery_terms_from_row(&row, subscription_id, gateway_account_id)?;
    let reservation = SubscriptionRecoveryReservation::from_locked_subscription_terms(
        command,
        gateway,
        command.attempt_id(),
        terms,
    )
    .map_err(|_| invalid_state())?;

    let inserted = insert_recovery_attempt(transaction, &reservation).await?;
    if inserted {
        let attempt = payment_attempt_by_idempotency(
            transaction,
            command.billing_scope_id(),
            command.subscriber_id(),
            command.idempotency_key(),
            true,
        )
        .await?
        .ok_or_else(invalid_state)?;
        return Ok(SubscriptionRecoveryReservationOutcome::Reserved(
            Box::new(reservation),
            Box::new(attempt),
        ));
    }

    if let Some(existing) = payment_attempt_by_idempotency(
        transaction,
        command.billing_scope_id(),
        command.subscriber_id(),
        command.idempotency_key(),
        true,
    )
    .await?
    {
        if !recovery_attempt_matches_command(&existing, command) {
            return Ok(SubscriptionRecoveryReservationOutcome::IdempotencyConflict);
        }
        return Ok(
            match recovery_attempt_for_replay(transaction, existing).await? {
                Some(existing) => {
                    SubscriptionRecoveryReservationOutcome::Replay(Box::new(existing))
                }
                None => SubscriptionRecoveryReservationOutcome::IdempotencyConflict,
            },
        );
    }
    Ok(SubscriptionRecoveryReservationOutcome::Rejected(
        SubscriptionRecoveryReservationRejection::AttemptInProgress,
    ))
}

/// Revalidates the exact locked snapshot and commits one-shot provider
/// admission. Every semantic rejection terminalizes the prepared attempt.
pub async fn admit_subscription_recovery_submission_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    reservation: &SubscriptionRecoveryReservation,
) -> Result<SubscriptionRecoverySubmissionOutcome, PaymentAttemptStoreError> {
    set_enrollment_timeouts(transaction).await?;
    let identity = reservation.identity();
    lock_subscription_aggregate(
        transaction,
        identity.subscriber_id(),
        reservation.plan_key(),
    )
    .await?;
    fail_stale_unsubmitted_payment_method_updates(transaction, reservation.subscription_id())
        .await?;
    fail_stale_unsubmitted_subscription_charges(transaction, reservation.subscription_id()).await?;
    let attempt = payment_attempt_by_idempotency(
        transaction,
        identity.billing_scope_id(),
        identity.subscriber_id(),
        reservation.request().idempotency_key(),
        true,
    )
    .await?
    .ok_or_else(invalid_state)?;
    if !recovery_attempt_belongs_to_reservation(&attempt, reservation) {
        return Err(invalid_state());
    }
    if attempt.status() != PaymentAttemptStatus::Pending
        || attempt.state().timestamps().submitted_at().is_some()
    {
        return Ok(SubscriptionRecoverySubmissionOutcome::AlreadyAdmitted(
            attempt,
        ));
    }

    let state_matches = recovery_attempt_matches_reservation(&attempt, reservation)
        && recovery_subscription_state_matches(transaction, reservation).await?
        && !blocking_subscription_charge_attempt_exists_except(
            transaction,
            reservation.subscription_id(),
            identity.attempt_id(),
        )
        .await?
        && !blocking_payment_method_update_exists(transaction, reservation.subscription_id())
            .await?;
    if !state_matches {
        return reject_locked_recovery(
            transaction,
            attempt,
            SubscriptionRecoverySubmissionRejection::BillingStateChanged,
            RECOVERY_STATE_CHANGED_TEXT,
        )
        .await;
    }
    let expected_gateway =
        ExpectedGatewayIdentity::from_reservation(identity, reservation.provider_key());
    if !gateway_identity_matches_account(transaction, &expected_gateway).await? {
        return reject_locked_recovery(
            transaction,
            attempt,
            SubscriptionRecoverySubmissionRejection::GatewayConfigurationChanged,
            RECOVERY_CONFIGURATION_CHANGED_TEXT,
        )
        .await;
    }

    let admitted = admit_prepared_attempt(transaction, &attempt).await?;
    Ok(SubscriptionRecoverySubmissionOutcome::Admitted(admitted))
}
