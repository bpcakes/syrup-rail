use std::fmt;

use sqlx::{PgConnection, PgPool};
use syrup_rail::{
    BillingEvent, BillingEventSubject, BillingScopeId, GatewayMutationError,
    GatewayNotSubmittedError, GatewayPaymentOutcome, GatewayPaymentStatus, GatewayProviderKey,
    GatewaySaleIntent, GatewaySaleRequest, PaymentAttempt, PaymentAttemptId, PaymentAttemptStatus,
    PaymentResolutionCode, ProcessorChargeProgression, ProcessorChargeRole, ProcessorEvidence,
    ResolvedGateway, SubscriptionEnrollmentPaymentResult, SubscriptionRenewalReservation,
    SubscriptionRenewalSubmissionOutcome, SubscriptionRenewalSubmissionRejection,
};

use crate::{
    BillingTransactionCoordinator, BillingTransactionSubjectState,
    attempts::find_payment_attempt_by_id_on_connection,
    processor_charges::{ObservedCharge, observe_processor_charge, transition_charge},
    renewal_failure::{RenewalFailureApplication, apply_resolved_automatic_renewal_failure},
};

use super::{
    APPROVED_APPLICATION_ATTEMPTS, APPROVED_EVIDENCE_RETRY_DELAY, APPROVED_EVIDENCE_WRITE_ATTEMPTS,
    AttemptResolutionStatus, AttemptTransition, BILLING_LOCK_TIMEOUT, INVALID_APPLICATION_STATE,
    OutcomeReservation, OutcomeResolutionBoundary, OutcomeResolutionCommand,
    RENEWAL_APPROVED_STORAGE_FAILURE_TEXT, RENEWAL_INCOMPLETE_APPROVAL_TEXT,
    RENEWAL_STALE_STATE_TEXT, RateLimitCooldown, SubscriptionEnrollmentApplicationError,
    advance_subscription_discount_after_successful_charge, clear_attempt_submission,
    extend_rate_limit_cooldown, finalize_approved_application, is_retryable_evidence_error,
    load_applied_subscription, load_subscription, lock_expected_reservation_attempt,
    lock_payment_method_domain, lock_subscription_aggregate, map_attempt_transition_error,
    mark_attempt_approved, mutation_error_evidence, not_submitted_resolution_code,
    park_locked_attempt, payment_result_for_attempt, payment_result_for_reservation_attempt,
    persist_attempt_transition, renewal_subscription_matches, resolve_pool_outcome,
    set_application_timeouts,
};

/// One committed final-admission result authorizing exactly one immediate
/// automatic recurring charge.
pub struct AdmittedSubscriptionRenewal {
    reservation: SubscriptionRenewalReservation,
    attempt: PaymentAttempt,
    payment_method_reference: syrup_rail::GatewayPaymentMethodReference,
}

impl AdmittedSubscriptionRenewal {
    pub const fn attempt(&self) -> &PaymentAttempt {
        &self.attempt
    }
}

impl fmt::Debug for AdmittedSubscriptionRenewal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AdmittedSubscriptionRenewal")
            .field("attempt", &self.attempt)
            .field("has_submission_authority", &true)
            .field("has_payment_method_reference", &true)
            .finish()
    }
}

#[derive(Debug)]
pub enum SubscriptionRenewalAdmissionOutcome {
    Admitted(Box<AdmittedSubscriptionRenewal>),
    AlreadyAdmitted(PaymentAttempt),
    Rejected {
        attempt: PaymentAttempt,
        reason: SubscriptionRenewalSubmissionRejection,
    },
}

#[derive(Debug)]
pub enum SubscriptionRenewalProviderResult {
    Payment(SubscriptionEnrollmentPaymentResult),
    NotSubmitted {
        payment: SubscriptionEnrollmentPaymentResult,
        error: GatewayNotSubmittedError,
    },
}

/// Commits final automatic-renewal admission and captures the exact stored credential.
pub async fn admit_subscription_renewal_submission(
    pool: &PgPool,
    reservation: &SubscriptionRenewalReservation,
) -> Result<SubscriptionRenewalAdmissionOutcome, SubscriptionEnrollmentApplicationError> {
    let mut transaction = pool.begin().await?;
    let outcome =
        crate::admit_subscription_renewal_submission_in_transaction(&mut transaction, reservation)
            .await?;
    let outcome = match outcome {
        SubscriptionRenewalSubmissionOutcome::Admitted(attempt) => {
            let identity = reservation.identity();
            let reference = sqlx::query_scalar::<_, String>(
                r#"
                SELECT gateway_payment_method_reference
                FROM billing_payment_methods
                WHERE id = $1 AND billing_scope_id = $2 AND subscriber_id = $3
                    AND gateway_account_id = $4 AND status = 'active'
                FOR SHARE
                "#,
            )
            .bind(reservation.expected_state().payment_method_id().as_uuid())
            .bind(identity.billing_scope_id().as_uuid())
            .bind(identity.subscriber_id().as_uuid())
            .bind(identity.gateway_account_id().as_uuid())
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or(SubscriptionEnrollmentApplicationError::InvalidState(
                INVALID_APPLICATION_STATE,
            ))?;
            let payment_method_reference =
                syrup_rail::GatewayPaymentMethodReference::new(reference).map_err(|_| {
                    SubscriptionEnrollmentApplicationError::InvalidState(INVALID_APPLICATION_STATE)
                })?;
            SubscriptionRenewalAdmissionOutcome::Admitted(Box::new(AdmittedSubscriptionRenewal {
                reservation: reservation.clone(),
                attempt,
                payment_method_reference,
            }))
        }
        SubscriptionRenewalSubmissionOutcome::AlreadyAdmitted(attempt) => {
            SubscriptionRenewalAdmissionOutcome::AlreadyAdmitted(attempt)
        }
        SubscriptionRenewalSubmissionOutcome::Rejected { attempt, reason } => {
            SubscriptionRenewalAdmissionOutcome::Rejected { attempt, reason }
        }
    };
    transaction.commit().await?;
    Ok(outcome)
}

/// Performs the one recurring sale authorized by committed renewal admission.
pub async fn submit_admitted_subscription_renewal(
    pool: &PgPool,
    coordinator: &dyn BillingTransactionCoordinator,
    admission: AdmittedSubscriptionRenewal,
    gateway: &ResolvedGateway,
) -> Result<SubscriptionRenewalProviderResult, SubscriptionEnrollmentApplicationError> {
    let identity = admission.reservation.identity();
    if admission.attempt.identity() != identity
        || admission.attempt.request() != admission.reservation.request()
        || admission.attempt.status() != PaymentAttemptStatus::Pending
        || admission
            .attempt
            .state()
            .timestamps()
            .submitted_at()
            .is_none()
        || gateway.billing_scope_id() != identity.billing_scope_id()
        || gateway.gateway_account_id() != identity.gateway_account_id()
        || gateway.gateway_configuration_id() != identity.gateway_configuration_id()
        || gateway.provider_key() != admission.reservation.provider_key()
    {
        return Err(SubscriptionEnrollmentApplicationError::SubmissionIdentityMismatch);
    }
    let charge =
        syrup_rail::ChargeAmount::try_from(admission.attempt.request().amount()).map_err(|_| {
            SubscriptionEnrollmentApplicationError::InvalidState(INVALID_APPLICATION_STATE)
        })?;
    let request = GatewaySaleRequest::new(
        charge,
        admission.attempt.request().gateway_order_id().clone(),
        GatewaySaleIntent::RecurringStoredCredential {
            payment_method_reference: admission.payment_method_reference,
            initial_transaction_id: admission
                .reservation
                .expected_state()
                .initial_transaction_id()
                .clone(),
        },
        None,
    );
    match gateway.sale(request).await {
        Ok(outcome) => apply_subscription_renewal_gateway_outcome(
            pool,
            coordinator,
            &admission.reservation,
            &outcome,
        )
        .await
        .map(SubscriptionRenewalProviderResult::Payment),
        Err(GatewayMutationError::NotSubmitted(error)) => {
            let evidence = mutation_error_evidence(error.detail());
            let cooldown = matches!(error, GatewayNotSubmittedError::RateLimited(_))
                .then_some(RateLimitCooldown::Account);
            let payment = resolve_renewal_non_approved_outcome(
                coordinator,
                &admission.reservation,
                &evidence,
                AttemptResolutionStatus::Failed,
                Some(not_submitted_resolution_code(&error)),
                cooldown,
                OutcomeResolutionBoundary::AdmittedNotSubmitted,
            )
            .await?;
            Ok(SubscriptionRenewalProviderResult::NotSubmitted { payment, error })
        }
        Err(GatewayMutationError::RateLimitedIndeterminate(detail)) => {
            resolve_renewal_unknown_outcome(
                pool,
                &admission.reservation,
                &mutation_error_evidence(&detail),
                Some(RateLimitCooldown::Provider),
            )
            .await
            .map(SubscriptionRenewalProviderResult::Payment)
        }
        Err(GatewayMutationError::Indeterminate(detail)) => resolve_renewal_unknown_outcome(
            pool,
            &admission.reservation,
            &mutation_error_evidence(&detail),
            None,
        )
        .await
        .map(SubscriptionRenewalProviderResult::Payment),
    }
}

pub async fn apply_subscription_renewal_gateway_outcome(
    pool: &PgPool,
    coordinator: &dyn BillingTransactionCoordinator,
    reservation: &SubscriptionRenewalReservation,
    outcome: &GatewayPaymentOutcome,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    match outcome.status() {
        GatewayPaymentStatus::Approved => {
            if outcome.transaction_id().is_none() {
                return park_renewal_approved_outcome(
                    pool,
                    reservation,
                    outcome.evidence(),
                    RENEWAL_INCOMPLETE_APPROVAL_TEXT,
                )
                .await;
            }
            for attempt_index in 0..APPROVED_APPLICATION_ATTEMPTS {
                match apply_renewal_approved_outcome(coordinator, reservation, outcome.evidence())
                    .await
                {
                    Ok(result) => return Ok(result),
                    Err(_) if attempt_index + 1 < APPROVED_APPLICATION_ATTEMPTS => {
                        tokio::time::sleep(APPROVED_EVIDENCE_RETRY_DELAY).await;
                    }
                    Err(_) => break,
                }
            }
            park_renewal_approved_outcome(
                pool,
                reservation,
                outcome.evidence(),
                RENEWAL_APPROVED_STORAGE_FAILURE_TEXT,
            )
            .await
        }
        GatewayPaymentStatus::Declined => {
            resolve_renewal_non_approved_outcome(
                coordinator,
                reservation,
                outcome.evidence(),
                AttemptResolutionStatus::Declined,
                None,
                None,
                OutcomeResolutionBoundary::Submitted,
            )
            .await
        }
        GatewayPaymentStatus::Failed => {
            resolve_renewal_non_approved_outcome(
                coordinator,
                reservation,
                outcome.evidence(),
                AttemptResolutionStatus::Failed,
                None,
                None,
                OutcomeResolutionBoundary::Submitted,
            )
            .await
        }
        GatewayPaymentStatus::Unknown => {
            resolve_renewal_unknown_outcome(pool, reservation, outcome.evidence(), None).await
        }
    }
}

pub async fn apply_reconciled_subscription_renewal_gateway_outcome(
    pool: &PgPool,
    coordinator: &dyn BillingTransactionCoordinator,
    billing_scope_id: BillingScopeId,
    attempt_id: PaymentAttemptId,
    outcome: &GatewayPaymentOutcome,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    let mut transaction = pool.begin().await?;
    let attempt = crate::find_payment_attempt_by_id_in_transaction(
        &mut transaction,
        billing_scope_id,
        attempt_id,
    )
    .await?
    .ok_or(SubscriptionEnrollmentApplicationError::InvalidState(
        "subscription renewal attempt was not found",
    ))?;
    let provider_key = sqlx::query_scalar::<_, String>(
        "SELECT provider_key FROM billing_gateway_accounts WHERE billing_scope_id = $1 AND id = $2",
    )
    .bind(billing_scope_id.as_uuid())
    .bind(attempt.identity().gateway_account_id().as_uuid())
    .fetch_optional(&mut *transaction)
    .await?
    .ok_or(SubscriptionEnrollmentApplicationError::InvalidState(
        "subscription renewal gateway account was not found",
    ))?;
    transaction.commit().await?;
    let provider_key = GatewayProviderKey::new(provider_key).map_err(|_| {
        SubscriptionEnrollmentApplicationError::InvalidState(
            "subscription renewal gateway provider key is invalid",
        )
    })?;
    let reservation = SubscriptionRenewalReservation::from_attempt(&attempt, provider_key)
        .map_err(|_| {
            SubscriptionEnrollmentApplicationError::InvalidState(
                "reconciled attempt is not a valid subscription renewal",
            )
        })?;
    apply_subscription_renewal_gateway_outcome(pool, coordinator, &reservation, outcome).await
}

async fn apply_renewal_approved_outcome(
    coordinator: &dyn BillingTransactionCoordinator,
    reservation: &SubscriptionRenewalReservation,
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
    let application = apply_renewal_approved_on_connection(
        transaction.connection(),
        subject_state,
        reservation,
        evidence,
    )
    .await;
    finalize_approved_application(transaction, application).await
}

async fn apply_renewal_approved_on_connection(
    connection: &mut PgConnection,
    subject_state: BillingTransactionSubjectState,
    reservation: &SubscriptionRenewalReservation,
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
        lock_expected_reservation_attempt(connection, OutcomeReservation::Renewal(reservation))
            .await?;
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
        if let ObservedCharge::Owned(charge) = observation {
            transition_charge(
                connection,
                charge.id,
                ProcessorChargeProgression::ExternalReversalRequired,
                None,
            )
            .await?;
        }
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

pub(crate) async fn resolve_renewal_non_approved_outcome(
    coordinator: &dyn BillingTransactionCoordinator,
    reservation: &SubscriptionRenewalReservation,
    evidence: &ProcessorEvidence,
    status: AttemptResolutionStatus,
    resolution_code: Option<PaymentResolutionCode>,
    cooldown: Option<RateLimitCooldown>,
    boundary: OutcomeResolutionBoundary,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    let renewal = reservation;
    let reservation = OutcomeReservation::Renewal(renewal);
    let identity = reservation.identity();
    let resolution =
        OutcomeResolutionCommand::non_approved(status, resolution_code, cooldown, boundary);
    let mut transaction = coordinator
        .begin(
            BillingEventSubject::new(identity.billing_scope_id(), identity.subscriber_id()),
            BILLING_LOCK_TIMEOUT,
        )
        .await?;
    let result = async {
        let connection = transaction.connection();
        set_application_timeouts(connection).await?;
        lock_subscription_aggregate(connection, identity.subscriber_id(), reservation.plan_key())
            .await?;
        let attempt = lock_expected_reservation_attempt(connection, reservation).await?;
        let may_resolve = resolution.may_resolve(
            attempt.status(),
            attempt.state().timestamps().submitted_at().is_some(),
        );
        let mut events = Vec::new();
        if may_resolve {
            let status = resolution.resolved_status(reservation.operation(), attempt.status());
            persist_attempt_transition(
                connection,
                &attempt,
                evidence,
                AttemptTransition::Resolved {
                    status,
                    resolution_code: resolution.resolution_code,
                },
            )
            .await
            .map_err(map_attempt_transition_error)?;
            if resolution.clears_submitted_at() {
                clear_attempt_submission(connection, &attempt).await?;
            } else if resolution.marks_renewal_past_due(status)
                && resolution.resolution_code.is_none()
            {
                let resolved_attempt = find_payment_attempt_by_id_on_connection(
                    connection,
                    identity.billing_scope_id(),
                    identity.attempt_id(),
                )
                .await?
                .ok_or(SubscriptionEnrollmentApplicationError::InvalidState(
                    INVALID_APPLICATION_STATE,
                ))?;
                if let RenewalFailureApplication::Applied {
                    events: applied_events,
                    ..
                } =
                    apply_resolved_automatic_renewal_failure(connection, &resolved_attempt).await?
                {
                    events = applied_events;
                }
            }
        }
        if let Some(cooldown) = resolution.cooldown {
            extend_rate_limit_cooldown(connection, reservation, cooldown).await?;
        }
        let result = payment_result_for_reservation_attempt(connection, reservation).await?;
        Ok::<_, SubscriptionEnrollmentApplicationError>((result, events))
    }
    .await;
    let (result, events) = match result {
        Ok(result) => result,
        Err(error) => {
            let _ = transaction.rollback().await;
            return Err(error);
        }
    };
    for event in &events {
        if let Err(error) = transaction.append_event(event).await {
            let _ = transaction.rollback().await;
            return Err(error.into());
        }
    }
    transaction.commit().await?;
    Ok(result)
}

async fn resolve_renewal_unknown_outcome(
    pool: &PgPool,
    reservation: &SubscriptionRenewalReservation,
    evidence: &ProcessorEvidence,
    cooldown: Option<RateLimitCooldown>,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    resolve_pool_outcome(
        pool,
        OutcomeReservation::Renewal(reservation),
        evidence,
        OutcomeResolutionCommand::unknown(cooldown),
    )
    .await
}

async fn park_renewal_approved_outcome(
    pool: &PgPool,
    reservation: &SubscriptionRenewalReservation,
    evidence: &ProcessorEvidence,
    message: &'static str,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    match try_park_renewal_approved_outcome(pool, reservation, evidence, message).await {
        Ok(result) => Ok(result),
        Err(_) => {
            observe_renewal_approved_evidence_with_retry(pool, reservation, evidence).await?;
            let mut transaction = pool.begin().await?;
            let attempt = find_payment_attempt_by_id_on_connection(
                &mut transaction,
                reservation.identity().billing_scope_id(),
                reservation.identity().attempt_id(),
            )
            .await?
            .ok_or(SubscriptionEnrollmentApplicationError::InvalidState(
                INVALID_APPLICATION_STATE,
            ))?;
            let result = if attempt.status() == PaymentAttemptStatus::Approved {
                payment_result_for_attempt(&mut transaction, attempt).await?
            } else {
                SubscriptionEnrollmentPaymentResult::confirmation_pending(
                    attempt,
                    evidence.clone(),
                )?
            };
            transaction.commit().await?;
            Ok(result)
        }
    }
}

async fn try_park_renewal_approved_outcome(
    pool: &PgPool,
    reservation: &SubscriptionRenewalReservation,
    evidence: &ProcessorEvidence,
    message: &'static str,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    let mut transaction = pool.begin().await?;
    set_application_timeouts(&mut transaction).await?;
    lock_subscription_aggregate(
        &mut transaction,
        reservation.identity().subscriber_id(),
        reservation.plan_key(),
    )
    .await?;
    let attempt = lock_expected_reservation_attempt(
        &mut transaction,
        OutcomeReservation::Renewal(reservation),
    )
    .await?;
    let attempt = if attempt.status() == PaymentAttemptStatus::Approved {
        observe_processor_charge(
            &mut transaction,
            &attempt,
            evidence,
            ProcessorChargeProgression::Applied,
        )
        .await?;
        attempt
    } else if attempt.status().is_terminal() {
        let progression =
            if evidence.transaction_id().is_some() && attempt.request().amount().cents() > 0 {
                ProcessorChargeProgression::ExternalReversalRequired
            } else {
                ProcessorChargeProgression::ReconciliationRequired
            };
        observe_processor_charge(&mut transaction, &attempt, evidence, progression).await?;
        attempt
    } else {
        observe_processor_charge(
            &mut transaction,
            &attempt,
            evidence,
            ProcessorChargeProgression::Pending,
        )
        .await?;
        park_locked_attempt(&mut transaction, &attempt, evidence, None, message).await?
    };
    let result = payment_result_for_attempt(&mut transaction, attempt).await?;
    transaction.commit().await?;
    Ok(result)
}

async fn observe_renewal_approved_evidence_with_retry(
    pool: &PgPool,
    reservation: &SubscriptionRenewalReservation,
    evidence: &ProcessorEvidence,
) -> Result<(), SubscriptionEnrollmentApplicationError> {
    for attempt_index in 0..APPROVED_EVIDENCE_WRITE_ATTEMPTS {
        let result = async {
            let mut transaction = pool.begin().await?;
            set_application_timeouts(&mut transaction).await?;
            let attempt = lock_expected_reservation_attempt(
                &mut transaction,
                OutcomeReservation::Renewal(reservation),
            )
            .await?;
            observe_processor_charge(
                &mut transaction,
                &attempt,
                evidence,
                ProcessorChargeProgression::Pending,
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
    Err(SubscriptionEnrollmentApplicationError::ApprovedEvidenceNotDurable)
}
