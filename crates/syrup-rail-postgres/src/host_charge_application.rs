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
    GatewayMutationCooldownScope, HostChargeStoreError, HostChargeSubmissionOutcome,
    HostChargeTargetError, HostChargeTargetStore, ProcessorChargeStoreError,
    admit_host_charge_submission_in_transaction,
    attempts::{
        AttemptApproval, AttemptResolutionStatus, AttemptTransition, PaymentAttemptStoreError,
        find_payment_attempt_by_id_on_connection, lock_payment_attempt_by_id_on_connection,
        persist_attempt_transition,
    },
    enrollment_application::{
        OutcomeResolutionBoundary, SubscriptionEnrollmentApplicationError,
        map_attempt_transition_error, mutation_error_evidence, park_locked_attempt,
        set_application_timeouts,
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

/// The readiness facts that can terminalize an already prepared host charge.
///
/// This remains host-specific: subscription workflows use their own outcome
/// command because they have different target and lifecycle effects.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HostChargeReadinessEvent {
    LiveModeUnavailable,
    RequestRejected,
    Malformed,
    Configuration,
    GatewayRateLimited,
    ProviderRateLimited,
}

/// A closed, host-specific command for applying every non-approved host
/// charge outcome. The command is opaque outside this module; callers can
/// construct only a causal outcome, while persistence gets its complete
/// projection rather than independently selectable behavior flags.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HostChargeResolutionCommand {
    event: HostChargeResolutionEvent,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HostChargeResolutionEvent {
    SubmittedDeclined,
    SubmittedFailed,
    SubmittedUnknown,
    SubmittedRateLimitedIndeterminate,
    NotSubmittedRequestRejected,
    NotSubmittedMalformed,
    NotSubmittedConfiguration,
    NotSubmittedUnavailable,
    NotSubmittedRateLimited,
    PreparedReadiness(HostChargeReadinessEvent),
    PreparedCooldown(GatewayMutationCooldownScope),
    AdmittedCooldown(GatewayMutationCooldownScope),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HostChargeTargetDisposition {
    Preserve,
    ReleaseForPaymentFailure,
}

impl HostChargeTargetDisposition {
    const fn releases_target(self) -> bool {
        matches!(self, Self::ReleaseForPaymentFailure)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct HostChargeResolutionProjection {
    status: AttemptResolutionStatus,
    resolution_code: Option<PaymentResolutionCode>,
    boundary: OutcomeResolutionBoundary,
    target_disposition: HostChargeTargetDisposition,
    extend_provider_cooldown: bool,
    clear_submitted_at: bool,
}

impl HostChargeResolutionProjection {
    const fn prepared_failure(resolution_code: PaymentResolutionCode) -> Self {
        Self {
            status: AttemptResolutionStatus::Failed,
            resolution_code: Some(resolution_code),
            boundary: OutcomeResolutionBoundary::Prepared,
            target_disposition: HostChargeTargetDisposition::Preserve,
            extend_provider_cooldown: false,
            clear_submitted_at: false,
        }
    }

    const fn admitted_failure(
        resolution_code: PaymentResolutionCode,
        target_disposition: HostChargeTargetDisposition,
    ) -> Self {
        Self {
            status: AttemptResolutionStatus::Failed,
            resolution_code: Some(resolution_code),
            boundary: OutcomeResolutionBoundary::AdmittedNotSubmitted,
            target_disposition,
            extend_provider_cooldown: false,
            clear_submitted_at: true,
        }
    }

    const fn submitted_resolution(status: AttemptResolutionStatus) -> Self {
        Self {
            status,
            resolution_code: None,
            boundary: OutcomeResolutionBoundary::Submitted,
            target_disposition: HostChargeTargetDisposition::ReleaseForPaymentFailure,
            extend_provider_cooldown: false,
            clear_submitted_at: false,
        }
    }

    const fn submitted_unknown(extend_provider_cooldown: bool) -> Self {
        Self {
            status: AttemptResolutionStatus::Unknown,
            resolution_code: None,
            boundary: OutcomeResolutionBoundary::Submitted,
            target_disposition: HostChargeTargetDisposition::Preserve,
            extend_provider_cooldown,
            clear_submitted_at: false,
        }
    }
}

impl HostChargeResolutionCommand {
    const fn submitted_declined() -> Self {
        Self {
            event: HostChargeResolutionEvent::SubmittedDeclined,
        }
    }

    const fn submitted_failed() -> Self {
        Self {
            event: HostChargeResolutionEvent::SubmittedFailed,
        }
    }

    const fn submitted_unknown() -> Self {
        Self {
            event: HostChargeResolutionEvent::SubmittedUnknown,
        }
    }

    const fn submitted_rate_limited_indeterminate() -> Self {
        Self {
            event: HostChargeResolutionEvent::SubmittedRateLimitedIndeterminate,
        }
    }

    const fn not_submitted(error: &GatewayNotSubmittedError) -> Self {
        let event = match error {
            GatewayNotSubmittedError::RequestRejected(_) => {
                HostChargeResolutionEvent::NotSubmittedRequestRejected
            }
            GatewayNotSubmittedError::Malformed(_) => {
                HostChargeResolutionEvent::NotSubmittedMalformed
            }
            GatewayNotSubmittedError::Configuration(_) => {
                HostChargeResolutionEvent::NotSubmittedConfiguration
            }
            GatewayNotSubmittedError::Unavailable(_) => {
                HostChargeResolutionEvent::NotSubmittedUnavailable
            }
            GatewayNotSubmittedError::RateLimited(_) => {
                HostChargeResolutionEvent::NotSubmittedRateLimited
            }
        };
        Self { event }
    }

    pub(crate) const fn prepared_readiness(event: HostChargeReadinessEvent) -> Self {
        Self {
            event: HostChargeResolutionEvent::PreparedReadiness(event),
        }
    }

    pub(crate) const fn prepared_cooldown(scope: GatewayMutationCooldownScope) -> Self {
        Self {
            event: HostChargeResolutionEvent::PreparedCooldown(scope),
        }
    }

    pub(crate) const fn admitted_cooldown(scope: GatewayMutationCooldownScope) -> Self {
        Self {
            event: HostChargeResolutionEvent::AdmittedCooldown(scope),
        }
    }

    const fn projection(self) -> HostChargeResolutionProjection {
        use AttemptResolutionStatus::{Declined, Failed};
        use GatewayMutationCooldownScope::{Account, Provider};
        use HostChargeResolutionEvent::{
            AdmittedCooldown, NotSubmittedConfiguration, NotSubmittedMalformed,
            NotSubmittedRateLimited, NotSubmittedRequestRejected, NotSubmittedUnavailable,
            PreparedCooldown, PreparedReadiness, SubmittedDeclined, SubmittedFailed,
            SubmittedRateLimitedIndeterminate, SubmittedUnknown,
        };
        use HostChargeTargetDisposition::{Preserve, ReleaseForPaymentFailure};
        match self.event {
            SubmittedDeclined => HostChargeResolutionProjection::submitted_resolution(Declined),
            SubmittedFailed => HostChargeResolutionProjection::submitted_resolution(Failed),
            SubmittedUnknown => HostChargeResolutionProjection::submitted_unknown(false),
            SubmittedRateLimitedIndeterminate => {
                HostChargeResolutionProjection::submitted_unknown(true)
            }
            NotSubmittedRequestRejected => HostChargeResolutionProjection::admitted_failure(
                PaymentResolutionCode::GatewayRequestRejectedBeforeSubmission,
                ReleaseForPaymentFailure,
            ),
            NotSubmittedMalformed => HostChargeResolutionProjection::admitted_failure(
                PaymentResolutionCode::GatewayMalformedBeforeSubmission,
                ReleaseForPaymentFailure,
            ),
            NotSubmittedConfiguration => HostChargeResolutionProjection::admitted_failure(
                PaymentResolutionCode::GatewayConfigurationBeforeSubmission,
                ReleaseForPaymentFailure,
            ),
            NotSubmittedUnavailable => HostChargeResolutionProjection::admitted_failure(
                PaymentResolutionCode::GatewayUnavailableBeforeSubmission,
                ReleaseForPaymentFailure,
            ),
            NotSubmittedRateLimited => HostChargeResolutionProjection::admitted_failure(
                PaymentResolutionCode::GatewayProviderRateLimitedBeforeSubmission,
                Preserve,
            ),
            PreparedReadiness(HostChargeReadinessEvent::LiveModeUnavailable) => {
                HostChargeResolutionProjection::prepared_failure(
                    PaymentResolutionCode::GatewayLiveReadinessFailedBeforeSubmission,
                )
            }
            PreparedReadiness(HostChargeReadinessEvent::RequestRejected) => {
                HostChargeResolutionProjection::prepared_failure(
                    PaymentResolutionCode::GatewayRequestRejectedBeforeSubmission,
                )
            }
            PreparedReadiness(HostChargeReadinessEvent::Malformed) => {
                HostChargeResolutionProjection::prepared_failure(
                    PaymentResolutionCode::GatewayMalformedBeforeSubmission,
                )
            }
            PreparedReadiness(HostChargeReadinessEvent::Configuration) => {
                HostChargeResolutionProjection::prepared_failure(
                    PaymentResolutionCode::GatewayConfigurationBeforeSubmission,
                )
            }
            PreparedReadiness(HostChargeReadinessEvent::GatewayRateLimited) => {
                HostChargeResolutionProjection::prepared_failure(
                    PaymentResolutionCode::GatewayProviderRateLimitedBeforeSubmission,
                )
            }
            PreparedReadiness(HostChargeReadinessEvent::ProviderRateLimited) => {
                let mut projection = HostChargeResolutionProjection::prepared_failure(
                    PaymentResolutionCode::GatewayProviderRateLimitedBeforeSubmission,
                );
                projection.extend_provider_cooldown = true;
                projection
            }
            PreparedCooldown(Account) => HostChargeResolutionProjection::prepared_failure(
                PaymentResolutionCode::GatewayAccountMutationCooldownBeforeSubmission,
            ),
            PreparedCooldown(Provider) => HostChargeResolutionProjection::prepared_failure(
                PaymentResolutionCode::GatewayProviderRateLimitedBeforeSubmission,
            ),
            AdmittedCooldown(Account) => HostChargeResolutionProjection::admitted_failure(
                PaymentResolutionCode::GatewayAccountMutationCooldownBeforeSubmission,
                Preserve,
            ),
            AdmittedCooldown(Provider) => HostChargeResolutionProjection::admitted_failure(
                PaymentResolutionCode::GatewayProviderRateLimitedBeforeSubmission,
                Preserve,
            ),
        }
    }

    const fn may_resolve(self, status: PaymentAttemptStatus, submitted: bool) -> bool {
        status.is_resolvable()
            && match self.projection().boundary {
                OutcomeResolutionBoundary::Prepared => !submitted,
                OutcomeResolutionBoundary::AdmittedNotSubmitted => submitted,
                OutcomeResolutionBoundary::Submitted => true,
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
            let payment = resolve_host_charge_command(
                pool,
                targets,
                &admission.reservation,
                &evidence,
                HostChargeResolutionCommand::not_submitted(&error),
            )
            .await?;
            Ok(HostChargeProviderResult::NotSubmitted { payment, error })
        }
        Err(GatewayMutationError::RateLimitedIndeterminate(detail)) => resolve_host_charge_command(
            pool,
            targets,
            &admission.reservation,
            &mutation_error_evidence(&detail),
            HostChargeResolutionCommand::submitted_rate_limited_indeterminate(),
        )
        .await
        .map(HostChargeProviderResult::Payment),
        Err(GatewayMutationError::Indeterminate(detail)) => resolve_host_charge_command(
            pool,
            targets,
            &admission.reservation,
            &mutation_error_evidence(&detail),
            HostChargeResolutionCommand::submitted_unknown(),
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
            resolve_host_charge_command(
                pool,
                targets,
                reservation,
                outcome.evidence(),
                HostChargeResolutionCommand::submitted_declined(),
            )
            .await
        }
        GatewayPaymentStatus::Failed => {
            resolve_host_charge_command(
                pool,
                targets,
                reservation,
                outcome.evidence(),
                HostChargeResolutionCommand::submitted_failed(),
            )
            .await
        }
        GatewayPaymentStatus::Unknown => {
            resolve_host_charge_command(
                pool,
                targets,
                reservation,
                outcome.evidence(),
                HostChargeResolutionCommand::submitted_unknown(),
            )
            .await
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
    resolution: HostChargeResolutionCommand,
) -> Result<HostChargePaymentResult, HostChargeApplicationError> {
    let projection = resolution.projection();
    let resolution_code =
        projection
            .resolution_code
            .ok_or(HostChargeApplicationError::InvalidState(
                INVALID_HOST_CHARGE_STATE,
            ))?;
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
    resolve_host_charge_command(pool, targets, reservation, &evidence, resolution).await
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

async fn resolve_host_charge_command(
    pool: &PgPool,
    targets: &dyn HostChargeTargetStore,
    reservation: &HostChargeReservation,
    evidence: &ProcessorEvidence,
    command: HostChargeResolutionCommand,
) -> Result<HostChargePaymentResult, HostChargeApplicationError> {
    let projection = command.projection();
    let identity = reservation.identity();
    let mut transaction = pool.begin().await?;
    set_application_timeouts(&mut transaction).await?;
    if projection.target_disposition.releases_target() {
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
    let may_resolve = command.may_resolve(
        attempt.status(),
        attempt.state().timestamps().submitted_at().is_some(),
    );
    if !may_resolve && projection.target_disposition.releases_target() {
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
                status: projection.status,
                resolution_code: projection.resolution_code,
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
        if projection.clear_submitted_at {
            sqlx::query(
                "UPDATE billing_payment_attempts SET submitted_at = NULL, updated_at = clock_timestamp() WHERE id = $1 AND status = $2",
            )
            .bind(identity.attempt_id().as_uuid())
            .bind(projection.status.as_str())
            .execute(&mut *transaction)
            .await?;
        }
    }
    if projection.extend_provider_cooldown {
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
        SubscriptionBillingServiceError, SubscriptionOfferStore, host_charge_ledger_admission,
        reserve_host_charge_in_transaction,
        test_support::{TestDatabase, create_gateway_account},
    };

    mod readiness_replay;

    #[test]
    fn host_charge_resolution_command_projects_every_causal_event() {
        macro_rules! projection {
            ($status:expr, $code:expr, $boundary:expr, $target:expr, $provider:expr, $clear:expr) => {
                HostChargeResolutionProjection {
                    status: $status,
                    resolution_code: $code,
                    boundary: $boundary,
                    target_disposition: $target,
                    extend_provider_cooldown: $provider,
                    clear_submitted_at: $clear,
                }
            };
        }

        let detail = GatewayDiagnostic::new("test");
        let cases = [
            (
                "submitted decline",
                HostChargeResolutionCommand::submitted_declined(),
                projection!(
                    AttemptResolutionStatus::Declined,
                    None,
                    OutcomeResolutionBoundary::Submitted,
                    HostChargeTargetDisposition::ReleaseForPaymentFailure,
                    false,
                    false
                ),
            ),
            (
                "submitted failure",
                HostChargeResolutionCommand::submitted_failed(),
                projection!(
                    AttemptResolutionStatus::Failed,
                    None,
                    OutcomeResolutionBoundary::Submitted,
                    HostChargeTargetDisposition::ReleaseForPaymentFailure,
                    false,
                    false
                ),
            ),
            (
                "submitted indeterminate",
                HostChargeResolutionCommand::submitted_unknown(),
                projection!(
                    AttemptResolutionStatus::Unknown,
                    None,
                    OutcomeResolutionBoundary::Submitted,
                    HostChargeTargetDisposition::Preserve,
                    false,
                    false
                ),
            ),
            (
                "submitted rate-limited indeterminate",
                HostChargeResolutionCommand::submitted_rate_limited_indeterminate(),
                projection!(
                    AttemptResolutionStatus::Unknown,
                    None,
                    OutcomeResolutionBoundary::Submitted,
                    HostChargeTargetDisposition::Preserve,
                    true,
                    false
                ),
            ),
            (
                "not-submitted request rejection",
                HostChargeResolutionCommand::not_submitted(
                    &GatewayNotSubmittedError::RequestRejected(detail.clone()),
                ),
                projection!(
                    AttemptResolutionStatus::Failed,
                    Some(PaymentResolutionCode::GatewayRequestRejectedBeforeSubmission),
                    OutcomeResolutionBoundary::AdmittedNotSubmitted,
                    HostChargeTargetDisposition::ReleaseForPaymentFailure,
                    false,
                    true
                ),
            ),
            (
                "not-submitted malformed request",
                HostChargeResolutionCommand::not_submitted(&GatewayNotSubmittedError::Malformed(
                    detail.clone(),
                )),
                projection!(
                    AttemptResolutionStatus::Failed,
                    Some(PaymentResolutionCode::GatewayMalformedBeforeSubmission),
                    OutcomeResolutionBoundary::AdmittedNotSubmitted,
                    HostChargeTargetDisposition::ReleaseForPaymentFailure,
                    false,
                    true
                ),
            ),
            (
                "not-submitted configuration",
                HostChargeResolutionCommand::not_submitted(
                    &GatewayNotSubmittedError::Configuration(detail.clone()),
                ),
                projection!(
                    AttemptResolutionStatus::Failed,
                    Some(PaymentResolutionCode::GatewayConfigurationBeforeSubmission),
                    OutcomeResolutionBoundary::AdmittedNotSubmitted,
                    HostChargeTargetDisposition::ReleaseForPaymentFailure,
                    false,
                    true
                ),
            ),
            (
                "not-submitted unavailable",
                HostChargeResolutionCommand::not_submitted(&GatewayNotSubmittedError::Unavailable(
                    detail.clone(),
                )),
                projection!(
                    AttemptResolutionStatus::Failed,
                    Some(PaymentResolutionCode::GatewayUnavailableBeforeSubmission),
                    OutcomeResolutionBoundary::AdmittedNotSubmitted,
                    HostChargeTargetDisposition::ReleaseForPaymentFailure,
                    false,
                    true
                ),
            ),
            (
                "not-submitted rate limit",
                HostChargeResolutionCommand::not_submitted(&GatewayNotSubmittedError::RateLimited(
                    detail.clone(),
                )),
                projection!(
                    AttemptResolutionStatus::Failed,
                    Some(PaymentResolutionCode::GatewayProviderRateLimitedBeforeSubmission),
                    OutcomeResolutionBoundary::AdmittedNotSubmitted,
                    HostChargeTargetDisposition::Preserve,
                    false,
                    true
                ),
            ),
            (
                "prepared live-mode readiness",
                HostChargeResolutionCommand::prepared_readiness(
                    HostChargeReadinessEvent::LiveModeUnavailable,
                ),
                projection!(
                    AttemptResolutionStatus::Failed,
                    Some(PaymentResolutionCode::GatewayLiveReadinessFailedBeforeSubmission),
                    OutcomeResolutionBoundary::Prepared,
                    HostChargeTargetDisposition::Preserve,
                    false,
                    false
                ),
            ),
            (
                "prepared rejected readiness",
                HostChargeResolutionCommand::prepared_readiness(
                    HostChargeReadinessEvent::RequestRejected,
                ),
                projection!(
                    AttemptResolutionStatus::Failed,
                    Some(PaymentResolutionCode::GatewayRequestRejectedBeforeSubmission),
                    OutcomeResolutionBoundary::Prepared,
                    HostChargeTargetDisposition::Preserve,
                    false,
                    false
                ),
            ),
            (
                "prepared malformed readiness",
                HostChargeResolutionCommand::prepared_readiness(
                    HostChargeReadinessEvent::Malformed,
                ),
                projection!(
                    AttemptResolutionStatus::Failed,
                    Some(PaymentResolutionCode::GatewayMalformedBeforeSubmission),
                    OutcomeResolutionBoundary::Prepared,
                    HostChargeTargetDisposition::Preserve,
                    false,
                    false
                ),
            ),
            (
                "prepared configuration readiness",
                HostChargeResolutionCommand::prepared_readiness(
                    HostChargeReadinessEvent::Configuration,
                ),
                projection!(
                    AttemptResolutionStatus::Failed,
                    Some(PaymentResolutionCode::GatewayConfigurationBeforeSubmission),
                    OutcomeResolutionBoundary::Prepared,
                    HostChargeTargetDisposition::Preserve,
                    false,
                    false
                ),
            ),
            (
                "prepared gateway rate limit",
                HostChargeResolutionCommand::prepared_readiness(
                    HostChargeReadinessEvent::GatewayRateLimited,
                ),
                projection!(
                    AttemptResolutionStatus::Failed,
                    Some(PaymentResolutionCode::GatewayProviderRateLimitedBeforeSubmission),
                    OutcomeResolutionBoundary::Prepared,
                    HostChargeTargetDisposition::Preserve,
                    false,
                    false
                ),
            ),
            (
                "prepared provider rate limit",
                HostChargeResolutionCommand::prepared_readiness(
                    HostChargeReadinessEvent::ProviderRateLimited,
                ),
                projection!(
                    AttemptResolutionStatus::Failed,
                    Some(PaymentResolutionCode::GatewayProviderRateLimitedBeforeSubmission),
                    OutcomeResolutionBoundary::Prepared,
                    HostChargeTargetDisposition::Preserve,
                    true,
                    false
                ),
            ),
            (
                "prepared account cooldown",
                HostChargeResolutionCommand::prepared_cooldown(
                    GatewayMutationCooldownScope::Account,
                ),
                projection!(
                    AttemptResolutionStatus::Failed,
                    Some(PaymentResolutionCode::GatewayAccountMutationCooldownBeforeSubmission),
                    OutcomeResolutionBoundary::Prepared,
                    HostChargeTargetDisposition::Preserve,
                    false,
                    false
                ),
            ),
            (
                "prepared provider cooldown",
                HostChargeResolutionCommand::prepared_cooldown(
                    GatewayMutationCooldownScope::Provider,
                ),
                projection!(
                    AttemptResolutionStatus::Failed,
                    Some(PaymentResolutionCode::GatewayProviderRateLimitedBeforeSubmission),
                    OutcomeResolutionBoundary::Prepared,
                    HostChargeTargetDisposition::Preserve,
                    false,
                    false
                ),
            ),
            (
                "admitted account cooldown",
                HostChargeResolutionCommand::admitted_cooldown(
                    GatewayMutationCooldownScope::Account,
                ),
                projection!(
                    AttemptResolutionStatus::Failed,
                    Some(PaymentResolutionCode::GatewayAccountMutationCooldownBeforeSubmission),
                    OutcomeResolutionBoundary::AdmittedNotSubmitted,
                    HostChargeTargetDisposition::Preserve,
                    false,
                    true
                ),
            ),
            (
                "admitted provider cooldown",
                HostChargeResolutionCommand::admitted_cooldown(
                    GatewayMutationCooldownScope::Provider,
                ),
                projection!(
                    AttemptResolutionStatus::Failed,
                    Some(PaymentResolutionCode::GatewayProviderRateLimitedBeforeSubmission),
                    OutcomeResolutionBoundary::AdmittedNotSubmitted,
                    HostChargeTargetDisposition::Preserve,
                    false,
                    true
                ),
            ),
        ];

        for (name, command, expected) in cases {
            assert_eq!(command.projection(), expected, "{name}");
        }
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
        outcome: Mutex<Option<Result<GatewayPaymentOutcome, GatewayMutationError>>>,
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
            self.outcome
                .lock()
                .await
                .take()
                .expect("one sale capability")
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
        outcome_status: GatewayPaymentStatus,
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
            Ok(match self.outcome_status {
                GatewayPaymentStatus::Approved => approved_outcome("host_txn_terminal_race"),
                status => non_approved_outcome(status),
            })
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

    #[derive(Default)]
    struct TransitionCountingTargets {
        payment_failed_transitions: AtomicUsize,
    }

    #[async_trait]
    impl HostChargeTargetStore for TransitionCountingTargets {
        async fn preflight_target(
            &self,
            connection: &mut PgConnection,
            reservation: &HostChargeTargetReservation,
        ) -> Result<HostChargeReservationDecision, HostChargeTargetError> {
            TestTargets.preflight_target(connection, reservation).await
        }

        async fn reserve_target(
            &self,
            connection: &mut PgConnection,
            reservation: &HostChargeTargetReservation,
        ) -> Result<HostChargeReservationDecision, HostChargeTargetError> {
            TestTargets.reserve_target(connection, reservation).await
        }

        async fn admit_submission(
            &self,
            connection: &mut PgConnection,
            admission: &HostChargeSubmissionAdmission,
        ) -> Result<HostChargeSubmissionDecision, HostChargeTargetError> {
            TestTargets.admit_submission(connection, admission).await
        }

        async fn apply_transition(
            &self,
            connection: &mut PgConnection,
            transition: HostChargeTargetTransition,
        ) -> Result<HostChargeTargetTransitionOutcome, HostChargeTargetError> {
            if transition.kind() == HostChargeTargetTransitionKind::PaymentFailed {
                self.payment_failed_transitions
                    .fetch_add(1, Ordering::SeqCst);
            }
            TestTargets.apply_transition(connection, transition).await
        }
    }

    struct RollbackReleaseTargets;

    #[async_trait]
    impl HostChargeTargetStore for RollbackReleaseTargets {
        async fn preflight_target(
            &self,
            connection: &mut PgConnection,
            reservation: &HostChargeTargetReservation,
        ) -> Result<HostChargeReservationDecision, HostChargeTargetError> {
            TestTargets.preflight_target(connection, reservation).await
        }

        async fn reserve_target(
            &self,
            connection: &mut PgConnection,
            reservation: &HostChargeTargetReservation,
        ) -> Result<HostChargeReservationDecision, HostChargeTargetError> {
            TestTargets.reserve_target(connection, reservation).await
        }

        async fn admit_submission(
            &self,
            connection: &mut PgConnection,
            admission: &HostChargeSubmissionAdmission,
        ) -> Result<HostChargeSubmissionDecision, HostChargeTargetError> {
            TestTargets.admit_submission(connection, admission).await
        }

        async fn apply_transition(
            &self,
            connection: &mut PgConnection,
            transition: HostChargeTargetTransition,
        ) -> Result<HostChargeTargetTransitionOutcome, HostChargeTargetError> {
            let outcome = TestTargets.apply_transition(connection, transition).await?;
            if transition.kind() == HostChargeTargetTransitionKind::PaymentFailed
                && outcome == HostChargeTargetTransitionOutcome::Applied
            {
                let updated = sqlx::query(
                    "UPDATE host_charge_targets SET status = 'released' \
                     WHERE id = $1 AND billing_scope_id = $2 AND subscriber_id = $3",
                )
                .bind(transition.target_id().as_uuid())
                .bind(transition.billing_scope_id().as_uuid())
                .bind(transition.subscriber_id().as_uuid())
                .execute(&mut *connection)
                .await
                .map_err(HostChargeTargetError::new)?;
                if updated.rows_affected() != 1 {
                    return Err(HostChargeTargetError::new(std::io::Error::other(
                        "host target transition did not update one row",
                    )));
                }
            }
            Ok(outcome)
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

    fn non_approved_outcome(status: GatewayPaymentStatus) -> GatewayPaymentOutcome {
        GatewayPaymentOutcome::new(
            status,
            ProcessorEvidence::new(
                None,
                None,
                None,
                None,
                Some(GatewayDiagnostic::new("declined or failed")),
                Some(GatewayDiagnostic::new("failed")),
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
                outcome: Mutex::new(Some(Ok(approved_outcome("host_txn_approved")))),
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
    async fn submitted_decline_and_failure_release_target_before_resolving_attempt()
    -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("host_submitted").await?;
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

            for (name, gateway_status, expected_status) in [
                (
                    "declined",
                    GatewayPaymentStatus::Declined,
                    PaymentAttemptStatus::Declined,
                ),
                (
                    "failed",
                    GatewayPaymentStatus::Failed,
                    PaymentAttemptStatus::Failed,
                ),
            ] {
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
                    outcome: Mutex::new(Some(Ok(non_approved_outcome(gateway_status)))),
                });
                let targets = Arc::new(TransitionCountingTargets::default());
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
                .with_host_charge_targets(targets.clone());
                let payment = service
                    .charge_host_target(ChargeHostTarget::new(
                        syrup_rail::BillingScopeId::new(account.billing_scope_id),
                        syrup_rail::SubscriberId::new(subscriber_id),
                        HostChargeTargetId::new(target_id),
                        GatewayConfigurationId::new(account.gateway_configuration_id),
                        PaymentToken::new(format!("tok_host_{name}"))?,
                        IdempotencyKey::new(format!("host-submitted-{name}"))?,
                        None,
                    ))
                    .await?;

                assert_eq!(payment.status(), expected_status, "{name}");
                assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 1, "{name}");
                assert_eq!(
                    targets.payment_failed_transitions.load(Ordering::SeqCst),
                    1,
                    "{name} must transition the host target before resolving the attempt"
                );
                let stored: (String, Option<String>, Option<DateTime<Utc>>) = sqlx::query_as(
                    "SELECT status, resolution_code, submitted_at FROM billing_payment_attempts WHERE id = $1",
                )
                .bind(payment.attempt().identity().attempt_id().as_uuid())
                .fetch_one(&database.pool)
                .await?;
                assert_eq!(stored.0, expected_status.as_str(), "{name}");
                assert!(stored.1.is_none(), "{name}");
                assert!(stored.2.is_some(), "{name} must retain submitted_at");
            }
            Ok::<_, Box<dyn Error>>(())
        }
        .await;
        let cleanup = database.cleanup().await;
        result?;
        cleanup?;
        Ok(())
    }

    #[tokio::test]
    async fn ordinary_and_rate_limited_not_submitted_commands_clear_admission_state()
    -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("host_notsub").await?;
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

            for (name, gateway_error, expected_code, expected_target_transitions) in [
                (
                    "ordinary",
                    GatewayNotSubmittedError::RequestRejected(GatewayDiagnostic::new(
                        "gateway rejected the charge",
                    )),
                    PaymentResolutionCode::GatewayRequestRejectedBeforeSubmission,
                    1,
                ),
                (
                    "rate-limited",
                    GatewayNotSubmittedError::RateLimited(GatewayDiagnostic::new(
                        "gateway throttled the charge",
                    )),
                    PaymentResolutionCode::GatewayProviderRateLimitedBeforeSubmission,
                    0,
                ),
            ] {
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
                    outcome: Mutex::new(Some(Err(GatewayMutationError::NotSubmitted(
                        gateway_error,
                    )))),
                });
                let targets = Arc::new(TransitionCountingTargets::default());
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
                .with_host_charge_targets(targets.clone());
                let error = service
                    .charge_host_target(ChargeHostTarget::new(
                        syrup_rail::BillingScopeId::new(account.billing_scope_id),
                        syrup_rail::SubscriberId::new(subscriber_id),
                        HostChargeTargetId::new(target_id),
                        GatewayConfigurationId::new(account.gateway_configuration_id),
                        PaymentToken::new(format!("tok_host_not_submitted_{name}"))?,
                        IdempotencyKey::new(format!("host-not-submitted-{name}"))?,
                        None,
                    ))
                    .await
                    .expect_err("the not-submitted error remains caller-visible");
                assert!(matches!(
                    error,
                    SubscriptionBillingServiceError::GatewayNotSubmitted(_)
                ));
                assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 1, "{name}");
                assert_eq!(
                    targets.payment_failed_transitions.load(Ordering::SeqCst),
                    expected_target_transitions,
                    "{name} target disposition"
                );

                let stored: (String, Option<String>, Option<DateTime<Utc>>) = sqlx::query_as(
                    "SELECT status, resolution_code, submitted_at \
                     FROM billing_payment_attempts \
                     WHERE billing_scope_id = $1 AND subscriber_id = $2",
                )
                .bind(account.billing_scope_id)
                .bind(subscriber_id)
                .fetch_one(&database.pool)
                .await?;
                assert_eq!(stored.0, "failed", "{name}");
                assert_eq!(stored.1.as_deref(), Some(expected_code.as_str()), "{name}");
                assert!(stored.2.is_none(), "{name} must clear submitted_at");
            }

            let provider_cooldown_active: bool = sqlx::query_scalar(
                "SELECT rate_limited_until > clock_timestamp() \
                 FROM billing_gateway_provider_rate_limits WHERE provider_key = 'nmi'",
            )
            .fetch_one(&database.pool)
            .await?;
            assert!(
                !provider_cooldown_active,
                "not-submitted rate limiting does not fabricate a provider cooldown"
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
                    Some("Mary Ann".into()),
                    Some("Smith".into()),
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
                    Some("Mary".into()),
                    Some("Ann Smith".into()),
                    Some("winner@example.test".into()),
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
    async fn concurrent_terminal_failure_rolls_back_target_release_and_reloads_winner()
    -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("h_release_race").await?;
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
                outcome_status: GatewayPaymentStatus::Declined,
            });
            let events = Arc::new(Mutex::new(Vec::new()));
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
                    events: Arc::clone(&events),
                }),
            )
            .with_host_charge_targets(Arc::new(RollbackReleaseTargets));
            let payment = service
                .charge_host_target(ChargeHostTarget::new(
                    syrup_rail::BillingScopeId::new(account.billing_scope_id),
                    syrup_rail::SubscriberId::new(subscriber_id),
                    HostChargeTargetId::new(target_id),
                    GatewayConfigurationId::new(account.gateway_configuration_id),
                    PaymentToken::new("tok_host_terminal_release_race")?,
                    IdempotencyKey::new("host-terminal-release-race")?,
                    None,
                ))
                .await?;

            assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 1);
            assert_eq!(payment.status(), PaymentAttemptStatus::Failed);
            assert!(payment.attempt().state().resolution_code().is_none());
            assert!(events.lock().await.is_empty());
            let target_status: String =
                sqlx::query_scalar("SELECT status FROM host_charge_targets WHERE id = $1")
                    .bind(target_id)
                    .fetch_one(&database.pool)
                    .await?;
            assert_eq!(
                target_status, "pending",
                "the losing target release must be rolled back before returning the winner"
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
                outcome_status: GatewayPaymentStatus::Approved,
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
