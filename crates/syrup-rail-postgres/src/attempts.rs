use std::fmt;

use chrono::{DateTime, Utc};
use sqlx::{PgConnection, Postgres, Row, Transaction, postgres::PgRow};
use syrup_rail::{
    BillingContactSnapshot, BillingPeriod, BillingScopeId, ChargeAmount, CumulativeRefundCents,
    CurrencyCode, DiscountClaimId, DiscountCodeId, GatewayAccountId, GatewayConfigurationId,
    GatewayDiagnostic, GatewayLifecycleState, GatewayOrderId, GatewayPaymentDescriptor,
    GatewayPaymentMethodReference, GatewayTransactionId, HostChargeTargetId, IdempotencyKey,
    LimitedDiscountMonths, Money, PaymentAttempt, PaymentAttemptFingerprint, PaymentAttemptId,
    PaymentAttemptIdentity, PaymentAttemptKind, PaymentAttemptLifecycle, PaymentAttemptRequest,
    PaymentAttemptState, PaymentAttemptStatus, PaymentAttemptTarget, PaymentAttemptTimestamps,
    PaymentMethodId, PaymentMethodUpdateSnapshot, PaymentResolutionCode, PercentOffBasisPoints,
    PlanKey, PositiveDiscountCents, ProcessorEvidence, SubscriberId, SubscriptionDiscountCode,
    SubscriptionDiscountDuration, SubscriptionDiscountKind, SubscriptionDiscountSnapshot,
    SubscriptionEnrollmentDiscountSnapshot, SubscriptionEnrollmentPreflightOutcome,
    SubscriptionEnrollmentReservation, SubscriptionEnrollmentReservationOutcome,
    SubscriptionEnrollmentReservationRejection, SubscriptionEnrollmentSubmissionOutcome,
    SubscriptionEnrollmentSubmissionRejection, SubscriptionId, SubscriptionInitialApplication,
    SubscriptionPaymentMethodReplacement, SubscriptionPaymentMethodReplacementPreflightOutcome,
    SubscriptionPaymentMethodReplacementRejection,
    SubscriptionPaymentMethodReplacementReservationOutcome,
    SubscriptionPaymentMethodReplacementSubmissionOutcome,
    SubscriptionPaymentMethodReplacementSubmissionRejection, SubscriptionPaymentStateSnapshot,
    SubscriptionRecoveryPreflightOutcome, SubscriptionRecoveryReservation,
    SubscriptionRecoveryReservationOutcome, SubscriptionRecoveryReservationRejection,
    SubscriptionRecoverySubmissionOutcome, SubscriptionRecoverySubmissionRejection,
    SubscriptionStatus,
};
use thiserror::Error;
use uuid::Uuid;

const INVALID_ATTEMPT_STATE: &str = "canonical payment attempt state is invalid";
const BILLING_ROW_LOCK_TIMEOUT: &str = "250ms";
const BILLING_OPERATION_TIMEOUT: &str = "5s";
const INITIAL_PREPARED_STALE_AFTER_SECONDS: i64 = 30 * 60;
const INITIAL_PREPARED_EXPIRED_TEXT: &str =
    "Prepared checkout expired before processor submission.";
const INITIAL_BILLING_STATE_CHANGED_TEXT: &str =
    "Checkout was canceled before submission because billing state changed.";
const INITIAL_TERMS_CHANGED_TEXT: &str =
    "Checkout was canceled before submission because enrollment terms changed.";
const INITIAL_CONFIGURATION_CHANGED_TEXT: &str =
    "Checkout was canceled before submission because payment configuration changed.";
const RECOVERY_STATE_CHANGED_TEXT: &str =
    "Subscription recovery was canceled before submission because billing state changed.";
const RECOVERY_CONFIGURATION_CHANGED_TEXT: &str =
    "Subscription recovery was canceled before submission because payment configuration changed.";
const PAYMENT_METHOD_REPLACEMENT_STATE_CHANGED_TEXT: &str =
    "Payment method replacement was canceled before submission because billing state changed.";
const PAYMENT_METHOD_REPLACEMENT_CONFIGURATION_CHANGED_TEXT: &str = "Payment method replacement was canceled before submission because payment configuration changed.";
const PAYMENT_METHOD_UPDATE_UNSUBMITTED_STALE_AFTER_SECONDS: i64 = 3 * 60;

const PAYMENT_ATTEMPT_SELECT: &str = r#"
    SELECT id, billing_scope_id, subscriber_id, plan_key,
        host_charge_target_id, subscription_id, payment_method_id,
        attempt_kind, status, idempotency_key, request_fingerprint,
        amount_cents, currency, billing_period_start_at,
        billing_period_end_at, gateway_account_id,
        gateway_configuration_id, gateway_order_id,
        gateway_transaction_id, gateway_payment_method_reference,
        gateway_response, gateway_response_code, gateway_response_text,
        gateway_condition, payment_type, card_brand, card_last4,
        card_exp_month, card_exp_year, submitted_at, resolved_at,
        created_at, updated_at, gateway_lifecycle_status,
        gateway_lifecycle_action, gateway_lifecycle_at,
        gateway_lifecycle_reconciled_at, refunded_amount_cents,
        billing_name, billing_email, resolution_code, review_required_at,
        payment_method_update_expected_payment_method_id,
        payment_method_update_expected_initial_transaction_id,
        subscription_expected_payment_method_id,
        subscription_expected_initial_transaction_id,
        subscription_expected_status,
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
        subscription_initial_discount_discounted_amount_cents
    FROM billing_payment_attempts
"#;

#[derive(Error)]
pub enum PaymentAttemptStoreError {
    #[error("payment attempt storage operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("{0}")]
    InvalidState(&'static str),
}

impl fmt::Debug for PaymentAttemptStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sql(_) => formatter.write_str("PaymentAttemptStoreError::Sql"),
            Self::InvalidState(detail) => formatter
                .debug_tuple("PaymentAttemptStoreError::InvalidState")
                .field(detail)
                .finish(),
        }
    }
}

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
        if attempt_is_already_replayable(&existing) {
            return Ok(SubscriptionEnrollmentReservationOutcome::Replay(existing));
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
    }

    let Some(offer) = offers
        .lock_current_offer(
            transaction,
            identity.billing_scope_id(),
            reservation.plan_key(),
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
        && attempt_is_already_replayable(&existing)
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
    if !gateway_identity_matches(transaction, reservation).await? {
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
    if attempt_is_already_replayable(&existing) {
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
    if attempt_is_already_replayable(&existing) {
        return Ok(SubscriptionEnrollmentPreflightOutcome::Replay(Box::new(
            existing,
        )));
    }
    if !initial_attempt_is_stale(transaction, existing.identity().attempt_id()).await? {
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
        .lock_current_offer(
            transaction,
            identity.billing_scope_id(),
            reservation.plan_key(),
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
    if !gateway_identity_matches(transaction, reservation).await? {
        return reject_locked_prepared_initial(
            transaction,
            attempt,
            SubscriptionEnrollmentSubmissionRejection::GatewayConfigurationChanged,
            INITIAL_CONFIGURATION_CHANGED_TEXT,
        )
        .await;
    }

    let attempt_id = attempt.identity().attempt_id();
    sqlx::query(
        r#"
        UPDATE billing_payment_attempts
        SET submitted_at = clock_timestamp(), updated_at = clock_timestamp()
        WHERE id = $1 AND status = 'pending' AND submitted_at IS NULL
        "#,
    )
    .bind(attempt_id.as_uuid())
    .execute(&mut **transaction)
    .await?;
    let admitted = find_payment_attempt_by_id_in_transaction(
        transaction,
        identity.billing_scope_id(),
        attempt_id,
    )
    .await?
    .ok_or_else(invalid_state)?;
    Ok(SubscriptionEnrollmentSubmissionOutcome::Admitted(admitted))
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
    let Some(existing) = payment_attempt_by_idempotency(
        transaction,
        command.billing_scope_id(),
        command.subscriber_id(),
        command.idempotency_key(),
        false,
    )
    .await?
    else {
        return Ok(SubscriptionRecoveryPreflightOutcome::Continue);
    };
    Ok(
        if recovery_attempt_matches_command(&existing, command)
            && recovery_attempt_matches_replay_context(transaction, &existing).await?
        {
            SubscriptionRecoveryPreflightOutcome::Replay(Box::new(existing))
        } else {
            SubscriptionRecoveryPreflightOutcome::IdempotencyConflict
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
        return Ok(
            if recovery_attempt_matches_command(&existing, command)
                && recovery_attempt_matches_replay_context(transaction, &existing).await?
            {
                SubscriptionRecoveryReservationOutcome::Replay(Box::new(existing))
            } else {
                SubscriptionRecoveryReservationOutcome::IdempotencyConflict
            },
        );
    }

    let row = sqlx::query(
        r#"
        SELECT id, gateway_account_id, payment_method_id, amount_cents, currency,
            next_renewal_at, initial_transaction_id, status,
            next_renewal_at <= clock_timestamp() AS is_due
        FROM billing_subscriptions
        WHERE billing_scope_id = $1 AND subscriber_id = $2 AND plan_key = $3
            AND status IN ('active', 'past_due')
        ORDER BY CASE status WHEN 'active' THEN 0 ELSE 1 END, updated_at DESC, id DESC
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
    let gateway_account_id = GatewayAccountId::new(row.try_get("gateway_account_id")?);
    if gateway_account_id != gateway.gateway_account_id()
        || !gateway_identity_matches_recovery(transaction, command, gateway).await?
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

    let payment_method_id = PaymentMethodId::new(row.try_get("payment_method_id")?);
    let period_start_at: DateTime<Utc> = row.try_get("next_renewal_at")?;
    let period =
        syrup_rail::next_monthly_billing_period(period_start_at).map_err(|_| invalid_state())?;
    let currency_value: String = row.try_get("currency")?;
    let currency = CurrencyCode::new(&currency_value).map_err(|_| invalid_state())?;
    let charge =
        ChargeAmount::new(row.try_get("amount_cents")?, currency).map_err(|_| invalid_state())?;
    let transaction_value: String = row.try_get("initial_transaction_id")?;
    let initial_transaction_id =
        GatewayTransactionId::new(transaction_value).map_err(|_| invalid_state())?;
    let status = row
        .try_get::<String, _>("status")?
        .parse::<SubscriptionStatus>()
        .map_err(|_| invalid_state())?;
    let reservation = SubscriptionRecoveryReservation::from_locked_subscription(
        command,
        gateway,
        command.attempt_id(),
        subscription_id,
        payment_method_id,
        initial_transaction_id,
        status,
        period,
        charge,
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
        return Ok(
            if recovery_attempt_matches_command(&existing, command)
                && recovery_attempt_matches_replay_context(transaction, &existing).await?
            {
                SubscriptionRecoveryReservationOutcome::Replay(Box::new(existing))
            } else {
                SubscriptionRecoveryReservationOutcome::IdempotencyConflict
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
    if !gateway_identity_matches_reservation(transaction, reservation).await? {
        return reject_locked_recovery(
            transaction,
            attempt,
            SubscriptionRecoverySubmissionRejection::GatewayConfigurationChanged,
            RECOVERY_CONFIGURATION_CHANGED_TEXT,
        )
        .await;
    }

    sqlx::query(
        "UPDATE billing_payment_attempts SET submitted_at = clock_timestamp(), updated_at = clock_timestamp() WHERE id = $1 AND status = 'pending' AND submitted_at IS NULL",
    )
    .bind(identity.attempt_id().as_uuid())
    .execute(&mut **transaction)
    .await?;
    let admitted = find_payment_attempt_by_id_in_transaction(
        transaction,
        identity.billing_scope_id(),
        identity.attempt_id(),
    )
    .await?
    .ok_or_else(invalid_state)?;
    Ok(SubscriptionRecoverySubmissionOutcome::Admitted(admitted))
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
    if blocking_subscription_charge_attempt_exists(transaction, subscription_id).await? {
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
    if gateway_account_id != gateway.gateway_account_id()
        || !gateway_identity_matches_payment_method_replacement(transaction, command, gateway)
            .await?
    {
        return Ok(
            SubscriptionPaymentMethodReplacementReservationOutcome::Rejected(
                SubscriptionPaymentMethodReplacementRejection::GatewayConfigurationChanged,
            ),
        );
    }
    let payment_method_id = PaymentMethodId::new(row.try_get("payment_method_id")?);
    let initial_transaction_id =
        GatewayTransactionId::new(row.try_get::<String, _>("initial_transaction_id")?)
            .map_err(|_| invalid_state())?;
    let currency =
        CurrencyCode::new(&row.try_get::<String, _>("currency")?).map_err(|_| invalid_state())?;
    let reservation = SubscriptionPaymentMethodReplacement::from_locked_subscription(
        command,
        gateway,
        subscription_id,
        payment_method_id,
        initial_transaction_id,
        currency,
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
        && !blocking_subscription_charge_attempt_exists(transaction, reservation.subscription_id())
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
    if !gateway_identity_matches_payment_method_replacement_reservation(transaction, reservation)
        .await?
    {
        return reject_locked_payment_method_replacement(
            transaction,
            attempt,
            SubscriptionPaymentMethodReplacementSubmissionRejection::GatewayConfigurationChanged,
            PAYMENT_METHOD_REPLACEMENT_CONFIGURATION_CHANGED_TEXT,
        )
        .await;
    }
    sqlx::query(
        "UPDATE billing_payment_attempts SET submitted_at = clock_timestamp(), updated_at = clock_timestamp() WHERE id = $1 AND status = 'pending' AND submitted_at IS NULL",
    )
    .bind(identity.attempt_id().as_uuid())
    .execute(&mut **transaction)
    .await?;
    let admitted = find_payment_attempt_by_id_in_transaction(
        transaction,
        identity.billing_scope_id(),
        identity.attempt_id(),
    )
    .await?
    .ok_or_else(invalid_state)?;
    Ok(SubscriptionPaymentMethodReplacementSubmissionOutcome::Admitted(admitted))
}

/// Loads an attempt by its exact scope and durable identity without locking it.
pub async fn find_payment_attempt_by_id_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    billing_scope_id: BillingScopeId,
    attempt_id: PaymentAttemptId,
) -> Result<Option<PaymentAttempt>, PaymentAttemptStoreError> {
    let query = format!("{PAYMENT_ATTEMPT_SELECT} WHERE billing_scope_id = $1 AND id = $2");
    let row = sqlx::query(&query)
        .bind(billing_scope_id.as_uuid())
        .bind(attempt_id.as_uuid())
        .fetch_optional(&mut **transaction)
        .await?;
    row.as_ref().map(payment_attempt_from_row).transpose()
}

/// Locks the exact owner-scoped idempotency row for replay or mutation.
pub async fn lock_payment_attempt_by_idempotency_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    idempotency_key: &IdempotencyKey,
) -> Result<Option<PaymentAttempt>, PaymentAttemptStoreError> {
    let query = format!(
        "{PAYMENT_ATTEMPT_SELECT} \
         WHERE billing_scope_id = $1 AND subscriber_id = $2 AND idempotency_key = $3 \
         FOR UPDATE"
    );
    let row = sqlx::query(&query)
        .bind(billing_scope_id.as_uuid())
        .bind(subscriber_id.as_uuid())
        .bind(idempotency_key.expose())
        .fetch_optional(&mut **transaction)
        .await?;
    row.as_ref().map(payment_attempt_from_row).transpose()
}

pub(crate) async fn lock_payment_attempt_by_id_on_connection(
    connection: &mut PgConnection,
    billing_scope_id: BillingScopeId,
    attempt_id: PaymentAttemptId,
) -> Result<Option<PaymentAttempt>, PaymentAttemptStoreError> {
    let query =
        format!("{PAYMENT_ATTEMPT_SELECT} WHERE billing_scope_id = $1 AND id = $2 FOR UPDATE");
    let row = sqlx::query(&query)
        .bind(billing_scope_id.as_uuid())
        .bind(attempt_id.as_uuid())
        .fetch_optional(&mut *connection)
        .await?;
    row.as_ref().map(payment_attempt_from_row).transpose()
}

pub(crate) async fn find_payment_attempt_by_id_on_connection(
    connection: &mut PgConnection,
    billing_scope_id: BillingScopeId,
    attempt_id: PaymentAttemptId,
) -> Result<Option<PaymentAttempt>, PaymentAttemptStoreError> {
    let query = format!("{PAYMENT_ATTEMPT_SELECT} WHERE billing_scope_id = $1 AND id = $2");
    let row = sqlx::query(&query)
        .bind(billing_scope_id.as_uuid())
        .bind(attempt_id.as_uuid())
        .fetch_optional(&mut *connection)
        .await?;
    row.as_ref().map(payment_attempt_from_row).transpose()
}

async fn set_enrollment_timeouts(
    transaction: &mut Transaction<'_, Postgres>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "SELECT set_config('lock_timeout', $1, true), set_config('statement_timeout', $2, true)",
    )
    .bind(BILLING_ROW_LOCK_TIMEOUT)
    .bind(BILLING_OPERATION_TIMEOUT)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn lock_subscription_aggregate(
    transaction: &mut Transaction<'_, Postgres>,
    subscriber_id: SubscriberId,
    plan_key: &PlanKey,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::uuid::text || ':' || $2, 0))")
        .bind(subscriber_id.as_uuid())
        .bind(plan_key.as_str())
        .execute(&mut **transaction)
        .await?;
    Ok(())
}

async fn payment_attempt_by_idempotency(
    transaction: &mut Transaction<'_, Postgres>,
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    idempotency_key: &IdempotencyKey,
    for_update: bool,
) -> Result<Option<PaymentAttempt>, PaymentAttemptStoreError> {
    let lock = if for_update { "FOR UPDATE" } else { "" };
    let query = format!(
        "{PAYMENT_ATTEMPT_SELECT} \
         WHERE billing_scope_id = $1 AND subscriber_id = $2 AND idempotency_key = $3 \
         {lock}"
    );
    let row = sqlx::query(&query)
        .bind(billing_scope_id.as_uuid())
        .bind(subscriber_id.as_uuid())
        .bind(idempotency_key.expose())
        .fetch_optional(&mut **transaction)
        .await?;
    row.as_ref().map(payment_attempt_from_row).transpose()
}

async fn lock_initial_attempt_rows(
    transaction: &mut Transaction<'_, Postgres>,
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    plan_key: &PlanKey,
) -> Result<(), sqlx::Error> {
    sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT id FROM billing_payment_attempts
        WHERE billing_scope_id = $1 AND subscriber_id = $2 AND plan_key = $3
            AND attempt_kind = 'subscription_initial'
        ORDER BY created_at, id FOR UPDATE
        "#,
    )
    .bind(billing_scope_id.as_uuid())
    .bind(subscriber_id.as_uuid())
    .bind(plan_key.as_str())
    .fetch_all(&mut **transaction)
    .await?;
    Ok(())
}

async fn lock_initial_charge_rows(
    transaction: &mut Transaction<'_, Postgres>,
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    plan_key: &PlanKey,
) -> Result<(), sqlx::Error> {
    sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT charges.id
        FROM billing_processor_charges charges
        INNER JOIN billing_payment_attempts attempts ON attempts.id = charges.attempt_id
        WHERE attempts.billing_scope_id = $1
            AND attempts.subscriber_id = $2
            AND attempts.plan_key = $3
            AND attempts.attempt_kind = 'subscription_initial'
        ORDER BY charges.observed_at, charges.id
        FOR UPDATE OF charges
        "#,
    )
    .bind(billing_scope_id.as_uuid())
    .bind(subscriber_id.as_uuid())
    .bind(plan_key.as_str())
    .fetch_all(&mut **transaction)
    .await?;
    Ok(())
}

async fn expire_stale_initial_attempts(
    transaction: &mut Transaction<'_, Postgres>,
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    plan_key: &PlanKey,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        r#"
        UPDATE billing_payment_attempts
        SET status = 'failed',
            gateway_response_text = COALESCE(gateway_response_text, $4),
            gateway_condition = COALESCE(gateway_condition, 'failed'),
            resolution_code = COALESCE(
                resolution_code,
                'subscription_initial_prepared_attempt_expired'
            ),
            resolved_at = clock_timestamp(), updated_at = clock_timestamp()
        WHERE billing_scope_id = $1 AND subscriber_id = $2 AND plan_key = $3
            AND attempt_kind = 'subscription_initial' AND status = 'pending'
            AND submitted_at IS NULL
            AND created_at <= clock_timestamp()
                - ($5::bigint * interval '1 second')
        "#,
    )
    .bind(billing_scope_id.as_uuid())
    .bind(subscriber_id.as_uuid())
    .bind(plan_key.as_str())
    .bind(INITIAL_PREPARED_EXPIRED_TEXT)
    .bind(INITIAL_PREPARED_STALE_AFTER_SECONDS)
    .execute(&mut **transaction)
    .await?;
    Ok(result.rows_affected())
}

async fn initial_attempt_is_stale(
    transaction: &mut Transaction<'_, Postgres>,
    attempt_id: PaymentAttemptId,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        r#"
        SELECT attempt_kind = 'subscription_initial'
            AND status = 'pending'
            AND submitted_at IS NULL
            AND created_at <= clock_timestamp()
                - ($2::bigint * interval '1 second')
        FROM billing_payment_attempts
        WHERE id = $1
        "#,
    )
    .bind(attempt_id.as_uuid())
    .bind(INITIAL_PREPARED_STALE_AFTER_SECONDS)
    .fetch_one(&mut **transaction)
    .await
}

async fn current_subscription_exists(
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

async fn active_grant_exists(
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

async fn unresolved_initial_charge_exists(
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

async fn blocking_initial_attempt_exists(
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

fn enrollment_request_from_locked_terms(
    reservation: &SubscriptionEnrollmentReservation,
    offer: &syrup_rail::SubscriptionOffer,
    saved_claim: Option<&syrup_rail::SubscriptionDiscountClaimRecord>,
) -> Option<PaymentAttemptRequest> {
    let saved_snapshot = saved_claim.map(|claim| claim.snapshot());
    if !reservation
        .expected_charge()
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
    let amount = reservation.expected_charge().charge().money();
    let fingerprint = PaymentAttemptFingerprint::for_subscription_initial(
        reservation.plan_key(),
        amount,
        discount.as_ref(),
    );
    Some(PaymentAttemptRequest::new(
        PaymentAttemptTarget::SubscriptionInitial {
            plan_key: reservation.plan_key().clone(),
            discount,
            application: None,
        },
        reservation.idempotency_key().clone(),
        fingerprint,
        amount,
        reservation.gateway_order_id().clone(),
        reservation.billing_contact().clone(),
    ))
}

async fn gateway_identity_matches(
    transaction: &mut Transaction<'_, Postgres>,
    reservation: &SubscriptionEnrollmentReservation,
) -> Result<bool, sqlx::Error> {
    let identity = reservation.identity();
    let row = sqlx::query_as::<_, (Uuid, Uuid, String)>(
        r#"
        SELECT id, gateway_configuration_id, provider_key
        FROM billing_gateway_accounts
        WHERE billing_scope_id = $1 FOR SHARE
        "#,
    )
    .bind(identity.billing_scope_id().as_uuid())
    .fetch_optional(&mut **transaction)
    .await?;
    Ok(
        row.is_some_and(|(account_id, configuration_id, provider_key)| {
            account_id == identity.gateway_account_id().into_uuid()
                && configuration_id == identity.gateway_configuration_id().into_uuid()
                && provider_key == reservation.provider_key().as_str()
        }),
    )
}

fn attempt_is_already_replayable(attempt: &PaymentAttempt) -> bool {
    attempt.status() != PaymentAttemptStatus::Pending
        || attempt.state().timestamps().submitted_at().is_some()
}

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
                AND status IN ('active', 'past_due')
                AND (
                    (
                        payment_method_id = $6
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

async fn gateway_identity_matches_recovery(
    transaction: &mut Transaction<'_, Postgres>,
    command: &syrup_rail::RecoverSubscriptionPayment,
    gateway: &syrup_rail::ResolvedGateway,
) -> Result<bool, sqlx::Error> {
    let row = sqlx::query_as::<_, (Uuid, Uuid, String)>(
        r#"
        SELECT id, gateway_configuration_id, provider_key
        FROM billing_gateway_accounts
        WHERE billing_scope_id = $1 AND id = $2
        FOR SHARE
        "#,
    )
    .bind(command.billing_scope_id().as_uuid())
    .bind(gateway.gateway_account_id().as_uuid())
    .fetch_optional(&mut **transaction)
    .await?;
    Ok(
        row.is_some_and(|(account_id, configuration_id, provider_key)| {
            account_id == gateway.gateway_account_id().into_uuid()
                && configuration_id == command.gateway_configuration_id().into_uuid()
                && provider_key == gateway.provider_key().as_str()
        }),
    )
}

async fn gateway_identity_matches_payment_method_replacement(
    transaction: &mut Transaction<'_, Postgres>,
    command: &syrup_rail::ReplaceSubscriptionPaymentMethod,
    gateway: &syrup_rail::ResolvedGateway,
) -> Result<bool, sqlx::Error> {
    let row = sqlx::query_as::<_, (Uuid, Uuid, String)>(
        r#"
        SELECT id, gateway_configuration_id, provider_key
        FROM billing_gateway_accounts
        WHERE billing_scope_id = $1 AND id = $2
        FOR SHARE
        "#,
    )
    .bind(command.billing_scope_id().as_uuid())
    .bind(gateway.gateway_account_id().as_uuid())
    .fetch_optional(&mut **transaction)
    .await?;
    Ok(
        row.is_some_and(|(account_id, configuration_id, provider_key)| {
            account_id == gateway.gateway_account_id().into_uuid()
                && configuration_id == command.gateway_configuration_id().into_uuid()
                && provider_key == gateway.provider_key().as_str()
        }),
    )
}

async fn gateway_identity_matches_payment_method_replacement_reservation(
    transaction: &mut Transaction<'_, Postgres>,
    reservation: &SubscriptionPaymentMethodReplacement,
) -> Result<bool, sqlx::Error> {
    let identity = reservation.identity();
    let row = sqlx::query_as::<_, (Uuid, Uuid, String)>(
        r#"
        SELECT id, gateway_configuration_id, provider_key
        FROM billing_gateway_accounts
        WHERE billing_scope_id = $1 AND id = $2
        FOR SHARE
        "#,
    )
    .bind(identity.billing_scope_id().as_uuid())
    .bind(identity.gateway_account_id().as_uuid())
    .fetch_optional(&mut **transaction)
    .await?;
    Ok(
        row.is_some_and(|(account_id, configuration_id, provider_key)| {
            account_id == identity.gateway_account_id().into_uuid()
                && configuration_id == identity.gateway_configuration_id().into_uuid()
                && provider_key == reservation.provider_key().as_str()
        }),
    )
}

async fn gateway_identity_matches_reservation(
    transaction: &mut Transaction<'_, Postgres>,
    reservation: &SubscriptionRecoveryReservation,
) -> Result<bool, sqlx::Error> {
    let identity = reservation.identity();
    let row = sqlx::query_as::<_, (Uuid, Uuid, String)>(
        r#"
        SELECT id, gateway_configuration_id, provider_key
        FROM billing_gateway_accounts
        WHERE billing_scope_id = $1 AND id = $2
        FOR SHARE
        "#,
    )
    .bind(identity.billing_scope_id().as_uuid())
    .bind(identity.gateway_account_id().as_uuid())
    .fetch_optional(&mut **transaction)
    .await?;
    Ok(
        row.is_some_and(|(account_id, configuration_id, provider_key)| {
            account_id == identity.gateway_account_id().into_uuid()
                && configuration_id == identity.gateway_configuration_id().into_uuid()
                && provider_key == reservation.provider_key().as_str()
        }),
    )
}

async fn fail_stale_unsubmitted_payment_method_updates(
    transaction: &mut Transaction<'_, Postgres>,
    subscription_id: SubscriptionId,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        UPDATE billing_payment_attempts
        SET status = 'failed',
            gateway_response_text = COALESCE(
                gateway_response_text,
                'Prepared payment method update expired before processor submission.'
            ),
            resolved_at = COALESCE(resolved_at, clock_timestamp()),
            updated_at = clock_timestamp()
        WHERE attempt_kind = 'subscription_payment_method_update'
            AND subscription_id = $1
            AND status = 'pending' AND submitted_at IS NULL
            AND created_at <= clock_timestamp()
                - ($2::bigint * interval '1 second')
        "#,
    )
    .bind(subscription_id.as_uuid())
    .bind(PAYMENT_METHOD_UPDATE_UNSUBMITTED_STALE_AFTER_SECONDS)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn blocking_subscription_charge_attempt_exists(
    transaction: &mut Transaction<'_, Postgres>,
    subscription_id: SubscriptionId,
) -> Result<bool, sqlx::Error> {
    blocking_subscription_charge_attempt_exists_except(
        transaction,
        subscription_id,
        PaymentAttemptId::new(Uuid::nil()),
    )
    .await
}

async fn blocking_subscription_charge_attempt_exists_except(
    transaction: &mut Transaction<'_, Postgres>,
    subscription_id: SubscriptionId,
    excluded_attempt_id: PaymentAttemptId,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM billing_payment_attempts AS attempts
            INNER JOIN billing_subscriptions AS subscriptions
                ON subscriptions.id = attempts.subscription_id
            WHERE attempts.subscription_id = $1
                AND attempts.id <> $2
                AND attempts.attempt_kind IN ('subscription_renewal', 'subscription_recovery')
                AND (
                    attempts.status IN ('pending', 'unknown', 'review_required')
                    OR (
                        attempts.status = 'approved'
                        AND attempts.billing_period_start_at = subscriptions.next_renewal_at
                    )
                )
        )
        "#,
    )
    .bind(subscription_id.as_uuid())
    .bind(excluded_attempt_id.as_uuid())
    .fetch_one(&mut **transaction)
    .await
}

async fn blocking_payment_method_update_exists(
    transaction: &mut Transaction<'_, Postgres>,
    subscription_id: SubscriptionId,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        r#"
        SELECT EXISTS (
            SELECT 1 FROM billing_payment_attempts
            WHERE subscription_id = $1
                AND attempt_kind = 'subscription_payment_method_update'
                AND status IN ('pending', 'unknown', 'review_required')
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
    sqlx::query(
        r#"
        UPDATE billing_payment_attempts
        SET status = 'failed', resolution_code = $2,
            gateway_response_text = $3,
            resolved_at = COALESCE(resolved_at, clock_timestamp()),
            updated_at = clock_timestamp()
        WHERE id = $1 AND status = 'pending' AND submitted_at IS NULL
        "#,
    )
    .bind(attempt.identity().attempt_id().as_uuid())
    .bind(resolution_code.as_str())
    .bind(message)
    .execute(&mut **transaction)
    .await?;
    let attempt = find_payment_attempt_by_id_in_transaction(
        transaction,
        attempt.identity().billing_scope_id(),
        attempt.identity().attempt_id(),
    )
    .await?
    .ok_or_else(invalid_state)?;
    Ok(SubscriptionRecoverySubmissionOutcome::Rejected { attempt, reason })
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
    sqlx::query(
        r#"
        UPDATE billing_payment_attempts
        SET status = 'failed', resolution_code = $2,
            gateway_response_text = $3,
            resolved_at = COALESCE(resolved_at, clock_timestamp()),
            updated_at = clock_timestamp()
        WHERE id = $1 AND status = 'pending' AND submitted_at IS NULL
        "#,
    )
    .bind(attempt.identity().attempt_id().as_uuid())
    .bind(resolution_code.as_str())
    .bind(message)
    .execute(&mut **transaction)
    .await?;
    let attempt = find_payment_attempt_by_id_in_transaction(
        transaction,
        attempt.identity().billing_scope_id(),
        attempt.identity().attempt_id(),
    )
    .await?
    .ok_or_else(invalid_state)?;
    Ok(SubscriptionPaymentMethodReplacementSubmissionOutcome::Rejected { attempt, reason })
}

fn replay_matches_reservation(
    attempt: &PaymentAttempt,
    reservation: &SubscriptionEnrollmentReservation,
) -> bool {
    let identity = attempt.identity();
    identity.billing_scope_id() == reservation.identity().billing_scope_id()
        && identity.subscriber_id() == reservation.identity().subscriber_id()
        && identity.gateway_account_id() == reservation.identity().gateway_account_id()
        && identity.gateway_configuration_id() == reservation.identity().gateway_configuration_id()
        && attempt.kind() == PaymentAttemptKind::SubscriptionInitial
        && attempt.request().target().plan_key() == Some(reservation.plan_key())
        && attempt
            .request()
            .fingerprint()
            .matches_subscription_initial_expected_charge(
                reservation.plan_key(),
                reservation.expected_charge(),
            )
}

fn replay_matches_command(
    attempt: &PaymentAttempt,
    command: &syrup_rail::EnrollSubscription,
) -> bool {
    let identity = attempt.identity();
    identity.billing_scope_id() == command.billing_scope_id()
        && identity.subscriber_id() == command.subscriber_id()
        && identity.gateway_configuration_id() == command.gateway_configuration_id()
        && attempt.kind() == PaymentAttemptKind::SubscriptionInitial
        && attempt.request().target().plan_key() == Some(command.plan_key())
        && attempt
            .request()
            .fingerprint()
            .matches_subscription_initial_expected_charge(
                command.plan_key(),
                command.expected_charge(),
            )
}

fn pending_attempt_matches_request(
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
}

fn attempt_identity_matches_requested_gateway(
    attempt: &PaymentAttempt,
    requested: PaymentAttemptIdentity,
) -> bool {
    let identity = attempt.identity();
    identity.billing_scope_id() == requested.billing_scope_id()
        && identity.subscriber_id() == requested.subscriber_id()
        && identity.gateway_account_id() == requested.gateway_account_id()
        && identity.gateway_configuration_id() == requested.gateway_configuration_id()
}

async fn insert_initial_attempt(
    transaction: &mut Transaction<'_, Postgres>,
    identity: PaymentAttemptIdentity,
    request: &PaymentAttemptRequest,
) -> Result<bool, PaymentAttemptStoreError> {
    let plan_key = request.target().plan_key().ok_or_else(invalid_state)?;
    let discount = request.target().enrollment_discount();
    let snapshot = discount.map(SubscriptionEnrollmentDiscountSnapshot::snapshot);
    let kind = snapshot.map(|snapshot| snapshot.kind());
    let duration = snapshot.map(|snapshot| snapshot.duration());
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
            billing_name, billing_email
        ) VALUES (
            $1, $2, $3, $4, 'subscription_initial', 'pending',
            $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16,
            $17, $18, $19, $20, $21, $22, $23, $24, $25
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
    .bind(request.billing_contact().name())
    .bind(request.billing_contact().email())
    .execute(&mut **transaction)
    .await?;
    Ok(result.rows_affected() == 1)
}

fn discount_amount_off(kind: SubscriptionDiscountKind) -> Option<i32> {
    match kind {
        SubscriptionDiscountKind::AmountOffCents(value) => Some(value.get()),
        SubscriptionDiscountKind::PercentOffBasisPoints(_) => None,
    }
}

fn discount_percent_off(kind: SubscriptionDiscountKind) -> Option<i32> {
    match kind {
        SubscriptionDiscountKind::AmountOffCents(_) => None,
        SubscriptionDiscountKind::PercentOffBasisPoints(value) => Some(i32::from(value.get())),
    }
}

fn discount_duration_months(duration: SubscriptionDiscountDuration) -> Option<i32> {
    match duration {
        SubscriptionDiscountDuration::Indefinite => None,
        SubscriptionDiscountDuration::LimitedMonths(value) => Some(i32::from(value.get())),
    }
}

async fn reject_prepared_initial(
    transaction: &mut Transaction<'_, Postgres>,
    reservation: &SubscriptionEnrollmentReservation,
    reason: SubscriptionEnrollmentSubmissionRejection,
    message: &'static str,
) -> Result<SubscriptionEnrollmentSubmissionOutcome, PaymentAttemptStoreError> {
    let identity = reservation.identity();
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

async fn reject_locked_prepared_initial(
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

fn map_discount_error(
    error: crate::SubscriptionDiscountOperationError,
) -> PaymentAttemptStoreError {
    match error {
        crate::SubscriptionDiscountOperationError::Sql(error) => {
            PaymentAttemptStoreError::Sql(error)
        }
        _ => invalid_state(),
    }
}

fn payment_attempt_from_row(row: &PgRow) -> Result<PaymentAttempt, PaymentAttemptStoreError> {
    let attempt_id = PaymentAttemptId::new(row.try_get("id")?);
    let identity = PaymentAttemptIdentity::new(
        attempt_id,
        BillingScopeId::new(row.try_get("billing_scope_id")?),
        SubscriberId::new(row.try_get("subscriber_id")?),
        GatewayAccountId::new(row.try_get("gateway_account_id")?),
        GatewayConfigurationId::new(row.try_get("gateway_configuration_id")?),
    );
    let kind = row
        .try_get::<String, _>("attempt_kind")?
        .parse::<PaymentAttemptKind>()
        .map_err(|_| invalid_state())?;
    let status = row
        .try_get::<String, _>("status")?
        .parse::<PaymentAttemptStatus>()
        .map_err(|_| invalid_state())?;
    let target = payment_attempt_target_from_row(row, kind, status)?;
    let currency =
        CurrencyCode::new(&row.try_get::<String, _>("currency")?).map_err(|_| invalid_state())?;
    let amount = Money::new(row.try_get("amount_cents")?, currency).map_err(|_| invalid_state())?;
    let order_value = row.try_get::<String, _>("gateway_order_id")?;
    let gateway_order_id = GatewayOrderId::from_generated_attempt(&order_value, attempt_id)
        .or_else(|_| GatewayOrderId::from_correlation(&order_value))
        .map_err(|_| invalid_state())?;
    let request = PaymentAttemptRequest::new(
        target,
        IdempotencyKey::new(row.try_get::<String, _>("idempotency_key")?)
            .map_err(|_| invalid_state())?,
        PaymentAttemptFingerprint::new(row.try_get::<String, _>("request_fingerprint")?)
            .map_err(|_| invalid_state())?,
        amount,
        gateway_order_id,
        BillingContactSnapshot::new(row.try_get("billing_name")?, row.try_get("billing_email")?),
    );
    let state = PaymentAttemptState::new(
        status,
        row.try_get::<Option<String>, _>("resolution_code")?
            .as_deref()
            .map(PaymentResolutionCode::try_from)
            .transpose()
            .map_err(|_| invalid_state())?,
        processor_evidence_from_row(row)?,
        lifecycle_from_row(row)?,
        PaymentAttemptTimestamps::new(
            row.try_get("submitted_at")?,
            row.try_get("resolved_at")?,
            row.try_get("review_required_at")?,
            row.try_get("created_at")?,
            row.try_get("updated_at")?,
        ),
    );
    PaymentAttempt::new(identity, request, state).map_err(|_| invalid_state())
}

fn payment_attempt_target_from_row(
    row: &PgRow,
    kind: PaymentAttemptKind,
    status: PaymentAttemptStatus,
) -> Result<PaymentAttemptTarget, PaymentAttemptStoreError> {
    let plan_key = row
        .try_get::<Option<String>, _>("plan_key")?
        .map(PlanKey::new)
        .transpose()
        .map_err(|_| invalid_state())?;
    let host_target = row
        .try_get::<Option<Uuid>, _>("host_charge_target_id")?
        .map(HostChargeTargetId::new);
    let subscription_id = row
        .try_get::<Option<Uuid>, _>("subscription_id")?
        .map(SubscriptionId::new);
    let payment_method_id = row
        .try_get::<Option<Uuid>, _>("payment_method_id")?
        .map(PaymentMethodId::new);
    let period = period_from_row(row)?;
    let method_update_snapshot = payment_method_update_snapshot_from_row(row, subscription_id)?;
    let subscription_snapshot = subscription_snapshot_from_row(row, subscription_id)?;
    let discount = enrollment_discount_from_row(row)?;

    match kind {
        PaymentAttemptKind::HostCharge
            if plan_key.is_none()
                && subscription_id.is_none()
                && payment_method_id.is_none()
                && period.is_none()
                && method_update_snapshot.is_none()
                && subscription_snapshot.is_none()
                && discount.is_none() =>
        {
            Ok(PaymentAttemptTarget::HostCharge {
                target_id: host_target.ok_or_else(invalid_state)?,
            })
        }
        PaymentAttemptKind::SubscriptionInitial
            if host_target.is_none()
                && period.is_none()
                && method_update_snapshot.is_none()
                && subscription_snapshot.is_none() =>
        {
            let application = match (subscription_id, payment_method_id) {
                (subscription_id, Some(payment_method_id)) => Some(
                    SubscriptionInitialApplication::new(subscription_id, payment_method_id),
                ),
                (None, None) => None,
                (Some(_), None) => return Err(invalid_state()),
            };
            if matches!(
                status,
                PaymentAttemptStatus::Pending | PaymentAttemptStatus::Unknown
            ) && application.is_some()
            {
                return Err(invalid_state());
            }
            Ok(PaymentAttemptTarget::SubscriptionInitial {
                plan_key: plan_key.ok_or_else(invalid_state)?,
                discount,
                application,
            })
        }
        PaymentAttemptKind::SubscriptionRenewal
            if host_target.is_none() && method_update_snapshot.is_none() && discount.is_none() =>
        {
            Ok(PaymentAttemptTarget::SubscriptionRenewal {
                plan_key: plan_key.ok_or_else(invalid_state)?,
                payment_method_id: payment_method_id.ok_or_else(invalid_state)?,
                period: period.ok_or_else(invalid_state)?,
                expected_state: subscription_snapshot.ok_or_else(invalid_state)?,
            })
        }
        PaymentAttemptKind::SubscriptionRecovery
            if host_target.is_none() && method_update_snapshot.is_none() && discount.is_none() =>
        {
            Ok(PaymentAttemptTarget::SubscriptionRecovery {
                plan_key: plan_key.ok_or_else(invalid_state)?,
                payment_method_id: payment_method_id.ok_or_else(invalid_state)?,
                period: period.ok_or_else(invalid_state)?,
                expected_state: subscription_snapshot.ok_or_else(invalid_state)?,
            })
        }
        PaymentAttemptKind::SubscriptionPaymentMethodUpdate
            if host_target.is_none()
                && period.is_none()
                && subscription_snapshot.is_none()
                && discount.is_none() =>
        {
            Ok(PaymentAttemptTarget::SubscriptionPaymentMethodUpdate {
                plan_key: plan_key.ok_or_else(invalid_state)?,
                payment_method_id: payment_method_id.ok_or_else(invalid_state)?,
                expected_state: method_update_snapshot.ok_or_else(invalid_state)?,
            })
        }
        _ => Err(invalid_state()),
    }
}

fn period_from_row(row: &PgRow) -> Result<Option<BillingPeriod>, PaymentAttemptStoreError> {
    let start = row.try_get::<Option<DateTime<Utc>>, _>("billing_period_start_at")?;
    let end = row.try_get::<Option<DateTime<Utc>>, _>("billing_period_end_at")?;
    match (start, end) {
        (None, None) => Ok(None),
        (Some(start), Some(end)) => BillingPeriod::new(start, end)
            .map(Some)
            .map_err(|_| invalid_state()),
        _ => Err(invalid_state()),
    }
}

fn payment_method_update_snapshot_from_row(
    row: &PgRow,
    subscription_id: Option<SubscriptionId>,
) -> Result<Option<PaymentMethodUpdateSnapshot>, PaymentAttemptStoreError> {
    let expected_method = row
        .try_get::<Option<Uuid>, _>("payment_method_update_expected_payment_method_id")?
        .map(PaymentMethodId::new);
    let expected_transaction =
        row.try_get::<Option<String>, _>("payment_method_update_expected_initial_transaction_id")?;
    match (subscription_id, expected_method, expected_transaction) {
        (Some(subscription_id), Some(payment_method_id), Some(transaction)) => {
            Ok(Some(PaymentMethodUpdateSnapshot::new(
                subscription_id,
                payment_method_id,
                GatewayTransactionId::new(transaction).map_err(|_| invalid_state())?,
            )))
        }
        (_, None, None) => Ok(None),
        _ => Err(invalid_state()),
    }
}

fn subscription_snapshot_from_row(
    row: &PgRow,
    subscription_id: Option<SubscriptionId>,
) -> Result<Option<SubscriptionPaymentStateSnapshot>, PaymentAttemptStoreError> {
    let expected_method = row
        .try_get::<Option<Uuid>, _>("subscription_expected_payment_method_id")?
        .map(PaymentMethodId::new);
    let expected_transaction =
        row.try_get::<Option<String>, _>("subscription_expected_initial_transaction_id")?;
    let expected_status = row.try_get::<Option<String>, _>("subscription_expected_status")?;
    match (
        subscription_id,
        expected_method,
        expected_transaction,
        expected_status,
    ) {
        (Some(subscription_id), Some(payment_method_id), Some(transaction), Some(status)) => {
            Ok(Some(
                SubscriptionPaymentStateSnapshot::new(
                    subscription_id,
                    payment_method_id,
                    GatewayTransactionId::new(transaction).map_err(|_| invalid_state())?,
                    status
                        .parse::<SubscriptionStatus>()
                        .map_err(|_| invalid_state())?,
                )
                .map_err(|_| invalid_state())?,
            ))
        }
        (_, None, None, None) => Ok(None),
        _ => Err(invalid_state()),
    }
}

fn enrollment_discount_from_row(
    row: &PgRow,
) -> Result<Option<SubscriptionEnrollmentDiscountSnapshot>, PaymentAttemptStoreError> {
    let claim_id = row
        .try_get::<Option<Uuid>, _>("subscription_initial_discount_claim_id")?
        .map(DiscountClaimId::new);
    let code_id = row
        .try_get::<Option<Uuid>, _>("subscription_initial_discount_code_id")?
        .map(DiscountCodeId::new);
    let code = row.try_get::<Option<String>, _>("subscription_initial_discount_code_snapshot")?;
    let label = row.try_get::<Option<String>, _>("subscription_initial_discount_label_snapshot")?;
    let kind = row.try_get::<Option<String>, _>("subscription_initial_discount_kind")?;
    let amount_off =
        row.try_get::<Option<i32>, _>("subscription_initial_discount_amount_off_cents")?;
    let percent_off =
        row.try_get::<Option<i32>, _>("subscription_initial_discount_percent_off_bps")?;
    let currency = row.try_get::<Option<String>, _>("subscription_initial_discount_currency")?;
    let duration = row.try_get::<Option<String>, _>("subscription_initial_discount_duration")?;
    let duration_months =
        row.try_get::<Option<i32>, _>("subscription_initial_discount_duration_months")?;
    let base_amount =
        row.try_get::<Option<i32>, _>("subscription_initial_discount_base_amount_cents")?;
    let discounted_amount =
        row.try_get::<Option<i32>, _>("subscription_initial_discount_discounted_amount_cents")?;

    if claim_id.is_none()
        && code_id.is_none()
        && code.is_none()
        && label.is_none()
        && kind.is_none()
        && amount_off.is_none()
        && percent_off.is_none()
        && currency.is_none()
        && duration.is_none()
        && duration_months.is_none()
        && base_amount.is_none()
        && discounted_amount.is_none()
    {
        return Ok(None);
    }

    let kind = match (kind.as_deref(), amount_off, percent_off) {
        (Some("amount_off"), Some(value), None) => SubscriptionDiscountKind::AmountOffCents(
            PositiveDiscountCents::new(value).map_err(|_| invalid_state())?,
        ),
        (Some("percent_off"), None, Some(value)) => {
            SubscriptionDiscountKind::PercentOffBasisPoints(
                PercentOffBasisPoints::new(u16::try_from(value).map_err(|_| invalid_state())?)
                    .map_err(|_| invalid_state())?,
            )
        }
        _ => return Err(invalid_state()),
    };
    let duration = match (duration.as_deref(), duration_months) {
        (Some("indefinite"), None) => SubscriptionDiscountDuration::Indefinite,
        (Some("limited_months"), Some(value)) => SubscriptionDiscountDuration::LimitedMonths(
            LimitedDiscountMonths::new(u8::try_from(value).map_err(|_| invalid_state())?)
                .map_err(|_| invalid_state())?,
        ),
        _ => return Err(invalid_state()),
    };
    let currency = CurrencyCode::new(currency.as_deref().ok_or_else(invalid_state)?)
        .map_err(|_| invalid_state())?;
    let snapshot = SubscriptionDiscountSnapshot::new(
        SubscriptionDiscountCode::new(code.as_deref().ok_or_else(invalid_state)?)
            .map_err(|_| invalid_state())?,
        label,
        kind,
        duration,
        ChargeAmount::new(base_amount.ok_or_else(invalid_state)?, currency)
            .map_err(|_| invalid_state())?,
        ChargeAmount::new(discounted_amount.ok_or_else(invalid_state)?, currency)
            .map_err(|_| invalid_state())?,
    )
    .map_err(|_| invalid_state())?;
    Ok(Some(SubscriptionEnrollmentDiscountSnapshot::new(
        claim_id.ok_or_else(invalid_state)?,
        code_id.ok_or_else(invalid_state)?,
        snapshot,
    )))
}

fn processor_evidence_from_row(row: &PgRow) -> Result<ProcessorEvidence, PaymentAttemptStoreError> {
    let card_last_four = row.try_get::<Option<String>, _>("card_last4")?;
    let card_exp_month = row.try_get::<Option<i16>, _>("card_exp_month")?;
    let card_exp_year = row.try_get::<Option<i16>, _>("card_exp_year")?;
    let descriptor = GatewayPaymentDescriptor::from_provider_parts(
        diagnostic(row, "payment_type")?,
        diagnostic(row, "card_brand")?,
        card_last_four.as_deref(),
        card_exp_month,
        card_exp_year,
    );
    if descriptor.card_last_four().is_some() != card_last_four.is_some()
        || descriptor.card_exp_month() != card_exp_month
        || descriptor.card_exp_year() != card_exp_year
    {
        return Err(invalid_state());
    }
    Ok(ProcessorEvidence::new(
        row.try_get::<Option<String>, _>("gateway_transaction_id")?
            .map(GatewayTransactionId::new)
            .transpose()
            .map_err(|_| invalid_state())?,
        row.try_get::<Option<String>, _>("gateway_payment_method_reference")?
            .map(GatewayPaymentMethodReference::new)
            .transpose()
            .map_err(|_| invalid_state())?,
        diagnostic(row, "gateway_response")?,
        diagnostic(row, "gateway_response_code")?,
        diagnostic(row, "gateway_response_text")?,
        diagnostic(row, "gateway_condition")?,
        descriptor,
    ))
}

fn diagnostic(row: &PgRow, column: &'static str) -> Result<Option<GatewayDiagnostic>, sqlx::Error> {
    row.try_get::<Option<String>, _>(column)
        .map(|value| value.map(|value| GatewayDiagnostic::new(&value)))
}

fn lifecycle_from_row(row: &PgRow) -> Result<PaymentAttemptLifecycle, PaymentAttemptStoreError> {
    let refunded = row.try_get::<i32, _>("refunded_amount_cents")?;
    let state = match row
        .try_get::<String, _>("gateway_lifecycle_status")?
        .as_str()
    {
        "unknown" if refunded == 0 => GatewayLifecycleState::Unknown,
        "pending_settlement" if refunded == 0 => GatewayLifecycleState::PendingSettlement,
        "voided" if refunded == 0 => GatewayLifecycleState::Voided,
        "settled" if refunded == 0 => GatewayLifecycleState::Settled {
            cumulative_refunded_cents: None,
        },
        "settled" if refunded > 0 => GatewayLifecycleState::Settled {
            cumulative_refunded_cents: Some(
                CumulativeRefundCents::new(refunded).map_err(|_| invalid_state())?,
            ),
        },
        "refunded" => GatewayLifecycleState::Refunded {
            cumulative_refunded_cents: CumulativeRefundCents::new(refunded)
                .map_err(|_| invalid_state())?,
        },
        "chargeback" if refunded == 0 => GatewayLifecycleState::Chargeback {
            cumulative_refunded_cents: None,
        },
        "chargeback" if refunded > 0 => GatewayLifecycleState::Chargeback {
            cumulative_refunded_cents: Some(
                CumulativeRefundCents::new(refunded).map_err(|_| invalid_state())?,
            ),
        },
        _ => return Err(invalid_state()),
    };
    Ok(PaymentAttemptLifecycle::new(
        state,
        diagnostic(row, "gateway_lifecycle_action")?,
        row.try_get("gateway_lifecycle_at")?,
        row.try_get("gateway_lifecycle_reconciled_at")?,
    ))
}

const fn invalid_state() -> PaymentAttemptStoreError {
    PaymentAttemptStoreError::InvalidState(INVALID_ATTEMPT_STATE)
}

#[cfg(test)]
mod tests {
    use std::{error::Error, sync::Arc};

    use async_trait::async_trait;
    use chrono::Duration;

    use super::*;
    use crate::test_support::{TestDatabase, create_gateway_account};

    struct TestOfferStore;

    #[async_trait]
    impl crate::SubscriptionOfferStore for TestOfferStore {
        async fn lock_current_offer(
            &self,
            connection: &mut sqlx::PgConnection,
            billing_scope_id: BillingScopeId,
            plan_key: &PlanKey,
        ) -> Result<Option<syrup_rail::SubscriptionOffer>, sqlx::Error> {
            let row = sqlx::query_as::<_, (i32, String)>(
                r#"
                SELECT amount_cents, currency FROM host_subscription_offers
                WHERE billing_scope_id = $1 AND plan_key = $2 FOR SHARE
                "#,
            )
            .bind(billing_scope_id.as_uuid())
            .bind(plan_key.as_str())
            .fetch_optional(connection)
            .await?;
            row.map(|(amount, currency)| {
                let currency = CurrencyCode::new(&currency)
                    .map_err(|_| sqlx::Error::Protocol("invalid host currency".to_owned()))?;
                let charge = ChargeAmount::new(amount, currency)
                    .map_err(|_| sqlx::Error::Protocol("invalid host charge".to_owned()))?;
                Ok(syrup_rail::SubscriptionOffer::new(plan_key.clone(), charge))
            })
            .transpose()
        }
    }

    struct TestReferenceFactory;

    impl syrup_rail::GatewayMutationReferenceFactory for TestReferenceFactory {
        fn for_attempt(
            &self,
            kind: PaymentAttemptKind,
            attempt_id: PaymentAttemptId,
        ) -> GatewayOrderId {
            assert_eq!(kind, PaymentAttemptKind::SubscriptionInitial);
            GatewayOrderId::from_generated_attempt(
                format!("sr_initial_{}", attempt_id.as_uuid().simple()),
                attempt_id,
            )
            .expect("test order ID should be canonical")
        }
    }

    struct NeverCalledGateway;

    #[async_trait]
    impl syrup_rail::PaymentGateway for NeverCalledGateway {
        async fn account_mode(
            &self,
        ) -> Result<syrup_rail::GatewayAccountMode, syrup_rail::GatewayError> {
            panic!("reservation must not perform provider I/O")
        }

        async fn sale(
            &self,
            _request: syrup_rail::GatewaySaleRequest,
        ) -> Result<syrup_rail::GatewayPaymentOutcome, syrup_rail::GatewayMutationError> {
            panic!("reservation must not perform provider I/O")
        }

        async fn store_payment_method(
            &self,
            _request: syrup_rail::GatewayStorePaymentMethodRequest,
        ) -> Result<syrup_rail::GatewayPaymentOutcome, syrup_rail::GatewayMutationError> {
            panic!("reservation must not perform provider I/O")
        }

        async fn query_transaction(
            &self,
            _request: syrup_rail::GatewayQueryRequest,
        ) -> Result<Option<syrup_rail::GatewayPaymentOutcome>, syrup_rail::GatewayError> {
            panic!("reservation must not perform provider I/O")
        }

        async fn query_transaction_reports(
            &self,
            _request: syrup_rail::GatewayTransactionReportRequest,
        ) -> Result<Vec<syrup_rail::GatewayTransactionReport>, syrup_rail::GatewayError> {
            panic!("reservation must not perform provider I/O")
        }
    }

    async fn install_host_offers(database: &TestDatabase) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            CREATE TABLE host_subscription_offers (
                billing_scope_id uuid NOT NULL,
                plan_key text NOT NULL,
                amount_cents integer NOT NULL,
                currency text NOT NULL,
                PRIMARY KEY (billing_scope_id, plan_key)
            )
            "#,
        )
        .execute(&database.pool)
        .await?;
        Ok(())
    }

    async fn set_offer(
        database: &TestDatabase,
        scope_id: Uuid,
        plan_key: &str,
        amount_cents: i32,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            INSERT INTO host_subscription_offers (
                billing_scope_id, plan_key, amount_cents, currency
            ) VALUES ($1, $2, $3, 'USD')
            ON CONFLICT (billing_scope_id, plan_key)
            DO UPDATE SET amount_cents = EXCLUDED.amount_cents
            "#,
        )
        .bind(scope_id)
        .bind(plan_key)
        .bind(amount_cents)
        .execute(&database.pool)
        .await?;
        Ok(())
    }

    fn resolved_gateway(
        account: crate::test_support::GatewayAccountFixture,
    ) -> syrup_rail::ResolvedGateway {
        syrup_rail::ResolvedGateway::new(
            BillingScopeId::new(account.billing_scope_id),
            GatewayAccountId::new(account.gateway_account_id),
            GatewayConfigurationId::new(account.gateway_configuration_id),
            syrup_rail::GatewayProviderKey::new("nmi").unwrap(),
            syrup_rail::GatewayLifecycleQueryPolicy::new(
                syrup_rail::GatewayLifecycleCursorKey::new("test_cursor").unwrap(),
                Duration::minutes(1),
                10,
                2,
                2,
                20,
            )
            .unwrap(),
            Arc::new(TestReferenceFactory),
            Arc::new(NeverCalledGateway),
        )
    }

    fn enrollment_command(
        account: crate::test_support::GatewayAccountFixture,
        subscriber_id: Uuid,
        attempt_id: Uuid,
        idempotency_key: &str,
        expected_charge: syrup_rail::SubscriptionEnrollmentExpectedCharge,
    ) -> syrup_rail::EnrollSubscription {
        syrup_rail::EnrollSubscription::new(
            PaymentAttemptId::new(attempt_id),
            BillingScopeId::new(account.billing_scope_id),
            SubscriberId::new(subscriber_id),
            GatewayConfigurationId::new(account.gateway_configuration_id),
            IdempotencyKey::new(idempotency_key).unwrap(),
            syrup_rail::PaymentToken::new("token-secret").unwrap(),
            syrup_rail::BillingContact::new(
                Some("Sensitive".to_owned()),
                Some("Name".to_owned()),
                Some("secret@example.test".to_owned()),
            )
            .unwrap(),
            expected_charge,
        )
    }

    fn full_price(
        plan_key: &str,
        amount_cents: i32,
    ) -> syrup_rail::SubscriptionEnrollmentExpectedCharge {
        syrup_rail::SubscriptionEnrollmentExpectedCharge::full_price(
            syrup_rail::SubscriptionOffer::new(
                PlanKey::new(plan_key).unwrap(),
                ChargeAmount::new(amount_cents, CurrencyCode::new("USD").unwrap()).unwrap(),
            ),
        )
    }

    async fn create_discount_code(
        database: &TestDatabase,
        scope_id: Uuid,
        plan_key: &str,
    ) -> Result<Uuid, sqlx::Error> {
        let code_id = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO billing_subscription_discount_codes (
                id, billing_scope_id, plan_key, code_normalized, display_code,
                label, status, discount_kind, percent_off_bps, currency,
                duration, duration_months
            ) VALUES (
                $1, $2, $3, 'SAVE20', 'SAVE20', 'Sensitive campaign',
                'active', 'percent_off', 2000, 'USD', 'limited_months', 3
            )
            "#,
        )
        .bind(code_id)
        .bind(scope_id)
        .bind(plan_key)
        .execute(&database.pool)
        .await?;
        Ok(code_id)
    }

    async fn create_saved_claim(
        database: &TestDatabase,
        scope_id: Uuid,
        subscriber_id: Uuid,
        plan_key: &str,
        code_id: Uuid,
    ) -> Result<Uuid, sqlx::Error> {
        let claim_id = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO billing_subscription_discount_claims (
                id, billing_scope_id, subscriber_id, plan_key,
                discount_code_id, code_snapshot, label_snapshot,
                discount_kind, percent_off_bps, currency, duration,
                duration_months, base_amount_cents, discounted_amount_cents,
                status
            ) VALUES (
                $1, $2, $3, $4, $5, 'SAVE20', 'Sensitive campaign',
                'percent_off', 2000, 'USD', 'limited_months', 3,
                1000, 800, 'saved'
            )
            "#,
        )
        .bind(claim_id)
        .bind(scope_id)
        .bind(subscriber_id)
        .bind(plan_key)
        .bind(code_id)
        .execute(&database.pool)
        .await?;
        Ok(claim_id)
    }

    fn discounted_expected(plan_key: &str) -> syrup_rail::SubscriptionEnrollmentExpectedCharge {
        let currency = CurrencyCode::new("USD").unwrap();
        syrup_rail::SubscriptionEnrollmentExpectedCharge::discounted(
            PlanKey::new(plan_key).unwrap(),
            syrup_rail::SubscriptionDiscountSnapshot::new(
                syrup_rail::SubscriptionDiscountCode::new("SAVE20").unwrap(),
                Some("Sensitive campaign".to_owned()),
                SubscriptionDiscountKind::PercentOffBasisPoints(
                    PercentOffBasisPoints::new(2_000).unwrap(),
                ),
                SubscriptionDiscountDuration::LimitedMonths(LimitedDiscountMonths::new(3).unwrap()),
                ChargeAmount::new(1_000, currency).unwrap(),
                ChargeAmount::new(800, currency).unwrap(),
            )
            .unwrap(),
        )
    }

    #[tokio::test]
    async fn enrollment_reservation_is_token_free_replayable_and_plan_bearing()
    -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("enroll_reserve").await?;
        install_host_offers(&database).await?;
        let account = create_gateway_account(&database.pool, "nmi").await?;
        set_offer(
            &database,
            account.billing_scope_id,
            "base_subscription",
            1_000,
        )
        .await?;
        set_offer(&database, account.billing_scope_id, "premium", 1_000).await?;
        let subscriber_id = Uuid::now_v7();
        let gateway = resolved_gateway(account);
        let mut mismatched_account = account;
        mismatched_account.gateway_configuration_id = Uuid::now_v7();
        let mismatched_command = enrollment_command(
            mismatched_account,
            subscriber_id,
            Uuid::now_v7(),
            "mismatched-gateway",
            full_price("base_subscription", 1_000),
        );
        assert_eq!(
            SubscriptionEnrollmentReservation::from_command(&mismatched_command, &gateway)
                .expect_err("reservation must bind to the resolved configuration"),
            syrup_rail::SubscriptionEnrollmentReservationBuildError::GatewayIdentityMismatch,
        );
        let command = enrollment_command(
            account,
            subscriber_id,
            Uuid::now_v7(),
            "same-key",
            full_price("base_subscription", 1_000),
        );
        let reservation = SubscriptionEnrollmentReservation::from_command(&command, &gateway)?;
        let mut transaction = database.pool.begin().await?;
        let reserved = reserve_subscription_enrollment_in_transaction(
            &mut transaction,
            &TestOfferStore,
            &reservation,
        )
        .await?;
        let attempt = match reserved {
            SubscriptionEnrollmentReservationOutcome::Reserved(attempt) => attempt,
            other => return Err(format!("unexpected reservation outcome: {other:?}").into()),
        };
        assert_eq!(attempt.status(), PaymentAttemptStatus::Pending);
        assert_eq!(
            attempt.request().fingerprint().expose(),
            "subscription_initial:base_subscription:1000:USD:discount:none:expected:full_price:1000:USD"
        );
        assert!(attempt.state().timestamps().submitted_at().is_none());
        transaction.commit().await?;

        let persisted: String = sqlx::query_scalar(
            "SELECT to_jsonb(attempts)::text FROM billing_payment_attempts attempts WHERE id = $1",
        )
        .bind(attempt.identity().attempt_id().as_uuid())
        .fetch_one(&database.pool)
        .await?;
        assert!(!persisted.contains("token-secret"));
        assert!(!format!("{reservation:?}").contains("token-secret"));

        let replay_command = enrollment_command(
            account,
            subscriber_id,
            Uuid::now_v7(),
            "same-key",
            full_price("base_subscription", 1_000),
        );
        let replay_reservation =
            SubscriptionEnrollmentReservation::from_command(&replay_command, &gateway)?;
        let mut transaction = database.pool.begin().await?;
        let replay = reserve_subscription_enrollment_in_transaction(
            &mut transaction,
            &TestOfferStore,
            &replay_reservation,
        )
        .await?;
        assert!(matches!(
            replay,
            SubscriptionEnrollmentReservationOutcome::Replay(ref replayed)
                if replayed.identity().attempt_id() == attempt.identity().attempt_id()
        ));
        transaction.commit().await?;

        let changed_plan_command = enrollment_command(
            account,
            subscriber_id,
            Uuid::now_v7(),
            "same-key",
            full_price("premium", 1_000),
        );
        let changed_plan_reservation =
            SubscriptionEnrollmentReservation::from_command(&changed_plan_command, &gateway)?;
        let mut transaction = database.pool.begin().await?;
        assert_eq!(
            reserve_subscription_enrollment_in_transaction(
                &mut transaction,
                &TestOfferStore,
                &changed_plan_reservation,
            )
            .await?,
            SubscriptionEnrollmentReservationOutcome::IdempotencyConflict,
        );
        transaction.rollback().await?;

        let mut transaction = database.pool.begin().await?;
        let admitted = admit_subscription_enrollment_submission_in_transaction(
            &mut transaction,
            &TestOfferStore,
            &replay_reservation,
        )
        .await?;
        assert!(matches!(
            admitted,
            SubscriptionEnrollmentSubmissionOutcome::Admitted(ref admitted)
                if admitted.identity().attempt_id() == attempt.identity().attempt_id()
                    && admitted.state().timestamps().submitted_at().is_some()
        ));
        transaction.commit().await?;
        database.cleanup().await
    }

    #[tokio::test]
    async fn same_key_stale_replay_expires_at_the_exact_boundary_without_a_live_offer()
    -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("enroll_stale").await?;
        install_host_offers(&database).await?;
        let account = create_gateway_account(&database.pool, "nmi").await?;
        let plan_key = "base_subscription";
        set_offer(&database, account.billing_scope_id, plan_key, 1_000).await?;
        let gateway = resolved_gateway(account);

        let before_boundary_subscriber = Uuid::now_v7();
        let before_boundary = SubscriptionEnrollmentReservation::from_command(
            &enrollment_command(
                account,
                before_boundary_subscriber,
                Uuid::now_v7(),
                "before-boundary",
                full_price(plan_key, 1_000),
            ),
            &gateway,
        )?;
        let mut transaction = database.pool.begin().await?;
        let before_boundary_attempt = match reserve_subscription_enrollment_in_transaction(
            &mut transaction,
            &TestOfferStore,
            &before_boundary,
        )
        .await?
        {
            SubscriptionEnrollmentReservationOutcome::Reserved(attempt) => attempt,
            other => return Err(format!("unexpected reservation outcome: {other:?}").into()),
        };
        transaction.commit().await?;
        sqlx::query(
            "UPDATE billing_payment_attempts SET created_at = clock_timestamp() - interval '29 minutes 59 seconds' WHERE id = $1",
        )
        .bind(before_boundary_attempt.identity().attempt_id().as_uuid())
        .execute(&database.pool)
        .await?;
        sqlx::query(
            "DELETE FROM host_subscription_offers WHERE billing_scope_id = $1 AND plan_key = $2",
        )
        .bind(account.billing_scope_id)
        .bind(plan_key)
        .execute(&database.pool)
        .await?;
        let mut transaction = database.pool.begin().await?;
        assert_eq!(
            reserve_subscription_enrollment_in_transaction(
                &mut transaction,
                &TestOfferStore,
                &before_boundary,
            )
            .await?,
            SubscriptionEnrollmentReservationOutcome::Rejected(
                SubscriptionEnrollmentReservationRejection::EnrollmentTermsChanged,
            )
        );
        transaction.rollback().await?;
        let before_boundary_status: String =
            sqlx::query_scalar("SELECT status FROM billing_payment_attempts WHERE id = $1")
                .bind(before_boundary_attempt.identity().attempt_id().as_uuid())
                .fetch_one(&database.pool)
                .await?;
        assert_eq!(before_boundary_status, "pending");

        set_offer(&database, account.billing_scope_id, plan_key, 1_000).await?;
        let boundary_subscriber = Uuid::now_v7();
        let boundary = SubscriptionEnrollmentReservation::from_command(
            &enrollment_command(
                account,
                boundary_subscriber,
                Uuid::now_v7(),
                "at-boundary",
                full_price(plan_key, 1_000),
            ),
            &gateway,
        )?;
        let mut transaction = database.pool.begin().await?;
        let boundary_attempt = match reserve_subscription_enrollment_in_transaction(
            &mut transaction,
            &TestOfferStore,
            &boundary,
        )
        .await?
        {
            SubscriptionEnrollmentReservationOutcome::Reserved(attempt) => attempt,
            other => return Err(format!("unexpected reservation outcome: {other:?}").into()),
        };
        transaction.commit().await?;
        sqlx::query(
            "UPDATE billing_payment_attempts SET created_at = clock_timestamp() - interval '30 minutes' WHERE id = $1",
        )
        .bind(boundary_attempt.identity().attempt_id().as_uuid())
        .execute(&database.pool)
        .await?;
        sqlx::query(
            "DELETE FROM host_subscription_offers WHERE billing_scope_id = $1 AND plan_key = $2",
        )
        .bind(account.billing_scope_id)
        .bind(plan_key)
        .execute(&database.pool)
        .await?;
        let boundary_replay = SubscriptionEnrollmentReservation::from_command(
            &enrollment_command(
                account,
                boundary_subscriber,
                Uuid::now_v7(),
                "at-boundary",
                full_price(plan_key, 1_000),
            ),
            &gateway,
        )?;

        let mut transaction = database.pool.begin().await?;
        let replay = reserve_subscription_enrollment_in_transaction(
            &mut transaction,
            &TestOfferStore,
            &boundary_replay,
        )
        .await?;
        let expired = match replay {
            SubscriptionEnrollmentReservationOutcome::Replay(attempt) => attempt,
            other => return Err(format!("unexpected stale replay outcome: {other:?}").into()),
        };
        assert_eq!(
            expired.identity().attempt_id(),
            boundary_attempt.identity().attempt_id()
        );
        assert_eq!(expired.status(), PaymentAttemptStatus::Failed);
        assert_eq!(
            expired.state().resolution_code(),
            Some(PaymentResolutionCode::SubscriptionInitialPreparedAttemptExpired)
        );
        assert!(expired.state().timestamps().submitted_at().is_none());
        transaction.commit().await?;
        database.cleanup().await
    }

    #[tokio::test]
    async fn saved_discount_survives_repricing_but_not_pre_submission_expiry()
    -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("enroll_discount").await?;
        install_host_offers(&database).await?;
        let account = create_gateway_account(&database.pool, "nmi").await?;
        let plan_key = "base_subscription";
        set_offer(&database, account.billing_scope_id, plan_key, 1_000).await?;
        let code_id = create_discount_code(&database, account.billing_scope_id, plan_key).await?;
        let gateway = resolved_gateway(account);

        let subscriber_a = Uuid::now_v7();
        let claim_a = create_saved_claim(
            &database,
            account.billing_scope_id,
            subscriber_a,
            plan_key,
            code_id,
        )
        .await?;
        let command_a = enrollment_command(
            account,
            subscriber_a,
            Uuid::now_v7(),
            "discount-a",
            discounted_expected(plan_key),
        );
        let reservation_a = SubscriptionEnrollmentReservation::from_command(&command_a, &gateway)?;
        let mut transaction = database.pool.begin().await?;
        let attempt_a = match reserve_subscription_enrollment_in_transaction(
            &mut transaction,
            &TestOfferStore,
            &reservation_a,
        )
        .await?
        {
            SubscriptionEnrollmentReservationOutcome::Reserved(attempt) => attempt,
            other => return Err(format!("unexpected discounted reservation: {other:?}").into()),
        };
        transaction.commit().await?;
        let discount = attempt_a
            .request()
            .target()
            .enrollment_discount()
            .expect("saved discount should be snapshotted");
        assert_eq!(discount.claim_id().as_uuid(), &claim_a);
        assert_eq!(discount.code_id().as_uuid(), &code_id);
        assert_eq!(attempt_a.request().amount().cents(), 800);
        assert_eq!(
            attempt_a.request().fingerprint().expose(),
            format!(
                "subscription_initial:base_subscription:800:USD:discount:{claim_a}:{code_id}:SAVE20:percent_off:none:2000:USD:1000:800:limited_months:3:expected:discounted:SAVE20:percent_off:none:2000:limited_months:3:USD:1000:800"
            )
        );
        let debug = format!("{attempt_a:?}");
        assert!(!debug.contains("SAVE20"));
        assert!(!debug.contains("Sensitive campaign"));

        set_offer(&database, account.billing_scope_id, plan_key, 1_400).await?;
        let mut transaction = database.pool.begin().await?;
        assert!(matches!(
            admit_subscription_enrollment_submission_in_transaction(
                &mut transaction,
                &TestOfferStore,
                &reservation_a,
            )
            .await?,
            SubscriptionEnrollmentSubmissionOutcome::Admitted(_)
        ));
        transaction.commit().await?;

        let subscriber_b = Uuid::now_v7();
        let claim_b = create_saved_claim(
            &database,
            account.billing_scope_id,
            subscriber_b,
            plan_key,
            code_id,
        )
        .await?;
        let command_b = enrollment_command(
            account,
            subscriber_b,
            Uuid::now_v7(),
            "discount-b",
            discounted_expected(plan_key),
        );
        let reservation_b = SubscriptionEnrollmentReservation::from_command(&command_b, &gateway)?;
        let mut transaction = database.pool.begin().await?;
        assert!(matches!(
            reserve_subscription_enrollment_in_transaction(
                &mut transaction,
                &TestOfferStore,
                &reservation_b,
            )
            .await?,
            SubscriptionEnrollmentReservationOutcome::Reserved(_)
        ));
        transaction.commit().await?;
        sqlx::query(
            "UPDATE billing_subscription_discount_claims SET status = 'expired' WHERE id = $1",
        )
        .bind(claim_b)
        .execute(&database.pool)
        .await?;
        let mut transaction = database.pool.begin().await?;
        let rejected = admit_subscription_enrollment_submission_in_transaction(
            &mut transaction,
            &TestOfferStore,
            &reservation_b,
        )
        .await?;
        assert!(matches!(
            rejected,
            SubscriptionEnrollmentSubmissionOutcome::Rejected {
                ref attempt,
                reason: SubscriptionEnrollmentSubmissionRejection::EnrollmentTermsChanged,
            } if attempt.status() == PaymentAttemptStatus::Failed
                && attempt.state().timestamps().submitted_at().is_none()
        ));
        transaction.commit().await?;
        database.cleanup().await
    }

    #[tokio::test]
    async fn active_grant_blocks_only_its_exact_plan() -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("enroll_grant").await?;
        install_host_offers(&database).await?;
        let account = create_gateway_account(&database.pool, "nmi").await?;
        set_offer(&database, account.billing_scope_id, "basic", 1_000).await?;
        set_offer(&database, account.billing_scope_id, "premium", 2_000).await?;
        let subscriber_id = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO billing_subscription_grants (
                id, billing_scope_id, subscriber_id, plan_key, grant_kind,
                reason, starts_at, ends_at, granted_by_actor_id
            ) VALUES (
                $1, $2, $3, 'basic', 'promotion', 'launch',
                clock_timestamp() - interval '1 minute',
                clock_timestamp() + interval '1 day', $4
            )
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(account.billing_scope_id)
        .bind(subscriber_id)
        .bind(Uuid::now_v7())
        .execute(&database.pool)
        .await?;
        let gateway = resolved_gateway(account);

        let basic = SubscriptionEnrollmentReservation::from_command(
            &enrollment_command(
                account,
                subscriber_id,
                Uuid::now_v7(),
                "basic-grant",
                full_price("basic", 1_000),
            ),
            &gateway,
        )?;
        let mut transaction = database.pool.begin().await?;
        assert_eq!(
            reserve_subscription_enrollment_in_transaction(
                &mut transaction,
                &TestOfferStore,
                &basic,
            )
            .await?,
            SubscriptionEnrollmentReservationOutcome::Rejected(
                SubscriptionEnrollmentReservationRejection::ActiveGrant,
            )
        );
        transaction.rollback().await?;

        let premium = SubscriptionEnrollmentReservation::from_command(
            &enrollment_command(
                account,
                subscriber_id,
                Uuid::now_v7(),
                "premium-with-basic-grant",
                full_price("premium", 2_000),
            ),
            &gateway,
        )?;
        let mut transaction = database.pool.begin().await?;
        assert!(matches!(
            reserve_subscription_enrollment_in_transaction(
                &mut transaction,
                &TestOfferStore,
                &premium,
            )
            .await?,
            SubscriptionEnrollmentReservationOutcome::Reserved(_)
        ));
        transaction.commit().await?;
        database.cleanup().await
    }

    #[tokio::test]
    async fn gateway_configuration_rotation_rejects_prepared_submission()
    -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("enroll_rotate").await?;
        install_host_offers(&database).await?;
        let account = create_gateway_account(&database.pool, "nmi").await?;
        let plan_key = "base_subscription";
        set_offer(&database, account.billing_scope_id, plan_key, 1_000).await?;
        let gateway = resolved_gateway(account);
        let reservation = SubscriptionEnrollmentReservation::from_command(
            &enrollment_command(
                account,
                Uuid::now_v7(),
                Uuid::now_v7(),
                "rotated-configuration",
                full_price(plan_key, 1_000),
            ),
            &gateway,
        )?;
        let mut transaction = database.pool.begin().await?;
        assert!(matches!(
            reserve_subscription_enrollment_in_transaction(
                &mut transaction,
                &TestOfferStore,
                &reservation,
            )
            .await?,
            SubscriptionEnrollmentReservationOutcome::Reserved(_)
        ));
        transaction.commit().await?;

        sqlx::query(
            "UPDATE billing_gateway_accounts SET gateway_configuration_id = $2, updated_at = clock_timestamp() WHERE id = $1",
        )
        .bind(account.gateway_account_id)
        .bind(Uuid::now_v7())
        .execute(&database.pool)
        .await?;
        let mut transaction = database.pool.begin().await?;
        let rejected = admit_subscription_enrollment_submission_in_transaction(
            &mut transaction,
            &TestOfferStore,
            &reservation,
        )
        .await?;
        assert!(matches!(
            rejected,
            SubscriptionEnrollmentSubmissionOutcome::Rejected {
                ref attempt,
                reason: SubscriptionEnrollmentSubmissionRejection::GatewayConfigurationChanged,
            } if attempt.status() == PaymentAttemptStatus::Failed
                && attempt.state().timestamps().submitted_at().is_none()
        ));
        transaction.commit().await?;
        database.cleanup().await
    }

    #[tokio::test]
    async fn loaders_preserve_exact_scope_and_redact_durable_values() -> Result<(), Box<dyn Error>>
    {
        let database = TestDatabase::start("attempt_owner").await?;
        let account = create_gateway_account(&database.pool, "nmi").await?;
        let attempt_id = Uuid::now_v7();
        let subscriber_id = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO billing_payment_attempts (
                id, billing_scope_id, subscriber_id, host_charge_target_id,
                attempt_kind, status, idempotency_key, request_fingerprint,
                amount_cents, currency, gateway_account_id,
                gateway_configuration_id, gateway_order_id,
                gateway_transaction_id, gateway_payment_method_reference,
                gateway_response, gateway_response_code, gateway_response_text,
                gateway_condition, billing_name, billing_email
            ) VALUES (
                $1, $2, $3, $4, 'host_charge', 'pending', $5, $6,
                1000, 'USD', $7, $8, $9, $10, $11, $12, $13, $14,
                $15, $16, $17
            )
            "#,
        )
        .bind(attempt_id)
        .bind(account.billing_scope_id)
        .bind(subscriber_id)
        .bind(Uuid::now_v7())
        .bind("idempotency-secret")
        .bind("fingerprint-secret")
        .bind(account.gateway_account_id)
        .bind(account.gateway_configuration_id)
        .bind("order-secret")
        .bind("transaction-secret")
        .bind("method-secret")
        .bind("response-secret")
        .bind("code-secret")
        .bind("text-secret")
        .bind("condition-secret")
        .bind("Sensitive Name")
        .bind("secret@example.test")
        .execute(&database.pool)
        .await?;

        let mut transaction = database.pool.begin().await?;
        assert!(
            find_payment_attempt_by_id_in_transaction(
                &mut transaction,
                BillingScopeId::new(Uuid::now_v7()),
                PaymentAttemptId::new(attempt_id),
            )
            .await?
            .is_none()
        );
        let attempt = lock_payment_attempt_by_idempotency_in_transaction(
            &mut transaction,
            BillingScopeId::new(account.billing_scope_id),
            SubscriberId::new(subscriber_id),
            &IdempotencyKey::new("idempotency-secret")?,
        )
        .await?
        .expect("exact owner row should load");
        assert_eq!(attempt.identity().attempt_id().as_uuid(), &attempt_id);
        assert_eq!(attempt.kind(), PaymentAttemptKind::HostCharge);
        assert_eq!(attempt.request().amount().cents(), 1_000);
        assert_eq!(
            attempt
                .state()
                .processor_evidence()
                .transaction_id()
                .expect("transaction ID")
                .expose(),
            "transaction-secret"
        );
        let debug = format!("{attempt:?}");
        for secret in [
            "idempotency-secret",
            "fingerprint-secret",
            "order-secret",
            "transaction-secret",
            "method-secret",
            "response-secret",
            "code-secret",
            "text-secret",
            "condition-secret",
            "Sensitive Name",
            "secret@example.test",
        ] {
            assert!(!debug.contains(secret), "debug leaked {secret}");
        }
        transaction.rollback().await?;
        database.cleanup().await
    }

    #[tokio::test]
    async fn recovery_keeps_related_and_expected_payment_methods_distinct()
    -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("attempt_recovery").await?;
        let account = create_gateway_account(&database.pool, "nmi").await?;
        let subscriber_id = Uuid::now_v7();
        let expected_method_id = Uuid::now_v7();
        let related_method_id = Uuid::now_v7();
        for (method_id, reference) in [
            (expected_method_id, "vault-expected"),
            (related_method_id, "vault-related"),
        ] {
            sqlx::query(
                r#"
                INSERT INTO billing_payment_methods (
                    id, billing_scope_id, subscriber_id, gateway_account_id,
                    gateway_payment_method_reference, status
                ) VALUES ($1, $2, $3, $4, $5, 'active')
                "#,
            )
            .bind(method_id)
            .bind(account.billing_scope_id)
            .bind(subscriber_id)
            .bind(account.gateway_account_id)
            .bind(reference)
            .execute(&database.pool)
            .await?;
        }
        let subscription_id = Uuid::now_v7();
        let period_start = Utc::now();
        let period_end = period_start + Duration::days(30);
        sqlx::query(
            r#"
            INSERT INTO billing_subscriptions (
                id, billing_scope_id, subscriber_id, plan_key, status,
                gateway_account_id, payment_method_id, amount_cents, currency,
                current_period_start_at, current_period_end_at, next_renewal_at,
                initial_transaction_id
            ) VALUES (
                $1, $2, $3, 'premium', 'active', $4, $5, 1000, 'USD',
                $6, $7, $7, 'txn-initial'
            )
            "#,
        )
        .bind(subscription_id)
        .bind(account.billing_scope_id)
        .bind(subscriber_id)
        .bind(account.gateway_account_id)
        .bind(related_method_id)
        .bind(period_start)
        .bind(period_end)
        .execute(&database.pool)
        .await?;

        let attempt_id = Uuid::now_v7();
        let charge_start = period_end;
        let charge_end = charge_start + Duration::days(30);
        let order_id = format!("sr_recovery_{}", attempt_id.simple());
        sqlx::query(
            r#"
            INSERT INTO billing_payment_attempts (
                id, billing_scope_id, subscriber_id, plan_key,
                subscription_id, payment_method_id, attempt_kind, status,
                idempotency_key, request_fingerprint, amount_cents, currency,
                billing_period_start_at, billing_period_end_at,
                gateway_account_id, gateway_configuration_id, gateway_order_id,
                gateway_transaction_id, submitted_at, resolved_at,
                subscription_expected_payment_method_id,
                subscription_expected_initial_transaction_id,
                subscription_expected_status
            ) VALUES (
                $1, $2, $3, 'premium', $4, $5,
                'subscription_recovery', 'approved', $6, $7, 1000, 'USD',
                $8, $9, $10, $11, $12, 'txn-recovery', now(), now(),
                $13, 'txn-initial', 'past_due'
            )
            "#,
        )
        .bind(attempt_id)
        .bind(account.billing_scope_id)
        .bind(subscriber_id)
        .bind(subscription_id)
        .bind(related_method_id)
        .bind("recovery-key")
        .bind("recovery-fingerprint")
        .bind(charge_start)
        .bind(charge_end)
        .bind(account.gateway_account_id)
        .bind(account.gateway_configuration_id)
        .bind(order_id)
        .bind(expected_method_id)
        .execute(&database.pool)
        .await?;

        let mut transaction = database.pool.begin().await?;
        let attempt = find_payment_attempt_by_id_in_transaction(
            &mut transaction,
            BillingScopeId::new(account.billing_scope_id),
            PaymentAttemptId::new(attempt_id),
        )
        .await?
        .expect("recovery row should load");
        let target = attempt.request().target();
        assert_eq!(
            target.payment_method_id().unwrap().as_uuid(),
            &related_method_id
        );
        assert_eq!(
            target.subscription_id().unwrap().as_uuid(),
            &subscription_id
        );
        assert_eq!(
            target
                .subscription_payment_state_snapshot()
                .expect("expected state")
                .payment_method_id()
                .as_uuid(),
            &expected_method_id
        );
        assert_eq!(
            target
                .subscription_payment_state_snapshot()
                .expect("expected state")
                .status(),
            SubscriptionStatus::PastDue
        );
        transaction.rollback().await?;
        database.cleanup().await
    }
}
