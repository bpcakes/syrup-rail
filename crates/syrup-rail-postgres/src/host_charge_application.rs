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
        None,
        None,
        None,
        None,
        Some(detail),
        condition,
        syrup_rail::GatewayPaymentDescriptor::default(),
    )
    .with_approval_evidence(syrup_rail::ProcessorApprovalEvidence::Absent);
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

include!("host_charge_application/resolution.rs");
