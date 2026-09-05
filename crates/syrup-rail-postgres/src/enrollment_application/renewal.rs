// agentic-loc-exception: Release-critical workflow remains under the absolute limit; split follow-up is tracked separately.

use std::fmt;

use sqlx::{PgConnection, PgPool};
use syrup_rail::{
    ApprovedProcessorEvidence, BillingEvent, BillingEventSubject, BillingScopeId,
    GatewayMutationError, GatewayNotSubmittedError, GatewayPaymentOutcome, GatewayPaymentStatus,
    GatewaySaleIntent, GatewaySaleRequest, PaymentAttempt, PaymentAttemptId, PaymentAttemptStatus,
    PaymentResolutionCode, ProcessorChargeProgression, ProcessorChargeRole, ProcessorEvidence,
    SubscriptionEnrollmentPaymentResult, SubscriptionRenewalReservation,
    SubscriptionRenewalSubmissionOutcome, SubscriptionRenewalSubmissionRejection,
};

use crate::{
    BillingTransactionCoordinator, BillingTransactionSubjectState,
    attempts::find_payment_attempt_by_id_on_connection,
    processor_charges::{ObservedCharge, observe_processor_charge, transition_charge},
    renewal_failure::{RenewalFailureApplication, apply_resolved_automatic_renewal_failure},
};

use super::{
    APPROVED_APPLICATION_ATTEMPTS, APPROVED_EVIDENCE_RETRY_DELAY, AttemptResolutionStatus,
    AttemptTransition, BILLING_LOCK_TIMEOUT, GatewayNotSubmittedPolicy, INVALID_APPLICATION_STATE,
    OutcomeApplication, OutcomeReservation, OutcomeResolutionBoundary, OutcomeResolutionCommand,
    PreparedAttemptReplay, RENEWAL_APPROVED_STORAGE_FAILURE_TEXT, RENEWAL_INCOMPLETE_APPROVAL_TEXT,
    RENEWAL_STALE_STATE_TEXT, RateLimitCooldown, SubscriptionEnrollmentApplicationError,
    advance_subscription_discount_after_successful_charge,
    append_subscription_observation_diagnostics, clear_resolved_attempt_submission,
    commit_rate_limit_cooldown, finalize_approved_application, load_applied_subscription,
    load_subscription, lock_expected_reservation_attempt, lock_payment_method_domain,
    lock_subscription_aggregate, map_attempt_transition_error, mark_attempt_approved,
    park_locked_attempt, payment_result_for_reservation_attempt, persist_attempt_transition,
    reconcile_non_approved_evidence, renewal_subscription_matches, resolve_pool_outcome,
    set_application_timeouts, stop_conflicting_subscription_approval,
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
    /// The provider mutation was not contacted. Renewal has no prepared replay
    /// path, so its returned payment remains a terminal canonical result. A
    /// concurrent terminal result is returned as `Payment` instead.
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
    gateway: crate::ModeVerifiedGateway<'_>,
) -> Result<SubscriptionRenewalProviderResult, SubscriptionEnrollmentApplicationError> {
    let identity = admission.reservation.identity();
    let resolved_gateway = gateway.resolved_gateway();
    if admission.attempt.identity() != identity
        || admission.attempt.request() != admission.reservation.request()
        || admission.attempt.status() != PaymentAttemptStatus::Pending
        || admission
            .attempt
            .state()
            .timestamps()
            .submitted_at()
            .is_none()
        || resolved_gateway.provider_key() != admission.reservation.provider_key()
    {
        return Err(SubscriptionEnrollmentApplicationError::SubmissionIdentityMismatch);
    }
    let Some(gateway) = gateway.authorize_attempt(&identity) else {
        return Err(SubscriptionEnrollmentApplicationError::SubmissionIdentityMismatch);
    };
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
        Err(error) => {
            let evidence = error.processor_evidence();
            match error {
                GatewayMutationError::NotSubmitted(error) => {
                    let policy = GatewayNotSubmittedPolicy::for_error(&error);
                    let application = resolve_renewal_non_approved_outcome(
                        pool,
                        coordinator,
                        &admission.reservation,
                        &evidence,
                        OutcomeResolutionCommand::non_approved(
                            AttemptResolutionStatus::Failed,
                            Some(policy.resolution_code()),
                            policy.cooldown(),
                            OutcomeResolutionBoundary::AdmittedNotSubmitted,
                        ),
                    )
                    .await?;
                    if application.should_surface_not_submitted(policy) {
                        Ok(SubscriptionRenewalProviderResult::NotSubmitted {
                            payment: application.payment,
                            error,
                        })
                    } else {
                        Ok(SubscriptionRenewalProviderResult::Payment(
                            application.payment,
                        ))
                    }
                }
                GatewayMutationError::RateLimitedIndeterminate(_) => {
                    resolve_renewal_unknown_outcome(
                        pool,
                        &admission.reservation,
                        &evidence,
                        Some(RateLimitCooldown::Provider),
                    )
                    .await
                    .map(SubscriptionRenewalProviderResult::Payment)
                }
                GatewayMutationError::Indeterminate(_) => {
                    resolve_renewal_unknown_outcome(pool, &admission.reservation, &evidence, None)
                        .await
                        .map(SubscriptionRenewalProviderResult::Payment)
                }
            }
        }
    }
}

pub async fn apply_subscription_renewal_gateway_outcome(
    pool: &PgPool,
    coordinator: &dyn BillingTransactionCoordinator,
    reservation: &SubscriptionRenewalReservation,
    outcome: &GatewayPaymentOutcome,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    apply_subscription_renewal_gateway_decision(pool, coordinator, reservation, outcome)
        .await
        .map(|result| append_subscription_observation_diagnostics(result, outcome.diagnostics()))
}

async fn apply_subscription_renewal_gateway_decision(
    pool: &PgPool,
    coordinator: &dyn BillingTransactionCoordinator,
    reservation: &SubscriptionRenewalReservation,
    outcome: &GatewayPaymentOutcome,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    match outcome.status() {
        GatewayPaymentStatus::Approved => {
            let approved_evidence = outcome.approved_evidence().ok_or(
                SubscriptionEnrollmentApplicationError::InvalidState(INVALID_APPLICATION_STATE),
            )?;
            if outcome.transaction_id().is_none() {
                return super::park_approved_outcome(
                    pool,
                    super::ApprovedParkingReservation::Renewal(reservation),
                    &approved_evidence,
                    RENEWAL_INCOMPLETE_APPROVAL_TEXT,
                )
                .await;
            }
            for attempt_index in 0..APPROVED_APPLICATION_ATTEMPTS {
                match apply_renewal_approved_outcome(coordinator, reservation, &approved_evidence)
                    .await
                {
                    Ok(result) => return Ok(result),
                    Err(_) if attempt_index + 1 < APPROVED_APPLICATION_ATTEMPTS => {
                        tokio::time::sleep(APPROVED_EVIDENCE_RETRY_DELAY).await;
                    }
                    Err(_) => break,
                }
            }
            super::park_approved_outcome(
                pool,
                super::ApprovedParkingReservation::Renewal(reservation),
                &approved_evidence,
                RENEWAL_APPROVED_STORAGE_FAILURE_TEXT,
            )
            .await
        }
        GatewayPaymentStatus::Declined => resolve_renewal_non_approved_outcome(
            pool,
            coordinator,
            reservation,
            outcome.evidence(),
            OutcomeResolutionCommand::non_approved(
                AttemptResolutionStatus::Declined,
                None,
                None,
                OutcomeResolutionBoundary::Submitted,
            ),
        )
        .await
        .map(OutcomeApplication::into_payment),
        GatewayPaymentStatus::Failed => resolve_renewal_non_approved_outcome(
            pool,
            coordinator,
            reservation,
            outcome.evidence(),
            OutcomeResolutionCommand::non_approved(
                AttemptResolutionStatus::Failed,
                None,
                None,
                OutcomeResolutionBoundary::Submitted,
            ),
        )
        .await
        .map(OutcomeApplication::into_payment),
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
    super::apply_reconciled_gateway_outcome_for(
        pool,
        coordinator,
        billing_scope_id,
        attempt_id,
        outcome,
        super::ReconciledApplicationEntry::Exact(super::ReservationOperation::Renewal),
    )
    .await
}

async fn apply_renewal_approved_outcome(
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
    let application = apply_renewal_approved_on_connection(
        transaction.connection(),
        subject_state,
        reservation,
        approved_evidence,
    )
    .await;
    finalize_approved_application(transaction, application).await
}

async fn apply_renewal_approved_on_connection(
    connection: &mut PgConnection,
    subject_state: BillingTransactionSubjectState,
    reservation: &SubscriptionRenewalReservation,
    approved_evidence: &ApprovedProcessorEvidence,
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
        identity.gateway_account_id(),
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
    pool: &PgPool,
    coordinator: &dyn BillingTransactionCoordinator,
    reservation: &SubscriptionRenewalReservation,
    evidence: &ProcessorEvidence,
    resolution: OutcomeResolutionCommand,
) -> Result<OutcomeApplication, SubscriptionEnrollmentApplicationError> {
    let renewal = reservation;
    let reservation = OutcomeReservation::Renewal(renewal);
    let identity = reservation.identity();
    if let Some(cooldown) = resolution.cooldown {
        commit_rate_limit_cooldown(pool, reservation, cooldown).await?;
    }
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
        let reconciled = reconcile_non_approved_evidence(&attempt, evidence);
        let diagnostics = reconciled.identity_conflict_diagnostics();
        let resolution = if reconciled.has_identity_conflict() {
            OutcomeResolutionCommand::unknown(None)
        } else {
            resolution
        };
        let evidence = &reconciled.evidence;
        let may_resolve = resolution.may_resolve(
            attempt.status(),
            attempt.state().timestamps().submitted_at().is_some(),
        );
        let mut events = Vec::new();
        // Renewal has no prepared-attempt replay entrypoint. Even when the
        // shared policy identifies a not-submitted failure as retry-safe for
        // resumable flows, renewal preserves its established terminal result.
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
                clear_resolved_attempt_submission(connection, &attempt).await?;
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
        let result = append_subscription_observation_diagnostics(
            payment_result_for_reservation_attempt(connection, reservation).await?,
            &diagnostics,
        );
        Ok::<_, SubscriptionEnrollmentApplicationError>((result, events, may_resolve))
    }
    .await;
    let (result, events, applied) = match result {
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
    Ok(OutcomeApplication {
        payment: result,
        applied,
        prepared_attempt_replay: PreparedAttemptReplay::Unsupported,
    })
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
    .map(OutcomeApplication::into_payment)
}
