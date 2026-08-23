use super::*;

fn renewal_attempt_belongs_to_reservation(
    attempt: &PaymentAttempt,
    reservation: &SubscriptionRenewalReservation,
) -> bool {
    attempt.identity() == reservation.identity()
        && attempt.kind() == PaymentAttemptKind::SubscriptionRenewal
        && attempt.request().idempotency_key() == reservation.request().idempotency_key()
        && attempt.request().gateway_order_id() == reservation.request().gateway_order_id()
}

async fn gateway_identity_matches_subscription(
    transaction: &mut Transaction<'_, Postgres>,
    subscription_id: SubscriptionId,
    expected: &ExpectedGatewayIdentity,
) -> Result<bool, sqlx::Error> {
    let row = sqlx::query_as::<_, (Uuid, Uuid, String)>(
        r#"
        SELECT accounts.id, accounts.gateway_configuration_id, accounts.provider_key
        FROM billing_subscriptions AS subscriptions
        JOIN billing_gateway_accounts AS accounts
            ON accounts.billing_scope_id = subscriptions.billing_scope_id
            AND accounts.id = subscriptions.gateway_account_id
        WHERE subscriptions.billing_scope_id = $1 AND subscriptions.id = $2
        FOR SHARE OF accounts
        "#,
    )
    .bind(expected.billing_scope_id().as_uuid())
    .bind(subscription_id.as_uuid())
    .fetch_optional(&mut **transaction)
    .await?;
    Ok(
        row.is_some_and(|(account_id, configuration_id, provider_key)| {
            expected.matches_row(account_id, configuration_id, &provider_key)
        }),
    )
}

fn locked_renewal_terms_from_row(
    row: &PgRow,
    subscription_id: SubscriptionId,
    gateway_account_id: GatewayAccountId,
    period_start_at: DateTime<Utc>,
    status_value: &str,
    attempt_sequence_count: i64,
) -> Result<SubscriptionRenewalLockedTerms, PaymentAttemptStoreError> {
    let currency =
        CurrencyCode::new(&row.try_get::<String, _>("currency")?).map_err(|_| invalid_state())?;
    let charge =
        ChargeAmount::new(row.try_get("amount_cents")?, currency).map_err(|_| invalid_state())?;
    let initial_transaction_id =
        GatewayTransactionId::new(row.try_get::<String, _>("initial_transaction_id")?)
            .map_err(|_| invalid_state())?;
    let status = status_value
        .parse::<SubscriptionStatus>()
        .map_err(|_| invalid_state())?;
    let recurring_period_kind: String = row.try_get("recurring_period_kind")?;
    let recurring_period_count: i32 = row.try_get("recurring_period_count")?;
    let recurring_period = subscription_period_rule_from_scalars(
        SubscriptionPeriodRuleScalars::new(&recurring_period_kind, recurring_period_count),
    )
    .map_err(map_subscription_persistence_error)?;
    let period = syrup_rail::next_billing_period(period_start_at, recurring_period)
        .map_err(|_| invalid_state())?;
    let payment_method_id = PaymentMethodId::new(row.try_get("payment_method_id")?);
    let expected_state = locked_subscription_payment_state(
        subscription_id,
        payment_method_id,
        initial_transaction_id,
        status,
    )?;
    Ok(SubscriptionRenewalLockedTerms::new(
        gateway_account_id,
        expected_state,
        period,
        charge,
        attempt_sequence_count,
    ))
}

async fn renewal_subscription_state_matches(
    transaction: &mut Transaction<'_, Postgres>,
    reservation: &SubscriptionRenewalReservation,
) -> Result<bool, sqlx::Error> {
    let identity = reservation.identity();
    let expected = reservation.expected_state();
    let row = sqlx::query(
        r#"
        SELECT status, payment_method_id, initial_transaction_id,
            amount_cents, currency, next_renewal_at,
            COALESCE(next_payment_attempt_at <= clock_timestamp(), false) AS is_due
        FROM billing_subscriptions
        WHERE id = $1 AND billing_scope_id = $2 AND subscriber_id = $3
            AND gateway_account_id = $4 AND plan_key = $5
        FOR NO KEY UPDATE
        "#,
    )
    .bind(reservation.subscription_id().as_uuid())
    .bind(identity.billing_scope_id().as_uuid())
    .bind(identity.subscriber_id().as_uuid())
    .bind(identity.gateway_account_id().as_uuid())
    .bind(reservation.plan_key().as_str())
    .fetch_optional(&mut **transaction)
    .await?;
    let Some(row) = row else {
        return Ok(false);
    };
    let status: String = row.try_get("status")?;
    let initial_transaction_id: String = row.try_get("initial_transaction_id")?;
    Ok(status == expected.status().as_str()
        && matches!(status.as_str(), "active" | "past_due")
        && row.try_get::<Uuid, _>("payment_method_id")? == expected.payment_method_id().into_uuid()
        && syrup_rail::canonical_gateway_transaction_ids_equal(
            &initial_transaction_id,
            expected.initial_transaction_id().expose(),
        )
        && row.try_get::<i32, _>("amount_cents")? == reservation.request().amount().cents()
        && row.try_get::<String, _>("currency")?
            == reservation.request().amount().currency().as_str()
        && row.try_get::<DateTime<Utc>, _>("next_renewal_at")? == *reservation.period().start_at()
        && row.try_get::<bool, _>("is_due")?)
}

async fn reject_locked_renewal(
    transaction: &mut Transaction<'_, Postgres>,
    attempt: PaymentAttempt,
    reason: SubscriptionRenewalSubmissionRejection,
    message: &'static str,
) -> Result<SubscriptionRenewalSubmissionOutcome, PaymentAttemptStoreError> {
    let resolution_code = match reason {
        SubscriptionRenewalSubmissionRejection::BillingStateChanged => {
            PaymentResolutionCode::SubscriptionRenewalRetryStateChangedBeforeCharge
        }
        SubscriptionRenewalSubmissionRejection::GatewayConfigurationChanged => {
            PaymentResolutionCode::GatewayConfigurationBeforeSubmission
        }
    };
    let attempt = reject_prepared_attempt(transaction, &attempt, resolution_code, message).await?;
    Ok(SubscriptionRenewalSubmissionOutcome::Rejected { attempt, reason })
}

fn map_renewal_store_error(error: crate::RenewalStoreError) -> PaymentAttemptStoreError {
    match error {
        crate::RenewalStoreError::Sql(error) => PaymentAttemptStoreError::Sql(error),
        crate::RenewalStoreError::MissingProviderCooldown => invalid_state(),
    }
}

/// Locks one exact due subscription and inserts its automatic-renewal attempt.
pub async fn reserve_subscription_renewal_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    command: syrup_rail::ChargeRenewal,
    gateway: &syrup_rail::ResolvedGateway,
) -> Result<SubscriptionRenewalReservationOutcome, PaymentAttemptStoreError> {
    set_enrollment_timeouts(transaction).await?;
    let locator = sqlx::query_as::<_, (Uuid, String)>(
        r#"
        SELECT subscriber_id, plan_key
        FROM billing_subscriptions
        WHERE billing_scope_id = $1 AND id = $2
        "#,
    )
    .bind(command.billing_scope_id().as_uuid())
    .bind(command.subscription_id().as_uuid())
    .fetch_optional(&mut **transaction)
    .await?;
    let Some((subscriber_id, plan_key)) = locator else {
        return Ok(SubscriptionRenewalReservationOutcome::Rejected(
            SubscriptionRenewalReservationRejection::SubscriptionNotFound,
        ));
    };
    let subscriber_id = SubscriberId::new(subscriber_id);
    let plan_key = PlanKey::new(plan_key).map_err(|_| invalid_state())?;
    lock_subscription_aggregate(transaction, subscriber_id, &plan_key).await?;

    let row = sqlx::query(
        r#"
        SELECT subscriber_id, plan_key, gateway_account_id, payment_method_id,
            amount_cents, currency, next_renewal_at, initial_transaction_id, status,
            recurring_period_kind, recurring_period_count,
            COALESCE(next_payment_attempt_at <= clock_timestamp(), false) AS is_due
        FROM billing_subscriptions
        WHERE billing_scope_id = $1 AND id = $2
        FOR SHARE
        "#,
    )
    .bind(command.billing_scope_id().as_uuid())
    .bind(command.subscription_id().as_uuid())
    .fetch_optional(&mut **transaction)
    .await?;
    let Some(row) = row else {
        return Ok(SubscriptionRenewalReservationOutcome::Rejected(
            SubscriptionRenewalReservationRejection::SubscriptionNotFound,
        ));
    };
    let status_value: String = row.try_get("status")?;
    let period_start_at: DateTime<Utc> = row.try_get("next_renewal_at")?;
    if !matches!(status_value.as_str(), "active" | "past_due")
        || period_start_at != *command.period_start_at()
        || !row.try_get::<bool, _>("is_due")?
    {
        return Ok(SubscriptionRenewalReservationOutcome::Rejected(
            SubscriptionRenewalReservationRejection::PaymentNotDue,
        ));
    }
    if row.try_get::<Uuid, _>("subscriber_id")? != subscriber_id.into_uuid()
        || row.try_get::<String, _>("plan_key")? != plan_key.as_str()
    {
        return Err(invalid_state());
    }

    fail_stale_unsubmitted_payment_method_updates(transaction, command.subscription_id()).await?;
    fail_stale_unsubmitted_subscription_charges(transaction, command.subscription_id()).await?;
    let gateway_account_id = GatewayAccountId::new(row.try_get("gateway_account_id")?);
    let expected_gateway = ExpectedGatewayIdentity::for_gateway(
        command.billing_scope_id(),
        gateway.gateway_configuration_id(),
        gateway,
    );
    if gateway_account_id != expected_gateway.gateway_account_id()
        || !gateway_identity_matches_subscription(
            transaction,
            command.subscription_id(),
            &expected_gateway,
        )
        .await?
    {
        return Ok(SubscriptionRenewalReservationOutcome::Rejected(
            SubscriptionRenewalReservationRejection::GatewayConfigurationChanged,
        ));
    }
    if blocking_subscription_charge_attempt_exists(transaction, command.subscription_id()).await? {
        return Ok(SubscriptionRenewalReservationOutcome::Rejected(
            SubscriptionRenewalReservationRejection::AttemptInProgress,
        ));
    }
    if blocking_payment_method_update_exists(transaction, command.subscription_id()).await? {
        return Ok(SubscriptionRenewalReservationOutcome::Rejected(
            SubscriptionRenewalReservationRejection::PaymentMethodUpdateInProgress,
        ));
    }
    let attempt_state = crate::renewal_attempt_state(
        transaction,
        command.subscription_id(),
        *command.period_start_at(),
        None,
    )
    .await
    .map_err(map_renewal_store_error)?;
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut **transaction)
        .await?;
    if attempt_state.blocks_automatic_retry(now) {
        return Ok(SubscriptionRenewalReservationOutcome::Rejected(
            SubscriptionRenewalReservationRejection::RetryBlocked,
        ));
    }

    let terms = locked_renewal_terms_from_row(
        &row,
        command.subscription_id(),
        gateway_account_id,
        period_start_at,
        &status_value,
        attempt_state.attempt_sequence_count,
    )?;
    let reservation = SubscriptionRenewalReservation::from_locked_subscription_terms(
        command,
        gateway,
        PaymentAttemptId::new(Uuid::now_v7()),
        subscriber_id,
        plan_key,
        terms,
    )
    .map_err(|_| invalid_state())?;
    if !insert_subscription_charge_attempt(
        transaction,
        reservation.identity(),
        reservation.request(),
    )
    .await?
    {
        return Ok(SubscriptionRenewalReservationOutcome::Rejected(
            SubscriptionRenewalReservationRejection::AttemptInProgress,
        ));
    }
    let attempt = find_payment_attempt_by_id_in_transaction(
        transaction,
        reservation.identity().billing_scope_id(),
        reservation.identity().attempt_id(),
    )
    .await?
    .ok_or_else(invalid_state)?;
    Ok(SubscriptionRenewalReservationOutcome::Reserved(
        Box::new(reservation),
        Box::new(attempt),
    ))
}

/// Revalidates one exact renewal snapshot and commits one-shot submission admission.
pub async fn admit_subscription_renewal_submission_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    reservation: &SubscriptionRenewalReservation,
) -> Result<SubscriptionRenewalSubmissionOutcome, PaymentAttemptStoreError> {
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
    let attempt = find_payment_attempt_by_id_in_transaction(
        transaction,
        identity.billing_scope_id(),
        identity.attempt_id(),
    )
    .await?
    .ok_or_else(invalid_state)?;
    if !renewal_attempt_belongs_to_reservation(&attempt, reservation) {
        return Err(invalid_state());
    }
    if attempt.status() != PaymentAttemptStatus::Pending
        || attempt.state().timestamps().submitted_at().is_some()
    {
        return Ok(SubscriptionRenewalSubmissionOutcome::AlreadyAdmitted(
            attempt,
        ));
    }
    let retry_state = crate::renewal_attempt_state(
        transaction,
        reservation.subscription_id(),
        *reservation.period().start_at(),
        Some(identity.attempt_id()),
    )
    .await
    .map_err(map_renewal_store_error)?;
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut **transaction)
        .await?;
    let state_matches = attempt.request() == reservation.request()
        && renewal_subscription_state_matches(transaction, reservation).await?
        && !retry_state.blocks_automatic_retry(now)
        && !blocking_subscription_charge_attempt_exists_except(
            transaction,
            reservation.subscription_id(),
            identity.attempt_id(),
        )
        .await?
        && !blocking_payment_method_update_exists(transaction, reservation.subscription_id())
            .await?;
    if !state_matches {
        return reject_locked_renewal(
            transaction,
            attempt,
            SubscriptionRenewalSubmissionRejection::BillingStateChanged,
            RENEWAL_STATE_CHANGED_TEXT,
        )
        .await;
    }
    let expected_gateway =
        ExpectedGatewayIdentity::from_reservation(identity, reservation.provider_key());
    if !gateway_identity_matches_account(transaction, &expected_gateway).await? {
        return reject_locked_renewal(
            transaction,
            attempt,
            SubscriptionRenewalSubmissionRejection::GatewayConfigurationChanged,
            RENEWAL_CONFIGURATION_CHANGED_TEXT,
        )
        .await;
    }
    let admitted = admit_prepared_attempt(transaction, &attempt).await?;
    Ok(SubscriptionRenewalSubmissionOutcome::Admitted(admitted))
}
