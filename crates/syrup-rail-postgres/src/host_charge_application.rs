use std::{fmt, time::Duration};

use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool};
use syrup_rail::{
    ApprovedProcessorEvidence, BillingEvent, BillingEventSubject, ChargeHostTarget,
    GatewayMutationError, GatewayNotSubmittedError, GatewayPaymentOutcome, GatewayPaymentStatus,
    GatewaySaleIntent, GatewaySaleRequest, HostChargePaymentResult,
    HostChargePaymentResultBuildError, HostChargeReservation, HostChargeTargetTransition,
    HostChargeTargetTransitionKind, HostChargeTargetTransitionOutcome, PaymentAttempt,
    PaymentAttemptStatus, PaymentResolutionCode, ProcessorChargeProgression, ProcessorChargeRole,
    ProcessorEvidence, ResolvedGateway,
};
use thiserror::Error;

use crate::{
    BillingTransactionCoordinator, BillingTransactionError, BillingTransactionSubjectState,
    HostChargeStoreError, HostChargeSubmissionOutcome, HostChargeTargetError,
    HostChargeTargetStore, ProcessorChargeStoreError, admit_host_charge_submission_in_transaction,
    attempts::{
        AttemptApproval, AttemptResolutionStatus, AttemptTransition, PaymentAttemptStoreError,
        find_payment_attempt_by_id_on_connection, lock_payment_attempt_by_id_on_connection,
        persist_attempt_transition,
    },
    enrollment_application::{
        OutcomeResolutionBoundary, RateLimitCooldown, SubscriptionEnrollmentApplicationError,
        map_attempt_transition_error, mutation_error_evidence, not_submitted_resolution_code,
        park_locked_attempt, set_application_timeouts,
    },
    processor_charges::{ObservedCharge, observe_processor_charge, transition_charge},
};

const BILLING_LOCK_TIMEOUT: Duration = Duration::from_millis(250);
const APPROVED_APPLICATION_ATTEMPTS: usize = 3;
const APPROVED_EVIDENCE_RETRY_DELAY: Duration = Duration::from_millis(50);
const INVALID_HOST_CHARGE_STATE: &str = "canonical host charge application state is invalid";
const APPROVED_STALE_TARGET_TEXT: &str =
    "Approved host charge could not update its target because the target changed.";
const APPROVED_STORAGE_FAILURE_TEXT: &str =
    "Approved host charge could not be applied; manual review is required.";

#[derive(Debug, Error)]
pub enum HostChargeApplicationError {
    #[error("host charge application storage failed")]
    Sql(#[from] sqlx::Error),
    #[error("host charge attempt storage failed")]
    Attempt(#[from] PaymentAttemptStoreError),
    #[error("host charge target operation failed")]
    Target(#[from] HostChargeTargetError),
    #[error("host charge storage operation failed")]
    Store(#[from] HostChargeStoreError),
    #[error("host charge processor evidence operation failed")]
    ProcessorCharge(#[from] ProcessorChargeStoreError),
    #[error("host billing transaction failed")]
    Transaction(#[from] BillingTransactionError),
    #[error("host billing event append failed")]
    Event(#[from] crate::BillingEventWriteError),
    #[error("shared payment application failed")]
    SharedApplication(#[from] SubscriptionEnrollmentApplicationError),
    #[error("admitted host charge does not match the submission command or gateway")]
    SubmissionIdentityMismatch,
    #[error("{0}")]
    InvalidState(&'static str),
}

impl From<HostChargePaymentResultBuildError> for HostChargeApplicationError {
    fn from(_: HostChargePaymentResultBuildError) -> Self {
        Self::InvalidState(INVALID_HOST_CHARGE_STATE)
    }
}

pub struct AdmittedHostCharge {
    reservation: HostChargeReservation,
    attempt: PaymentAttempt,
}

impl AdmittedHostCharge {
    pub const fn attempt(&self) -> &PaymentAttempt {
        &self.attempt
    }
}

impl fmt::Debug for AdmittedHostCharge {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AdmittedHostCharge")
            .field("attempt", &self.attempt)
            .field("has_submission_authority", &true)
            .finish()
    }
}

#[derive(Debug)]
pub enum HostChargeAdmissionOutcome {
    Admitted(Box<AdmittedHostCharge>),
    AlreadyAdmitted(PaymentAttempt),
    Rejected {
        attempt: PaymentAttempt,
        reason: syrup_rail::HostChargeTargetRejection,
    },
}

#[derive(Debug)]
pub enum HostChargeProviderResult {
    Payment(HostChargePaymentResult),
    NotSubmitted {
        payment: HostChargePaymentResult,
        error: GatewayNotSubmittedError,
    },
}

/// The only pre-submission states host charge can resolve from. Keeping the
/// boundary and cooldown together makes the two formerly positional flags
/// explicit at the application boundary.
#[derive(Clone, Copy)]
pub(crate) struct HostChargeBeforeSubmissionResolution {
    boundary: OutcomeResolutionBoundary,
    cooldown: Option<RateLimitCooldown>,
}

impl HostChargeBeforeSubmissionResolution {
    pub(crate) const fn prepared() -> Self {
        Self {
            boundary: OutcomeResolutionBoundary::Prepared,
            cooldown: None,
        }
    }

    pub(crate) const fn admitted_not_submitted() -> Self {
        Self {
            boundary: OutcomeResolutionBoundary::AdmittedNotSubmitted,
            cooldown: None,
        }
    }

    pub(crate) const fn prepared_provider_rate_limited() -> Self {
        Self {
            boundary: OutcomeResolutionBoundary::Prepared,
            cooldown: Some(RateLimitCooldown::Provider),
        }
    }
}

pub async fn admit_host_charge_submission(
    pool: &PgPool,
    targets: &dyn HostChargeTargetStore,
    reservation: &HostChargeReservation,
) -> Result<HostChargeAdmissionOutcome, HostChargeApplicationError> {
    let mut transaction = pool.begin().await?;
    let outcome =
        admit_host_charge_submission_in_transaction(&mut transaction, targets, reservation).await?;
    transaction.commit().await?;
    Ok(match outcome {
        HostChargeSubmissionOutcome::Admitted(attempt) => {
            HostChargeAdmissionOutcome::Admitted(Box::new(AdmittedHostCharge {
                reservation: reservation.clone(),
                attempt,
            }))
        }
        HostChargeSubmissionOutcome::AlreadyAdmitted(attempt) => {
            HostChargeAdmissionOutcome::AlreadyAdmitted(attempt)
        }
        HostChargeSubmissionOutcome::Rejected { attempt, reason } => {
            HostChargeAdmissionOutcome::Rejected { attempt, reason }
        }
    })
}

pub async fn submit_admitted_host_charge(
    pool: &PgPool,
    coordinator: &dyn BillingTransactionCoordinator,
    targets: &dyn HostChargeTargetStore,
    admission: AdmittedHostCharge,
    command: &ChargeHostTarget,
    gateway: &ResolvedGateway,
) -> Result<HostChargeProviderResult, HostChargeApplicationError> {
    let reconstructed = HostChargeReservation::from_command(
        command,
        admission.reservation.snapshot(),
        gateway,
        admission.attempt.identity().attempt_id(),
    )
    .map_err(|_| HostChargeApplicationError::SubmissionIdentityMismatch)?;
    if !host_charge_submission_matches_reservation(&reconstructed, &admission.reservation)
        || admission.attempt.identity() != admission.reservation.identity()
        || admission.attempt.request() != admission.reservation.request()
        || admission.attempt.status() != PaymentAttemptStatus::Pending
        || admission
            .attempt
            .state()
            .timestamps()
            .submitted_at()
            .is_none()
    {
        return Err(HostChargeApplicationError::SubmissionIdentityMismatch);
    }
    let request = GatewaySaleRequest::new(
        admission.reservation.snapshot().charge(),
        admission.attempt.request().gateway_order_id().clone(),
        GatewaySaleIntent::OneTime {
            payment_token: command.payment_token().clone(),
        },
        command.billing_contact().cloned(),
    );
    match gateway.sale(request).await {
        Ok(outcome) => apply_host_charge_gateway_outcome(
            pool,
            coordinator,
            targets,
            &admission.reservation,
            &outcome,
        )
        .await
        .map(HostChargeProviderResult::Payment),
        Err(GatewayMutationError::NotSubmitted(error)) => {
            let evidence = mutation_error_evidence(error.detail());
            let release_target = !matches!(error, GatewayNotSubmittedError::RateLimited(_));
            let payment = resolve_host_charge_non_approved(
                pool,
                targets,
                &admission.reservation,
                &evidence,
                AttemptResolutionStatus::Failed,
                Some(not_submitted_resolution_code(&error)),
                OutcomeResolutionBoundary::AdmittedNotSubmitted,
                release_target,
                None,
            )
            .await?;
            Ok(HostChargeProviderResult::NotSubmitted { payment, error })
        }
        Err(GatewayMutationError::RateLimitedIndeterminate(detail)) => resolve_host_charge_unknown(
            pool,
            &admission.reservation,
            &mutation_error_evidence(&detail),
            Some(RateLimitCooldown::Provider),
        )
        .await
        .map(HostChargeProviderResult::Payment),
        Err(GatewayMutationError::Indeterminate(detail)) => resolve_host_charge_unknown(
            pool,
            &admission.reservation,
            &mutation_error_evidence(&detail),
            None,
        )
        .await
        .map(HostChargeProviderResult::Payment),
    }
}

fn host_charge_submission_matches_reservation(
    reconstructed: &HostChargeReservation,
    durable: &HostChargeReservation,
) -> bool {
    let request = reconstructed.request();
    let durable_request = durable.request();
    reconstructed.identity() == durable.identity()
        && reconstructed.snapshot() == durable.snapshot()
        && request.target() == durable_request.target()
        && request.idempotency_key() == durable_request.idempotency_key()
        && request.fingerprint() == durable_request.fingerprint()
        && request.amount() == durable_request.amount()
        && request.gateway_order_id() == durable_request.gateway_order_id()
        && request.billing_contact() == durable_request.billing_contact()
}

pub async fn apply_host_charge_gateway_outcome(
    pool: &PgPool,
    coordinator: &dyn BillingTransactionCoordinator,
    targets: &dyn HostChargeTargetStore,
    reservation: &HostChargeReservation,
    outcome: &GatewayPaymentOutcome,
) -> Result<HostChargePaymentResult, HostChargeApplicationError> {
    match outcome.status() {
        GatewayPaymentStatus::Approved => {
            let approved_evidence =
                outcome
                    .approved_evidence()
                    .ok_or(HostChargeApplicationError::InvalidState(
                        INVALID_HOST_CHARGE_STATE,
                    ))?;
            if outcome.transaction_id().is_none() {
                return durably_park_host_charge_approved(pool, reservation, &approved_evidence)
                    .await;
            }
            for attempt_index in 0..APPROVED_APPLICATION_ATTEMPTS {
                match apply_host_charge_approved(
                    coordinator,
                    targets,
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
            durably_park_host_charge_approved(pool, reservation, &approved_evidence).await
        }
        GatewayPaymentStatus::Declined => {
            resolve_host_charge_non_approved(
                pool,
                targets,
                reservation,
                outcome.evidence(),
                AttemptResolutionStatus::Declined,
                None,
                OutcomeResolutionBoundary::Submitted,
                true,
                None,
            )
            .await
        }
        GatewayPaymentStatus::Failed => {
            resolve_host_charge_non_approved(
                pool,
                targets,
                reservation,
                outcome.evidence(),
                AttemptResolutionStatus::Failed,
                None,
                OutcomeResolutionBoundary::Submitted,
                true,
                None,
            )
            .await
        }
        GatewayPaymentStatus::Unknown => {
            resolve_host_charge_unknown(pool, reservation, outcome.evidence(), None).await
        }
    }
}

pub async fn apply_reconciled_host_charge_gateway_outcome(
    pool: &PgPool,
    coordinator: &dyn BillingTransactionCoordinator,
    targets: &dyn HostChargeTargetStore,
    billing_scope_id: syrup_rail::BillingScopeId,
    attempt_id: syrup_rail::PaymentAttemptId,
    outcome: &GatewayPaymentOutcome,
) -> Result<HostChargePaymentResult, HostChargeApplicationError> {
    let mut transaction = pool.begin().await?;
    let attempt = crate::find_payment_attempt_by_id_in_transaction(
        &mut transaction,
        billing_scope_id,
        attempt_id,
    )
    .await?
    .ok_or(HostChargeApplicationError::InvalidState(
        INVALID_HOST_CHARGE_STATE,
    ))?;
    transaction.commit().await?;
    let reservation = HostChargeReservation::from_attempt(&attempt)
        .map_err(|_| HostChargeApplicationError::InvalidState(INVALID_HOST_CHARGE_STATE))?;
    apply_host_charge_gateway_outcome(pool, coordinator, targets, &reservation, outcome).await
}

pub(crate) async fn resolve_host_charge_before_submission(
    pool: &PgPool,
    targets: &dyn HostChargeTargetStore,
    reservation: &HostChargeReservation,
    detail: syrup_rail::GatewayDiagnostic,
    resolution_code: PaymentResolutionCode,
    resolution: HostChargeBeforeSubmissionResolution,
) -> Result<HostChargePaymentResult, HostChargeApplicationError> {
    let condition = if matches!(
        resolution_code,
        PaymentResolutionCode::GatewayAccountMutationCooldownBeforeSubmission
            | PaymentResolutionCode::GatewayProviderRateLimitedBeforeSubmission
    ) {
        None
    } else {
        Some(syrup_rail::GatewayDiagnostic::new("failed"))
    };
    let evidence = ProcessorEvidence::new(
        None,
        None,
        None,
        None,
        Some(detail),
        condition,
        syrup_rail::GatewayPaymentDescriptor::default(),
    );
    resolve_host_charge_non_approved(
        pool,
        targets,
        reservation,
        &evidence,
        AttemptResolutionStatus::Failed,
        Some(resolution_code),
        resolution.boundary,
        false,
        resolution.cooldown,
    )
    .await
}

async fn apply_host_charge_approved(
    coordinator: &dyn BillingTransactionCoordinator,
    targets: &dyn HostChargeTargetStore,
    reservation: &HostChargeReservation,
    approved_evidence: &ApprovedProcessorEvidence,
) -> Result<HostChargePaymentResult, HostChargeApplicationError> {
    let evidence = approved_evidence.evidence();
    let identity = reservation.identity();
    let mut transaction = coordinator
        .begin(
            BillingEventSubject::new(identity.billing_scope_id(), identity.subscriber_id()),
            BILLING_LOCK_TIMEOUT,
        )
        .await?;
    if transaction.subject_state() != BillingTransactionSubjectState::LiveRecipient {
        let _ = transaction.rollback().await;
        return Err(HostChargeApplicationError::InvalidState(
            "a host charge payment event requires a live recipient",
        ));
    }
    let connection = transaction.connection();
    set_application_timeouts(connection).await?;
    let effective_at: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *connection)
        .await?;
    let target_outcome = targets
        .apply_transition(
            connection,
            HostChargeTargetTransition::new(
                identity.billing_scope_id(),
                identity.subscriber_id(),
                identity.attempt_id(),
                reservation.snapshot().target_id(),
                HostChargeTargetTransitionKind::Paid,
                effective_at,
            ),
        )
        .await?;
    let attempt = lock_expected_host_charge(connection, reservation).await?;
    if attempt.status() == PaymentAttemptStatus::Approved {
        observe_processor_charge(
            connection,
            &attempt,
            evidence,
            ProcessorChargeProgression::Applied,
        )
        .await?;
        transaction.commit().await?;
        return Ok(HostChargePaymentResult::new(attempt)?);
    }
    if attempt.status().is_terminal() {
        transaction.rollback().await?;
        return observe_terminal_host_charge_approval(
            coordinator,
            reservation,
            &attempt,
            approved_evidence,
        )
        .await;
    }
    let observation = observe_processor_charge(
        connection,
        &attempt,
        evidence,
        ProcessorChargeProgression::Pending,
    )
    .await?;
    let ObservedCharge::Owned(charge) = observation else {
        let _ = transaction.rollback().await;
        return Err(HostChargeApplicationError::InvalidState(
            "the approved gateway transaction belongs to another payment attempt",
        ));
    };
    if charge.role == ProcessorChargeRole::Additional {
        let _ = transaction.rollback().await;
        return Err(HostChargeApplicationError::InvalidState(
            "an additional approved host charge requires external reversal",
        ));
    }
    match target_outcome {
        HostChargeTargetTransitionOutcome::Applied
        | HostChargeTargetTransitionOutcome::ExactReplay => {
            persist_attempt_transition(
                connection,
                &attempt,
                evidence,
                AttemptTransition::Approved(AttemptApproval::HostCharge),
            )
            .await
            .map_err(map_attempt_transition_error)?;
            transition_charge(
                connection,
                charge.id,
                ProcessorChargeProgression::Applied,
                None,
            )
            .await?;
            let applied = find_payment_attempt_by_id_on_connection(
                connection,
                identity.billing_scope_id(),
                identity.attempt_id(),
            )
            .await?
            .ok_or(HostChargeApplicationError::InvalidState(
                INVALID_HOST_CHARGE_STATE,
            ))?;
            let event = BillingEvent::HostChargePaid {
                attempt_id: identity.attempt_id(),
                target_id: reservation.snapshot().target_id(),
                charge: reservation.snapshot().charge(),
            };
            transaction.append_event(&event).await?;
            transaction.commit().await?;
            Ok(HostChargePaymentResult::new(applied)?)
        }
        HostChargeTargetTransitionOutcome::StaleTarget => {
            transition_charge(
                connection,
                charge.id,
                ProcessorChargeProgression::ExternalReversalRequired,
                Some(PaymentResolutionCode::HostChargeApprovedStaleState),
            )
            .await?;
            let parked = park_locked_attempt(
                connection,
                &attempt,
                evidence,
                Some(PaymentResolutionCode::HostChargeApprovedStaleState),
                APPROVED_STALE_TARGET_TEXT,
            )
            .await?;
            transaction.commit().await?;
            Ok(HostChargePaymentResult::new(parked)?)
        }
        HostChargeTargetTransitionOutcome::Unchanged { .. } => {
            let _ = transaction.rollback().await;
            Err(HostChargeApplicationError::InvalidState(
                INVALID_HOST_CHARGE_STATE,
            ))
        }
    }
}

async fn observe_terminal_host_charge_approval(
    coordinator: &dyn BillingTransactionCoordinator,
    reservation: &HostChargeReservation,
    terminal_attempt: &PaymentAttempt,
    approved_evidence: &ApprovedProcessorEvidence,
) -> Result<HostChargePaymentResult, HostChargeApplicationError> {
    let evidence = approved_evidence.evidence();
    let identity = reservation.identity();
    let mut transaction = coordinator
        .begin(
            BillingEventSubject::new(identity.billing_scope_id(), identity.subscriber_id()),
            BILLING_LOCK_TIMEOUT,
        )
        .await?;
    let connection = transaction.connection();
    set_application_timeouts(connection).await?;
    if matches!(
        observe_processor_charge(
            connection,
            terminal_attempt,
            evidence,
            ProcessorChargeProgression::Pending,
        )
        .await?,
        ObservedCharge::OwnedByOtherAttempt
    ) {
        let _ = transaction.rollback().await;
        return Err(HostChargeApplicationError::InvalidState(
            "the approved gateway transaction belongs to another payment attempt",
        ));
    }
    let locked = lock_expected_host_charge(connection, reservation).await?;
    if !locked.status().is_terminal() || locked.status() == PaymentAttemptStatus::Approved {
        let _ = transaction.rollback().await;
        return Err(HostChargeApplicationError::InvalidState(
            INVALID_HOST_CHARGE_STATE,
        ));
    }
    transaction.commit().await?;
    Ok(HostChargePaymentResult::confirmation_pending(
        locked,
        approved_evidence.clone(),
    )?)
}

#[allow(clippy::too_many_arguments)]
async fn resolve_host_charge_non_approved(
    pool: &PgPool,
    targets: &dyn HostChargeTargetStore,
    reservation: &HostChargeReservation,
    evidence: &ProcessorEvidence,
    status: AttemptResolutionStatus,
    resolution_code: Option<PaymentResolutionCode>,
    boundary: OutcomeResolutionBoundary,
    release_target: bool,
    cooldown: Option<RateLimitCooldown>,
) -> Result<HostChargePaymentResult, HostChargeApplicationError> {
    let identity = reservation.identity();
    let mut transaction = pool.begin().await?;
    set_application_timeouts(&mut transaction).await?;
    if release_target {
        let effective_at = sqlx::query_scalar("SELECT clock_timestamp()")
            .fetch_one(&mut *transaction)
            .await?;
        let target_outcome = targets
            .apply_transition(
                &mut transaction,
                HostChargeTargetTransition::new(
                    identity.billing_scope_id(),
                    identity.subscriber_id(),
                    identity.attempt_id(),
                    reservation.snapshot().target_id(),
                    HostChargeTargetTransitionKind::PaymentFailed,
                    effective_at,
                ),
            )
            .await?;
        if target_outcome == HostChargeTargetTransitionOutcome::StaleTarget {
            return Err(HostChargeApplicationError::InvalidState(
                INVALID_HOST_CHARGE_STATE,
            ));
        }
    }
    let attempt = lock_expected_host_charge(&mut transaction, reservation).await?;
    let may_resolve = attempt.status().is_resolvable()
        && match boundary {
            OutcomeResolutionBoundary::Prepared => {
                attempt.state().timestamps().submitted_at().is_none()
            }
            OutcomeResolutionBoundary::AdmittedNotSubmitted => {
                attempt.state().timestamps().submitted_at().is_some()
            }
            OutcomeResolutionBoundary::Submitted => true,
        };
    if !may_resolve && release_target {
        transaction.rollback().await?;
        let mut reload = pool.begin().await?;
        let attempt = crate::find_payment_attempt_by_id_in_transaction(
            &mut reload,
            identity.billing_scope_id(),
            identity.attempt_id(),
        )
        .await?
        .ok_or(HostChargeApplicationError::InvalidState(
            INVALID_HOST_CHARGE_STATE,
        ))?;
        reload.commit().await?;
        return Ok(HostChargePaymentResult::new(attempt)?);
    }
    if may_resolve {
        persist_attempt_transition(
            &mut transaction,
            &attempt,
            evidence,
            AttemptTransition::Resolved {
                status,
                resolution_code,
            },
        )
        .await
        .map_err(map_attempt_transition_error)?;
        if evidence.indicates_approved_payment() {
            observe_processor_charge(
                &mut transaction,
                &attempt,
                evidence,
                ProcessorChargeProgression::Pending,
            )
            .await?;
        }
        if boundary == OutcomeResolutionBoundary::AdmittedNotSubmitted {
            sqlx::query(
                "UPDATE billing_payment_attempts SET submitted_at = NULL, updated_at = clock_timestamp() WHERE id = $1 AND status = $2",
            )
            .bind(identity.attempt_id().as_uuid())
            .bind(status.as_str())
            .execute(&mut *transaction)
            .await?;
        }
    }
    if host_charge_provider_cooldown_requested(cooldown) {
        extend_host_charge_provider_cooldown(&mut transaction, reservation).await?;
    }
    let attempt = find_payment_attempt_by_id_on_connection(
        &mut transaction,
        identity.billing_scope_id(),
        identity.attempt_id(),
    )
    .await?
    .ok_or(HostChargeApplicationError::InvalidState(
        INVALID_HOST_CHARGE_STATE,
    ))?;
    transaction.commit().await?;
    Ok(HostChargePaymentResult::new(attempt)?)
}

async fn resolve_host_charge_unknown(
    pool: &PgPool,
    reservation: &HostChargeReservation,
    evidence: &ProcessorEvidence,
    cooldown: Option<RateLimitCooldown>,
) -> Result<HostChargePaymentResult, HostChargeApplicationError> {
    let identity = reservation.identity();
    let mut transaction = pool.begin().await?;
    set_application_timeouts(&mut transaction).await?;
    let attempt = lock_expected_host_charge(&mut transaction, reservation).await?;
    if !attempt.status().is_terminal() {
        persist_attempt_transition(
            &mut transaction,
            &attempt,
            evidence,
            AttemptTransition::Resolved {
                status: AttemptResolutionStatus::Unknown,
                resolution_code: None,
            },
        )
        .await
        .map_err(map_attempt_transition_error)?;
        if evidence.indicates_approved_payment() {
            observe_processor_charge(
                &mut transaction,
                &attempt,
                evidence,
                ProcessorChargeProgression::Pending,
            )
            .await?;
        }
    }
    if host_charge_provider_cooldown_requested(cooldown) {
        extend_host_charge_provider_cooldown(&mut transaction, reservation).await?;
    }
    let attempt = find_payment_attempt_by_id_on_connection(
        &mut transaction,
        identity.billing_scope_id(),
        identity.attempt_id(),
    )
    .await?
    .ok_or(HostChargeApplicationError::InvalidState(
        INVALID_HOST_CHARGE_STATE,
    ))?;
    transaction.commit().await?;
    Ok(HostChargePaymentResult::new(attempt)?)
}

const fn host_charge_provider_cooldown_requested(cooldown: Option<RateLimitCooldown>) -> bool {
    matches!(cooldown, Some(RateLimitCooldown::Provider))
}

async fn extend_host_charge_provider_cooldown(
    connection: &mut PgConnection,
    reservation: &HostChargeReservation,
) -> Result<(), HostChargeApplicationError> {
    let updated = sqlx::query(
        r#"
        UPDATE billing_gateway_provider_rate_limits
        SET rate_limited_until = GREATEST(
            rate_limited_until,
            clock_timestamp() + make_interval(secs => $4)
        )
        WHERE provider_key = (
            SELECT provider_key
            FROM billing_gateway_accounts
            WHERE id = $1 AND billing_scope_id = $2
                AND gateway_configuration_id = $3
        )
        "#,
    )
    .bind(reservation.identity().gateway_account_id().as_uuid())
    .bind(reservation.identity().billing_scope_id().as_uuid())
    .bind(reservation.identity().gateway_configuration_id().as_uuid())
    .bind(syrup_rail::RENEWAL_PROVIDER_RATE_LIMIT_RETRY_AFTER_SECONDS)
    .execute(connection)
    .await?;
    if updated.rows_affected() != 1 {
        return Err(HostChargeApplicationError::InvalidState(
            INVALID_HOST_CHARGE_STATE,
        ));
    }
    Ok(())
}

async fn park_host_charge_approved(
    pool: &PgPool,
    reservation: &HostChargeReservation,
    evidence: &ProcessorEvidence,
) -> Result<HostChargePaymentResult, HostChargeApplicationError> {
    let mut transaction = pool.begin().await?;
    set_application_timeouts(&mut transaction).await?;
    let attempt = lock_expected_host_charge(&mut transaction, reservation).await?;
    let progression = if evidence.transaction_id().is_some() {
        ProcessorChargeProgression::ExternalReversalRequired
    } else {
        ProcessorChargeProgression::ReconciliationRequired
    };
    let observation =
        observe_processor_charge(&mut transaction, &attempt, evidence, progression).await?;
    if let ObservedCharge::Owned(charge) = observation {
        transition_charge(&mut transaction, charge.id, progression, None).await?;
    }
    let parked = park_locked_attempt(
        &mut transaction,
        &attempt,
        evidence,
        None,
        APPROVED_STORAGE_FAILURE_TEXT,
    )
    .await?;
    transaction.commit().await?;
    Ok(HostChargePaymentResult::new(parked)?)
}

async fn durably_park_host_charge_approved(
    pool: &PgPool,
    reservation: &HostChargeReservation,
    approved_evidence: &ApprovedProcessorEvidence,
) -> Result<HostChargePaymentResult, HostChargeApplicationError> {
    let evidence = approved_evidence.evidence();
    if let Ok(payment) = park_host_charge_approved(pool, reservation, evidence).await {
        return Ok(payment);
    }
    crate::store_compensating_processor_charge(
        pool,
        reservation.identity().attempt_id(),
        reservation.request().gateway_order_id(),
        evidence,
    )
    .await?;
    let mut transaction = pool.begin().await?;
    let attempt = crate::find_payment_attempt_by_id_in_transaction(
        &mut transaction,
        reservation.identity().billing_scope_id(),
        reservation.identity().attempt_id(),
    )
    .await?
    .ok_or(HostChargeApplicationError::InvalidState(
        INVALID_HOST_CHARGE_STATE,
    ))?;
    transaction.commit().await?;
    Ok(HostChargePaymentResult::confirmation_pending(
        attempt,
        approved_evidence.clone(),
    )?)
}

async fn lock_expected_host_charge(
    connection: &mut PgConnection,
    reservation: &HostChargeReservation,
) -> Result<PaymentAttempt, HostChargeApplicationError> {
    let identity = reservation.identity();
    let attempt = lock_payment_attempt_by_id_on_connection(
        connection,
        identity.billing_scope_id(),
        identity.attempt_id(),
    )
    .await?
    .ok_or(HostChargeApplicationError::InvalidState(
        INVALID_HOST_CHARGE_STATE,
    ))?;
    if attempt.identity() != identity || attempt.request() != reservation.request() {
        return Err(HostChargeApplicationError::InvalidState(
            INVALID_HOST_CHARGE_STATE,
        ));
    }
    Ok(attempt)
}

#[cfg(test)]
mod tests {
    use std::{
        error::Error,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use async_trait::async_trait;
    use chrono::Duration as ChronoDuration;
    use sqlx::{Postgres, Transaction};
    use syrup_rail::{
        BillingContact, BillingEventKey, ChargeAmount, CurrencyCode, EndUserMutationAdmission,
        EndUserMutationAdmissionResult, EndUserMutationCommand, GatewayAccountId,
        GatewayAccountMode, GatewayConfigurationId, GatewayDiagnostic, GatewayError,
        GatewayLifecycleCursorKey, GatewayLifecycleQueryPolicy, GatewayMutationError,
        GatewayMutationReferenceFactory, GatewayOrderId, GatewayPaymentDescriptor,
        GatewayProviderKey, GatewayQueryRequest, GatewayResolutionError, GatewayResolver,
        GatewayStorePaymentMethodRequest, GatewayTransactionId, GatewayTransactionReport,
        GatewayTransactionReportRequest, HostChargeTargetId, HostChargeTargetNoChange,
        IdempotencyKey, PaymentAttemptId, PaymentAttemptKind, PaymentGateway, PaymentToken,
        ResolvedGateway,
    };
    use tokio::sync::{Mutex, Notify, oneshot};
    use uuid::Uuid;

    use super::*;
    use crate::{
        BillingEventWriteError, BillingTransaction, HostChargeLedgerAdmission,
        HostChargeLedgerAdmissionMode, HostChargeLedgerAdmissionQuery,
        HostChargeReservationDecision, HostChargeReservationOutcome, HostChargeSubmissionAdmission,
        HostChargeSubmissionDecision, HostChargeTargetReservation, SubscriptionBillingService,
        SubscriptionOfferStore, host_charge_ledger_admission, reserve_host_charge_in_transaction,
        test_support::{TestDatabase, create_gateway_account},
    };

    mod readiness_replay;

    #[test]
    fn before_submission_resolution_modes_keep_boundary_and_cooldown_distinct() {
        let prepared = HostChargeBeforeSubmissionResolution::prepared();
        assert_eq!(prepared.boundary, OutcomeResolutionBoundary::Prepared);
        assert!(prepared.cooldown.is_none());

        let admitted = HostChargeBeforeSubmissionResolution::admitted_not_submitted();
        assert_eq!(
            admitted.boundary,
            OutcomeResolutionBoundary::AdmittedNotSubmitted
        );
        assert!(admitted.cooldown.is_none());

        let rate_limited = HostChargeBeforeSubmissionResolution::prepared_provider_rate_limited();
        assert_eq!(rate_limited.boundary, OutcomeResolutionBoundary::Prepared);
        assert!(matches!(
            rate_limited.cooldown,
            Some(RateLimitCooldown::Provider)
        ));
    }

    #[test]
    fn host_charge_cooldown_only_extends_provider_scope() {
        assert!(!host_charge_provider_cooldown_requested(None));
        assert!(!host_charge_provider_cooldown_requested(Some(
            RateLimitCooldown::Account
        )));
        assert!(host_charge_provider_cooldown_requested(Some(
            RateLimitCooldown::Provider
        )));
    }

    struct TestReferenceFactory;

    impl GatewayMutationReferenceFactory for TestReferenceFactory {
        fn for_attempt(
            &self,
            _kind: PaymentAttemptKind,
            attempt_id: PaymentAttemptId,
        ) -> GatewayOrderId {
            GatewayOrderId::from_generated_attempt(
                format!("test_host_{}", attempt_id.as_uuid().simple()),
                attempt_id,
            )
            .expect("valid host test reference")
        }
    }

    struct ScriptedGateway {
        sale_calls: AtomicUsize,
        outcome: Mutex<Option<GatewayPaymentOutcome>>,
    }

    #[async_trait]
    impl PaymentGateway for ScriptedGateway {
        async fn account_mode(&self) -> Result<GatewayAccountMode, GatewayError> {
            Ok(GatewayAccountMode::Live)
        }

        async fn sale(
            &self,
            _request: GatewaySaleRequest,
        ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
            self.sale_calls.fetch_add(1, Ordering::SeqCst);
            Ok(self
                .outcome
                .lock()
                .await
                .take()
                .expect("one sale capability"))
        }

        async fn store_payment_method(
            &self,
            _request: GatewayStorePaymentMethodRequest,
        ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
            panic!("host charge must not store a payment method")
        }

        async fn query_transaction(
            &self,
            _request: GatewayQueryRequest,
        ) -> Result<Option<GatewayPaymentOutcome>, GatewayError> {
            panic!("foreground host charge must not query")
        }

        async fn query_transaction_reports(
            &self,
            _request: GatewayTransactionReportRequest,
        ) -> Result<Vec<GatewayTransactionReport>, GatewayError> {
            panic!("foreground host charge must not query reports")
        }
    }

    struct RateLimitedAfterReservationGateway {
        readiness_calls: AtomicUsize,
        sale_calls: AtomicUsize,
    }

    struct TerminalRaceGateway {
        pool: PgPool,
        sale_calls: AtomicUsize,
    }

    struct RacingPreparedRetryGateway {
        readiness_calls: AtomicUsize,
        sale_calls: AtomicUsize,
        blocked_readiness_started: Mutex<Option<oneshot::Sender<()>>>,
        sale_started: Mutex<Option<oneshot::Sender<()>>>,
        release_readiness: Notify,
        release_sale: Notify,
    }

    #[async_trait]
    impl PaymentGateway for RacingPreparedRetryGateway {
        async fn account_mode(&self) -> Result<GatewayAccountMode, GatewayError> {
            if self.readiness_calls.fetch_add(1, Ordering::SeqCst) == 1 {
                if let Some(started) = self.blocked_readiness_started.lock().await.take() {
                    let _ = started.send(());
                }
                self.release_readiness.notified().await;
                Ok(GatewayAccountMode::Test)
            } else {
                Ok(GatewayAccountMode::Live)
            }
        }

        async fn sale(
            &self,
            _request: GatewaySaleRequest,
        ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
            self.sale_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(started) = self.sale_started.lock().await.take() {
                let _ = started.send(());
            }
            self.release_sale.notified().await;
            Ok(approved_outcome("host_txn_prepared_retry"))
        }

        async fn store_payment_method(
            &self,
            _request: GatewayStorePaymentMethodRequest,
        ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
            panic!("host charge must not store a payment method")
        }

        async fn query_transaction(
            &self,
            _request: GatewayQueryRequest,
        ) -> Result<Option<GatewayPaymentOutcome>, GatewayError> {
            panic!("foreground host charge must not query")
        }

        async fn query_transaction_reports(
            &self,
            _request: GatewayTransactionReportRequest,
        ) -> Result<Vec<GatewayTransactionReport>, GatewayError> {
            panic!("foreground host charge must not query reports")
        }
    }

    #[async_trait]
    impl PaymentGateway for TerminalRaceGateway {
        async fn account_mode(&self) -> Result<GatewayAccountMode, GatewayError> {
            Ok(GatewayAccountMode::Live)
        }

        async fn sale(
            &self,
            request: GatewaySaleRequest,
        ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
            self.sale_calls.fetch_add(1, Ordering::SeqCst);
            let updated = sqlx::query(
                r#"
                UPDATE billing_payment_attempts
                SET status = 'failed', resolved_at = clock_timestamp(),
                    gateway_response_text = 'simulated terminal race',
                    gateway_condition = 'failed', updated_at = clock_timestamp()
                WHERE gateway_order_id = $1 AND status = 'pending'
                "#,
            )
            .bind(request.order_id().expose())
            .execute(&self.pool)
            .await
            .expect("simulate a terminal attempt race");
            assert_eq!(updated.rows_affected(), 1);
            Ok(approved_outcome("host_txn_terminal_race"))
        }

        async fn store_payment_method(
            &self,
            _request: GatewayStorePaymentMethodRequest,
        ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
            panic!("host charge must not store a payment method")
        }

        async fn query_transaction(
            &self,
            _request: GatewayQueryRequest,
        ) -> Result<Option<GatewayPaymentOutcome>, GatewayError> {
            panic!("foreground host charge must not query")
        }

        async fn query_transaction_reports(
            &self,
            _request: GatewayTransactionReportRequest,
        ) -> Result<Vec<GatewayTransactionReport>, GatewayError> {
            panic!("foreground host charge must not query reports")
        }
    }

    #[async_trait]
    impl PaymentGateway for RateLimitedAfterReservationGateway {
        async fn account_mode(&self) -> Result<GatewayAccountMode, GatewayError> {
            if self.readiness_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(GatewayAccountMode::Live)
            } else {
                Err(GatewayError::RateLimited(GatewayDiagnostic::new(
                    "provider throttled the readiness check",
                )))
            }
        }

        async fn sale(
            &self,
            _request: GatewaySaleRequest,
        ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
            self.sale_calls.fetch_add(1, Ordering::SeqCst);
            panic!("rate-limited host charge must not submit a sale")
        }

        async fn store_payment_method(
            &self,
            _request: GatewayStorePaymentMethodRequest,
        ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
            panic!("host charge must not store a payment method")
        }

        async fn query_transaction(
            &self,
            _request: GatewayQueryRequest,
        ) -> Result<Option<GatewayPaymentOutcome>, GatewayError> {
            panic!("foreground host charge must not query")
        }

        async fn query_transaction_reports(
            &self,
            _request: GatewayTransactionReportRequest,
        ) -> Result<Vec<GatewayTransactionReport>, GatewayError> {
            panic!("foreground host charge must not query reports")
        }
    }

    struct StaticResolver {
        gateway: ResolvedGateway,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl GatewayResolver for StaticResolver {
        async fn resolve(
            &self,
            billing_scope_id: syrup_rail::BillingScopeId,
            gateway_account_id: GatewayAccountId,
            gateway_configuration_id: GatewayConfigurationId,
            provider_key: GatewayProviderKey,
        ) -> Result<ResolvedGateway, GatewayResolutionError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if billing_scope_id != self.gateway.billing_scope_id()
                || gateway_account_id != self.gateway.gateway_account_id()
                || gateway_configuration_id != self.gateway.gateway_configuration_id()
                || provider_key != *self.gateway.provider_key()
            {
                return Err(GatewayResolutionError::ConfigurationChanged);
            }
            Ok(self.gateway.clone())
        }
    }

    struct PermitAdmission {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl EndUserMutationAdmission for PermitAdmission {
        async fn admit(&self, _command: EndUserMutationCommand) -> EndUserMutationAdmissionResult {
            self.calls.fetch_add(1, Ordering::SeqCst);
            EndUserMutationAdmissionResult::Allowed
        }
    }

    struct UnusedOffers;

    #[async_trait]
    impl SubscriptionOfferStore for UnusedOffers {
        async fn lock_current_offer(
            &self,
            _connection: &mut PgConnection,
            _billing_scope_id: syrup_rail::BillingScopeId,
            _plan_key: &syrup_rail::PlanKey,
        ) -> Result<Option<syrup_rail::SubscriptionOffer>, sqlx::Error> {
            panic!("host charge must not load a subscription offer")
        }
    }

    #[derive(Clone)]
    struct TestCoordinator {
        pool: PgPool,
        events: Arc<Mutex<Vec<BillingEvent>>>,
    }

    #[async_trait]
    impl BillingTransactionCoordinator for TestCoordinator {
        async fn begin(
            &self,
            _subject: BillingEventSubject,
            _lock_timeout: Duration,
        ) -> Result<Box<dyn BillingTransaction>, BillingTransactionError> {
            Ok(Box::new(TestTransaction {
                transaction: Some(
                    self.pool
                        .begin()
                        .await
                        .map_err(BillingTransactionError::new)?,
                ),
                events: Arc::clone(&self.events),
            }))
        }
    }

    struct TestTransaction {
        transaction: Option<Transaction<'static, Postgres>>,
        events: Arc<Mutex<Vec<BillingEvent>>>,
    }

    #[async_trait]
    impl BillingTransaction for TestTransaction {
        fn connection(&mut self) -> &mut PgConnection {
            &mut *self.transaction.as_mut().expect("active transaction")
        }

        fn subject_state(&self) -> BillingTransactionSubjectState {
            BillingTransactionSubjectState::LiveRecipient
        }

        async fn append_event(
            &mut self,
            event: &BillingEvent,
        ) -> Result<(), BillingEventWriteError> {
            self.events.lock().await.push(event.clone());
            Ok(())
        }

        async fn commit(mut self: Box<Self>) -> Result<(), BillingTransactionError> {
            self.transaction
                .take()
                .expect("active transaction")
                .commit()
                .await
                .map_err(BillingTransactionError::new)
        }

        async fn rollback(mut self: Box<Self>) -> Result<(), BillingTransactionError> {
            self.transaction
                .take()
                .expect("active transaction")
                .rollback()
                .await
                .map_err(BillingTransactionError::new)
        }
    }

    struct TestTargets;

    #[async_trait]
    impl HostChargeTargetStore for TestTargets {
        async fn preflight_target(
            &self,
            connection: &mut PgConnection,
            reservation: &HostChargeTargetReservation,
        ) -> Result<HostChargeReservationDecision, HostChargeTargetError> {
            self.reserve_target(connection, reservation).await
        }

        async fn reserve_target(
            &self,
            connection: &mut PgConnection,
            reservation: &HostChargeTargetReservation,
        ) -> Result<HostChargeReservationDecision, HostChargeTargetError> {
            let row = sqlx::query_as::<_, (String, i32, String)>(
                r#"
                SELECT status, amount_cents, currency
                FROM host_charge_targets
                WHERE id = $1 AND billing_scope_id = $2 AND subscriber_id = $3
                FOR UPDATE
                "#,
            )
            .bind(reservation.target_id().as_uuid())
            .bind(reservation.billing_scope_id().as_uuid())
            .bind(reservation.subscriber_id().as_uuid())
            .fetch_optional(&mut *connection)
            .await
            .map_err(HostChargeTargetError::new)?;
            let Some((status, cents, currency)) = row else {
                return Ok(HostChargeReservationDecision::Rejected {
                    reason: syrup_rail::HostChargeTargetRejection::TargetUnavailable,
                });
            };
            let ledger = host_charge_ledger_admission(
                connection,
                &HostChargeLedgerAdmissionQuery::new(
                    reservation.billing_scope_id(),
                    reservation.subscriber_id(),
                    reservation.target_id(),
                    HostChargeLedgerAdmissionMode::Reserve {
                        idempotency_key: reservation.idempotency_key().clone(),
                    },
                ),
            )
            .await
            .map_err(HostChargeTargetError::new)?;
            if ledger == HostChargeLedgerAdmission::IdempotentContender {
                return Ok(HostChargeReservationDecision::IdempotentContender);
            }
            if ledger != HostChargeLedgerAdmission::Safe || status != "pending" {
                return Ok(HostChargeReservationDecision::Rejected {
                    reason: syrup_rail::HostChargeTargetRejection::LedgerUnsafe,
                });
            }
            let charge = ChargeAmount::new(cents, CurrencyCode::new(&currency).unwrap()).unwrap();
            Ok(HostChargeReservationDecision::Reserved(
                syrup_rail::HostChargeTargetSnapshot::new(reservation.target_id(), charge),
            ))
        }

        async fn admit_submission(
            &self,
            connection: &mut PgConnection,
            admission: &HostChargeSubmissionAdmission,
        ) -> Result<HostChargeSubmissionDecision, HostChargeTargetError> {
            let row = sqlx::query_as::<_, (String, i32, String)>(
                r#"
                SELECT status, amount_cents, currency
                FROM host_charge_targets
                WHERE id = $1 AND billing_scope_id = $2 AND subscriber_id = $3
                FOR UPDATE
                "#,
            )
            .bind(admission.target_id().as_uuid())
            .bind(admission.billing_scope_id().as_uuid())
            .bind(admission.subscriber_id().as_uuid())
            .fetch_optional(&mut *connection)
            .await
            .map_err(HostChargeTargetError::new)?;
            let Some((status, cents, currency)) = row else {
                return Ok(HostChargeSubmissionDecision::Rejected {
                    reason: syrup_rail::HostChargeTargetRejection::TargetUnavailable,
                });
            };
            let charge = ChargeAmount::new(cents, CurrencyCode::new(&currency).unwrap()).unwrap();
            let ledger = host_charge_ledger_admission(
                connection,
                &HostChargeLedgerAdmissionQuery::new(
                    admission.billing_scope_id(),
                    admission.subscriber_id(),
                    admission.target_id(),
                    HostChargeLedgerAdmissionMode::Submit {
                        attempt_id: admission.attempt_id(),
                    },
                ),
            )
            .await
            .map_err(HostChargeTargetError::new)?;
            if ledger == HostChargeLedgerAdmission::Safe
                && status == "pending"
                && charge == admission.expected_charge()
            {
                Ok(HostChargeSubmissionDecision::Admitted(
                    syrup_rail::HostChargeTargetSnapshot::new(admission.target_id(), charge),
                ))
            } else {
                Ok(HostChargeSubmissionDecision::Rejected {
                    reason: syrup_rail::HostChargeTargetRejection::ChargeChanged,
                })
            }
        }

        async fn apply_transition(
            &self,
            connection: &mut PgConnection,
            transition: HostChargeTargetTransition,
        ) -> Result<HostChargeTargetTransitionOutcome, HostChargeTargetError> {
            let current: Option<String> = sqlx::query_scalar(
                r#"
                SELECT status FROM host_charge_targets
                WHERE id = $1 AND billing_scope_id = $2 AND subscriber_id = $3
                FOR UPDATE
                "#,
            )
            .bind(transition.target_id().as_uuid())
            .bind(transition.billing_scope_id().as_uuid())
            .bind(transition.subscriber_id().as_uuid())
            .fetch_optional(&mut *connection)
            .await
            .map_err(HostChargeTargetError::new)?;
            let Some(current) = current else {
                return Ok(HostChargeTargetTransitionOutcome::StaleTarget);
            };
            match transition.kind() {
                HostChargeTargetTransitionKind::Paid if current == "pending" => {
                    sqlx::query(
                        "UPDATE host_charge_targets SET status = 'paid', paid_at = $2 WHERE id = $1",
                    )
                    .bind(transition.target_id().as_uuid())
                    .bind(transition.effective_at())
                    .execute(connection)
                    .await
                    .map_err(HostChargeTargetError::new)?;
                    Ok(HostChargeTargetTransitionOutcome::Applied)
                }
                HostChargeTargetTransitionKind::Paid if current == "paid" => {
                    Ok(HostChargeTargetTransitionOutcome::ExactReplay)
                }
                HostChargeTargetTransitionKind::Paid => {
                    Ok(HostChargeTargetTransitionOutcome::StaleTarget)
                }
                HostChargeTargetTransitionKind::PaymentFailed if current == "pending" => {
                    Ok(HostChargeTargetTransitionOutcome::Applied)
                }
                _ => Ok(HostChargeTargetTransitionOutcome::Unchanged {
                    reason: HostChargeTargetNoChange::InapplicableState,
                }),
            }
        }
    }

    fn resolved_gateway(
        account: crate::test_support::GatewayAccountFixture,
        gateway: Arc<dyn PaymentGateway>,
    ) -> ResolvedGateway {
        ResolvedGateway::new(
            syrup_rail::BillingScopeId::new(account.billing_scope_id),
            GatewayAccountId::new(account.gateway_account_id),
            GatewayConfigurationId::new(account.gateway_configuration_id),
            GatewayProviderKey::new("nmi").unwrap(),
            GatewayLifecycleQueryPolicy::new(
                GatewayLifecycleCursorKey::new("host_test").unwrap(),
                ChronoDuration::minutes(1),
                10,
                2,
                2,
                20,
            )
            .unwrap(),
            Arc::new(TestReferenceFactory),
            gateway,
        )
    }

    fn approved_outcome(transaction_id: &str) -> GatewayPaymentOutcome {
        GatewayPaymentOutcome::new(
            GatewayPaymentStatus::Approved,
            ProcessorEvidence::new(
                Some(GatewayTransactionId::new(transaction_id).unwrap()),
                None,
                Some(GatewayDiagnostic::new("1")),
                None,
                Some(GatewayDiagnostic::new("approved")),
                Some(GatewayDiagnostic::new("complete")),
                GatewayPaymentDescriptor::default(),
            ),
        )
    }

    #[tokio::test]
    async fn foreground_host_charge_is_one_shot_atomic_and_replay_first()
    -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("rail_host_svc").await?;
        let result = async {
            sqlx::query(
                r#"
                CREATE TABLE host_charge_targets (
                    id uuid PRIMARY KEY,
                    billing_scope_id uuid NOT NULL,
                    subscriber_id uuid NOT NULL,
                    status text NOT NULL,
                    amount_cents integer NOT NULL,
                    currency text NOT NULL,
                    paid_at timestamptz
                )
                "#,
            )
            .execute(&database.pool)
            .await?;
            let account = create_gateway_account(&database.pool, "nmi").await?;
            let subscriber_id = Uuid::now_v7();
            let target_id = Uuid::now_v7();
            sqlx::query(
                "INSERT INTO host_charge_targets VALUES ($1, $2, $3, 'pending', 1250, 'USD', NULL)",
            )
            .bind(target_id)
            .bind(account.billing_scope_id)
            .bind(subscriber_id)
            .execute(&database.pool)
            .await?;

            let gateway = Arc::new(ScriptedGateway {
                sale_calls: AtomicUsize::new(0),
                outcome: Mutex::new(Some(approved_outcome("host_txn_approved"))),
            });
            let resolver = Arc::new(StaticResolver {
                gateway: resolved_gateway(account, gateway.clone()),
                calls: AtomicUsize::new(0),
            });
            let admission = Arc::new(PermitAdmission {
                calls: AtomicUsize::new(0),
            });
            let events = Arc::new(Mutex::new(Vec::new()));
            let coordinator = Arc::new(TestCoordinator {
                pool: database.pool.clone(),
                events: Arc::clone(&events),
            });
            let targets = Arc::new(TestTargets);
            let service = SubscriptionBillingService::new(
                database.pool.clone(),
                Arc::new(UnusedOffers),
                resolver.clone(),
                admission.clone(),
                coordinator,
            )
            .with_host_charge_targets(targets);
            let command = ChargeHostTarget::new(
                syrup_rail::BillingScopeId::new(account.billing_scope_id),
                syrup_rail::SubscriberId::new(subscriber_id),
                HostChargeTargetId::new(target_id),
                GatewayConfigurationId::new(account.gateway_configuration_id),
                PaymentToken::new("tok_host_once")?,
                IdempotencyKey::new("host-idempotency")?,
                Some(BillingContact::new(
                    None,
                    None,
                    Some("host@example.test".into()),
                )?),
            );

            let first = service.charge_host_target(command.clone()).await?;
            assert_eq!(first.status(), PaymentAttemptStatus::Approved);
            assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 1);
            assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
            assert_eq!(admission.calls.load(Ordering::SeqCst), 1);
            let replay = service.charge_host_target(command).await?;
            assert_eq!(replay.attempt(), first.attempt());
            assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 1);
            assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
            assert_eq!(admission.calls.load(Ordering::SeqCst), 1);

            let target_status: String =
                sqlx::query_scalar("SELECT status FROM host_charge_targets WHERE id = $1")
                    .bind(target_id)
                    .fetch_one(&database.pool)
                    .await?;
            assert_eq!(target_status, "paid");
            let charge_state: String = sqlx::query_scalar(
                "SELECT progression_state FROM billing_processor_charges WHERE attempt_id = $1",
            )
            .bind(first.attempt().identity().attempt_id().as_uuid())
            .fetch_one(&database.pool)
            .await?;
            assert_eq!(charge_state, "applied");
            let event_keys = events
                .lock()
                .await
                .iter()
                .map(BillingEvent::semantic_key)
                .collect::<Vec<_>>();
            assert_eq!(
                event_keys,
                vec![BillingEventKey::HostChargePaid(HostChargeTargetId::new(
                    target_id
                ))]
            );
            Ok::<_, Box<dyn Error>>(())
        }
        .await;
        let cleanup = database.cleanup().await;
        result?;
        cleanup?;
        Ok(())
    }

    #[tokio::test]
    async fn reservation_race_replays_equivalent_contact_and_rejects_changed_contact()
    -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("rail_host_race").await?;
        let result = async {
            sqlx::query(
                r#"
                CREATE TABLE host_charge_targets (
                    id uuid PRIMARY KEY,
                    billing_scope_id uuid NOT NULL,
                    subscriber_id uuid NOT NULL,
                    status text NOT NULL,
                    amount_cents integer NOT NULL,
                    currency text NOT NULL,
                    paid_at timestamptz
                )
                "#,
            )
            .execute(&database.pool)
            .await?;
            let account = create_gateway_account(&database.pool, "nmi").await?;
            let subscriber_id = Uuid::now_v7();
            let target_id = Uuid::now_v7();
            sqlx::query(
                "INSERT INTO host_charge_targets VALUES ($1, $2, $3, 'pending', 1250, 'USD', NULL)",
            )
            .bind(target_id)
            .bind(account.billing_scope_id)
            .bind(subscriber_id)
            .execute(&database.pool)
            .await?;
            let gateway = resolved_gateway(
                account,
                Arc::new(ScriptedGateway {
                    sale_calls: AtomicUsize::new(0),
                    outcome: Mutex::new(None),
                }),
            );
            let snapshot = syrup_rail::HostChargeTargetSnapshot::new(
                HostChargeTargetId::new(target_id),
                ChargeAmount::new(1250, CurrencyCode::new("USD")?)?,
            );
            let command = ChargeHostTarget::new(
                syrup_rail::BillingScopeId::new(account.billing_scope_id),
                syrup_rail::SubscriberId::new(subscriber_id),
                HostChargeTargetId::new(target_id),
                GatewayConfigurationId::new(account.gateway_configuration_id),
                PaymentToken::new("tok_host_winner")?,
                IdempotencyKey::new("host-reservation-race")?,
                Some(BillingContact::new(
                    None,
                    None,
                    Some("winner@example.test".into()),
                )?),
            );
            let winner_id = PaymentAttemptId::new(Uuid::now_v7());
            let winner =
                HostChargeReservation::from_command(&command, snapshot, &gateway, winner_id)?;
            let mut transaction = database.pool.begin().await?;
            let outcome =
                reserve_host_charge_in_transaction(&mut transaction, &TestTargets, &winner).await?;
            assert!(matches!(outcome, HostChargeReservationOutcome::Reserved(_)));
            transaction.commit().await?;

            let retry = ChargeHostTarget::new(
                command.billing_scope_id(),
                command.subscriber_id(),
                command.target_id(),
                command.gateway_configuration_id(),
                PaymentToken::new("tok_host_retry")?,
                command.idempotency_key().clone(),
                command.billing_contact().cloned(),
            );
            let contender = HostChargeReservation::from_command(
                &retry,
                snapshot,
                &gateway,
                PaymentAttemptId::new(Uuid::now_v7()),
            )?;
            let mut transaction = database.pool.begin().await?;
            let outcome =
                reserve_host_charge_in_transaction(&mut transaction, &TestTargets, &contender)
                    .await?;
            transaction.commit().await?;
            let HostChargeReservationOutcome::Replay(attempt) = outcome else {
                panic!("matching contender should replay the durable winner");
            };
            assert_eq!(attempt.identity().attempt_id(), winner_id);
            assert_eq!(
                attempt.request().billing_contact().email(),
                Some("winner@example.test")
            );

            let changed_contact_retry = ChargeHostTarget::new(
                command.billing_scope_id(),
                command.subscriber_id(),
                command.target_id(),
                command.gateway_configuration_id(),
                PaymentToken::new("tok_host_changed_contact")?,
                command.idempotency_key().clone(),
                Some(BillingContact::new(
                    None,
                    None,
                    Some("changed@example.test".into()),
                )?),
            );
            let changed_contact_contender = HostChargeReservation::from_command(
                &changed_contact_retry,
                snapshot,
                &gateway,
                PaymentAttemptId::new(Uuid::now_v7()),
            )?;
            let mut transaction = database.pool.begin().await?;
            let outcome = reserve_host_charge_in_transaction(
                &mut transaction,
                &TestTargets,
                &changed_contact_contender,
            )
            .await?;
            transaction.rollback().await?;
            assert_eq!(outcome, HostChargeReservationOutcome::IdempotencyConflict);
            Ok::<_, Box<dyn Error>>(())
        }
        .await;
        let cleanup = database.cleanup().await;
        result?;
        cleanup?;
        Ok(())
    }

    #[tokio::test]
    async fn post_reservation_rate_limit_resolves_attempt_and_extends_provider_cooldown()
    -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("rail_host_rate").await?;
        let result = async {
            sqlx::query(
                r#"
                CREATE TABLE host_charge_targets (
                    id uuid PRIMARY KEY,
                    billing_scope_id uuid NOT NULL,
                    subscriber_id uuid NOT NULL,
                    status text NOT NULL,
                    amount_cents integer NOT NULL,
                    currency text NOT NULL,
                    paid_at timestamptz
                )
                "#,
            )
            .execute(&database.pool)
            .await?;
            let account = create_gateway_account(&database.pool, "nmi").await?;
            let subscriber_id = Uuid::now_v7();
            let target_id = Uuid::now_v7();
            sqlx::query(
                "INSERT INTO host_charge_targets VALUES ($1, $2, $3, 'pending', 1250, 'USD', NULL)",
            )
            .bind(target_id)
            .bind(account.billing_scope_id)
            .bind(subscriber_id)
            .execute(&database.pool)
            .await?;

            let gateway = Arc::new(RateLimitedAfterReservationGateway {
                readiness_calls: AtomicUsize::new(0),
                sale_calls: AtomicUsize::new(0),
            });
            let resolver = Arc::new(StaticResolver {
                gateway: resolved_gateway(account, gateway.clone()),
                calls: AtomicUsize::new(0),
            });
            let admission = Arc::new(PermitAdmission {
                calls: AtomicUsize::new(0),
            });
            let service = SubscriptionBillingService::new(
                database.pool.clone(),
                Arc::new(UnusedOffers),
                resolver,
                admission,
                Arc::new(TestCoordinator {
                    pool: database.pool.clone(),
                    events: Arc::new(Mutex::new(Vec::new())),
                }),
            )
            .with_host_charge_targets(Arc::new(TestTargets));
            let command = ChargeHostTarget::new(
                syrup_rail::BillingScopeId::new(account.billing_scope_id),
                syrup_rail::SubscriberId::new(subscriber_id),
                HostChargeTargetId::new(target_id),
                GatewayConfigurationId::new(account.gateway_configuration_id),
                PaymentToken::new("tok_host_throttled")?,
                IdempotencyKey::new("host-throttled")?,
                None,
            );

            let error = service
                .charge_host_target(command)
                .await
                .expect_err("provider throttle must be reported");
            assert!(matches!(
                error,
                crate::SubscriptionBillingServiceError::GatewayMutationCooldown {
                    scope: crate::GatewayMutationCooldownScope::Provider
                }
            ));
            assert_eq!(gateway.readiness_calls.load(Ordering::SeqCst), 2);
            assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 0);

            let attempt = sqlx::query_as::<_, (String, Option<String>, Option<DateTime<Utc>>)>(
                r#"
                SELECT status, resolution_code, submitted_at
                FROM billing_payment_attempts
                WHERE billing_scope_id = $1 AND subscriber_id = $2
                "#,
            )
            .bind(account.billing_scope_id)
            .bind(subscriber_id)
            .fetch_one(&database.pool)
            .await?;
            assert_eq!(attempt.0, "failed");
            assert_eq!(
                attempt.1.as_deref(),
                Some("gateway_provider_rate_limited_before_submission")
            );
            assert!(attempt.2.is_none());
            let cooldown_is_active: bool = sqlx::query_scalar(
                "SELECT rate_limited_until > clock_timestamp() FROM billing_gateway_provider_rate_limits WHERE provider_key = 'nmi'",
            )
            .fetch_one(&database.pool)
            .await?;
            assert!(cooldown_is_active);
            let target_status: String =
                sqlx::query_scalar("SELECT status FROM host_charge_targets WHERE id = $1")
                    .bind(target_id)
                    .fetch_one(&database.pool)
                    .await?;
            assert_eq!(target_status, "pending");
            Ok::<_, Box<dyn Error>>(())
        }
        .await;
        let cleanup = database.cleanup().await;
        result?;
        cleanup?;
        Ok(())
    }

    #[tokio::test]
    async fn terminal_approval_race_does_not_mark_host_target_paid() -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("rail_host_race").await?;
        let result = async {
            sqlx::query(
                r#"
                CREATE TABLE host_charge_targets (
                    id uuid PRIMARY KEY,
                    billing_scope_id uuid NOT NULL,
                    subscriber_id uuid NOT NULL,
                    status text NOT NULL,
                    amount_cents integer NOT NULL,
                    currency text NOT NULL,
                    paid_at timestamptz
                )
                "#,
            )
            .execute(&database.pool)
            .await?;
            let account = create_gateway_account(&database.pool, "nmi").await?;
            let subscriber_id = Uuid::now_v7();
            let target_id = Uuid::now_v7();
            sqlx::query(
                "INSERT INTO host_charge_targets VALUES ($1, $2, $3, 'pending', 1250, 'USD', NULL)",
            )
            .bind(target_id)
            .bind(account.billing_scope_id)
            .bind(subscriber_id)
            .execute(&database.pool)
            .await?;

            let gateway = Arc::new(TerminalRaceGateway {
                pool: database.pool.clone(),
                sale_calls: AtomicUsize::new(0),
            });
            let resolver = Arc::new(StaticResolver {
                gateway: resolved_gateway(account, gateway.clone()),
                calls: AtomicUsize::new(0),
            });
            let events = Arc::new(Mutex::new(Vec::new()));
            let service = SubscriptionBillingService::new(
                database.pool.clone(),
                Arc::new(UnusedOffers),
                resolver,
                Arc::new(PermitAdmission {
                    calls: AtomicUsize::new(0),
                }),
                Arc::new(TestCoordinator {
                    pool: database.pool.clone(),
                    events: Arc::clone(&events),
                }),
            )
            .with_host_charge_targets(Arc::new(TestTargets));
            let payment = service
                .charge_host_target(ChargeHostTarget::new(
                    syrup_rail::BillingScopeId::new(account.billing_scope_id),
                    syrup_rail::SubscriberId::new(subscriber_id),
                    HostChargeTargetId::new(target_id),
                    GatewayConfigurationId::new(account.gateway_configuration_id),
                    PaymentToken::new("tok_host_race")?,
                    IdempotencyKey::new("host-race")?,
                    None,
                ))
                .await?;

            assert_eq!(payment.status(), PaymentAttemptStatus::Unknown);
            assert_eq!(payment.attempt().status(), PaymentAttemptStatus::Failed);
            assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 1);
            assert!(events.lock().await.is_empty());
            let target_status: String =
                sqlx::query_scalar("SELECT status FROM host_charge_targets WHERE id = $1")
                    .bind(target_id)
                    .fetch_one(&database.pool)
                    .await?;
            assert_eq!(target_status, "pending");
            let charge_state: String = sqlx::query_scalar(
                "SELECT progression_state FROM billing_processor_charges WHERE attempt_id = $1",
            )
            .bind(payment.attempt().identity().attempt_id().as_uuid())
            .fetch_one(&database.pool)
            .await?;
            assert_eq!(charge_state, "pending");
            Ok::<_, Box<dyn Error>>(())
        }
        .await;
        let cleanup = database.cleanup().await;
        result?;
        cleanup?;
        Ok(())
    }

    #[tokio::test]
    async fn matching_retry_resumes_prepared_attempt_without_readiness_loser_overwrite()
    -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("rail_host_resume").await?;
        let result = async {
            sqlx::query(
                r#"
                CREATE TABLE host_charge_targets (
                    id uuid PRIMARY KEY,
                    billing_scope_id uuid NOT NULL,
                    subscriber_id uuid NOT NULL,
                    status text NOT NULL,
                    amount_cents integer NOT NULL,
                    currency text NOT NULL,
                    paid_at timestamptz
                )
                "#,
            )
            .execute(&database.pool)
            .await?;
            let account = create_gateway_account(&database.pool, "nmi").await?;
            let subscriber_id = Uuid::now_v7();
            let target_id = Uuid::now_v7();
            sqlx::query(
                "INSERT INTO host_charge_targets VALUES ($1, $2, $3, 'pending', 1250, 'USD', NULL)",
            )
            .bind(target_id)
            .bind(account.billing_scope_id)
            .bind(subscriber_id)
            .execute(&database.pool)
            .await?;

            let (readiness_started_tx, readiness_started_rx) = oneshot::channel();
            let (sale_started_tx, sale_started_rx) = oneshot::channel();
            let gateway = Arc::new(RacingPreparedRetryGateway {
                readiness_calls: AtomicUsize::new(0),
                sale_calls: AtomicUsize::new(0),
                blocked_readiness_started: Mutex::new(Some(readiness_started_tx)),
                sale_started: Mutex::new(Some(sale_started_tx)),
                release_readiness: Notify::new(),
                release_sale: Notify::new(),
            });
            let service = SubscriptionBillingService::new(
                database.pool.clone(),
                Arc::new(UnusedOffers),
                Arc::new(StaticResolver {
                    gateway: resolved_gateway(account, gateway.clone()),
                    calls: AtomicUsize::new(0),
                }),
                Arc::new(PermitAdmission {
                    calls: AtomicUsize::new(0),
                }),
                Arc::new(TestCoordinator {
                    pool: database.pool.clone(),
                    events: Arc::new(Mutex::new(Vec::new())),
                }),
            )
            .with_host_charge_targets(Arc::new(TestTargets));
            let command = ChargeHostTarget::new(
                syrup_rail::BillingScopeId::new(account.billing_scope_id),
                syrup_rail::SubscriberId::new(subscriber_id),
                HostChargeTargetId::new(target_id),
                GatewayConfigurationId::new(account.gateway_configuration_id),
                PaymentToken::new("tok_host_resume")?,
                IdempotencyKey::new("host-resume")?,
                None,
            );

            let first_service = service.clone();
            let first_command = command.clone();
            let first =
                tokio::spawn(async move { first_service.charge_host_target(first_command).await });
            readiness_started_rx.await?;
            let retry = ChargeHostTarget::new(
                command.billing_scope_id(),
                command.subscriber_id(),
                command.target_id(),
                command.gateway_configuration_id(),
                PaymentToken::new("tok_host_resume_retry")?,
                command.idempotency_key().clone(),
                command.billing_contact().cloned(),
            );
            let second_service = service.clone();
            let second =
                tokio::spawn(async move { second_service.charge_host_target(retry).await });
            sale_started_rx.await?;
            gateway.release_readiness.notify_one();
            let first = first.await??;
            assert_eq!(first.status(), PaymentAttemptStatus::Pending);
            assert!(
                first
                    .attempt()
                    .state()
                    .timestamps()
                    .submitted_at()
                    .is_some()
            );
            gateway.release_sale.notify_one();
            let second = second.await??;
            assert_eq!(second.status(), PaymentAttemptStatus::Approved);
            assert_eq!(first.attempt().identity(), second.attempt().identity());
            assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 1);
            Ok::<_, Box<dyn Error>>(())
        }
        .await;
        let cleanup = database.cleanup().await;
        result?;
        cleanup?;
        Ok(())
    }
}
