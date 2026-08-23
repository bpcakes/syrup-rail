use super::*;

pub(super) async fn initial_attempt_is_stale(
    transaction: &mut Transaction<'_, Postgres>,
    attempt_id: PaymentAttemptId,
) -> Result<bool, sqlx::Error> {
    let policy = LocalAttemptPolicy::for_kind(PaymentAttemptKind::SubscriptionInitial);
    sqlx::query_scalar(
        r#"
        SELECT attempt_kind = 'subscription_initial'
            AND status = ANY($2::text[])
            AND submitted_at IS NULL
            AND created_at <= clock_timestamp()
                - ($3::bigint * interval '1 second')
        FROM billing_payment_attempts
        WHERE id = $1
        "#,
    )
    .bind(attempt_id.as_uuid())
    .bind(LocalAttemptPolicy::expirable_status_values())
    .bind(policy.stale_after_seconds())
    .fetch_one(&mut **transaction)
    .await
}

pub(super) async fn current_subscription_exists(
    transaction: &mut Transaction<'_, Postgres>,
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    plan_key: &PlanKey,
) -> Result<bool, sqlx::Error> {
    let rows = sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT id FROM billing_subscriptions
        WHERE billing_scope_id = $1 AND subscriber_id = $2 AND plan_key = $3
            AND (
                status IN ('active', 'past_due')
                OR (status = 'canceled' AND current_period_end_at > clock_timestamp())
            )
        ORDER BY updated_at DESC, id DESC FOR NO KEY UPDATE
        "#,
    )
    .bind(billing_scope_id.as_uuid())
    .bind(subscriber_id.as_uuid())
    .bind(plan_key.as_str())
    .fetch_all(&mut **transaction)
    .await?;
    Ok(!rows.is_empty())
}

pub(super) async fn active_grant_exists(
    transaction: &mut Transaction<'_, Postgres>,
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    plan_key: &PlanKey,
) -> Result<bool, sqlx::Error> {
    let rows = sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT id FROM billing_subscription_grants
        WHERE billing_scope_id = $1 AND subscriber_id = $2 AND plan_key = $3
            AND revoked_at IS NULL
            AND starts_at <= clock_timestamp() AND ends_at > clock_timestamp()
        ORDER BY ends_at DESC, id DESC FOR UPDATE
        "#,
    )
    .bind(billing_scope_id.as_uuid())
    .bind(subscriber_id.as_uuid())
    .bind(plan_key.as_str())
    .fetch_all(&mut **transaction)
    .await?;
    Ok(!rows.is_empty())
}

pub(super) async fn unresolved_initial_charge_exists(
    transaction: &mut Transaction<'_, Postgres>,
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    plan_key: &PlanKey,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM billing_processor_charges charges
            INNER JOIN billing_payment_attempts attempts ON attempts.id = charges.attempt_id
            WHERE attempts.billing_scope_id = $1
                AND attempts.subscriber_id = $2
                AND attempts.plan_key = $3
                AND attempts.attempt_kind = 'subscription_initial'
                AND charges.progression_state IN (
                    'pending', 'reconciliation_required', 'external_reversal_required'
                )
        )
        "#,
    )
    .bind(billing_scope_id.as_uuid())
    .bind(subscriber_id.as_uuid())
    .bind(plan_key.as_str())
    .fetch_one(&mut **transaction)
    .await
}

pub(super) async fn blocking_initial_attempt_exists(
    transaction: &mut Transaction<'_, Postgres>,
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    plan_key: &PlanKey,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        r#"
        SELECT EXISTS (
            SELECT 1 FROM billing_payment_attempts
            WHERE billing_scope_id = $1 AND subscriber_id = $2 AND plan_key = $3
                AND attempt_kind = 'subscription_initial'
                AND (
                    status IN ('pending', 'unknown')
                    OR (
                        status = 'review_required'
                        AND resolution_code IS DISTINCT FROM
                            'subscription_initial_current_subscription_conflict'
                    )
                )
        )
        "#,
    )
    .bind(billing_scope_id.as_uuid())
    .bind(subscriber_id.as_uuid())
    .bind(plan_key.as_str())
    .fetch_one(&mut **transaction)
    .await
}

pub(super) fn enrollment_request_from_locked_terms(
    reservation: &SubscriptionEnrollmentReservation,
    offer: &syrup_rail::SubscriptionOffer,
    saved_claim: Option<&syrup_rail::SubscriptionDiscountClaimRecord>,
) -> Option<PaymentAttemptRequest> {
    let saved_snapshot = saved_claim.map(|claim| claim.snapshot());
    if !reservation
        .expected_terms()
        .matches_locked_terms(offer, saved_snapshot)
    {
        return None;
    }
    let discount = saved_claim.map(|claim| {
        SubscriptionEnrollmentDiscountSnapshot::new(
            claim.id(),
            claim.discount_code_id(),
            claim.snapshot().clone(),
        )
    });
    let amount = reservation.expected_terms().initial_charge().money();
    let durable_offer = reservation.expected_terms().durable_offer();
    Some(PaymentAttemptRequest::canonical(
        PaymentAttemptTarget::SubscriptionInitial {
            terms_version: SubscriptionEnrollmentTermsVersion::V2,
            offer: durable_offer,
            discount,
            application: None,
        },
        reservation.idempotency_key().clone(),
        amount,
        reservation.gateway_order_id().clone(),
        reservation.billing_contact().clone(),
    ))
}

pub(super) async fn gateway_identity_matches_scope(
    transaction: &mut Transaction<'_, Postgres>,
    expected: &ExpectedGatewayIdentity,
) -> Result<bool, sqlx::Error> {
    let row = sqlx::query_as::<_, (Uuid, Uuid, String)>(
        r#"
        SELECT id, gateway_configuration_id, provider_key
        FROM billing_gateway_accounts
        WHERE billing_scope_id = $1 FOR SHARE
        "#,
    )
    .bind(expected.billing_scope_id().as_uuid())
    .fetch_optional(&mut **transaction)
    .await?;
    Ok(
        row.is_some_and(|(account_id, configuration_id, provider_key)| {
            expected.matches_row(account_id, configuration_id, &provider_key)
        }),
    )
}

pub(super) fn replay_matches_reservation(
    attempt: &PaymentAttempt,
    reservation: &SubscriptionEnrollmentReservation,
) -> bool {
    let identity = attempt.identity();
    identity.billing_scope_id() == reservation.identity().billing_scope_id()
        && identity.subscriber_id() == reservation.identity().subscriber_id()
        && identity.gateway_account_id() == reservation.identity().gateway_account_id()
        && identity.gateway_configuration_id() == reservation.identity().gateway_configuration_id()
        && attempt.kind() == PaymentAttemptKind::SubscriptionInitial
        && attempt.request().billing_contact() == reservation.billing_contact()
        && initial_attempt_matches_expected(attempt, reservation.expected_terms())
}

pub(super) fn replay_matches_command(
    attempt: &PaymentAttempt,
    command: &syrup_rail::EnrollSubscription,
) -> bool {
    let identity = attempt.identity();
    identity.billing_scope_id() == command.billing_scope_id()
        && identity.subscriber_id() == command.subscriber_id()
        && identity.gateway_configuration_id() == command.gateway_configuration_id()
        && attempt.kind() == PaymentAttemptKind::SubscriptionInitial
        && attempt.request().billing_contact()
            == &BillingContactSnapshot::from_billing_contact(command.billing_contact())
        && initial_attempt_matches_expected(attempt, command.expected_terms())
}

pub(super) fn initial_attempt_matches_expected(
    attempt: &PaymentAttempt,
    expected: &syrup_rail::SubscriptionEnrollmentExpectedTerms,
) -> bool {
    let PaymentAttemptTarget::SubscriptionInitial {
        terms_version,
        offer,
        discount,
        ..
    } = attempt.request().target()
    else {
        return false;
    };
    if offer != &expected.durable_offer()
        || attempt.request().amount() != expected.initial_charge().money()
        || !match (discount.as_ref(), expected.discount_snapshot()) {
            (None, None) => true,
            (Some(durable), Some(expected)) => durable.snapshot().has_same_charge_terms(expected),
            _ => false,
        }
    {
        return false;
    }
    match terms_version {
        SubscriptionEnrollmentTermsVersion::V1 => attempt
            .request()
            .fingerprint()
            .matches_subscription_initial_expected_terms_v1(expected.plan_key(), expected),
        SubscriptionEnrollmentTermsVersion::V2 => attempt
            .request()
            .fingerprint()
            .matches_subscription_initial_v2(offer, attempt.request().amount(), discount.as_ref()),
    }
}

pub(super) fn pending_attempt_matches_request(
    attempt: &PaymentAttempt,
    requested_identity: PaymentAttemptIdentity,
    requested: &PaymentAttemptRequest,
) -> bool {
    let identity = attempt.identity();
    identity.billing_scope_id() == requested_identity.billing_scope_id()
        && identity.subscriber_id() == requested_identity.subscriber_id()
        && identity.gateway_account_id() == requested_identity.gateway_account_id()
        && identity.gateway_configuration_id() == requested_identity.gateway_configuration_id()
        && attempt.status() == PaymentAttemptStatus::Pending
        && attempt.state().timestamps().submitted_at().is_none()
        && attempt.request().target() == requested.target()
        && attempt.request().fingerprint() == requested.fingerprint()
        && attempt.request().amount() == requested.amount()
        && attempt.request().billing_contact() == requested.billing_contact()
}

pub(super) fn attempt_identity_matches_requested_gateway(
    attempt: &PaymentAttempt,
    requested: PaymentAttemptIdentity,
) -> bool {
    let identity = attempt.identity();
    identity.billing_scope_id() == requested.billing_scope_id()
        && identity.subscriber_id() == requested.subscriber_id()
        && identity.gateway_account_id() == requested.gateway_account_id()
        && identity.gateway_configuration_id() == requested.gateway_configuration_id()
}

pub(super) async fn insert_initial_attempt(
    transaction: &mut Transaction<'_, Postgres>,
    identity: PaymentAttemptIdentity,
    request: &PaymentAttemptRequest,
) -> Result<bool, PaymentAttemptStoreError> {
    let plan_key = request.target().plan_key().ok_or_else(invalid_state)?;
    let PaymentAttemptTarget::SubscriptionInitial {
        terms_version,
        offer,
        ..
    } = request.target()
    else {
        return Err(invalid_state());
    };
    let discount = request.target().enrollment_discount();
    let snapshot = discount.map(SubscriptionEnrollmentDiscountSnapshot::snapshot);
    let kind = snapshot.map(|snapshot| snapshot.kind());
    let duration = snapshot.map(|snapshot| snapshot.duration());
    let trial = offer.start().paid_trial();
    let recurring = offer.recurring();
    let retry_delays = offer
        .renewal_failure()
        .schedule()
        .retry_delays()
        .iter()
        .map(|delay| i64::from(delay.seconds().get()))
        .collect::<Vec<_>>();
    let result = sqlx::query(
        r#"
        INSERT INTO billing_payment_attempts (
            id, billing_scope_id, subscriber_id, plan_key, attempt_kind, status,
            idempotency_key, request_fingerprint, amount_cents, currency,
            gateway_account_id, gateway_configuration_id, gateway_order_id,
            subscription_initial_discount_claim_id,
            subscription_initial_discount_code_id,
            subscription_initial_discount_code_snapshot,
            subscription_initial_discount_label_snapshot,
            subscription_initial_discount_kind,
            subscription_initial_discount_amount_off_cents,
            subscription_initial_discount_percent_off_bps,
            subscription_initial_discount_currency,
            subscription_initial_discount_duration,
            subscription_initial_discount_duration_months,
            subscription_initial_discount_base_amount_cents,
            subscription_initial_discount_discounted_amount_cents,
            subscription_initial_terms_version,
            subscription_initial_start_kind,
            subscription_initial_trial_amount_cents,
            subscription_initial_trial_period_kind,
            subscription_initial_trial_period_count,
            subscription_initial_recurring_base_amount_cents,
            subscription_initial_recurring_period_kind,
            subscription_initial_recurring_period_count,
            subscription_initial_dunning_retry_delays_seconds,
            subscription_initial_dunning_exhaustion,
            subscription_initial_past_due_access,
            billing_first_name, billing_last_name, billing_email
        ) VALUES (
            $1, $2, $3, $4, 'subscription_initial', 'pending',
            $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16,
            $17, $18, $19, $20, $21, $22, $23, $24, $25, $26, $27,
            $28, $29, $30, $31, $32, $33, $34, $35, $36, $37
        )
        ON CONFLICT (billing_scope_id, subscriber_id, idempotency_key) DO NOTHING
        "#,
    )
    .bind(identity.attempt_id().as_uuid())
    .bind(identity.billing_scope_id().as_uuid())
    .bind(identity.subscriber_id().as_uuid())
    .bind(plan_key.as_str())
    .bind(request.idempotency_key().expose())
    .bind(request.fingerprint().expose())
    .bind(request.amount().cents())
    .bind(request.amount().currency().as_str())
    .bind(identity.gateway_account_id().as_uuid())
    .bind(identity.gateway_configuration_id().as_uuid())
    .bind(request.gateway_order_id().expose())
    .bind(discount.map(|discount| discount.claim_id().into_uuid()))
    .bind(discount.map(|discount| discount.code_id().into_uuid()))
    .bind(snapshot.map(|snapshot| snapshot.code().as_str()))
    .bind(snapshot.and_then(|snapshot| snapshot.label()))
    .bind(kind.map(|kind| kind.as_str()))
    .bind(kind.and_then(discount_amount_off))
    .bind(kind.and_then(discount_percent_off))
    .bind(snapshot.map(|snapshot| snapshot.currency().as_str()))
    .bind(duration.map(|duration| duration.as_str()))
    .bind(duration.and_then(discount_duration_months))
    .bind(snapshot.map(|snapshot| snapshot.base_charge().cents()))
    .bind(snapshot.map(|snapshot| snapshot.discounted_charge().cents()))
    .bind(i16::try_from(terms_version.get()).map_err(|_| invalid_state())?)
    .bind(offer.start().as_str())
    .bind(trial.map(|trial| trial.charge().cents()))
    .bind(trial.map(|trial| trial.period().as_str()))
    .bind(trial.map(|trial| i32::from(trial.period().count().get())))
    .bind(recurring.charge().cents())
    .bind(recurring.period().as_str())
    .bind(i32::from(recurring.period().count().get()))
    .bind(retry_delays)
    .bind(offer.renewal_failure().exhaustion().as_str())
    .bind(offer.renewal_failure().past_due_access().as_str())
    .bind(request.billing_contact().first_name())
    .bind(request.billing_contact().last_name())
    .bind(request.billing_contact().email())
    .execute(&mut **transaction)
    .await?;
    Ok(result.rows_affected() == 1)
}

pub(super) fn discount_amount_off(kind: SubscriptionDiscountKind) -> Option<i32> {
    match kind {
        SubscriptionDiscountKind::AmountOffCents(value) => Some(value.get()),
        SubscriptionDiscountKind::PercentOffBasisPoints(_) => None,
    }
}

pub(super) fn discount_percent_off(kind: SubscriptionDiscountKind) -> Option<i32> {
    match kind {
        SubscriptionDiscountKind::AmountOffCents(_) => None,
        SubscriptionDiscountKind::PercentOffBasisPoints(value) => Some(i32::from(value.get())),
    }
}

pub(super) fn discount_duration_months(duration: SubscriptionDiscountDuration) -> Option<i32> {
    match duration {
        SubscriptionDiscountDuration::Indefinite => None,
        SubscriptionDiscountDuration::LimitedMonths(value) => Some(i32::from(value.get())),
    }
}

pub(super) async fn reject_prepared_initial(
    transaction: &mut Transaction<'_, Postgres>,
    reservation: &SubscriptionEnrollmentReservation,
    reason: SubscriptionEnrollmentSubmissionRejection,
    message: &'static str,
) -> Result<SubscriptionEnrollmentSubmissionOutcome, PaymentAttemptStoreError> {
    let identity = reservation.identity();
    let attempt = lock_payment_attempt_by_idempotency(
        transaction,
        identity.billing_scope_id(),
        identity.subscriber_id(),
        reservation.idempotency_key(),
    )
    .await?
    .ok_or_else(invalid_state)?;
    if !attempt_identity_matches_requested_gateway(&attempt, identity)
        || attempt.kind() != PaymentAttemptKind::SubscriptionInitial
    {
        return Err(invalid_state());
    }
    if attempt.status() != PaymentAttemptStatus::Pending
        || attempt.state().timestamps().submitted_at().is_some()
    {
        return Ok(SubscriptionEnrollmentSubmissionOutcome::AlreadyAdmitted(
            attempt,
        ));
    }
    reject_locked_prepared_initial(transaction, attempt, reason, message).await
}

pub(super) async fn reject_locked_prepared_initial(
    transaction: &mut Transaction<'_, Postgres>,
    attempt: PaymentAttempt,
    reason: SubscriptionEnrollmentSubmissionRejection,
    message: &'static str,
) -> Result<SubscriptionEnrollmentSubmissionOutcome, PaymentAttemptStoreError> {
    let identity = attempt.identity();
    let result = sqlx::query(
        r#"
        UPDATE billing_payment_attempts
        SET status = 'failed', gateway_response_text = $2,
            gateway_condition = 'failed', resolved_at = clock_timestamp(),
            updated_at = clock_timestamp()
        WHERE id = $1 AND status = 'pending' AND submitted_at IS NULL
        "#,
    )
    .bind(identity.attempt_id().as_uuid())
    .bind(message)
    .execute(&mut **transaction)
    .await?;
    if result.rows_affected() != 1 {
        return Err(invalid_state());
    }
    let attempt = find_payment_attempt_by_id_in_transaction(
        transaction,
        identity.billing_scope_id(),
        identity.attempt_id(),
    )
    .await?
    .ok_or_else(invalid_state)?;
    Ok(SubscriptionEnrollmentSubmissionOutcome::Rejected { attempt, reason })
}

pub(super) fn map_discount_error(
    error: crate::SubscriptionDiscountOperationError,
) -> PaymentAttemptStoreError {
    match error {
        crate::SubscriptionDiscountOperationError::Sql(error) => {
            PaymentAttemptStoreError::Sql(error)
        }
        _ => invalid_state(),
    }
}
