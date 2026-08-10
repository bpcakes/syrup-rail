use super::*;

fn payment_method_replacement_attempt_matches_command(
    attempt: &PaymentAttempt,
    command: &syrup_rail::ReplaceSubscriptionPaymentMethod,
) -> bool {
    let identity = attempt.identity();
    let PaymentAttemptTarget::SubscriptionPaymentMethodUpdate {
        plan_key,
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
        && attempt
            .request()
            .fingerprint()
            .matches_subscription_payment_method_update(plan_key, expected_state)
}

async fn payment_method_replacement_attempt_matches_replay_context(
    transaction: &mut Transaction<'_, Postgres>,
    attempt: &PaymentAttempt,
) -> Result<bool, sqlx::Error> {
    let PaymentAttemptTarget::SubscriptionPaymentMethodUpdate {
        plan_key,
        payment_method_id,
        expected_state,
    } = attempt.request().target()
    else {
        return Ok(false);
    };
    let identity = attempt.identity();
    let approved_transaction = attempt
        .state()
        .processor_evidence()
        .transaction_id()
        .map(GatewayTransactionId::expose);
    sqlx::query_scalar(
        r#"
        SELECT EXISTS (
            SELECT 1 FROM billing_subscriptions
            WHERE id = $1 AND billing_scope_id = $2 AND subscriber_id = $3
                AND gateway_account_id = $4 AND plan_key = $5
                AND (
                    (
                        status IN ('active', 'past_due')
                        AND payment_method_id = $6
                        AND initial_transaction_id = $7
                    )
                    OR (
                        $8 = 'approved'
                        AND payment_method_id = $9
                        AND initial_transaction_id = $10
                    )
                )
        )
        "#,
    )
    .bind(expected_state.subscription_id().as_uuid())
    .bind(identity.billing_scope_id().as_uuid())
    .bind(identity.subscriber_id().as_uuid())
    .bind(identity.gateway_account_id().as_uuid())
    .bind(plan_key.as_str())
    .bind(expected_state.payment_method_id().as_uuid())
    .bind(expected_state.expected_initial_transaction_id().expose())
    .bind(attempt.status().as_str())
    .bind(payment_method_id.as_uuid())
    .bind(approved_transaction)
    .fetch_one(&mut **transaction)
    .await
}

fn payment_method_replacement_attempt_belongs_to_reservation(
    attempt: &PaymentAttempt,
    reservation: &SubscriptionPaymentMethodReplacement,
) -> bool {
    attempt.identity() == reservation.identity()
        && attempt.kind() == PaymentAttemptKind::SubscriptionPaymentMethodUpdate
        && attempt.request().idempotency_key() == reservation.request().idempotency_key()
        && attempt.request().gateway_order_id() == reservation.request().gateway_order_id()
}

fn locked_payment_method_replacement_terms_from_row(
    row: &PgRow,
    subscription_id: SubscriptionId,
    gateway_account_id: GatewayAccountId,
) -> Result<SubscriptionPaymentMethodReplacementLockedTerms, PaymentAttemptStoreError> {
    let payment_method_id = PaymentMethodId::new(row.try_get("payment_method_id")?);
    let initial_transaction_id =
        GatewayTransactionId::new(row.try_get::<String, _>("initial_transaction_id")?)
            .map_err(|_| invalid_state())?;
    let currency =
        CurrencyCode::new(&row.try_get::<String, _>("currency")?).map_err(|_| invalid_state())?;
    let expected_state = PaymentMethodUpdateSnapshot::new(
        subscription_id,
        payment_method_id,
        initial_transaction_id,
    );
    Ok(SubscriptionPaymentMethodReplacementLockedTerms::new(
        gateway_account_id,
        expected_state,
        currency,
    ))
}

async fn blocking_subscription_charge_attempt_exists_for_method_replacement(
    transaction: &mut Transaction<'_, Postgres>,
    subscription_id: SubscriptionId,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM billing_payment_attempts AS attempts
            INNER JOIN billing_subscriptions AS subscriptions
                ON subscriptions.id = attempts.subscription_id
            WHERE attempts.subscription_id = $1
                AND attempts.attempt_kind IN ('subscription_renewal', 'subscription_recovery')
                AND attempts.billing_period_start_at = subscriptions.next_renewal_at
                AND attempts.status IN ('pending', 'unknown', 'approved')
        )
        "#,
    )
    .bind(subscription_id.as_uuid())
    .fetch_one(&mut **transaction)
    .await
}

async fn blocking_payment_method_update_exists_except(
    transaction: &mut Transaction<'_, Postgres>,
    subscription_id: SubscriptionId,
    excluded_attempt_id: PaymentAttemptId,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        r#"
        SELECT EXISTS (
            SELECT 1 FROM billing_payment_attempts
            WHERE subscription_id = $1 AND id <> $2
                AND attempt_kind = 'subscription_payment_method_update'
                AND status IN ('pending', 'unknown', 'review_required')
        )
        "#,
    )
    .bind(subscription_id.as_uuid())
    .bind(excluded_attempt_id.as_uuid())
    .fetch_one(&mut **transaction)
    .await
}

async fn payment_method_replacement_subscription_state_matches(
    transaction: &mut Transaction<'_, Postgres>,
    reservation: &SubscriptionPaymentMethodReplacement,
) -> Result<bool, sqlx::Error> {
    let identity = reservation.identity();
    let expected = reservation.expected_state();
    sqlx::query_scalar(
        r#"
        SELECT EXISTS (
            SELECT 1 FROM billing_subscriptions
            WHERE id = $1 AND billing_scope_id = $2 AND subscriber_id = $3
                AND gateway_account_id = $4 AND plan_key = $5
                AND status IN ('active', 'past_due')
                AND payment_method_id = $6 AND initial_transaction_id = $7
        )
        "#,
    )
    .bind(expected.subscription_id().as_uuid())
    .bind(identity.billing_scope_id().as_uuid())
    .bind(identity.subscriber_id().as_uuid())
    .bind(identity.gateway_account_id().as_uuid())
    .bind(reservation.plan_key().as_str())
    .bind(expected.payment_method_id().as_uuid())
    .bind(expected.expected_initial_transaction_id().expose())
    .fetch_one(&mut **transaction)
    .await
}

async fn insert_payment_method_replacement_attempt(
    transaction: &mut Transaction<'_, Postgres>,
    reservation: &SubscriptionPaymentMethodReplacement,
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
            gateway_account_id, gateway_configuration_id, gateway_order_id,
            billing_name, billing_email,
            payment_method_update_expected_payment_method_id,
            payment_method_update_expected_initial_transaction_id
        ) VALUES (
            $1, $2, $3, $4, $5, $6,
            'subscription_payment_method_update', 'pending', $7, $8, 0, $9,
            $10, $11, $12, $13, $14, $15, $16
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
    .bind(request.amount().currency().as_str())
    .bind(identity.gateway_account_id().as_uuid())
    .bind(identity.gateway_configuration_id().as_uuid())
    .bind(request.gateway_order_id().expose())
    .bind(request.billing_contact().name())
    .bind(request.billing_contact().email())
    .bind(expected.payment_method_id().as_uuid())
    .bind(expected.expected_initial_transaction_id().expose())
    .execute(&mut **transaction)
    .await?;
    Ok(result.rows_affected() == 1)
}

async fn reject_locked_payment_method_replacement(
    transaction: &mut Transaction<'_, Postgres>,
    attempt: PaymentAttempt,
    reason: SubscriptionPaymentMethodReplacementSubmissionRejection,
    message: &'static str,
) -> Result<SubscriptionPaymentMethodReplacementSubmissionOutcome, PaymentAttemptStoreError> {
    let resolution_code = match reason {
        SubscriptionPaymentMethodReplacementSubmissionRejection::BillingStateChanged => {
            PaymentResolutionCode::SubscriptionRenewalRetryStateChangedBeforeCharge
        }
        SubscriptionPaymentMethodReplacementSubmissionRejection::GatewayConfigurationChanged => {
            PaymentResolutionCode::GatewayConfigurationBeforeSubmission
        }
    };
    let attempt = reject_prepared_attempt(transaction, &attempt, resolution_code, message).await?;
    Ok(SubscriptionPaymentMethodReplacementSubmissionOutcome::Rejected { attempt, reason })
}

/// Resolves payment-method replacement idempotency before host admission.
pub async fn preflight_subscription_payment_method_replacement_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    command: &syrup_rail::ReplaceSubscriptionPaymentMethod,
) -> Result<SubscriptionPaymentMethodReplacementPreflightOutcome, PaymentAttemptStoreError> {
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
        return Ok(SubscriptionPaymentMethodReplacementPreflightOutcome::Continue);
    };
    Ok(
        if payment_method_replacement_attempt_matches_command(&existing, command)
            && payment_method_replacement_attempt_matches_replay_context(transaction, &existing)
                .await?
        {
            SubscriptionPaymentMethodReplacementPreflightOutcome::Replay(Box::new(existing))
        } else {
            SubscriptionPaymentMethodReplacementPreflightOutcome::IdempotencyConflict
        },
    )
}

/// Locks the exact subscription baseline and reserves a token-free replacement
/// attempt before Customer Vault I/O.
pub async fn reserve_subscription_payment_method_replacement_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    command: &syrup_rail::ReplaceSubscriptionPaymentMethod,
    gateway: &syrup_rail::ResolvedGateway,
) -> Result<SubscriptionPaymentMethodReplacementReservationOutcome, PaymentAttemptStoreError> {
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
        return Ok(
            if payment_method_replacement_attempt_matches_command(&existing, command)
                && payment_method_replacement_attempt_matches_replay_context(transaction, &existing)
                    .await?
            {
                SubscriptionPaymentMethodReplacementReservationOutcome::Replay(Box::new(existing))
            } else {
                SubscriptionPaymentMethodReplacementReservationOutcome::IdempotencyConflict
            },
        );
    }

    let row = sqlx::query(
        r#"
        SELECT id, gateway_account_id, payment_method_id, initial_transaction_id,
            status, currency
        FROM billing_subscriptions
        WHERE billing_scope_id = $1 AND subscriber_id = $2 AND plan_key = $3
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
        return Ok(
            SubscriptionPaymentMethodReplacementReservationOutcome::Rejected(
                SubscriptionPaymentMethodReplacementRejection::SubscriptionNotFound,
            ),
        );
    };
    let status: String = row.try_get("status")?;
    if !matches!(status.as_str(), "active" | "past_due") {
        return Ok(
            SubscriptionPaymentMethodReplacementReservationOutcome::Rejected(
                SubscriptionPaymentMethodReplacementRejection::SubscriptionIneligible,
            ),
        );
    }
    let subscription_id = SubscriptionId::new(row.try_get("id")?);
    fail_stale_unsubmitted_payment_method_updates(transaction, subscription_id).await?;
    if blocking_subscription_charge_attempt_exists_for_method_replacement(
        transaction,
        subscription_id,
    )
    .await?
    {
        return Ok(
            SubscriptionPaymentMethodReplacementReservationOutcome::Rejected(
                SubscriptionPaymentMethodReplacementRejection::ChargeAttemptInProgress,
            ),
        );
    }
    if blocking_payment_method_update_exists(transaction, subscription_id).await? {
        return Ok(
            SubscriptionPaymentMethodReplacementReservationOutcome::Rejected(
                SubscriptionPaymentMethodReplacementRejection::PaymentMethodUpdateInProgress,
            ),
        );
    }
    let gateway_account_id = GatewayAccountId::new(row.try_get("gateway_account_id")?);
    let expected_gateway = ExpectedGatewayIdentity::for_gateway(
        command.billing_scope_id(),
        command.gateway_configuration_id(),
        gateway,
    );
    if gateway_account_id != expected_gateway.gateway_account_id()
        || !gateway_identity_matches_account(transaction, &expected_gateway).await?
    {
        return Ok(
            SubscriptionPaymentMethodReplacementReservationOutcome::Rejected(
                SubscriptionPaymentMethodReplacementRejection::GatewayConfigurationChanged,
            ),
        );
    }
    let terms = locked_payment_method_replacement_terms_from_row(
        &row,
        subscription_id,
        gateway_account_id,
    )?;
    let reservation = SubscriptionPaymentMethodReplacement::from_locked_subscription_terms(
        command, gateway, terms,
    )
    .map_err(|_| invalid_state())?;
    let inserted = insert_payment_method_replacement_attempt(transaction, &reservation).await?;
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
        return Ok(
            SubscriptionPaymentMethodReplacementReservationOutcome::Reserved(
                Box::new(reservation),
                Box::new(attempt),
            ),
        );
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
        return Ok(
            if payment_method_replacement_attempt_matches_command(&existing, command)
                && payment_method_replacement_attempt_matches_replay_context(transaction, &existing)
                    .await?
            {
                SubscriptionPaymentMethodReplacementReservationOutcome::Replay(Box::new(existing))
            } else {
                SubscriptionPaymentMethodReplacementReservationOutcome::IdempotencyConflict
            },
        );
    }
    Ok(
        SubscriptionPaymentMethodReplacementReservationOutcome::Rejected(
            SubscriptionPaymentMethodReplacementRejection::PaymentMethodUpdateInProgress,
        ),
    )
}

/// Revalidates the exact subscription baseline and commits one-shot Customer
/// Vault admission. Semantic drift terminalizes the prepared attempt.
pub async fn admit_subscription_payment_method_replacement_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    reservation: &SubscriptionPaymentMethodReplacement,
) -> Result<SubscriptionPaymentMethodReplacementSubmissionOutcome, PaymentAttemptStoreError> {
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
    let attempt = payment_attempt_by_idempotency(
        transaction,
        identity.billing_scope_id(),
        identity.subscriber_id(),
        reservation.request().idempotency_key(),
        true,
    )
    .await?
    .ok_or_else(invalid_state)?;
    if !payment_method_replacement_attempt_belongs_to_reservation(&attempt, reservation) {
        return Err(invalid_state());
    }
    if attempt.status() != PaymentAttemptStatus::Pending
        || attempt.state().timestamps().submitted_at().is_some()
    {
        return Ok(SubscriptionPaymentMethodReplacementSubmissionOutcome::AlreadyAdmitted(attempt));
    }
    let state_matches = attempt.request() == reservation.request()
        && payment_method_replacement_subscription_state_matches(transaction, reservation).await?
        && !blocking_subscription_charge_attempt_exists_for_method_replacement(
            transaction,
            reservation.subscription_id(),
        )
        .await?
        && !blocking_payment_method_update_exists_except(
            transaction,
            reservation.subscription_id(),
            identity.attempt_id(),
        )
        .await?;
    if !state_matches {
        return reject_locked_payment_method_replacement(
            transaction,
            attempt,
            SubscriptionPaymentMethodReplacementSubmissionRejection::BillingStateChanged,
            PAYMENT_METHOD_REPLACEMENT_STATE_CHANGED_TEXT,
        )
        .await;
    }
    let expected_gateway =
        ExpectedGatewayIdentity::from_reservation(identity, reservation.provider_key());
    if !gateway_identity_matches_account(transaction, &expected_gateway).await? {
        return reject_locked_payment_method_replacement(
            transaction,
            attempt,
            SubscriptionPaymentMethodReplacementSubmissionRejection::GatewayConfigurationChanged,
            PAYMENT_METHOD_REPLACEMENT_CONFIGURATION_CHANGED_TEXT,
        )
        .await;
    }
    let admitted = admit_prepared_attempt(transaction, &attempt).await?;
    Ok(SubscriptionPaymentMethodReplacementSubmissionOutcome::Admitted(admitted))
}
