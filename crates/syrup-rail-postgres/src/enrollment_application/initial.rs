use std::fmt;

use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool};
use syrup_rail::{
    ApprovedProcessorEvidence, BillingEvent, BillingEventSubject, BillingPeriod, BillingScopeId,
    EnrollSubscription, GatewayMutationError, GatewayNotSubmittedError, GatewayPaymentOutcome,
    GatewayPaymentStatus, GatewayProviderKey, GatewaySaleIntent, GatewaySaleRequest,
    GatewayTransactionId, PaymentAttempt, PaymentAttemptId, PaymentAttemptStatus, PaymentMethodId,
    PaymentResolutionCode, ProcessorChargeProgression, ProcessorChargeRole, ProcessorEvidence,
    SubscriptionDiscountDuration, SubscriptionDiscountKind, SubscriptionEnrollmentPaymentResult,
    SubscriptionEnrollmentReservation, SubscriptionEnrollmentSubmissionOutcome,
    SubscriptionEnrollmentSubmissionRejection, SubscriptionId, SubscriptionPhase,
    next_billing_period,
};
use uuid::Uuid;

use crate::{
    BillingTransactionCoordinator, BillingTransactionSubjectState, ModeVerifiedGateway,
    attempts::find_payment_attempt_by_id_on_connection,
    processor_charges::{
        LockFreeApprovedEvidenceTerms, ObservedCharge, observe_processor_charge, transition_charge,
    },
};

use super::{
    APPROVED_APPLICATION_ATTEMPTS, APPROVED_EVIDENCE_RETRY_DELAY, APPROVED_EVIDENCE_WRITE_ATTEMPTS,
    APPROVED_STORAGE_FAILURE_TEXT, AttemptResolutionStatus, BILLING_LOCK_TIMEOUT,
    CURRENT_GRANT_CONFLICT_TEXT, CURRENT_SUBSCRIPTION_CONFLICT_TEXT, GatewayNotSubmittedPolicy,
    INCOMPLETE_APPROVAL_TEXT, INVALID_APPLICATION_STATE, OutcomeApplication, OutcomeReservation,
    OutcomeResolutionBoundary, OutcomeResolutionCommand, RateLimitCooldown,
    SubscriptionEnrollmentApplicationError, TERMINAL_APPROVAL_RACE_TEXT,
    append_subscription_observation_diagnostics, apply_resumable_not_submitted_policy,
    finalize_approved_application, is_retryable_evidence_error, load_applied_subscription,
    load_subscription, lock_expected_reservation_attempt, lock_payment_method_domain,
    lock_subscription_aggregate, mark_attempt_approved, mutation_error_evidence,
    park_locked_attempt, payment_result_for_attempt,
    persist_approved_evidence_without_attempt_lock, resolve_pool_outcome, set_application_timeouts,
    stop_conflicting_subscription_approval, upsert_payment_method,
};

mod approval;

use approval::apply_approved_outcome;

/// One committed final-admission result that authorizes exactly one immediate
/// provider submission by consuming this value.
pub struct AdmittedSubscriptionEnrollment {
    reservation: SubscriptionEnrollmentReservation,
    attempt: PaymentAttempt,
}

impl AdmittedSubscriptionEnrollment {
    pub const fn attempt(&self) -> &PaymentAttempt {
        &self.attempt
    }
}

impl fmt::Debug for AdmittedSubscriptionEnrollment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AdmittedSubscriptionEnrollment")
            .field("attempt", &self.attempt)
            .field("has_submission_authority", &true)
            .finish()
    }
}

#[derive(Debug)]
pub enum SubscriptionEnrollmentAdmissionOutcome {
    Admitted(Box<AdmittedSubscriptionEnrollment>),
    AlreadyAdmitted(PaymentAttempt),
    Rejected {
        attempt: PaymentAttempt,
        reason: SubscriptionEnrollmentSubmissionRejection,
    },
}

#[derive(Debug)]
pub enum SubscriptionEnrollmentProviderResult {
    Payment(SubscriptionEnrollmentPaymentResult),
    /// The provider mutation was not contacted. A retry-safe readiness failure
    /// can carry the same pending, prepared payment for same-key replay. A
    /// concurrent terminal result is returned as `Payment` instead.
    NotSubmitted {
        payment: SubscriptionEnrollmentPaymentResult,
        error: GatewayNotSubmittedError,
    },
}

impl SubscriptionEnrollmentProviderResult {
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
/// Owns and commits final enrollment admission before exposing a one-shot
/// submission capability. A rolled-back transaction can never yield the
/// capability consumed by [`submit_admitted_subscription_enrollment`].
pub async fn admit_subscription_enrollment_submission(
    pool: &PgPool,
    offers: &dyn crate::SubscriptionOfferStore,
    reservation: &SubscriptionEnrollmentReservation,
) -> Result<SubscriptionEnrollmentAdmissionOutcome, SubscriptionEnrollmentApplicationError> {
    let mut transaction = pool.begin().await?;
    let outcome = crate::admit_subscription_enrollment_submission_in_transaction(
        &mut transaction,
        offers,
        reservation,
    )
    .await?;
    transaction.commit().await?;
    Ok(match outcome {
        SubscriptionEnrollmentSubmissionOutcome::Admitted(attempt) => {
            SubscriptionEnrollmentAdmissionOutcome::Admitted(Box::new(
                AdmittedSubscriptionEnrollment {
                    reservation: reservation.clone(),
                    attempt,
                },
            ))
        }
        SubscriptionEnrollmentSubmissionOutcome::AlreadyAdmitted(attempt) => {
            SubscriptionEnrollmentAdmissionOutcome::AlreadyAdmitted(attempt)
        }
        SubscriptionEnrollmentSubmissionOutcome::Rejected { attempt, reason } => {
            SubscriptionEnrollmentAdmissionOutcome::Rejected { attempt, reason }
        }
    })
}

/// Performs the one provider sale authorized by a committed final admission,
/// then applies or durably parks its result.
///
/// This function holds no database transaction or lock across provider I/O.
pub async fn submit_admitted_subscription_enrollment(
    pool: &PgPool,
    coordinator: &dyn BillingTransactionCoordinator,
    admission: AdmittedSubscriptionEnrollment,
    command: &EnrollSubscription,
    gateway: ModeVerifiedGateway<'_>,
) -> Result<SubscriptionEnrollmentProviderResult, SubscriptionEnrollmentApplicationError> {
    let resolved_gateway = gateway.resolved_gateway();
    let reconstructed = SubscriptionEnrollmentReservation::from_command_for_attempt(
        command,
        resolved_gateway,
        admission.attempt.identity().attempt_id(),
        admission
            .reservation
            .identity()
            .required_gateway_account_mode(),
    )
    .map_err(|_| SubscriptionEnrollmentApplicationError::SubmissionIdentityMismatch)?;
    if reconstructed != admission.reservation
        || admission.attempt.identity() != admission.reservation.identity()
        || admission.attempt.status() != PaymentAttemptStatus::Pending
        || admission
            .attempt
            .state()
            .timestamps()
            .submitted_at()
            .is_none()
    {
        return Err(SubscriptionEnrollmentApplicationError::SubmissionIdentityMismatch);
    }
    let Some(gateway) = gateway.authorize_attempt(&admission.reservation.identity()) else {
        return Err(SubscriptionEnrollmentApplicationError::SubmissionIdentityMismatch);
    };

    let charge = syrup_rail::ChargeAmount::new(
        admission.attempt.request().amount().cents(),
        admission.attempt.request().amount().currency(),
    )
    .map_err(|_| SubscriptionEnrollmentApplicationError::InvalidState(INVALID_APPLICATION_STATE))?;
    let request = GatewaySaleRequest::new(
        charge,
        admission.attempt.request().gateway_order_id().clone(),
        GatewaySaleIntent::InitialStoredCredential {
            payment_token: command.payment_token().clone(),
        },
        Some(command.billing_contact().clone()),
    );
    match gateway.sale(request).await {
        Ok(outcome) => apply_subscription_enrollment_gateway_outcome(
            pool,
            coordinator,
            &admission.reservation,
            &outcome,
        )
        .await
        .map(SubscriptionEnrollmentProviderResult::Payment),
        Err(GatewayMutationError::NotSubmitted(error)) => {
            let evidence = mutation_error_evidence(error.detail());
            let policy = GatewayNotSubmittedPolicy::for_error(&error);
            let application = apply_resumable_not_submitted_policy(
                pool,
                OutcomeReservation::Initial(&admission.reservation),
                &evidence,
                policy,
            )
            .await?;
            if application.should_surface_not_submitted(policy) {
                Ok(SubscriptionEnrollmentProviderResult::NotSubmitted {
                    payment: application.payment,
                    error,
                })
            } else {
                Ok(SubscriptionEnrollmentProviderResult::Payment(
                    application.payment,
                ))
            }
        }
        Err(GatewayMutationError::RateLimitedIndeterminate(detail)) => resolve_unknown_outcome(
            pool,
            &admission.reservation,
            &mutation_error_evidence(&detail),
            Some(RateLimitCooldown::Provider),
        )
        .await
        .map(SubscriptionEnrollmentProviderResult::Payment),
        Err(GatewayMutationError::Indeterminate(detail)) => resolve_unknown_outcome(
            pool,
            &admission.reservation,
            &mutation_error_evidence(&detail),
            None,
        )
        .await
        .map(SubscriptionEnrollmentProviderResult::Payment),
    }
}

/// Applies one initial-enrollment gateway outcome to the durable billing ledger.
///
/// Approved application begins through the host coordinator so its recipient
/// authorization lock precedes every shared lock. If the atomic application
/// fails, this operation returns pending confirmation only after either the
/// review-required attempt or immutable processor charge has committed.
pub async fn apply_subscription_enrollment_gateway_outcome(
    pool: &PgPool,
    coordinator: &dyn BillingTransactionCoordinator,
    reservation: &SubscriptionEnrollmentReservation,
    outcome: &GatewayPaymentOutcome,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    apply_subscription_enrollment_gateway_decision(pool, coordinator, reservation, outcome)
        .await
        .map(|result| append_subscription_observation_diagnostics(result, outcome.diagnostics()))
}

async fn apply_subscription_enrollment_gateway_decision(
    pool: &PgPool,
    coordinator: &dyn BillingTransactionCoordinator,
    reservation: &SubscriptionEnrollmentReservation,
    outcome: &GatewayPaymentOutcome,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    match outcome.status() {
        GatewayPaymentStatus::Approved => {
            let approved_evidence = outcome.approved_evidence().ok_or(
                SubscriptionEnrollmentApplicationError::InvalidState(INVALID_APPLICATION_STATE),
            )?;
            if outcome.transaction_id().is_none() || outcome.payment_method_reference().is_none() {
                return park_approved_outcome(
                    pool,
                    reservation,
                    &approved_evidence,
                    INCOMPLETE_APPROVAL_TEXT,
                )
                .await;
            }

            for attempt_index in 0..APPROVED_APPLICATION_ATTEMPTS {
                match apply_approved_outcome(coordinator, reservation, outcome.evidence()).await {
                    Ok(result) => return Ok(result),
                    Err(_) if attempt_index + 1 < APPROVED_APPLICATION_ATTEMPTS => {
                        tokio::time::sleep(APPROVED_EVIDENCE_RETRY_DELAY).await;
                    }
                    Err(_) => break,
                }
            }
            park_approved_outcome(
                pool,
                reservation,
                &approved_evidence,
                APPROVED_STORAGE_FAILURE_TEXT,
            )
            .await
        }
        GatewayPaymentStatus::Declined => {
            resolve_non_approved_outcome(
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
            resolve_non_approved_outcome(
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
            resolve_unknown_outcome(pool, reservation, outcome.evidence(), None).await
        }
    }
}

/// Applies an already-observed provider outcome to an exact durable enrollment attempt.
///
/// This is the reconciliation counterpart to foreground enrollment. It rebuilds
/// the secret-free reservation from immutable attempt state and the canonical
/// gateway account, then enters the same atomic application path. It never
/// resolves a live gateway or submits another provider mutation.
pub async fn apply_reconciled_subscription_enrollment_gateway_outcome(
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
        "subscription enrollment attempt was not found",
    ))?;
    let provider_key = sqlx::query_scalar::<_, String>(
        r#"
        SELECT provider_key
        FROM billing_gateway_accounts
        WHERE billing_scope_id = $1 AND id = $2
        "#,
    )
    .bind(billing_scope_id.as_uuid())
    .bind(attempt.identity().gateway_account_id().as_uuid())
    .fetch_optional(&mut *transaction)
    .await?
    .ok_or(SubscriptionEnrollmentApplicationError::InvalidState(
        "subscription enrollment gateway account was not found",
    ))?;
    transaction.commit().await?;

    let provider_key = GatewayProviderKey::new(provider_key).map_err(|_| {
        SubscriptionEnrollmentApplicationError::InvalidState(
            "subscription enrollment gateway provider key is invalid",
        )
    })?;
    let reservation = SubscriptionEnrollmentReservation::from_attempt(&attempt, provider_key)
        .map_err(|_| {
            SubscriptionEnrollmentApplicationError::InvalidState(
                "reconciled attempt is not a valid subscription enrollment",
            )
        })?;
    apply_subscription_enrollment_gateway_outcome(pool, coordinator, &reservation, outcome).await
}

pub(crate) async fn resolve_non_approved_outcome(
    pool: &PgPool,
    reservation: &SubscriptionEnrollmentReservation,
    evidence: &ProcessorEvidence,
    status: AttemptResolutionStatus,
    resolution_code: Option<PaymentResolutionCode>,
    cooldown: Option<RateLimitCooldown>,
    boundary: OutcomeResolutionBoundary,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    resolve_pool_outcome(
        pool,
        OutcomeReservation::Initial(reservation),
        evidence,
        OutcomeResolutionCommand::non_approved(status, resolution_code, cooldown, boundary),
    )
    .await
    .map(OutcomeApplication::into_payment)
}

async fn resolve_unknown_outcome(
    pool: &PgPool,
    reservation: &SubscriptionEnrollmentReservation,
    evidence: &ProcessorEvidence,
    cooldown: Option<RateLimitCooldown>,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    resolve_pool_outcome(
        pool,
        OutcomeReservation::Initial(reservation),
        evidence,
        OutcomeResolutionCommand::unknown(cooldown),
    )
    .await
    .map(OutcomeApplication::into_payment)
}

async fn park_approved_outcome(
    pool: &PgPool,
    reservation: &SubscriptionEnrollmentReservation,
    approved_evidence: &ApprovedProcessorEvidence,
    message: &'static str,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    let evidence = approved_evidence.evidence();
    match try_park_approved_outcome(pool, reservation, evidence, message).await {
        Ok(result) => Ok(result),
        Err(_) => {
            observe_approved_evidence_with_retry(pool, reservation, evidence).await?;
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
    }
}

async fn try_park_approved_outcome(
    pool: &PgPool,
    reservation: &SubscriptionEnrollmentReservation,
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
        OutcomeReservation::Initial(reservation),
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

async fn observe_approved_evidence_with_retry(
    pool: &PgPool,
    reservation: &SubscriptionEnrollmentReservation,
    evidence: &ProcessorEvidence,
) -> Result<(), SubscriptionEnrollmentApplicationError> {
    let mut last_error = None;
    for attempt_index in 0..APPROVED_EVIDENCE_WRITE_ATTEMPTS {
        let result = async {
            let mut transaction = pool.begin().await?;
            set_application_timeouts(&mut transaction).await?;
            let attempt = lock_expected_reservation_attempt(
                &mut transaction,
                OutcomeReservation::Initial(reservation),
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
            Err(error) if is_retryable_evidence_error(&error) => {
                last_error = Some(error);
                if attempt_index + 1 < APPROVED_EVIDENCE_WRITE_ATTEMPTS {
                    tokio::time::sleep(APPROVED_EVIDENCE_RETRY_DELAY).await;
                } else {
                    break;
                }
            }
            Err(error) => return Err(error),
        }
    }
    let _ = last_error;
    persist_approved_evidence_without_attempt_lock(
        pool,
        LockFreeApprovedEvidenceTerms::initial(reservation),
        evidence,
    )
    .await
}
