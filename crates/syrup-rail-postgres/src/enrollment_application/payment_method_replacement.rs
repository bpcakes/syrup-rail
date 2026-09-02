use std::fmt;

use sqlx::{PgConnection, PgPool, Row};
use syrup_rail::{
    ApprovedProcessorEvidence, BillingEvent, BillingEventSubject, BillingScopeId,
    GatewayMutationError, GatewayNotSubmittedError, GatewayPaymentDiagnostic,
    GatewayPaymentOutcome, GatewayPaymentStatus, GatewayProviderKey,
    GatewayStorePaymentMethodRequest, PaymentAttempt, PaymentAttemptId, PaymentAttemptStatus,
    PaymentCardDisplay, PaymentMethodId, PaymentResolutionCode, ProcessorChargeProgression,
    ProcessorChargeRole, ProcessorEvidence, ReplaceSubscriptionPaymentMethod,
    SubscriptionEnrollmentPaymentResult, SubscriptionPaymentMethodReplacement,
    SubscriptionPaymentMethodReplacementSubmissionOutcome,
    SubscriptionPaymentMethodReplacementSubmissionRejection,
};

use crate::{
    BillingTransactionCoordinator, BillingTransactionSubjectState,
    attempts::find_payment_attempt_by_id_on_connection,
    processor_charges::{
        LockFreeApprovedEvidenceTerms, ObservedCharge, observe_processor_charge, transition_charge,
    },
};

use super::{
    APPROVED_APPLICATION_ATTEMPTS, APPROVED_EVIDENCE_RETRY_DELAY, APPROVED_EVIDENCE_WRITE_ATTEMPTS,
    AttemptResolutionStatus, BILLING_LOCK_TIMEOUT, GatewayNotSubmittedPolicy,
    INVALID_APPLICATION_STATE, OutcomeApplication, OutcomeReservation, OutcomeResolutionBoundary,
    OutcomeResolutionCommand, PAYMENT_METHOD_REPLACEMENT_INCOMPLETE_APPROVAL_TEXT,
    PAYMENT_METHOD_REPLACEMENT_STALE_STATE_TEXT, PAYMENT_METHOD_REPLACEMENT_STORAGE_FAILURE_TEXT,
    RateLimitCooldown, SubscriptionEnrollmentApplicationError,
    append_subscription_observation_diagnostics, apply_resumable_not_submitted_policy,
    disable_payment_method_if_unreferenced, finalize_approved_application,
    is_retryable_evidence_error, load_applied_subscription, load_subscription,
    lock_expected_reservation_attempt, lock_payment_method_domain, lock_subscription_aggregate,
    mark_attempt_approved, mutation_error_evidence, park_locked_attempt,
    payment_result_for_attempt, persist_approved_evidence_without_attempt_lock,
    reconcile_non_approved_evidence, resolve_locked_outcome, resolve_pool_outcome,
    set_application_timeouts, stop_conflicting_subscription_approval, upsert_payment_method,
};

mod approval;

use approval::{
    apply_locked_payment_method_replacement_approved_outcome,
    apply_payment_method_replacement_approved_outcome,
    lock_payment_method_replacement_application_attempt,
};

/// One committed final-admission result authorizing exactly one immediate
/// Customer Vault mutation.
pub struct AdmittedSubscriptionPaymentMethodReplacement {
    reservation: SubscriptionPaymentMethodReplacement,
    attempt: PaymentAttempt,
}

impl AdmittedSubscriptionPaymentMethodReplacement {
    pub const fn attempt(&self) -> &PaymentAttempt {
        &self.attempt
    }
}

impl fmt::Debug for AdmittedSubscriptionPaymentMethodReplacement {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AdmittedSubscriptionPaymentMethodReplacement")
            .field("attempt", &self.attempt)
            .field("has_submission_authority", &true)
            .finish()
    }
}

#[derive(Debug)]
pub enum SubscriptionPaymentMethodReplacementAdmissionOutcome {
    Admitted(Box<AdmittedSubscriptionPaymentMethodReplacement>),
    AlreadyAdmitted(PaymentAttempt),
    Rejected {
        attempt: PaymentAttempt,
        reason: SubscriptionPaymentMethodReplacementSubmissionRejection,
    },
}

#[derive(Debug)]
pub enum SubscriptionPaymentMethodReplacementProviderResult {
    Payment(SubscriptionEnrollmentPaymentResult),
    /// The provider mutation was not contacted. A retry-safe readiness failure
    /// can carry the same pending, prepared payment for same-key replay. A
    /// concurrent terminal result is returned as `Payment` instead.
    NotSubmitted {
        payment: SubscriptionEnrollmentPaymentResult,
        error: GatewayNotSubmittedError,
    },
}

impl SubscriptionPaymentMethodReplacementProviderResult {
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

/// Commits final payment-method replacement admission before exposing its
/// one-shot Customer Vault capability.
pub async fn admit_subscription_payment_method_replacement(
    pool: &PgPool,
    reservation: &SubscriptionPaymentMethodReplacement,
) -> Result<
    SubscriptionPaymentMethodReplacementAdmissionOutcome,
    SubscriptionEnrollmentApplicationError,
> {
    let mut transaction = pool.begin().await?;
    let outcome = crate::admit_subscription_payment_method_replacement_in_transaction(
        &mut transaction,
        reservation,
    )
    .await?;
    transaction.commit().await?;
    Ok(match outcome {
        SubscriptionPaymentMethodReplacementSubmissionOutcome::Admitted(attempt) => {
            SubscriptionPaymentMethodReplacementAdmissionOutcome::Admitted(Box::new(
                AdmittedSubscriptionPaymentMethodReplacement {
                    reservation: reservation.clone(),
                    attempt,
                },
            ))
        }
        SubscriptionPaymentMethodReplacementSubmissionOutcome::AlreadyAdmitted(attempt) => {
            SubscriptionPaymentMethodReplacementAdmissionOutcome::AlreadyAdmitted(attempt)
        }
        SubscriptionPaymentMethodReplacementSubmissionOutcome::Rejected { attempt, reason } => {
            SubscriptionPaymentMethodReplacementAdmissionOutcome::Rejected { attempt, reason }
        }
    })
}

/// Performs the one Customer Vault mutation authorized by committed admission.
pub async fn submit_admitted_subscription_payment_method_replacement(
    pool: &PgPool,
    coordinator: &dyn BillingTransactionCoordinator,
    admission: AdmittedSubscriptionPaymentMethodReplacement,
    command: &ReplaceSubscriptionPaymentMethod,
    gateway: crate::ModeVerifiedGateway<'_>,
) -> Result<
    SubscriptionPaymentMethodReplacementProviderResult,
    SubscriptionEnrollmentApplicationError,
> {
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
    let request = GatewayStorePaymentMethodRequest::new(
        command.payment_token().clone(),
        admission.attempt.request().gateway_order_id().clone(),
        Some(command.billing_contact().clone()),
    );
    match gateway.store_payment_method(request).await {
        Ok(outcome) => apply_subscription_payment_method_replacement_gateway_outcome(
            pool,
            coordinator,
            &admission.reservation,
            &outcome,
        )
        .await
        .map(SubscriptionPaymentMethodReplacementProviderResult::Payment),
        Err(GatewayMutationError::NotSubmitted(error)) => {
            let evidence = mutation_error_evidence(error.detail());
            let policy = GatewayNotSubmittedPolicy::for_error(&error);
            let application = apply_resumable_not_submitted_policy(
                pool,
                OutcomeReservation::PaymentMethodReplacement(&admission.reservation),
                &evidence,
                policy,
            )
            .await?;
            if application.should_surface_not_submitted(policy) {
                Ok(
                    SubscriptionPaymentMethodReplacementProviderResult::NotSubmitted {
                        payment: application.payment,
                        error,
                    },
                )
            } else {
                Ok(SubscriptionPaymentMethodReplacementProviderResult::Payment(
                    application.payment,
                ))
            }
        }
        Err(GatewayMutationError::RateLimitedIndeterminate(detail)) => {
            resolve_payment_method_replacement_unknown_outcome(
                pool,
                &admission.reservation,
                &mutation_error_evidence(&detail),
                Some(RateLimitCooldown::Provider),
            )
            .await
            .map(SubscriptionPaymentMethodReplacementProviderResult::Payment)
        }
        Err(GatewayMutationError::Indeterminate(detail)) => {
            resolve_payment_method_replacement_unknown_outcome(
                pool,
                &admission.reservation,
                &mutation_error_evidence(&detail),
                None,
            )
            .await
            .map(SubscriptionPaymentMethodReplacementProviderResult::Payment)
        }
    }
}

pub async fn apply_subscription_payment_method_replacement_gateway_outcome(
    pool: &PgPool,
    coordinator: &dyn BillingTransactionCoordinator,
    reservation: &SubscriptionPaymentMethodReplacement,
    outcome: &GatewayPaymentOutcome,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    apply_subscription_payment_method_replacement_gateway_decision(
        pool,
        coordinator,
        reservation,
        outcome,
    )
    .await
    .map(|result| append_subscription_observation_diagnostics(result, outcome.diagnostics()))
}

async fn apply_subscription_payment_method_replacement_gateway_decision(
    pool: &PgPool,
    coordinator: &dyn BillingTransactionCoordinator,
    reservation: &SubscriptionPaymentMethodReplacement,
    outcome: &GatewayPaymentOutcome,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    match outcome.status() {
        GatewayPaymentStatus::Approved => {
            let approved_evidence = outcome.approved_evidence().ok_or(
                SubscriptionEnrollmentApplicationError::InvalidState(INVALID_APPLICATION_STATE),
            )?;
            if outcome.transaction_id().is_none() || outcome.payment_method_reference().is_none() {
                return park_payment_method_replacement_approved_outcome(
                    pool,
                    reservation,
                    &approved_evidence,
                    PAYMENT_METHOD_REPLACEMENT_INCOMPLETE_APPROVAL_TEXT,
                )
                .await;
            }
            for attempt_index in 0..APPROVED_APPLICATION_ATTEMPTS {
                match apply_payment_method_replacement_approved_outcome(
                    coordinator,
                    reservation,
                    &approved_evidence,
                )
                .await
                {
                    Ok(result) => return Ok(result),
                    Err(_) if attempt_index + 1 < APPROVED_APPLICATION_ATTEMPTS => {
                        tokio::time::sleep(APPROVED_EVIDENCE_RETRY_DELAY).await;
                    }
                    Err(_) => break,
                }
            }
            park_payment_method_replacement_approved_outcome(
                pool,
                reservation,
                &approved_evidence,
                PAYMENT_METHOD_REPLACEMENT_STORAGE_FAILURE_TEXT,
            )
            .await
        }
        GatewayPaymentStatus::Declined => {
            resolve_payment_method_replacement_non_approved_outcome(
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
            resolve_payment_method_replacement_non_approved_outcome(
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
            resolve_payment_method_replacement_unknown_outcome(
                pool,
                reservation,
                outcome.evidence(),
                None,
            )
            .await
        }
    }
}

/// Applies an observed replacement outcome after comparing it with the current
/// durable evidence under the canonical host, aggregate, and attempt locks.
pub async fn apply_reconciled_subscription_payment_method_replacement_gateway_outcome(
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
        "payment method replacement attempt was not found",
    ))?;
    let provider_key = sqlx::query_scalar::<_, String>(
        "SELECT provider_key FROM billing_gateway_accounts WHERE billing_scope_id = $1 AND id = $2",
    )
    .bind(billing_scope_id.as_uuid())
    .bind(attempt.identity().gateway_account_id().as_uuid())
    .fetch_optional(&mut *transaction)
    .await?
    .ok_or(SubscriptionEnrollmentApplicationError::InvalidState(
        "payment method replacement gateway account was not found",
    ))?;
    transaction.commit().await?;
    let provider_key = GatewayProviderKey::new(provider_key).map_err(|_| {
        SubscriptionEnrollmentApplicationError::InvalidState(
            "payment method replacement gateway provider key is invalid",
        )
    })?;
    let reservation = SubscriptionPaymentMethodReplacement::from_attempt(&attempt, provider_key)
        .map_err(|_| {
            SubscriptionEnrollmentApplicationError::InvalidState(
                "reconciled attempt is not a valid payment method replacement",
            )
        })?;
    if outcome.status() == GatewayPaymentStatus::Approved {
        for attempt_index in 0..APPROVED_APPLICATION_ATTEMPTS {
            match apply_locked_reconciled_payment_method_replacement_outcome(
                coordinator,
                &reservation,
                outcome,
            )
            .await
            {
                Ok(result) => return Ok(result),
                Err(_) if attempt_index + 1 < APPROVED_APPLICATION_ATTEMPTS => {
                    tokio::time::sleep(APPROVED_EVIDENCE_RETRY_DELAY).await;
                }
                Err(_) => break,
            }
        }
        return park_reconciled_payment_method_replacement_approved_outcome(
            pool,
            &reservation,
            outcome,
            PAYMENT_METHOD_REPLACEMENT_STORAGE_FAILURE_TEXT,
        )
        .await;
    }
    apply_locked_reconciled_non_approved_payment_method_replacement_outcome(
        pool,
        &reservation,
        outcome,
    )
    .await
}

async fn apply_locked_reconciled_non_approved_payment_method_replacement_outcome(
    pool: &PgPool,
    reservation: &SubscriptionPaymentMethodReplacement,
    observed_outcome: &GatewayPaymentOutcome,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    let identity = reservation.identity();
    let mut transaction = pool.begin().await?;
    set_application_timeouts(&mut transaction).await?;
    lock_subscription_aggregate(
        &mut transaction,
        identity.subscriber_id(),
        reservation.plan_key(),
    )
    .await?;
    let attempt = lock_expected_reservation_attempt(
        &mut transaction,
        OutcomeReservation::PaymentMethodReplacement(reservation),
    )
    .await?;
    let reconciled = reconciled_outcome_with_persisted_evidence(&attempt, observed_outcome);
    let diagnostics = reconciled.observation_diagnostics;
    let outcome = reconciled.outcome;
    let resolution = match outcome.status() {
        GatewayPaymentStatus::Declined => OutcomeResolutionCommand::non_approved(
            AttemptResolutionStatus::Declined,
            None,
            None,
            OutcomeResolutionBoundary::Submitted,
        ),
        GatewayPaymentStatus::Failed => OutcomeResolutionCommand::non_approved(
            AttemptResolutionStatus::Failed,
            None,
            None,
            OutcomeResolutionBoundary::Submitted,
        ),
        GatewayPaymentStatus::Unknown => OutcomeResolutionCommand::unknown(None),
        GatewayPaymentStatus::Approved => {
            return Err(SubscriptionEnrollmentApplicationError::InvalidState(
                INVALID_APPLICATION_STATE,
            ));
        }
    };
    let application = resolve_locked_outcome(
        &mut transaction,
        OutcomeReservation::PaymentMethodReplacement(reservation),
        attempt,
        outcome.evidence(),
        resolution,
    )
    .await?;
    transaction.commit().await?;
    Ok(application
        .into_payment()
        .with_observation_diagnostics(diagnostics))
}

async fn apply_locked_reconciled_payment_method_replacement_outcome(
    coordinator: &dyn BillingTransactionCoordinator,
    reservation: &SubscriptionPaymentMethodReplacement,
    observed_outcome: &GatewayPaymentOutcome,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    let identity = reservation.identity();
    let mut transaction = coordinator
        .begin(
            BillingEventSubject::new(identity.billing_scope_id(), identity.subscriber_id()),
            BILLING_LOCK_TIMEOUT,
        )
        .await?;
    let subject_state = transaction.subject_state();
    let (application, diagnostics) = {
        let connection = transaction.connection();
        let attempt =
            lock_payment_method_replacement_application_attempt(connection, reservation).await?;
        let reconciled = reconciled_outcome_with_persisted_evidence(&attempt, observed_outcome);
        let diagnostics = reconciled.observation_diagnostics;
        if let Some(evidence) = reconciled.conflicting_approved_evidence.as_ref() {
            observe_processor_charge(
                connection,
                &attempt,
                evidence,
                ProcessorChargeProgression::ReconciliationRequired,
            )
            .await?;
        }
        let outcome = reconciled.outcome;
        let application = apply_locked_reconciled_payment_method_replacement_decision(
            connection,
            subject_state,
            reservation,
            attempt,
            &outcome,
        )
        .await;
        (application, diagnostics)
    };
    finalize_approved_application(transaction, application)
        .await
        .map(|result| result.with_observation_diagnostics(diagnostics))
}

async fn apply_locked_reconciled_payment_method_replacement_decision(
    connection: &mut PgConnection,
    subject_state: BillingTransactionSubjectState,
    reservation: &SubscriptionPaymentMethodReplacement,
    attempt: PaymentAttempt,
    outcome: &GatewayPaymentOutcome,
) -> Result<
    (SubscriptionEnrollmentPaymentResult, Option<BillingEvent>),
    SubscriptionEnrollmentApplicationError,
> {
    match outcome.status() {
        GatewayPaymentStatus::Approved => {
            let approved_evidence = outcome.approved_evidence().ok_or(
                SubscriptionEnrollmentApplicationError::InvalidState(INVALID_APPLICATION_STATE),
            )?;
            let missing_identity =
                outcome.transaction_id().is_none() || outcome.payment_method_reference().is_none();
            if attempt.status().is_resolvable() && missing_identity {
                let parked = park_locked_payment_method_replacement_approved_outcome(
                    connection,
                    attempt,
                    outcome.evidence(),
                    PAYMENT_METHOD_REPLACEMENT_INCOMPLETE_APPROVAL_TEXT,
                )
                .await?;
                return Ok((parked, None));
            }
            apply_locked_payment_method_replacement_approved_outcome(
                connection,
                subject_state,
                reservation,
                attempt,
                &approved_evidence,
            )
            .await
        }
        GatewayPaymentStatus::Declined => resolve_locked_outcome(
            connection,
            OutcomeReservation::PaymentMethodReplacement(reservation),
            attempt,
            outcome.evidence(),
            OutcomeResolutionCommand::non_approved(
                AttemptResolutionStatus::Declined,
                None,
                None,
                OutcomeResolutionBoundary::Submitted,
            ),
        )
        .await
        .map(|application| (application.into_payment(), None)),
        GatewayPaymentStatus::Failed => resolve_locked_outcome(
            connection,
            OutcomeReservation::PaymentMethodReplacement(reservation),
            attempt,
            outcome.evidence(),
            OutcomeResolutionCommand::non_approved(
                AttemptResolutionStatus::Failed,
                None,
                None,
                OutcomeResolutionBoundary::Submitted,
            ),
        )
        .await
        .map(|application| (application.into_payment(), None)),
        GatewayPaymentStatus::Unknown => resolve_locked_outcome(
            connection,
            OutcomeReservation::PaymentMethodReplacement(reservation),
            attempt,
            outcome.evidence(),
            OutcomeResolutionCommand::unknown(None),
        )
        .await
        .map(|application| (application.into_payment(), None)),
    }
}

struct ReconciledPaymentMethodReplacementOutcome {
    outcome: GatewayPaymentOutcome,
    observation_diagnostics: Vec<GatewayPaymentDiagnostic>,
    conflicting_approved_evidence: Option<ProcessorEvidence>,
}

fn reconciled_outcome_with_persisted_evidence(
    attempt: &PaymentAttempt,
    outcome: &GatewayPaymentOutcome,
) -> ReconciledPaymentMethodReplacementOutcome {
    let observed = outcome.evidence();
    let persisted = attempt.state().processor_evidence();
    let is_resolvable = attempt.status().is_resolvable();
    let reconciled_evidence = reconcile_non_approved_evidence(attempt, observed);
    let reconciled_identity_diagnostics = reconciled_evidence.identity_conflict_diagnostics();
    let transaction_id_conflict = reconciled_evidence.transaction_id_conflict;
    let payment_method_reference_conflict = reconciled_evidence.payment_method_reference_conflict;
    let diagnosed_identity_conflict = is_resolvable
        && matches!(
            outcome.status(),
            GatewayPaymentStatus::Approved | GatewayPaymentStatus::Unknown
        )
        && (outcome
            .has_diagnostic(GatewayPaymentDiagnostic::InvalidOrConflictingTransactionIdentifier)
            || outcome.has_diagnostic(
                GatewayPaymentDiagnostic::InvalidOrConflictingPaymentMethodReference,
            ));
    let identity_conflict = is_resolvable
        && (transaction_id_conflict
            || payment_method_reference_conflict
            || diagnosed_identity_conflict);
    let evidence = if !is_resolvable {
        observed.clone()
    } else if identity_conflict {
        persisted.clone()
    } else if outcome.status() == GatewayPaymentStatus::Approved {
        // An approval may authorize a stored-method rebind, so every identity
        // it needs must come from that approving observation itself. Never
        // restore a missing or quarantined identity from older evidence.
        observed.clone()
    } else {
        reconciled_evidence.evidence
    };
    let mut observation_diagnostics = outcome.diagnostics().to_vec();
    for diagnostic in reconciled_identity_diagnostics {
        if !observation_diagnostics.contains(&diagnostic) {
            observation_diagnostics.push(diagnostic);
        }
    }
    ReconciledPaymentMethodReplacementOutcome {
        outcome: GatewayPaymentOutcome::new(
            if identity_conflict {
                GatewayPaymentStatus::Unknown
            } else {
                outcome.status()
            },
            evidence,
        ),
        observation_diagnostics,
        conflicting_approved_evidence: (identity_conflict
            && outcome.status() == GatewayPaymentStatus::Approved)
            .then(|| observed.clone()),
    }
}

pub(crate) async fn resolve_payment_method_replacement_non_approved_outcome(
    pool: &PgPool,
    reservation: &SubscriptionPaymentMethodReplacement,
    evidence: &ProcessorEvidence,
    status: AttemptResolutionStatus,
    resolution_code: Option<PaymentResolutionCode>,
    cooldown: Option<RateLimitCooldown>,
    boundary: OutcomeResolutionBoundary,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    resolve_pool_outcome(
        pool,
        OutcomeReservation::PaymentMethodReplacement(reservation),
        evidence,
        OutcomeResolutionCommand::non_approved(status, resolution_code, cooldown, boundary),
    )
    .await
    .map(OutcomeApplication::into_payment)
}

async fn resolve_payment_method_replacement_unknown_outcome(
    pool: &PgPool,
    reservation: &SubscriptionPaymentMethodReplacement,
    evidence: &ProcessorEvidence,
    cooldown: Option<RateLimitCooldown>,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    resolve_pool_outcome(
        pool,
        OutcomeReservation::PaymentMethodReplacement(reservation),
        evidence,
        OutcomeResolutionCommand::unknown(cooldown),
    )
    .await
    .map(OutcomeApplication::into_payment)
}

async fn park_payment_method_replacement_approved_outcome(
    pool: &PgPool,
    reservation: &SubscriptionPaymentMethodReplacement,
    approved_evidence: &ApprovedProcessorEvidence,
    message: &'static str,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    let evidence = approved_evidence.evidence();
    match try_park_payment_method_replacement_approved_outcome(pool, reservation, evidence, message)
        .await
    {
        Ok(result) => Ok(result),
        Err(_) => {
            recover_payment_method_replacement_approved_evidence(
                pool,
                reservation,
                approved_evidence,
            )
            .await
        }
    }
}

async fn park_reconciled_payment_method_replacement_approved_outcome(
    pool: &PgPool,
    reservation: &SubscriptionPaymentMethodReplacement,
    observed_outcome: &GatewayPaymentOutcome,
    storage_failure_message: &'static str,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    for attempt_index in 0..APPROVED_EVIDENCE_WRITE_ATTEMPTS {
        match try_park_reconciled_payment_method_replacement_approved_outcome(
            pool,
            reservation,
            observed_outcome,
            storage_failure_message,
        )
        .await
        {
            Ok(result) => return Ok(result),
            Err(error)
                if is_retryable_evidence_error(&error)
                    && attempt_index + 1 < APPROVED_EVIDENCE_WRITE_ATTEMPTS =>
            {
                tokio::time::sleep(APPROVED_EVIDENCE_RETRY_DELAY).await;
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("the approved evidence retry count is nonzero")
}

async fn recover_payment_method_replacement_approved_evidence(
    pool: &PgPool,
    reservation: &SubscriptionPaymentMethodReplacement,
    approved_evidence: &ApprovedProcessorEvidence,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    observe_payment_method_replacement_approved_evidence_with_retry(
        pool,
        reservation,
        approved_evidence.evidence(),
    )
    .await?;
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
            approved_evidence.clone(),
        )?
    };
    transaction.commit().await?;
    Ok(result)
}

async fn try_park_payment_method_replacement_approved_outcome(
    pool: &PgPool,
    reservation: &SubscriptionPaymentMethodReplacement,
    evidence: &ProcessorEvidence,
    message: &'static str,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    let mut transaction = pool.begin().await?;
    set_application_timeouts(&mut transaction).await?;
    let attempt = lock_expected_reservation_attempt(
        &mut transaction,
        OutcomeReservation::PaymentMethodReplacement(reservation),
    )
    .await?;
    let result = park_locked_payment_method_replacement_approved_outcome(
        &mut transaction,
        attempt,
        evidence,
        message,
    )
    .await?;
    transaction.commit().await?;
    Ok(result)
}

async fn try_park_reconciled_payment_method_replacement_approved_outcome(
    pool: &PgPool,
    reservation: &SubscriptionPaymentMethodReplacement,
    observed_outcome: &GatewayPaymentOutcome,
    storage_failure_message: &'static str,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    let mut transaction = pool.begin().await?;
    set_application_timeouts(&mut transaction).await?;
    let attempt = lock_expected_reservation_attempt(
        &mut transaction,
        OutcomeReservation::PaymentMethodReplacement(reservation),
    )
    .await?;
    let reconciled = reconciled_outcome_with_persisted_evidence(&attempt, observed_outcome);
    let diagnostics = reconciled.observation_diagnostics;
    if let Some(evidence) = reconciled.conflicting_approved_evidence.as_ref() {
        observe_processor_charge(
            &mut transaction,
            &attempt,
            evidence,
            ProcessorChargeProgression::ReconciliationRequired,
        )
        .await?;
    }
    let outcome = reconciled.outcome;
    let result = match outcome.status() {
        GatewayPaymentStatus::Approved => {
            let message = if outcome.transaction_id().is_none()
                || outcome.payment_method_reference().is_none()
            {
                PAYMENT_METHOD_REPLACEMENT_INCOMPLETE_APPROVAL_TEXT
            } else {
                storage_failure_message
            };
            park_locked_payment_method_replacement_approved_outcome(
                &mut transaction,
                attempt,
                outcome.evidence(),
                message,
            )
            .await?
        }
        GatewayPaymentStatus::Unknown => resolve_locked_outcome(
            &mut transaction,
            OutcomeReservation::PaymentMethodReplacement(reservation),
            attempt,
            outcome.evidence(),
            OutcomeResolutionCommand::unknown(None),
        )
        .await?
        .into_payment(),
        GatewayPaymentStatus::Declined | GatewayPaymentStatus::Failed => {
            return Err(SubscriptionEnrollmentApplicationError::InvalidState(
                INVALID_APPLICATION_STATE,
            ));
        }
    };
    transaction.commit().await?;
    Ok(result.with_observation_diagnostics(diagnostics))
}

async fn park_locked_payment_method_replacement_approved_outcome(
    connection: &mut PgConnection,
    attempt: PaymentAttempt,
    evidence: &ProcessorEvidence,
    message: &'static str,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    let attempt = if attempt.status() == PaymentAttemptStatus::Approved {
        observe_processor_charge(
            &mut *connection,
            &attempt,
            evidence,
            ProcessorChargeProgression::Applied,
        )
        .await?;
        attempt
    } else if attempt.status().is_terminal() {
        observe_processor_charge(
            &mut *connection,
            &attempt,
            evidence,
            ProcessorChargeProgression::ReconciliationRequired,
        )
        .await?;
        attempt
    } else {
        let observation = observe_processor_charge(
            &mut *connection,
            &attempt,
            evidence,
            ProcessorChargeProgression::Pending,
        )
        .await?;
        let message = match observation {
            ObservedCharge::Owned(_) => message,
            ObservedCharge::OwnedByOtherAttempt => {
                "The approved gateway transaction is already owned by another payment attempt."
            }
        };
        park_locked_attempt(&mut *connection, &attempt, evidence, None, message).await?
    };
    payment_result_for_attempt(connection, attempt).await
}

async fn observe_payment_method_replacement_approved_evidence_with_retry(
    pool: &PgPool,
    reservation: &SubscriptionPaymentMethodReplacement,
    evidence: &ProcessorEvidence,
) -> Result<(), SubscriptionEnrollmentApplicationError> {
    for attempt_index in 0..APPROVED_EVIDENCE_WRITE_ATTEMPTS {
        let result = async {
            let mut transaction = pool.begin().await?;
            set_application_timeouts(&mut transaction).await?;
            let attempt = lock_expected_reservation_attempt(
                &mut transaction,
                OutcomeReservation::PaymentMethodReplacement(reservation),
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
    persist_approved_evidence_without_attempt_lock(
        pool,
        LockFreeApprovedEvidenceTerms::payment_method_replacement(reservation),
        evidence,
    )
    .await
}
