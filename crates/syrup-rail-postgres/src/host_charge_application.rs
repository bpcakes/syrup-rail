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
        map_attempt_transition_error, park_locked_attempt, processor_identity_conflict_diagnostics,
        reconcile_non_approved_evidence, restore_prepared_attempt_submission,
        same_processor_transaction, set_application_timeouts,
        should_surface_not_submitted_application,
    },
    processor_charges::{
        ChargeRecord, ObservedCharge, observe_processor_charge,
        promote_conflicting_charge_to_external_reversal, transition_charge,
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

enum LockedHostChargeApproval {
    ConflictingIdentity(PaymentAttempt),
    AlreadyApplied(PaymentAttempt),
    Terminal(PaymentAttempt),
    Ready(PaymentAttempt, ChargeRecord),
}

enum HostChargeApprovedDisposition {
    Commit(Option<Box<BillingEvent>>),
    ObserveConflictAfterRollback,
    ObserveTerminalAfterRollback,
}

struct HostChargeApprovedApplication {
    attempt: PaymentAttempt,
    disposition: HostChargeApprovedDisposition,
}

impl HostChargeApprovedApplication {
    fn commit(attempt: PaymentAttempt, event: Option<BillingEvent>) -> Self {
        Self {
            attempt,
            disposition: HostChargeApprovedDisposition::Commit(event.map(Box::new)),
        }
    }

    const fn observe_conflict_after_rollback(attempt: PaymentAttempt) -> Self {
        Self {
            attempt,
            disposition: HostChargeApprovedDisposition::ObserveConflictAfterRollback,
        }
    }

    const fn observe_terminal_after_rollback(attempt: PaymentAttempt) -> Self {
        Self {
            attempt,
            disposition: HostChargeApprovedDisposition::ObserveTerminalAfterRollback,
        }
    }
}

enum HostChargeObservationDisposition {
    Commit,
    Rollback,
}

struct HostChargeObservation {
    payment: HostChargePaymentResult,
    disposition: HostChargeObservationDisposition,
}

impl HostChargeObservation {
    const fn commit(payment: HostChargePaymentResult) -> Self {
        Self {
            payment,
            disposition: HostChargeObservationDisposition::Commit,
        }
    }

    const fn rollback(payment: HostChargePaymentResult) -> Self {
        Self {
            payment,
            disposition: HostChargeObservationDisposition::Rollback,
        }
    }
}

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
        Err(error) => {
            let evidence = error.processor_evidence();
            match error {
                GatewayMutationError::NotSubmitted(error) => {
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
                GatewayMutationError::RateLimitedIndeterminate(_) => resolve_host_charge_unknown(
                    pool,
                    &admission.reservation,
                    &evidence,
                    Some((&provider_key, RateLimitCooldown::Provider)),
                )
                .await
                .map(HostChargeProviderResult::Payment),
                GatewayMutationError::Indeterminate(_) => {
                    resolve_host_charge_unknown(pool, &admission.reservation, &evidence, None)
                        .await
                        .map(HostChargeProviderResult::Payment)
                }
            }
        }
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
        syrup_rail::ProcessorApprovalEvidence::Absent,
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
    let subject_state = transaction.subject_state();
    let application = apply_host_charge_approved_on_connection(
        transaction.connection(),
        subject_state,
        targets,
        reservation,
        evidence,
    )
    .await;
    finalize_host_charge_approved_application(
        transaction,
        coordinator,
        reservation,
        approved_evidence,
        application,
    )
    .await
}

async fn apply_host_charge_approved_on_connection(
    connection: &mut PgConnection,
    subject_state: BillingTransactionSubjectState,
    targets: &dyn HostChargeTargetStore,
    reservation: &HostChargeReservation,
    evidence: &ProcessorEvidence,
) -> Result<HostChargeApprovedApplication, HostChargeApplicationError> {
    if subject_state != BillingTransactionSubjectState::LiveRecipient {
        return Err(HostChargeApplicationError::InvalidState(
            "a host charge payment event requires a live recipient",
        ));
    }
    set_application_timeouts(connection).await?;
    let identity = reservation.identity();
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
    let (attempt, charge) =
        match classify_locked_host_charge_approval(connection, reservation, evidence).await? {
            LockedHostChargeApproval::ConflictingIdentity(attempt) => {
                return Ok(HostChargeApprovedApplication::observe_conflict_after_rollback(attempt));
            }
            LockedHostChargeApproval::AlreadyApplied(attempt) => {
                return Ok(HostChargeApprovedApplication::commit(attempt, None));
            }
            LockedHostChargeApproval::Terminal(attempt) => {
                return Ok(HostChargeApprovedApplication::observe_terminal_after_rollback(attempt));
            }
            LockedHostChargeApproval::Ready(attempt, charge) => (attempt, charge),
        };
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
            Ok(HostChargeApprovedApplication::commit(applied, Some(event)))
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
            Ok(HostChargeApprovedApplication::commit(parked, None))
        }
        HostChargeTargetTransitionOutcome::Unchanged { .. } => Err(
            HostChargeApplicationError::InvalidState(INVALID_HOST_CHARGE_STATE),
        ),
    }
}

async fn finalize_host_charge_approved_application(
    mut transaction: Box<dyn crate::BillingTransaction>,
    coordinator: &dyn BillingTransactionCoordinator,
    reservation: &HostChargeReservation,
    approved_evidence: &ApprovedProcessorEvidence,
    application: Result<HostChargeApprovedApplication, HostChargeApplicationError>,
) -> Result<HostChargePaymentResult, HostChargeApplicationError> {
    match application {
        Err(error) => {
            let _ = transaction.rollback().await;
            Err(error)
        }
        Ok(application) => match application.disposition {
            HostChargeApprovedDisposition::ObserveConflictAfterRollback => {
                transaction.rollback().await?;
                observe_conflicting_host_charge_approval(
                    coordinator,
                    reservation,
                    approved_evidence.evidence(),
                )
                .await
            }
            HostChargeApprovedDisposition::ObserveTerminalAfterRollback => {
                transaction.rollback().await?;
                observe_terminal_host_charge_approval(
                    coordinator,
                    reservation,
                    &application.attempt,
                    approved_evidence,
                )
                .await
            }
            HostChargeApprovedDisposition::Commit(event) => {
                let payment = match HostChargePaymentResult::new(application.attempt) {
                    Ok(payment) => payment,
                    Err(error) => {
                        let _ = transaction.rollback().await;
                        return Err(error.into());
                    }
                };
                if let Some(event) = event.as_deref()
                    && let Err(error) = transaction.append_event(event).await
                {
                    let _ = transaction.rollback().await;
                    return Err(error.into());
                }
                transaction.commit().await?;
                Ok(payment)
            }
        },
    }
}

async fn classify_locked_host_charge_approval(
    connection: &mut PgConnection,
    reservation: &HostChargeReservation,
    evidence: &ProcessorEvidence,
) -> Result<LockedHostChargeApproval, HostChargeApplicationError> {
    let attempt = lock_expected_host_charge(connection, reservation).await?;
    if !processor_identity_conflict_diagnostics(&attempt, evidence).is_empty() {
        return Ok(LockedHostChargeApproval::ConflictingIdentity(attempt));
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
        return Ok(LockedHostChargeApproval::AlreadyApplied(attempt));
    }
    if attempt.status().is_terminal() {
        return Ok(LockedHostChargeApproval::Terminal(attempt));
    }
    let observation = observe_processor_charge(
        connection,
        &attempt,
        evidence,
        ProcessorChargeProgression::Pending,
    )
    .await?;
    let ObservedCharge::Owned(charge) = observation else {
        return Err(HostChargeApplicationError::InvalidState(
            "the approved gateway transaction belongs to another payment attempt",
        ));
    };
    if charge.role == ProcessorChargeRole::Additional {
        return Err(HostChargeApplicationError::InvalidState(
            "an additional approved host charge requires external reversal",
        ));
    }
    Ok(LockedHostChargeApproval::Ready(attempt, charge))
}

include!("host_charge_application/resolution.rs");
