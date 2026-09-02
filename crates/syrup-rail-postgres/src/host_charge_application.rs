use std::{fmt, time::Duration};

use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool};
use syrup_rail::{
    ApprovedProcessorEvidence, BillingEvent, BillingEventSubject, ChargeHostTarget,
    GatewayMutationError, GatewayNotSubmittedError, GatewayPaymentDiagnostic,
    GatewayPaymentOutcome, GatewayPaymentStatus, GatewayProviderKey, GatewaySaleIntent,
    GatewaySaleRequest, HostChargePaymentResult, HostChargePaymentResultBuildError,
    HostChargeReservation, HostChargeTargetTransition, HostChargeTargetTransitionKind,
    HostChargeTargetTransitionOutcome, PaymentAttempt, PaymentAttemptStatus, PaymentResolutionCode,
    ProcessorChargeProgression, ProcessorChargeRole, ProcessorEvidence,
};
use thiserror::Error;

use crate::host_charges::host_charge_attempt_matches_reservation;
use crate::{
    BillingTransactionCoordinator, BillingTransactionError, BillingTransactionSubjectState,
    HostChargeStoreError, HostChargeSubmissionOutcome, HostChargeTargetError,
    HostChargeTargetStore, ModeVerifiedGateway, ProcessorChargeStoreError,
    admit_host_charge_submission_in_transaction,
    attempts::{
        AttemptApproval, AttemptResolutionStatus, AttemptTransition, PaymentAttemptStoreError,
        find_payment_attempt_by_id_on_connection, lock_payment_attempt_by_id_on_connection,
        persist_attempt_transition,
    },
    enrollment_application::{
        GatewayNotSubmittedPolicy, OutcomeResolutionBoundary, PreparedAttemptReplay,
        RateLimitCooldown, RateLimitCooldownCommitError, RateLimitCooldownOperation,
        SubscriptionEnrollmentApplicationError, commit_rate_limit_cooldown_for_operation,
        map_attempt_transition_error, mutation_error_evidence, park_locked_attempt,
        processor_identity_conflict_diagnostics, reconcile_non_approved_evidence,
        restore_prepared_attempt_submission, same_processor_transaction, set_application_timeouts,
        should_surface_not_submitted_application,
    },
    processor_charges::{
        ObservedCharge, observe_processor_charge, promote_conflicting_charge_to_external_reversal,
        transition_charge,
    },
};

const BILLING_LOCK_TIMEOUT: Duration = Duration::from_millis(250);
const APPROVED_APPLICATION_ATTEMPTS: usize = 3;
const APPROVED_EVIDENCE_RETRY_DELAY: Duration = Duration::from_millis(50);
const INVALID_HOST_CHARGE_STATE: &str = "canonical host charge application state is invalid";
const APPROVED_STALE_TARGET_TEXT: &str =
    "Approved host charge could not update its target because the target changed.";
const APPROVED_STORAGE_FAILURE_TEXT: &str =
    "Approved host charge could not be applied; manual review is required.";

fn append_host_observation_diagnostics(
    result: HostChargePaymentResult,
    diagnostics: &[GatewayPaymentDiagnostic],
) -> HostChargePaymentResult {
    let mut combined = result.observation_diagnostics().to_vec();
    combined.extend_from_slice(diagnostics);
    result.with_observation_diagnostics(combined)
}

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
    /// The provider mutation was not contacted. A retry-safe readiness failure
    /// can carry the same pending, prepared payment for same-key replay. A
    /// concurrent terminal result is returned as `Payment` instead.
    NotSubmitted {
        payment: HostChargePaymentResult,
        error: GatewayNotSubmittedError,
    },
}

struct HostChargeResolutionApplication {
    payment: HostChargePaymentResult,
    applied: bool,
}

impl HostChargeResolutionApplication {
    fn into_payment(self) -> HostChargePaymentResult {
        self.payment
    }

    fn should_surface_not_submitted(&self, policy: GatewayNotSubmittedPolicy) -> bool {
        should_surface_not_submitted_application(
            self.applied,
            self.payment.attempt(),
            policy,
            PreparedAttemptReplay::Supported,
        )
    }
}

/// Complete persistence behavior for a host charge resolved before provider
/// submission.
#[derive(Clone, Copy)]
pub(crate) struct HostChargeBeforeSubmissionResolution {
    boundary: OutcomeResolutionBoundary,
    cooldown: Option<RateLimitCooldown>,
}

#[derive(Clone, Copy)]
struct HostChargeNonApprovedResolution<'provider> {
    boundary: OutcomeResolutionBoundary,
    cooldown: Option<(&'provider GatewayProviderKey, RateLimitCooldown)>,
}

impl HostChargeNonApprovedResolution<'_> {
    const fn submitted() -> Self {
        Self {
            boundary: OutcomeResolutionBoundary::Submitted,
            cooldown: None,
        }
    }
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

    pub(crate) const fn with_not_submitted_policy(self, policy: GatewayNotSubmittedPolicy) -> Self {
        Self {
            boundary: self.boundary,
            cooldown: policy.cooldown(),
        }
    }

    const fn with_provider(
        self,
        provider_key: &GatewayProviderKey,
    ) -> HostChargeNonApprovedResolution<'_> {
        HostChargeNonApprovedResolution {
            boundary: self.boundary,
            cooldown: match self.cooldown {
                Some(cooldown) => Some((provider_key, cooldown)),
                None => None,
            },
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
    gateway: ModeVerifiedGateway<'_>,
) -> Result<HostChargeProviderResult, HostChargeApplicationError> {
    let resolved_gateway = gateway.resolved_gateway();
    let reconstructed = HostChargeReservation::from_command(
        command,
        admission.reservation.snapshot(),
        resolved_gateway,
        admission.attempt.identity().attempt_id(),
        admission
            .reservation
            .identity()
            .required_gateway_account_mode(),
    )
    .map_err(|_| HostChargeApplicationError::SubmissionIdentityMismatch)?;
    if reconstructed != admission.reservation
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
    let Some(gateway) = gateway.authorize_attempt(&admission.reservation.identity()) else {
        return Err(HostChargeApplicationError::SubmissionIdentityMismatch);
    };
    let request = GatewaySaleRequest::new(
        admission.reservation.snapshot().charge(),
        admission.attempt.request().gateway_order_id().clone(),
        GatewaySaleIntent::OneTime {
            payment_token: command.payment_token().clone(),
        },
        command.billing_contact().cloned(),
    );
    let provider_key = gateway.provider_key().clone();
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
            let policy = GatewayNotSubmittedPolicy::for_error(&error);
            let application = if policy.restores_prepared_attempt_when_supported() {
                restore_admitted_host_charge_for_retry(pool, &admission.reservation).await?
            } else {
                resolve_host_charge_non_approved(
                    pool,
                    targets,
                    &admission.reservation,
                    &evidence,
                    AttemptResolutionStatus::Failed,
                    Some(policy.resolution_code()),
                    HostChargeBeforeSubmissionResolution::admitted_not_submitted()
                        .with_not_submitted_policy(policy)
                        .with_provider(&provider_key),
                )
                .await?
            };
            if application.should_surface_not_submitted(policy) {
                Ok(HostChargeProviderResult::NotSubmitted {
                    payment: application.payment,
                    error,
                })
            } else {
                Ok(HostChargeProviderResult::Payment(application.payment))
            }
        }
        Err(GatewayMutationError::RateLimitedIndeterminate(detail)) => resolve_host_charge_unknown(
            pool,
            &admission.reservation,
            &mutation_error_evidence(&detail),
            Some((&provider_key, RateLimitCooldown::Provider)),
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

pub async fn apply_host_charge_gateway_outcome(
    pool: &PgPool,
    coordinator: &dyn BillingTransactionCoordinator,
    targets: &dyn HostChargeTargetStore,
    reservation: &HostChargeReservation,
    outcome: &GatewayPaymentOutcome,
) -> Result<HostChargePaymentResult, HostChargeApplicationError> {
    apply_host_charge_gateway_decision(pool, coordinator, targets, reservation, outcome)
        .await
        .map(|result| append_host_observation_diagnostics(result, outcome.diagnostics()))
}

async fn apply_host_charge_gateway_decision(
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
        GatewayPaymentStatus::Declined => resolve_host_charge_non_approved(
            pool,
            targets,
            reservation,
            outcome.evidence(),
            AttemptResolutionStatus::Declined,
            None,
            HostChargeNonApprovedResolution::submitted(),
        )
        .await
        .map(HostChargeResolutionApplication::into_payment),
        GatewayPaymentStatus::Failed => resolve_host_charge_non_approved(
            pool,
            targets,
            reservation,
            outcome.evidence(),
            AttemptResolutionStatus::Failed,
            None,
            HostChargeNonApprovedResolution::submitted(),
        )
        .await
        .map(HostChargeResolutionApplication::into_payment),
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
    provider_key: &GatewayProviderKey,
    detail: syrup_rail::GatewayDiagnostic,
    resolution_code: PaymentResolutionCode,
    resolution: HostChargeBeforeSubmissionResolution,
) -> Result<HostChargePaymentResult, HostChargeApplicationError> {
    let condition = if matches!(
        resolution_code,
        PaymentResolutionCode::GatewayAccountMutationCooldownBeforeSubmission
            | PaymentResolutionCode::GatewayAccountRateLimitedBeforeSubmission
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
        resolution.with_provider(provider_key),
    )
    .await
    .map(HostChargeResolutionApplication::into_payment)
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
    if !processor_identity_conflict_diagnostics(&attempt, evidence).is_empty() {
        transaction.rollback().await?;
        return observe_conflicting_host_charge_approval(coordinator, reservation, evidence).await;
    }
    if attempt.status() == PaymentAttemptStatus::Approved {
        // Host target callbacks establish the repository-wide target -> attempt
        // lock order. Once this lock proves approval already committed, a
        // refused Paid replay is harmless: lifecycle reconciliation may have
        // legitimately advanced the target to a reversed state.
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

async fn observe_conflicting_host_charge_approval(
    coordinator: &dyn BillingTransactionCoordinator,
    reservation: &HostChargeReservation,
    evidence: &ProcessorEvidence,
) -> Result<HostChargePaymentResult, HostChargeApplicationError> {
    let identity = reservation.identity();
    let mut transaction = coordinator
        .begin(
            BillingEventSubject::new(identity.billing_scope_id(), identity.subscriber_id()),
            BILLING_LOCK_TIMEOUT,
        )
        .await?;
    let connection = transaction.connection();
    set_application_timeouts(connection).await?;
    let attempt = lock_expected_host_charge(connection, reservation).await?;
    let diagnostics = processor_identity_conflict_diagnostics(&attempt, evidence);
    if diagnostics.is_empty() {
        transaction.rollback().await?;
        return Ok(HostChargePaymentResult::new(attempt)?);
    }
    if attempt.status() == PaymentAttemptStatus::Approved
        && same_processor_transaction(&attempt, evidence)
    {
        transaction.commit().await?;
        return Ok(append_host_observation_diagnostics(
            HostChargePaymentResult::new(attempt)?,
            &diagnostics,
        ));
    }
    let progression = if attempt.status().is_resolvable() {
        ProcessorChargeProgression::ReconciliationRequired
    } else {
        ProcessorChargeProgression::ExternalReversalRequired
    };
    let observation = observe_processor_charge(connection, &attempt, evidence, progression).await?;
    if progression == ProcessorChargeProgression::ExternalReversalRequired
        && evidence.transaction_id().is_some()
    {
        promote_conflicting_charge_to_external_reversal(connection, observation).await?;
    }
    transaction.commit().await?;
    Ok(append_host_observation_diagnostics(
        HostChargePaymentResult::new(attempt)?,
        &diagnostics,
    ))
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

async fn resolve_host_charge_non_approved(
    pool: &PgPool,
    targets: &dyn HostChargeTargetStore,
    reservation: &HostChargeReservation,
    evidence: &ProcessorEvidence,
    status: AttemptResolutionStatus,
    resolution_code: Option<PaymentResolutionCode>,
    resolution: HostChargeNonApprovedResolution<'_>,
) -> Result<HostChargeResolutionApplication, HostChargeApplicationError> {
    let identity = reservation.identity();
    // Provider/account backoff is independent of whether this attempt still
    // owns the host target or can still be resolved. Commit it first so a
    // concurrent operator/reconciliation resolution, stale target, or later
    // host callback failure cannot roll the observed throttle back. A crash
    // after this commit is fail-safe: it may delay work, but same-key replay
    // can still finish the unresolved attempt.
    if let Some((provider_key, cooldown)) = resolution.cooldown {
        commit_host_charge_cooldown(pool, reservation, provider_key, cooldown).await?;
    }
    let mut transaction = pool.begin().await?;
    set_application_timeouts(&mut transaction).await?;
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
                match resolution.boundary {
                    OutcomeResolutionBoundary::Prepared
                    | OutcomeResolutionBoundary::AdmittedNotSubmitted => {
                        HostChargeTargetTransitionKind::ReleasedBeforeSubmission
                    }
                    OutcomeResolutionBoundary::Submitted => {
                        HostChargeTargetTransitionKind::PaymentFailed
                    }
                },
                effective_at,
            ),
        )
        .await?;
    if !target_outcome.is_applied() {
        tracing::warn!(
            target: "syrup_rail::host_charge_target",
            billing_scope_id = %identity.billing_scope_id().as_uuid(),
            subscriber_id = %identity.subscriber_id().as_uuid(),
            attempt_id = %identity.attempt_id().as_uuid(),
            target_id = %reservation.snapshot().target_id().as_uuid(),
            boundary = ?resolution.boundary,
            ?target_outcome,
            "host target refused a payment outcome; leaving the canonical attempt unresolved"
        );
        transaction.rollback().await?;
        let mut application =
            canonical_host_charge_resolution_application(pool, reservation).await?;
        let diagnostics =
            processor_identity_conflict_diagnostics(application.payment.attempt(), evidence);
        application.payment =
            append_host_observation_diagnostics(application.payment, &diagnostics);
        if !host_charge_attempt_may_resolve(application.payment.attempt(), resolution.boundary) {
            return Ok(application);
        }
        return Err(HostChargeApplicationError::InvalidState(
            INVALID_HOST_CHARGE_STATE,
        ));
    }
    let attempt = lock_expected_host_charge(&mut transaction, reservation).await?;
    let reconciled = reconcile_non_approved_evidence(&attempt, evidence);
    let diagnostics = reconciled.identity_conflict_diagnostics();
    let may_resolve = host_charge_attempt_may_resolve(&attempt, resolution.boundary);
    if !may_resolve {
        transaction.rollback().await?;
        let mut canonical = canonical_host_charge_resolution_application(pool, reservation).await?;
        canonical.payment = append_host_observation_diagnostics(canonical.payment, &diagnostics);
        return Ok(canonical);
    }
    if reconciled.has_identity_conflict() {
        transaction.rollback().await?;
        let mut canonical = canonical_host_charge_resolution_application(pool, reservation).await?;
        canonical.payment = append_host_observation_diagnostics(canonical.payment, &diagnostics);
        return Ok(canonical);
    }
    let evidence = &reconciled.evidence;
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
    if resolution.boundary == OutcomeResolutionBoundary::AdmittedNotSubmitted {
        let cleared = sqlx::query(
            "UPDATE billing_payment_attempts SET submitted_at = NULL, updated_at = clock_timestamp() WHERE id = $1 AND status = $2",
        )
        .bind(identity.attempt_id().as_uuid())
        .bind(status.as_str())
        .execute(&mut *transaction)
        .await?;
        if cleared.rows_affected() != 1 {
            return Err(HostChargeApplicationError::InvalidState(
                INVALID_HOST_CHARGE_STATE,
            ));
        }
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
    Ok(HostChargeResolutionApplication {
        payment: HostChargePaymentResult::new(attempt)?,
        applied: true,
    })
}

fn host_charge_attempt_may_resolve(
    attempt: &PaymentAttempt,
    boundary: OutcomeResolutionBoundary,
) -> bool {
    attempt.status().is_resolvable()
        && match boundary {
            OutcomeResolutionBoundary::Prepared => {
                attempt.state().timestamps().submitted_at().is_none()
            }
            OutcomeResolutionBoundary::AdmittedNotSubmitted => {
                attempt.state().timestamps().submitted_at().is_some()
            }
            OutcomeResolutionBoundary::Submitted => true,
        }
}

async fn canonical_host_charge_resolution_application(
    pool: &PgPool,
    reservation: &HostChargeReservation,
) -> Result<HostChargeResolutionApplication, HostChargeApplicationError> {
    let identity = reservation.identity();
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
    Ok(HostChargeResolutionApplication {
        payment: HostChargePaymentResult::new(attempt)?,
        applied: false,
    })
}

async fn restore_admitted_host_charge_for_retry(
    pool: &PgPool,
    reservation: &HostChargeReservation,
) -> Result<HostChargeResolutionApplication, HostChargeApplicationError> {
    // Keep this domain wrapper separate from subscriber restoration: host
    // charges have their own exact lock and result projection. Both wrappers
    // delegate the atomic submitted-at transition to the shared primitive.
    let identity = reservation.identity();
    let mut transaction = pool.begin().await?;
    set_application_timeouts(&mut transaction).await?;
    let attempt = lock_expected_host_charge(&mut transaction, reservation).await?;
    let restored = attempt.status() == PaymentAttemptStatus::Pending
        && attempt.state().timestamps().submitted_at().is_some();
    if restored {
        restore_prepared_attempt_submission(&mut transaction, &attempt).await?;
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
    if restored {
        tracing::warn!(
            target: "syrup_rail::gateway_control_plane",
            attempt_id = %identity.attempt_id().as_uuid(),
            attempt_kind = attempt.kind().as_str(),
            required_gateway_account_mode = identity.required_gateway_account_mode().as_str(),
            "restored admitted host charge after pre-submission control-plane failure"
        );
    }
    Ok(HostChargeResolutionApplication {
        payment: HostChargePaymentResult::new(attempt)?,
        applied: restored,
    })
}

async fn resolve_host_charge_unknown(
    pool: &PgPool,
    reservation: &HostChargeReservation,
    evidence: &ProcessorEvidence,
    cooldown: Option<(&GatewayProviderKey, RateLimitCooldown)>,
) -> Result<HostChargePaymentResult, HostChargeApplicationError> {
    let identity = reservation.identity();
    if let Some((provider_key, cooldown)) = cooldown {
        commit_host_charge_cooldown(pool, reservation, provider_key, cooldown).await?;
    }
    let mut transaction = pool.begin().await?;
    set_application_timeouts(&mut transaction).await?;
    let attempt = lock_expected_host_charge(&mut transaction, reservation).await?;
    let reconciled = reconcile_non_approved_evidence(&attempt, evidence);
    let diagnostics = reconciled.identity_conflict_diagnostics();
    if !attempt.status().is_terminal() {
        let evidence = &reconciled.evidence;
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
    Ok(append_host_observation_diagnostics(
        HostChargePaymentResult::new(attempt)?,
        &diagnostics,
    ))
}

async fn commit_host_charge_cooldown(
    pool: &PgPool,
    reservation: &HostChargeReservation,
    provider_key: &GatewayProviderKey,
    cooldown: RateLimitCooldown,
) -> Result<(), HostChargeApplicationError> {
    match commit_rate_limit_cooldown_for_operation(
        pool,
        reservation.identity(),
        provider_key,
        cooldown,
        RateLimitCooldownOperation::HostCharge,
    )
    .await
    {
        Ok(()) => Ok(()),
        Err(RateLimitCooldownCommitError::Sql(error)) => Err(error.into()),
        Err(RateLimitCooldownCommitError::MissingProviderCooldown) => Err(
            HostChargeApplicationError::InvalidState(INVALID_HOST_CHARGE_STATE),
        ),
    }
}

async fn park_host_charge_approved(
    pool: &PgPool,
    reservation: &HostChargeReservation,
    evidence: &ProcessorEvidence,
) -> Result<HostChargePaymentResult, HostChargeApplicationError> {
    let mut transaction = pool.begin().await?;
    set_application_timeouts(&mut transaction).await?;
    let attempt = lock_expected_host_charge(&mut transaction, reservation).await?;
    let diagnostics = processor_identity_conflict_diagnostics(&attempt, evidence);
    if !diagnostics.is_empty() {
        if attempt.status() == PaymentAttemptStatus::Approved
            && same_processor_transaction(&attempt, evidence)
        {
            transaction.commit().await?;
            return Ok(append_host_observation_diagnostics(
                HostChargePaymentResult::new(attempt)?,
                &diagnostics,
            ));
        }
        let progression = if attempt.status().is_resolvable() {
            ProcessorChargeProgression::ReconciliationRequired
        } else {
            ProcessorChargeProgression::ExternalReversalRequired
        };
        let observation =
            observe_processor_charge(&mut transaction, &attempt, evidence, progression).await?;
        if progression == ProcessorChargeProgression::ExternalReversalRequired {
            promote_conflicting_charge_to_external_reversal(&mut transaction, observation).await?;
        } else if let ObservedCharge::Owned(charge) = observation {
            transition_charge(&mut transaction, charge.id, progression, None).await?;
        }
        transaction.commit().await?;
        return Ok(append_host_observation_diagnostics(
            HostChargePaymentResult::new(attempt)?,
            &diagnostics,
        ));
    }
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
    if !host_charge_attempt_matches_reservation(&attempt, reservation) {
        return Err(HostChargeApplicationError::InvalidState(
            INVALID_HOST_CHARGE_STATE,
        ));
    }
    Ok(attempt)
}

#[cfg(test)]
mod tests;
