use std::fmt;

use sqlx::{PgConnection, PgPool};
use syrup_rail::{
    ApprovedProcessorEvidence, BillingEvent, BillingEventSubject, BillingScopeId,
    GatewayMutationError, GatewayNotSubmittedError, GatewayPaymentOutcome, GatewayPaymentStatus,
    GatewaySaleIntent, GatewaySaleRequest, PaymentAttempt, PaymentAttemptId, PaymentAttemptStatus,
    PaymentResolutionCode, ProcessorChargeProgression, ProcessorChargeRole, ProcessorEvidence,
    RecoverSubscriptionPayment, SubscriptionEnrollmentPaymentResult,
    SubscriptionRecoveryReservation, SubscriptionRecoverySubmissionOutcome,
    SubscriptionRecoverySubmissionRejection,
};

use crate::{
    BillingTransactionCoordinator, BillingTransactionSubjectState,
    attempts::find_payment_attempt_by_id_on_connection,
    processor_charges::{ObservedCharge, observe_processor_charge, transition_charge},
};

use super::{
    APPROVED_APPLICATION_ATTEMPTS, APPROVED_EVIDENCE_RETRY_DELAY, AttemptResolutionStatus,
    BILLING_LOCK_TIMEOUT, GatewayNotSubmittedPolicy, INVALID_APPLICATION_STATE, OutcomeApplication,
    OutcomeReservation, OutcomeResolutionBoundary, OutcomeResolutionCommand,
    RECOVERY_APPROVED_STORAGE_FAILURE_TEXT, RECOVERY_INCOMPLETE_APPROVAL_TEXT,
    RECOVERY_STALE_STATE_TEXT, RateLimitCooldown, SubscriptionEnrollmentApplicationError,
    advance_subscription_discount_after_successful_charge, apply_resumable_not_submitted_policy,
    disable_payment_method_if_unreferenced, finalize_approved_application,
    load_applied_subscription, load_subscription, lock_expected_reservation_attempt,
    lock_payment_method_domain, lock_subscription_aggregate, mark_attempt_approved,
    mutation_error_evidence, park_locked_attempt, recovery_subscription_matches,
    resolve_pool_outcome, set_application_timeouts, upsert_payment_method,
};

/// One committed final-admission result authorizing exactly one immediate
/// subscription-recovery submission.
pub struct AdmittedSubscriptionRecovery {
    reservation: SubscriptionRecoveryReservation,
    attempt: PaymentAttempt,
}

impl AdmittedSubscriptionRecovery {
    pub const fn attempt(&self) -> &PaymentAttempt {
        &self.attempt
    }
}

impl fmt::Debug for AdmittedSubscriptionRecovery {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AdmittedSubscriptionRecovery")
            .field("attempt", &self.attempt)
            .field("has_submission_authority", &true)
            .finish()
    }
}

#[derive(Debug)]
pub enum SubscriptionRecoveryAdmissionOutcome {
    Admitted(Box<AdmittedSubscriptionRecovery>),
    AlreadyAdmitted(PaymentAttempt),
    Rejected {
        attempt: PaymentAttempt,
        reason: SubscriptionRecoverySubmissionRejection,
    },
}

#[derive(Debug)]
pub enum SubscriptionRecoveryProviderResult {
    Payment(SubscriptionEnrollmentPaymentResult),
    /// The provider mutation was not contacted. A retry-safe readiness failure
    /// can carry the same pending, prepared payment for same-key replay. A
    /// concurrent terminal result is returned as `Payment` instead.
    NotSubmitted {
        payment: SubscriptionEnrollmentPaymentResult,
        error: GatewayNotSubmittedError,
    },
}

impl SubscriptionRecoveryProviderResult {
    pub const fn payment(&self) -> &SubscriptionEnrollmentPaymentResult {
        match self {
            Self::Payment(payment) | Self::NotSubmitted { payment, .. } => payment,
        }
    }

    pub fn into_payment(self) -> SubscriptionEnrollmentPaymentResult {
        match self {
            Self::Payment(payment) | Self::NotSubmitted { payment, .. } => payment,
        }
    }
}

/// Commits final recovery admission before exposing its one-shot capability.
pub async fn admit_subscription_recovery_submission(
    pool: &PgPool,
    reservation: &SubscriptionRecoveryReservation,
) -> Result<SubscriptionRecoveryAdmissionOutcome, SubscriptionEnrollmentApplicationError> {
    let mut transaction = pool.begin().await?;
    let outcome =
        crate::admit_subscription_recovery_submission_in_transaction(&mut transaction, reservation)
            .await?;
    transaction.commit().await?;
    Ok(match outcome {
        SubscriptionRecoverySubmissionOutcome::Admitted(attempt) => {
            SubscriptionRecoveryAdmissionOutcome::Admitted(Box::new(AdmittedSubscriptionRecovery {
                reservation: reservation.clone(),
                attempt,
            }))
        }
        SubscriptionRecoverySubmissionOutcome::AlreadyAdmitted(attempt) => {
            SubscriptionRecoveryAdmissionOutcome::AlreadyAdmitted(attempt)
        }
        SubscriptionRecoverySubmissionOutcome::Rejected { attempt, reason } => {
            SubscriptionRecoveryAdmissionOutcome::Rejected { attempt, reason }
        }
    })
}

/// Performs the one provider sale authorized by committed recovery admission,
/// then applies or durably parks its result without holding a database lock
/// across provider I/O.
pub async fn submit_admitted_subscription_recovery(
    pool: &PgPool,
    coordinator: &dyn BillingTransactionCoordinator,
    admission: AdmittedSubscriptionRecovery,
    command: &RecoverSubscriptionPayment,
    gateway: crate::ModeVerifiedGateway<'_>,
) -> Result<SubscriptionRecoveryProviderResult, SubscriptionEnrollmentApplicationError> {
    let resolved_gateway = gateway.resolved_gateway();
    if admission.attempt.identity() != admission.reservation.identity()
        || admission.attempt.request() != admission.reservation.request()
        || admission.attempt.status() != PaymentAttemptStatus::Pending
        || admission
            .attempt
            .state()
            .timestamps()
            .submitted_at()
            .is_none()
        || !admission
            .reservation
            .matches_submission(command, resolved_gateway)
    {
        return Err(SubscriptionEnrollmentApplicationError::SubmissionIdentityMismatch);
    }
    let Some(gateway) = gateway.authorize_attempt(&admission.reservation.identity()) else {
        return Err(SubscriptionEnrollmentApplicationError::SubmissionIdentityMismatch);
    };
    let charge =
        syrup_rail::ChargeAmount::try_from(admission.attempt.request().amount()).map_err(|_| {
            SubscriptionEnrollmentApplicationError::InvalidState(INVALID_APPLICATION_STATE)
        })?;
    let request = GatewaySaleRequest::new(
        charge,
        admission.attempt.request().gateway_order_id().clone(),
        GatewaySaleIntent::InitialStoredCredential {
            payment_token: command.payment_token().clone(),
        },
        Some(command.billing_contact().clone()),
    );
    match gateway.sale(request).await {
        Ok(outcome) => apply_subscription_recovery_gateway_outcome(
            pool,
            coordinator,
            &admission.reservation,
            &outcome,
        )
        .await
        .map(SubscriptionRecoveryProviderResult::Payment),
        Err(GatewayMutationError::NotSubmitted(error)) => {
            let evidence = mutation_error_evidence(error.detail());
            let policy = GatewayNotSubmittedPolicy::for_error(&error);
            let application = apply_resumable_not_submitted_policy(
                pool,
                OutcomeReservation::Recovery(&admission.reservation),
                &evidence,
                policy,
            )
            .await?;
            if application.should_surface_not_submitted(policy) {
                Ok(SubscriptionRecoveryProviderResult::NotSubmitted {
                    payment: application.payment,
                    error,
                })
            } else {
                Ok(SubscriptionRecoveryProviderResult::Payment(
                    application.payment,
                ))
            }
        }
        Err(GatewayMutationError::RateLimitedIndeterminate(detail)) => {
            resolve_recovery_unknown_outcome(
                pool,
                &admission.reservation,
                &mutation_error_evidence(&detail),
                Some(RateLimitCooldown::Provider),
            )
            .await
            .map(SubscriptionRecoveryProviderResult::Payment)
        }
        Err(GatewayMutationError::Indeterminate(detail)) => resolve_recovery_unknown_outcome(
            pool,
            &admission.reservation,
            &mutation_error_evidence(&detail),
            None,
        )
        .await
        .map(SubscriptionRecoveryProviderResult::Payment),
    }
}

/// Applies one recovery outcome through the canonical charge ledger and host
/// billing transaction.
pub async fn apply_subscription_recovery_gateway_outcome(
    pool: &PgPool,
    coordinator: &dyn BillingTransactionCoordinator,
    reservation: &SubscriptionRecoveryReservation,
    outcome: &GatewayPaymentOutcome,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    apply_subscription_recovery_gateway_decision(pool, coordinator, reservation, outcome)
        .await
        .map(|result| result.with_gateway_diagnostics(outcome.diagnostics().to_vec()))
}

async fn apply_subscription_recovery_gateway_decision(
    pool: &PgPool,
    coordinator: &dyn BillingTransactionCoordinator,
    reservation: &SubscriptionRecoveryReservation,
    outcome: &GatewayPaymentOutcome,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    match outcome.status() {
        GatewayPaymentStatus::Approved => {
            let approved_evidence = outcome.approved_evidence().ok_or(
                SubscriptionEnrollmentApplicationError::InvalidState(INVALID_APPLICATION_STATE),
            )?;
            if outcome.transaction_id().is_none() || outcome.payment_method_reference().is_none() {
                return super::park_approved_outcome(
                    pool,
                    OutcomeReservation::Recovery(reservation),
                    &approved_evidence,
                    RECOVERY_INCOMPLETE_APPROVAL_TEXT,
                )
                .await;
            }
            for attempt_index in 0..APPROVED_APPLICATION_ATTEMPTS {
                match apply_recovery_approved_outcome(coordinator, reservation, &approved_evidence)
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
                OutcomeReservation::Recovery(reservation),
                &approved_evidence,
                RECOVERY_APPROVED_STORAGE_FAILURE_TEXT,
            )
            .await
        }
        GatewayPaymentStatus::Declined => {
            resolve_recovery_non_approved_outcome(
                pool,
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
            resolve_recovery_non_approved_outcome(
                pool,
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
            resolve_recovery_unknown_outcome(pool, reservation, outcome.evidence(), None).await
        }
    }
}

/// Re-enters the same recovery application authority from durable evidence and
/// never resolves a live gateway or submits another mutation.
pub async fn apply_reconciled_subscription_recovery_gateway_outcome(
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
        super::ReconciledApplicationEntry::Exact(super::ReservationOperation::Recovery),
    )
    .await
}

async fn apply_recovery_approved_outcome(
    coordinator: &dyn BillingTransactionCoordinator,
    reservation: &SubscriptionRecoveryReservation,
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
    let application = apply_recovery_approved_on_connection(
        transaction.connection(),
        subject_state,
        reservation,
        approved_evidence,
    )
    .await;
    finalize_approved_application(transaction, application).await
}

async fn apply_recovery_approved_on_connection(
    connection: &mut PgConnection,
    subject_state: BillingTransactionSubjectState,
    reservation: &SubscriptionRecoveryReservation,
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
        lock_expected_reservation_attempt(connection, OutcomeReservation::Recovery(reservation))
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
            "a subscription recovery event requires a live recipient",
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
            SubscriptionEnrollmentPaymentResult::confirmation_pending(
                attempt,
                approved_evidence.clone(),
            )?,
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
    if !recovery_subscription_matches(connection, reservation).await? {
        transition_charge(
            connection,
            charge.id,
            ProcessorChargeProgression::ExternalReversalRequired,
            Some(PaymentResolutionCode::SubscriptionApprovedRecoveryStaleState),
        )
        .await?;
        let parked = park_locked_attempt(
            connection,
            &attempt,
            evidence,
            Some(PaymentResolutionCode::SubscriptionApprovedRecoveryStaleState),
            RECOVERY_STALE_STATE_TEXT,
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
    let old_method_id = reservation.expected_state().payment_method_id();
    let method_id = upsert_payment_method(connection, &attempt, evidence).await?;
    let updated = sqlx::query(
        r#"
        UPDATE billing_subscriptions
        SET status = 'active', phase = 'recurring', payment_method_id = $2,
            current_period_start_at = $3, current_period_end_at = $4,
            next_renewal_at = $4, next_payment_attempt_at = $4,
            initial_transaction_id = $5,
            updated_at = clock_timestamp()
        WHERE id = $1 AND billing_scope_id = $6 AND subscriber_id = $7
            AND gateway_account_id = $8 AND plan_key = $9
            -- Reservation of new v2 recoveries requires past_due. Application
            -- also honors exact active authority snapshotted by durable v1
            -- attempts that survive the maintenance cutover.
            AND status = $10 AND status IN ('active', 'past_due')
            AND payment_method_id = $11 AND initial_transaction_id = $12
            AND next_renewal_at = $3
        "#,
    )
    .bind(reservation.subscription_id().as_uuid())
    .bind(method_id.as_uuid())
    .bind(reservation.period().start_at())
    .bind(reservation.period().end_at())
    .bind(transaction_id.expose())
    .bind(identity.billing_scope_id().as_uuid())
    .bind(identity.subscriber_id().as_uuid())
    .bind(identity.gateway_account_id().as_uuid())
    .bind(reservation.plan_key().as_str())
    .bind(reservation.expected_state().status().as_str())
    .bind(old_method_id.as_uuid())
    .bind(
        reservation
            .expected_state()
            .initial_transaction_id()
            .expose(),
    )
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
    disable_payment_method_if_unreferenced(connection, old_method_id).await?;
    mark_attempt_approved(
        connection,
        &attempt,
        evidence,
        reservation.subscription_id(),
        method_id,
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

pub(crate) async fn resolve_recovery_non_approved_outcome(
    pool: &PgPool,
    reservation: &SubscriptionRecoveryReservation,
    evidence: &ProcessorEvidence,
    status: AttemptResolutionStatus,
    resolution_code: Option<PaymentResolutionCode>,
    cooldown: Option<RateLimitCooldown>,
    boundary: OutcomeResolutionBoundary,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    resolve_pool_outcome(
        pool,
        OutcomeReservation::Recovery(reservation),
        evidence,
        OutcomeResolutionCommand::non_approved(status, resolution_code, cooldown, boundary),
    )
    .await
    .map(OutcomeApplication::into_payment)
}

async fn resolve_recovery_unknown_outcome(
    pool: &PgPool,
    reservation: &SubscriptionRecoveryReservation,
    evidence: &ProcessorEvidence,
    cooldown: Option<RateLimitCooldown>,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    resolve_pool_outcome(
        pool,
        OutcomeReservation::Recovery(reservation),
        evidence,
        OutcomeResolutionCommand::unknown(cooldown),
    )
    .await
    .map(OutcomeApplication::into_payment)
}
