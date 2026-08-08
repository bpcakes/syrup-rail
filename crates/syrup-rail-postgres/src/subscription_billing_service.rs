use std::{fmt, sync::Arc, time::Duration};

use sqlx::PgPool;
use syrup_rail::{
    BillingScopeId, ChargeRenewal, EndUserMutationAdmission, EndUserMutationAdmissionResult,
    EndUserMutationCommand, EndUserMutationOperation, EnrollSubscription, GatewayAccountId,
    GatewayAccountMode, GatewayDiagnostic, GatewayError, GatewayNotSubmittedError,
    GatewayPaymentDescriptor, GatewayPaymentOutcome, GatewayProviderKey, GatewayResolutionError,
    GatewayResolver, PaymentAttempt, PaymentAttemptId, PaymentAttemptKind, PaymentAttemptStatus,
    PaymentResolutionCode, ProcessorEvidence, RecoverSubscriptionPayment,
    ReplaceSubscriptionPaymentMethod, SubscriptionEnrollmentPaymentResult,
    SubscriptionEnrollmentPreflightOutcome, SubscriptionEnrollmentReservation,
    SubscriptionEnrollmentReservationBuildError, SubscriptionEnrollmentReservationOutcome,
    SubscriptionEnrollmentReservationRejection, SubscriptionEnrollmentSubmissionRejection,
    SubscriptionPaymentMethodReplacement, SubscriptionPaymentMethodReplacementPreflightOutcome,
    SubscriptionPaymentMethodReplacementRejection,
    SubscriptionPaymentMethodReplacementReservationOutcome,
    SubscriptionPaymentMethodReplacementSubmissionRejection, SubscriptionRecoveryPreflightOutcome,
    SubscriptionRecoveryReservation, SubscriptionRecoveryReservationOutcome,
    SubscriptionRecoveryReservationRejection, SubscriptionRecoverySubmissionRejection,
    SubscriptionRenewalOutcome, SubscriptionRenewalReservation,
    SubscriptionRenewalReservationOutcome, SubscriptionRenewalReservationRejection,
};
use thiserror::Error;

use crate::{
    BillingTransactionCoordinator, PaymentAttemptStoreError,
    SubscriptionEnrollmentAdmissionOutcome, SubscriptionEnrollmentApplicationError,
    SubscriptionEnrollmentProviderResult, SubscriptionOfferStore,
    SubscriptionPaymentMethodReplacementAdmissionOutcome,
    SubscriptionPaymentMethodReplacementProviderResult, SubscriptionRecoveryAdmissionOutcome,
    SubscriptionRecoveryProviderResult, SubscriptionRenewalAdmissionOutcome,
    SubscriptionRenewalProviderResult, admit_subscription_enrollment_submission,
    admit_subscription_payment_method_replacement, admit_subscription_recovery_submission,
    admit_subscription_renewal_submission,
    apply_reconciled_subscription_enrollment_gateway_outcome,
    apply_reconciled_subscription_payment_method_replacement_gateway_outcome,
    apply_reconciled_subscription_recovery_gateway_outcome,
    apply_reconciled_subscription_renewal_gateway_outcome,
    enrollment_application::{
        OutcomeResolutionBoundary, RateLimitCooldown, payment_result_for_attempt,
        resolve_non_approved_outcome, resolve_payment_method_replacement_non_approved_outcome,
        resolve_recovery_non_approved_outcome, resolve_renewal_non_approved_outcome,
    },
    preflight_subscription_enrollment_in_transaction,
    preflight_subscription_payment_method_replacement_in_transaction,
    preflight_subscription_recovery_in_transaction, reserve_subscription_enrollment_in_transaction,
    reserve_subscription_payment_method_replacement_in_transaction,
    reserve_subscription_recovery_in_transaction, reserve_subscription_renewal_in_transaction,
    submit_admitted_subscription_enrollment,
    submit_admitted_subscription_payment_method_replacement, submit_admitted_subscription_recovery,
    submit_admitted_subscription_renewal,
};

const INVALID_SERVICE_STATE: &str = "canonical subscription enrollment service state is invalid";
const LIVE_READINESS_FAILED_TEXT: &str =
    "Payment was not submitted because the payment processor was not ready for live transactions.";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GatewayMutationCooldownScope {
    Account,
    Provider,
}

#[derive(Error)]
pub enum SubscriptionEnrollmentServiceError {
    #[error("subscription enrollment storage failed")]
    Sql(#[from] sqlx::Error),
    #[error("subscription enrollment attempt storage failed")]
    Attempt(#[from] PaymentAttemptStoreError),
    #[error("subscription enrollment application failed")]
    Application(#[from] SubscriptionEnrollmentApplicationError),
    #[error("the idempotency key belongs to a different payment request")]
    IdempotencyConflict,
    #[error("end-user mutation admission was denied")]
    AdmissionDenied { retry_after: Duration },
    #[error("end-user mutation admission timed out")]
    AdmissionTimeout,
    #[error("end-user mutation admission is unavailable")]
    AdmissionUnavailable,
    #[error("gateway account or configuration changed")]
    GatewayConfigurationChanged,
    #[error("gateway resolution failed")]
    GatewayResolution(#[from] GatewayResolutionError),
    #[error("gateway resolver returned a different canonical identity")]
    ResolvedGatewayIdentityMismatch,
    #[error("gateway mutation cooldown is active")]
    GatewayMutationCooldown { scope: GatewayMutationCooldownScope },
    #[error("subscription enrollment reservation was rejected")]
    ReservationRejected(SubscriptionEnrollmentReservationRejection),
    #[error("subscription enrollment submission was rejected")]
    SubmissionRejected(SubscriptionEnrollmentSubmissionRejection),
    #[error("subscription recovery reservation was rejected")]
    RecoveryReservationRejected(SubscriptionRecoveryReservationRejection),
    #[error("subscription recovery submission was rejected")]
    RecoverySubmissionRejected(SubscriptionRecoverySubmissionRejection),
    #[error("subscription renewal reservation was rejected")]
    RenewalReservationRejected(SubscriptionRenewalReservationRejection),
    #[error("subscription payment method replacement reservation was rejected")]
    PaymentMethodReplacementReservationRejected(SubscriptionPaymentMethodReplacementRejection),
    #[error("subscription payment method replacement submission was rejected")]
    PaymentMethodReplacementSubmissionRejected(
        SubscriptionPaymentMethodReplacementSubmissionRejection,
    ),
    #[error("gateway mutation was not submitted")]
    GatewayNotSubmitted(#[source] GatewayNotSubmittedError),
    #[error("gateway readiness check failed")]
    GatewayReadiness(#[source] GatewayError),
    #[error("{0}")]
    InvalidState(&'static str),
}

impl fmt::Debug for SubscriptionEnrollmentServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sql(_) => formatter.write_str("SubscriptionEnrollmentServiceError::Sql"),
            Self::Attempt(_) => formatter.write_str("SubscriptionEnrollmentServiceError::Attempt"),
            Self::Application(_) => {
                formatter.write_str("SubscriptionEnrollmentServiceError::Application")
            }
            Self::IdempotencyConflict => {
                formatter.write_str("SubscriptionEnrollmentServiceError::IdempotencyConflict")
            }
            Self::AdmissionDenied { retry_after } => formatter
                .debug_struct("SubscriptionEnrollmentServiceError::AdmissionDenied")
                .field("retry_after", retry_after)
                .finish(),
            Self::AdmissionTimeout => {
                formatter.write_str("SubscriptionEnrollmentServiceError::AdmissionTimeout")
            }
            Self::AdmissionUnavailable => {
                formatter.write_str("SubscriptionEnrollmentServiceError::AdmissionUnavailable")
            }
            Self::GatewayConfigurationChanged => formatter
                .write_str("SubscriptionEnrollmentServiceError::GatewayConfigurationChanged"),
            Self::GatewayResolution(error) => formatter
                .debug_tuple("SubscriptionEnrollmentServiceError::GatewayResolution")
                .field(error)
                .finish(),
            Self::ResolvedGatewayIdentityMismatch => formatter
                .write_str("SubscriptionEnrollmentServiceError::ResolvedGatewayIdentityMismatch"),
            Self::GatewayMutationCooldown { scope } => formatter
                .debug_struct("SubscriptionEnrollmentServiceError::GatewayMutationCooldown")
                .field("scope", scope)
                .finish(),
            Self::ReservationRejected(reason) => formatter
                .debug_tuple("SubscriptionEnrollmentServiceError::ReservationRejected")
                .field(reason)
                .finish(),
            Self::SubmissionRejected(reason) => formatter
                .debug_tuple("SubscriptionEnrollmentServiceError::SubmissionRejected")
                .field(reason)
                .finish(),
            Self::RecoveryReservationRejected(reason) => formatter
                .debug_tuple("SubscriptionEnrollmentServiceError::RecoveryReservationRejected")
                .field(reason)
                .finish(),
            Self::RecoverySubmissionRejected(reason) => formatter
                .debug_tuple("SubscriptionEnrollmentServiceError::RecoverySubmissionRejected")
                .field(reason)
                .finish(),
            Self::RenewalReservationRejected(reason) => formatter
                .debug_tuple("SubscriptionEnrollmentServiceError::RenewalReservationRejected")
                .field(reason)
                .finish(),
            Self::PaymentMethodReplacementReservationRejected(reason) => formatter
                .debug_tuple(
                    "SubscriptionEnrollmentServiceError::PaymentMethodReplacementReservationRejected",
                )
                .field(reason)
                .finish(),
            Self::PaymentMethodReplacementSubmissionRejected(reason) => formatter
                .debug_tuple(
                    "SubscriptionEnrollmentServiceError::PaymentMethodReplacementSubmissionRejected",
                )
                .field(reason)
                .finish(),
            Self::GatewayNotSubmitted(error) => formatter
                .debug_tuple("SubscriptionEnrollmentServiceError::GatewayNotSubmitted")
                .field(error)
                .finish(),
            Self::GatewayReadiness(error) => formatter
                .debug_tuple("SubscriptionEnrollmentServiceError::GatewayReadiness")
                .field(error)
                .finish(),
            Self::InvalidState(detail) => formatter
                .debug_tuple("SubscriptionEnrollmentServiceError::InvalidState")
                .field(detail)
                .finish(),
        }
    }
}

#[derive(Clone)]
pub struct SubscriptionBillingService {
    pool: PgPool,
    offers: Arc<dyn SubscriptionOfferStore>,
    resolver: Arc<dyn GatewayResolver>,
    admission: Arc<dyn EndUserMutationAdmission>,
    coordinator: Arc<dyn BillingTransactionCoordinator>,
}

impl SubscriptionBillingService {
    pub fn new(
        pool: PgPool,
        offers: Arc<dyn SubscriptionOfferStore>,
        resolver: Arc<dyn GatewayResolver>,
        admission: Arc<dyn EndUserMutationAdmission>,
        coordinator: Arc<dyn BillingTransactionCoordinator>,
    ) -> Self {
        Self {
            pool,
            offers,
            resolver,
            admission,
            coordinator,
        }
    }

    /// Runs one complete initial-subscription payment boundary.
    ///
    /// Matching replay and conflict are resolved before host admission. No
    /// database transaction or lock is held across host admission, gateway
    /// resolution, readiness I/O, or the one provider mutation.
    pub async fn enroll(
        &self,
        command: EnrollSubscription,
    ) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentServiceError> {
        match self.preflight(&command).await? {
            SubscriptionEnrollmentPreflightOutcome::Continue => {}
            SubscriptionEnrollmentPreflightOutcome::Replay(attempt) => {
                return self.payment_result(*attempt).await;
            }
            SubscriptionEnrollmentPreflightOutcome::IdempotencyConflict => {
                return Err(SubscriptionEnrollmentServiceError::IdempotencyConflict);
            }
        }

        match self
            .admission
            .admit(EndUserMutationCommand::new(
                command.billing_scope_id(),
                command.subscriber_id(),
                EndUserMutationOperation::SubscriptionInitial,
            ))
            .await
        {
            EndUserMutationAdmissionResult::Allowed => {}
            EndUserMutationAdmissionResult::Denied { retry_after } => {
                return Err(SubscriptionEnrollmentServiceError::AdmissionDenied {
                    retry_after: retry_after.get(),
                });
            }
            EndUserMutationAdmissionResult::Timeout => {
                return Err(SubscriptionEnrollmentServiceError::AdmissionTimeout);
            }
            EndUserMutationAdmissionResult::Unavailable => {
                return Err(SubscriptionEnrollmentServiceError::AdmissionUnavailable);
            }
        }

        let account = self
            .gateway_account(
                command.billing_scope_id(),
                command.gateway_configuration_id(),
            )
            .await?;
        if let Some(scope) = self.active_cooldown(&account).await? {
            return Err(SubscriptionEnrollmentServiceError::GatewayMutationCooldown { scope });
        }
        let gateway = self
            .resolver
            .resolve(
                command.billing_scope_id(),
                account.account_id,
                command.gateway_configuration_id(),
                account.provider_key.clone(),
            )
            .await?;
        if gateway.billing_scope_id() != command.billing_scope_id()
            || gateway.gateway_account_id() != account.account_id
            || gateway.gateway_configuration_id() != command.gateway_configuration_id()
            || gateway.provider_key() != &account.provider_key
        {
            return Err(SubscriptionEnrollmentServiceError::ResolvedGatewayIdentityMismatch);
        }
        let mut reservation = SubscriptionEnrollmentReservation::from_command(&command, &gateway)
            .map_err(map_reservation_build_error)?;

        let attempt = match self.reserve(&reservation).await? {
            SubscriptionEnrollmentReservationOutcome::Reserved(attempt)
            | SubscriptionEnrollmentReservationOutcome::Replay(attempt)
                if attempt.status() == PaymentAttemptStatus::Pending
                    && attempt.state().timestamps().submitted_at().is_none() =>
            {
                attempt
            }
            SubscriptionEnrollmentReservationOutcome::Replay(attempt) => {
                return self.payment_result(attempt).await;
            }
            SubscriptionEnrollmentReservationOutcome::IdempotencyConflict => {
                return Err(SubscriptionEnrollmentServiceError::IdempotencyConflict);
            }
            SubscriptionEnrollmentReservationOutcome::Rejected(reason) => {
                return Err(SubscriptionEnrollmentServiceError::ReservationRejected(
                    reason,
                ));
            }
            SubscriptionEnrollmentReservationOutcome::Reserved(_) => {
                return Err(SubscriptionEnrollmentServiceError::InvalidState(
                    INVALID_SERVICE_STATE,
                ));
            }
        };
        if reservation.identity().attempt_id() != attempt.identity().attempt_id() {
            reservation = SubscriptionEnrollmentReservation::from_command_for_attempt(
                &command,
                &gateway,
                attempt.identity().attempt_id(),
            )
            .map_err(map_reservation_build_error)?;
        }

        if let Some(scope) = self.active_cooldown(&account).await? {
            return self
                .resolve_cooldown(&reservation, scope, OutcomeResolutionBoundary::Prepared)
                .await;
        }
        match gateway.account_mode().await {
            Ok(GatewayAccountMode::Live) => {}
            Ok(GatewayAccountMode::Test) => {
                return self
                    .resolve_readiness_failure(
                        &reservation,
                        GatewayDiagnostic::new(LIVE_READINESS_FAILED_TEXT),
                        PaymentResolutionCode::GatewayLiveReadinessFailedBeforeSubmission,
                        None,
                        OutcomeResolutionBoundary::Prepared,
                    )
                    .await;
            }
            Err(GatewayError::RateLimited(detail)) => {
                return self
                    .resolve_provider_readiness_rate_limit(&reservation, detail)
                    .await;
            }
            Err(_) => {
                return self
                    .resolve_readiness_failure(
                        &reservation,
                        GatewayDiagnostic::new(LIVE_READINESS_FAILED_TEXT),
                        PaymentResolutionCode::GatewayLiveReadinessFailedBeforeSubmission,
                        None,
                        OutcomeResolutionBoundary::Prepared,
                    )
                    .await;
            }
        }

        let admission = match admit_subscription_enrollment_submission(
            &self.pool,
            self.offers.as_ref(),
            &reservation,
        )
        .await?
        {
            SubscriptionEnrollmentAdmissionOutcome::Admitted(admission) => *admission,
            SubscriptionEnrollmentAdmissionOutcome::AlreadyAdmitted(attempt) => {
                return self.payment_result(attempt).await;
            }
            SubscriptionEnrollmentAdmissionOutcome::Rejected { reason, .. } => {
                return Err(SubscriptionEnrollmentServiceError::SubmissionRejected(
                    reason,
                ));
            }
        };

        if let Some(scope) = self.active_cooldown(&account).await? {
            return self
                .resolve_cooldown(
                    &reservation,
                    scope,
                    OutcomeResolutionBoundary::AdmittedNotSubmitted,
                )
                .await;
        }
        match submit_admitted_subscription_enrollment(
            &self.pool,
            self.coordinator.as_ref(),
            admission,
            &command,
            &gateway,
        )
        .await?
        {
            SubscriptionEnrollmentProviderResult::Payment(payment) => Ok(payment),
            SubscriptionEnrollmentProviderResult::NotSubmitted { payment, error } => {
                preserve_concurrent_terminal_payment(payment, error)
            }
        }
    }

    /// Runs one complete automatic recurring-renewal boundary.
    ///
    /// Stale, future, canceled, paced, and contended work is a successful
    /// no-op. The operation never invokes end-user admission or the live offer
    /// store and never holds a database lock across provider I/O.
    pub async fn renew(
        &self,
        command: ChargeRenewal,
    ) -> Result<SubscriptionRenewalOutcome, SubscriptionEnrollmentServiceError> {
        let Some(account) = self.renewal_gateway_account(command).await? else {
            return Ok(SubscriptionRenewalOutcome::Noop);
        };
        if self
            .active_cooldown(&account.as_gateway_snapshot())
            .await?
            .is_some()
        {
            return Ok(SubscriptionRenewalOutcome::Noop);
        }
        let gateway = self
            .resolver
            .resolve(
                command.billing_scope_id(),
                account.account_id,
                account.configuration_id,
                account.provider_key.clone(),
            )
            .await?;
        if gateway.billing_scope_id() != command.billing_scope_id()
            || gateway.gateway_account_id() != account.account_id
            || gateway.gateway_configuration_id() != account.configuration_id
            || gateway.provider_key() != &account.provider_key
        {
            return Err(SubscriptionEnrollmentServiceError::ResolvedGatewayIdentityMismatch);
        }
        match gateway.account_mode().await {
            Ok(GatewayAccountMode::Live) => {}
            Ok(GatewayAccountMode::Test) => {
                return Err(SubscriptionEnrollmentServiceError::GatewayReadiness(
                    GatewayError::Configuration(GatewayDiagnostic::new(LIVE_READINESS_FAILED_TEXT)),
                ));
            }
            Err(GatewayError::RateLimited(_)) => {
                self.extend_provider_cooldown(&account.provider_key).await?;
                return Ok(SubscriptionRenewalOutcome::Noop);
            }
            Err(error) => return Err(SubscriptionEnrollmentServiceError::GatewayReadiness(error)),
        }

        let (reservation, attempt) = match self.reserve_renewal(command, &gateway).await? {
            SubscriptionRenewalReservationOutcome::Reserved(reservation, attempt) => {
                (*reservation, *attempt)
            }
            SubscriptionRenewalReservationOutcome::Rejected(
                SubscriptionRenewalReservationRejection::PaymentMethodUpdateInProgress,
            ) => {
                return Err(
                    SubscriptionEnrollmentServiceError::RenewalReservationRejected(
                        SubscriptionRenewalReservationRejection::PaymentMethodUpdateInProgress,
                    ),
                );
            }
            SubscriptionRenewalReservationOutcome::Rejected(
                SubscriptionRenewalReservationRejection::GatewayConfigurationChanged,
            ) => {
                return Err(SubscriptionEnrollmentServiceError::GatewayConfigurationChanged);
            }
            SubscriptionRenewalReservationOutcome::Rejected(_) => {
                return Ok(SubscriptionRenewalOutcome::Noop);
            }
        };
        if attempt.status() != PaymentAttemptStatus::Pending
            || attempt.state().timestamps().submitted_at().is_some()
            || attempt.identity() != reservation.identity()
        {
            return Err(SubscriptionEnrollmentServiceError::InvalidState(
                INVALID_SERVICE_STATE,
            ));
        }
        if let Some(scope) = self.active_cooldown(&account.as_gateway_snapshot()).await? {
            self.resolve_renewal_cooldown(&reservation, scope, OutcomeResolutionBoundary::Prepared)
                .await?;
            return Ok(SubscriptionRenewalOutcome::Noop);
        }
        if !self
            .renewal_readiness_open(&reservation, &gateway, OutcomeResolutionBoundary::Prepared)
            .await?
        {
            return Ok(SubscriptionRenewalOutcome::Noop);
        }

        let admission = match admit_subscription_renewal_submission(&self.pool, &reservation).await
        {
            Err(error) if is_retryable_renewal_admission_error(&error) => {
                self.resolve_renewal_readiness_failure(
                    &reservation,
                    GatewayDiagnostic::new(
                        "subscription billing state could not be locked for final admission",
                    ),
                    PaymentResolutionCode::SubscriptionRenewalRetryStateChangedBeforeCharge,
                    None,
                    OutcomeResolutionBoundary::Prepared,
                )
                .await?;
                return Ok(SubscriptionRenewalOutcome::Noop);
            }
            Err(error) => return Err(error.into()),
            Ok(outcome) => match outcome {
                SubscriptionRenewalAdmissionOutcome::Admitted(admission) => *admission,
                SubscriptionRenewalAdmissionOutcome::AlreadyAdmitted(attempt) => {
                    return self
                        .payment_result(attempt)
                        .await
                        .map(Box::new)
                        .map(SubscriptionRenewalOutcome::Payment);
                }
                SubscriptionRenewalAdmissionOutcome::Rejected { attempt, .. } => {
                    return self
                        .payment_result(attempt)
                        .await
                        .map(Box::new)
                        .map(SubscriptionRenewalOutcome::Payment);
                }
            },
        };
        if let Some(scope) = self.active_cooldown(&account.as_gateway_snapshot()).await? {
            self.resolve_renewal_cooldown(
                &reservation,
                scope,
                OutcomeResolutionBoundary::AdmittedNotSubmitted,
            )
            .await?;
            return Ok(SubscriptionRenewalOutcome::Noop);
        }
        match submit_admitted_subscription_renewal(
            &self.pool,
            self.coordinator.as_ref(),
            admission,
            &gateway,
        )
        .await?
        {
            SubscriptionRenewalProviderResult::Payment(payment) => {
                Ok(SubscriptionRenewalOutcome::Payment(Box::new(payment)))
            }
            SubscriptionRenewalProviderResult::NotSubmitted { payment, error } => {
                Ok(SubscriptionRenewalOutcome::NotSubmitted {
                    payment: Box::new(payment),
                    error,
                })
            }
        }
    }

    /// Runs one complete subscriber-initiated recovery payment boundary.
    ///
    /// The command carries only the owner, requested plan/configuration, and
    /// memory-only token/contact. Reservation derives the exact due period,
    /// amount, subscription, and payment-state snapshot under lock.
    pub async fn recover(
        &self,
        command: RecoverSubscriptionPayment,
    ) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentServiceError> {
        match self.preflight_recovery(&command).await? {
            SubscriptionRecoveryPreflightOutcome::Continue => {}
            SubscriptionRecoveryPreflightOutcome::Replay(attempt) => {
                return self.payment_result(*attempt).await;
            }
            SubscriptionRecoveryPreflightOutcome::IdempotencyConflict => {
                return Err(SubscriptionEnrollmentServiceError::IdempotencyConflict);
            }
        }

        match self
            .admission
            .admit(EndUserMutationCommand::new(
                command.billing_scope_id(),
                command.subscriber_id(),
                EndUserMutationOperation::SubscriptionRecovery,
            ))
            .await
        {
            EndUserMutationAdmissionResult::Allowed => {}
            EndUserMutationAdmissionResult::Denied { retry_after } => {
                return Err(SubscriptionEnrollmentServiceError::AdmissionDenied {
                    retry_after: retry_after.get(),
                });
            }
            EndUserMutationAdmissionResult::Timeout => {
                return Err(SubscriptionEnrollmentServiceError::AdmissionTimeout);
            }
            EndUserMutationAdmissionResult::Unavailable => {
                return Err(SubscriptionEnrollmentServiceError::AdmissionUnavailable);
            }
        }

        let account = self
            .gateway_account(
                command.billing_scope_id(),
                command.gateway_configuration_id(),
            )
            .await?;
        if let Some(scope) = self.active_cooldown(&account).await? {
            return Err(SubscriptionEnrollmentServiceError::GatewayMutationCooldown { scope });
        }
        let gateway = self
            .resolver
            .resolve(
                command.billing_scope_id(),
                account.account_id,
                command.gateway_configuration_id(),
                account.provider_key.clone(),
            )
            .await?;
        if gateway.billing_scope_id() != command.billing_scope_id()
            || gateway.gateway_account_id() != account.account_id
            || gateway.gateway_configuration_id() != command.gateway_configuration_id()
            || gateway.provider_key() != &account.provider_key
        {
            return Err(SubscriptionEnrollmentServiceError::ResolvedGatewayIdentityMismatch);
        }

        let (reservation, attempt) = match self.reserve_recovery(&command, &gateway).await? {
            SubscriptionRecoveryReservationOutcome::Reserved(reservation, attempt) => {
                (*reservation, *attempt)
            }
            SubscriptionRecoveryReservationOutcome::Replay(attempt) => {
                return self.payment_result(*attempt).await;
            }
            SubscriptionRecoveryReservationOutcome::IdempotencyConflict => {
                return Err(SubscriptionEnrollmentServiceError::IdempotencyConflict);
            }
            SubscriptionRecoveryReservationOutcome::Rejected(reason) => {
                return Err(
                    SubscriptionEnrollmentServiceError::RecoveryReservationRejected(reason),
                );
            }
        };
        if attempt.status() != PaymentAttemptStatus::Pending
            || attempt.state().timestamps().submitted_at().is_some()
            || attempt.identity() != reservation.identity()
        {
            return Err(SubscriptionEnrollmentServiceError::InvalidState(
                INVALID_SERVICE_STATE,
            ));
        }

        if let Some(scope) = self.active_cooldown(&account).await? {
            return self
                .resolve_recovery_cooldown(&reservation, scope, OutcomeResolutionBoundary::Prepared)
                .await;
        }
        match gateway.account_mode().await {
            Ok(GatewayAccountMode::Live) => {}
            Ok(GatewayAccountMode::Test) => {
                return self
                    .resolve_recovery_readiness_failure(
                        &reservation,
                        GatewayDiagnostic::new(LIVE_READINESS_FAILED_TEXT),
                        PaymentResolutionCode::GatewayLiveReadinessFailedBeforeSubmission,
                        None,
                        OutcomeResolutionBoundary::Prepared,
                    )
                    .await;
            }
            Err(GatewayError::RateLimited(detail)) => {
                return self
                    .resolve_recovery_provider_readiness_rate_limit(
                        &reservation,
                        detail,
                        OutcomeResolutionBoundary::Prepared,
                    )
                    .await;
            }
            Err(_) => {
                return self
                    .resolve_recovery_readiness_failure(
                        &reservation,
                        GatewayDiagnostic::new(LIVE_READINESS_FAILED_TEXT),
                        PaymentResolutionCode::GatewayLiveReadinessFailedBeforeSubmission,
                        None,
                        OutcomeResolutionBoundary::Prepared,
                    )
                    .await;
            }
        }

        let admission =
            match admit_subscription_recovery_submission(&self.pool, &reservation).await? {
                SubscriptionRecoveryAdmissionOutcome::Admitted(admission) => *admission,
                SubscriptionRecoveryAdmissionOutcome::AlreadyAdmitted(attempt) => {
                    return self.payment_result(attempt).await;
                }
                SubscriptionRecoveryAdmissionOutcome::Rejected { attempt, .. } => {
                    return self.payment_result(attempt).await;
                }
            };
        if let Some(scope) = self.active_cooldown(&account).await? {
            return self
                .resolve_recovery_cooldown(
                    &reservation,
                    scope,
                    OutcomeResolutionBoundary::AdmittedNotSubmitted,
                )
                .await;
        }
        match gateway.account_mode().await {
            Ok(GatewayAccountMode::Live) => {}
            Ok(GatewayAccountMode::Test) => {
                return self
                    .resolve_recovery_readiness_failure(
                        &reservation,
                        GatewayDiagnostic::new(LIVE_READINESS_FAILED_TEXT),
                        PaymentResolutionCode::GatewayLiveReadinessFailedBeforeSubmission,
                        None,
                        OutcomeResolutionBoundary::AdmittedNotSubmitted,
                    )
                    .await;
            }
            Err(GatewayError::RateLimited(detail)) => {
                return self
                    .resolve_recovery_provider_readiness_rate_limit(
                        &reservation,
                        detail,
                        OutcomeResolutionBoundary::AdmittedNotSubmitted,
                    )
                    .await;
            }
            Err(_) => {
                return self
                    .resolve_recovery_readiness_failure(
                        &reservation,
                        GatewayDiagnostic::new(LIVE_READINESS_FAILED_TEXT),
                        PaymentResolutionCode::GatewayLiveReadinessFailedBeforeSubmission,
                        None,
                        OutcomeResolutionBoundary::AdmittedNotSubmitted,
                    )
                    .await;
            }
        }
        if let Some(scope) = self.active_cooldown(&account).await? {
            return self
                .resolve_recovery_cooldown(
                    &reservation,
                    scope,
                    OutcomeResolutionBoundary::AdmittedNotSubmitted,
                )
                .await;
        }
        match submit_admitted_subscription_recovery(
            &self.pool,
            self.coordinator.as_ref(),
            admission,
            &command,
            &gateway,
        )
        .await?
        {
            SubscriptionRecoveryProviderResult::Payment(payment) => Ok(payment),
            SubscriptionRecoveryProviderResult::NotSubmitted { payment, error } => {
                preserve_concurrent_terminal_payment(payment, error)
            }
        }
    }

    /// Runs one complete stored payment-method replacement boundary.
    pub async fn replace_payment_method(
        &self,
        command: ReplaceSubscriptionPaymentMethod,
    ) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentServiceError> {
        match self.preflight_payment_method_replacement(&command).await? {
            SubscriptionPaymentMethodReplacementPreflightOutcome::Continue => {}
            SubscriptionPaymentMethodReplacementPreflightOutcome::Replay(attempt) => {
                return self.payment_result(*attempt).await;
            }
            SubscriptionPaymentMethodReplacementPreflightOutcome::IdempotencyConflict => {
                return Err(SubscriptionEnrollmentServiceError::IdempotencyConflict);
            }
        }
        match self
            .admission
            .admit(EndUserMutationCommand::new(
                command.billing_scope_id(),
                command.subscriber_id(),
                EndUserMutationOperation::SubscriptionPaymentMethodUpdate,
            ))
            .await
        {
            EndUserMutationAdmissionResult::Allowed => {}
            EndUserMutationAdmissionResult::Denied { retry_after } => {
                return Err(SubscriptionEnrollmentServiceError::AdmissionDenied {
                    retry_after: retry_after.get(),
                });
            }
            EndUserMutationAdmissionResult::Timeout => {
                return Err(SubscriptionEnrollmentServiceError::AdmissionTimeout);
            }
            EndUserMutationAdmissionResult::Unavailable => {
                return Err(SubscriptionEnrollmentServiceError::AdmissionUnavailable);
            }
        }
        let account = self
            .gateway_account(
                command.billing_scope_id(),
                command.gateway_configuration_id(),
            )
            .await?;
        if let Some(scope) = self.active_cooldown(&account).await? {
            return Err(SubscriptionEnrollmentServiceError::GatewayMutationCooldown { scope });
        }
        let gateway = self
            .resolver
            .resolve(
                command.billing_scope_id(),
                account.account_id,
                command.gateway_configuration_id(),
                account.provider_key.clone(),
            )
            .await?;
        if gateway.billing_scope_id() != command.billing_scope_id()
            || gateway.gateway_account_id() != account.account_id
            || gateway.gateway_configuration_id() != command.gateway_configuration_id()
            || gateway.provider_key() != &account.provider_key
        {
            return Err(SubscriptionEnrollmentServiceError::ResolvedGatewayIdentityMismatch);
        }
        let (reservation, attempt) = match self
            .reserve_payment_method_replacement(&command, &gateway)
            .await?
        {
            SubscriptionPaymentMethodReplacementReservationOutcome::Reserved(
                reservation,
                attempt,
            ) => (*reservation, *attempt),
            SubscriptionPaymentMethodReplacementReservationOutcome::Replay(attempt) => {
                return self.payment_result(*attempt).await;
            }
            SubscriptionPaymentMethodReplacementReservationOutcome::IdempotencyConflict => {
                return Err(SubscriptionEnrollmentServiceError::IdempotencyConflict);
            }
            SubscriptionPaymentMethodReplacementReservationOutcome::Rejected(reason) => {
                return Err(
                    SubscriptionEnrollmentServiceError::PaymentMethodReplacementReservationRejected(
                        reason,
                    ),
                );
            }
        };
        if attempt.status() != PaymentAttemptStatus::Pending
            || attempt.state().timestamps().submitted_at().is_some()
            || attempt.identity() != reservation.identity()
        {
            return Err(SubscriptionEnrollmentServiceError::InvalidState(
                INVALID_SERVICE_STATE,
            ));
        }
        if let Some(scope) = self.active_cooldown(&account).await? {
            return self
                .resolve_payment_method_replacement_cooldown(
                    &reservation,
                    scope,
                    OutcomeResolutionBoundary::Prepared,
                )
                .await;
        }
        match gateway.account_mode().await {
            Ok(GatewayAccountMode::Live) => {}
            Ok(GatewayAccountMode::Test) => {
                return self
                    .resolve_payment_method_replacement_readiness_failure(
                        &reservation,
                        GatewayDiagnostic::new(LIVE_READINESS_FAILED_TEXT),
                        PaymentResolutionCode::GatewayLiveReadinessFailedBeforeSubmission,
                        None,
                        OutcomeResolutionBoundary::Prepared,
                    )
                    .await;
            }
            Err(GatewayError::RateLimited(detail)) => {
                return self
                    .resolve_payment_method_replacement_provider_readiness_rate_limit(
                        &reservation,
                        detail,
                        OutcomeResolutionBoundary::Prepared,
                    )
                    .await;
            }
            Err(_) => {
                return self
                    .resolve_payment_method_replacement_readiness_failure(
                        &reservation,
                        GatewayDiagnostic::new(LIVE_READINESS_FAILED_TEXT),
                        PaymentResolutionCode::GatewayLiveReadinessFailedBeforeSubmission,
                        None,
                        OutcomeResolutionBoundary::Prepared,
                    )
                    .await;
            }
        }
        let admission =
            match admit_subscription_payment_method_replacement(&self.pool, &reservation).await? {
                SubscriptionPaymentMethodReplacementAdmissionOutcome::Admitted(admission) => {
                    *admission
                }
                SubscriptionPaymentMethodReplacementAdmissionOutcome::AlreadyAdmitted(attempt) => {
                    return self.payment_result(attempt).await;
                }
                SubscriptionPaymentMethodReplacementAdmissionOutcome::Rejected {
                    attempt, ..
                } => {
                    return self.payment_result(attempt).await;
                }
            };
        if let Some(scope) = self.active_cooldown(&account).await? {
            return self
                .resolve_payment_method_replacement_cooldown(
                    &reservation,
                    scope,
                    OutcomeResolutionBoundary::AdmittedNotSubmitted,
                )
                .await;
        }
        match gateway.account_mode().await {
            Ok(GatewayAccountMode::Live) => {}
            Ok(GatewayAccountMode::Test) => {
                return self
                    .resolve_payment_method_replacement_readiness_failure(
                        &reservation,
                        GatewayDiagnostic::new(LIVE_READINESS_FAILED_TEXT),
                        PaymentResolutionCode::GatewayLiveReadinessFailedBeforeSubmission,
                        None,
                        OutcomeResolutionBoundary::AdmittedNotSubmitted,
                    )
                    .await;
            }
            Err(GatewayError::RateLimited(detail)) => {
                return self
                    .resolve_payment_method_replacement_provider_readiness_rate_limit(
                        &reservation,
                        detail,
                        OutcomeResolutionBoundary::AdmittedNotSubmitted,
                    )
                    .await;
            }
            Err(_) => {
                return self
                    .resolve_payment_method_replacement_readiness_failure(
                        &reservation,
                        GatewayDiagnostic::new(LIVE_READINESS_FAILED_TEXT),
                        PaymentResolutionCode::GatewayLiveReadinessFailedBeforeSubmission,
                        None,
                        OutcomeResolutionBoundary::AdmittedNotSubmitted,
                    )
                    .await;
            }
        }
        if let Some(scope) = self.active_cooldown(&account).await? {
            return self
                .resolve_payment_method_replacement_cooldown(
                    &reservation,
                    scope,
                    OutcomeResolutionBoundary::AdmittedNotSubmitted,
                )
                .await;
        }
        match submit_admitted_subscription_payment_method_replacement(
            &self.pool,
            self.coordinator.as_ref(),
            admission,
            &command,
            &gateway,
        )
        .await?
        {
            SubscriptionPaymentMethodReplacementProviderResult::Payment(payment) => Ok(payment),
            SubscriptionPaymentMethodReplacementProviderResult::NotSubmitted { payment, error } => {
                preserve_concurrent_terminal_payment(payment, error)
            }
        }
    }

    /// Applies an already-observed provider outcome without another submission.
    ///
    /// Reconciliation enters the same application authority as foreground
    /// enrollment but reconstructs its secret-free reservation from the exact
    /// durable attempt and canonical gateway account.
    pub async fn apply_reconciled_outcome(
        &self,
        billing_scope_id: BillingScopeId,
        attempt_id: PaymentAttemptId,
        outcome: &GatewayPaymentOutcome,
    ) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentServiceError> {
        let mut transaction = self.pool.begin().await?;
        let attempt = crate::find_payment_attempt_by_id_in_transaction(
            &mut transaction,
            billing_scope_id,
            attempt_id,
        )
        .await?
        .ok_or(SubscriptionEnrollmentServiceError::InvalidState(
            "reconciled subscription payment attempt was not found",
        ))?;
        transaction.commit().await?;
        match attempt.kind() {
            PaymentAttemptKind::SubscriptionInitial => {
                apply_reconciled_subscription_enrollment_gateway_outcome(
                    &self.pool,
                    self.coordinator.as_ref(),
                    billing_scope_id,
                    attempt_id,
                    outcome,
                )
                .await
            }
            PaymentAttemptKind::SubscriptionRecovery => {
                apply_reconciled_subscription_recovery_gateway_outcome(
                    &self.pool,
                    self.coordinator.as_ref(),
                    billing_scope_id,
                    attempt_id,
                    outcome,
                )
                .await
            }
            PaymentAttemptKind::SubscriptionRenewal => {
                apply_reconciled_subscription_renewal_gateway_outcome(
                    &self.pool,
                    self.coordinator.as_ref(),
                    billing_scope_id,
                    attempt_id,
                    outcome,
                )
                .await
            }
            PaymentAttemptKind::SubscriptionPaymentMethodUpdate => {
                apply_reconciled_subscription_payment_method_replacement_gateway_outcome(
                    &self.pool,
                    self.coordinator.as_ref(),
                    billing_scope_id,
                    attempt_id,
                    outcome,
                )
                .await
            }
            _ => Err(SubscriptionEnrollmentApplicationError::InvalidState(
                "attempt kind is not owned by the subscription billing service",
            )),
        }
        .map_err(Into::into)
    }

    async fn preflight(
        &self,
        command: &EnrollSubscription,
    ) -> Result<SubscriptionEnrollmentPreflightOutcome, SubscriptionEnrollmentServiceError> {
        let mut transaction = self.pool.begin().await?;
        let outcome =
            preflight_subscription_enrollment_in_transaction(&mut transaction, command).await?;
        transaction.commit().await?;
        Ok(outcome)
    }

    async fn preflight_recovery(
        &self,
        command: &RecoverSubscriptionPayment,
    ) -> Result<SubscriptionRecoveryPreflightOutcome, SubscriptionEnrollmentServiceError> {
        let mut transaction = self.pool.begin().await?;
        let outcome =
            preflight_subscription_recovery_in_transaction(&mut transaction, command).await?;
        transaction.commit().await?;
        Ok(outcome)
    }

    async fn preflight_payment_method_replacement(
        &self,
        command: &ReplaceSubscriptionPaymentMethod,
    ) -> Result<
        SubscriptionPaymentMethodReplacementPreflightOutcome,
        SubscriptionEnrollmentServiceError,
    > {
        let mut transaction = self.pool.begin().await?;
        let outcome = preflight_subscription_payment_method_replacement_in_transaction(
            &mut transaction,
            command,
        )
        .await?;
        transaction.commit().await?;
        Ok(outcome)
    }

    async fn reserve(
        &self,
        reservation: &SubscriptionEnrollmentReservation,
    ) -> Result<SubscriptionEnrollmentReservationOutcome, SubscriptionEnrollmentServiceError> {
        let mut transaction = self.pool.begin().await?;
        let outcome = reserve_subscription_enrollment_in_transaction(
            &mut transaction,
            self.offers.as_ref(),
            reservation,
        )
        .await?;
        transaction.commit().await?;
        Ok(outcome)
    }

    async fn reserve_recovery(
        &self,
        command: &RecoverSubscriptionPayment,
        gateway: &syrup_rail::ResolvedGateway,
    ) -> Result<SubscriptionRecoveryReservationOutcome, SubscriptionEnrollmentServiceError> {
        let mut transaction = self.pool.begin().await?;
        let outcome =
            reserve_subscription_recovery_in_transaction(&mut transaction, command, gateway)
                .await?;
        transaction.commit().await?;
        Ok(outcome)
    }

    async fn reserve_renewal(
        &self,
        command: ChargeRenewal,
        gateway: &syrup_rail::ResolvedGateway,
    ) -> Result<SubscriptionRenewalReservationOutcome, SubscriptionEnrollmentServiceError> {
        let mut transaction = self.pool.begin().await?;
        let outcome =
            reserve_subscription_renewal_in_transaction(&mut transaction, command, gateway).await?;
        transaction.commit().await?;
        Ok(outcome)
    }

    async fn reserve_payment_method_replacement(
        &self,
        command: &ReplaceSubscriptionPaymentMethod,
        gateway: &syrup_rail::ResolvedGateway,
    ) -> Result<
        SubscriptionPaymentMethodReplacementReservationOutcome,
        SubscriptionEnrollmentServiceError,
    > {
        let mut transaction = self.pool.begin().await?;
        let outcome = reserve_subscription_payment_method_replacement_in_transaction(
            &mut transaction,
            command,
            gateway,
        )
        .await?;
        transaction.commit().await?;
        Ok(outcome)
    }

    async fn payment_result(
        &self,
        attempt: PaymentAttempt,
    ) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentServiceError> {
        let mut transaction = self.pool.begin().await?;
        let result = payment_result_for_attempt(&mut transaction, attempt).await?;
        transaction.commit().await?;
        Ok(result)
    }

    async fn gateway_account(
        &self,
        billing_scope_id: BillingScopeId,
        gateway_configuration_id: syrup_rail::GatewayConfigurationId,
    ) -> Result<GatewayAccountSnapshot, SubscriptionEnrollmentServiceError> {
        let row = sqlx::query_as::<_, (uuid::Uuid, String)>(
            r#"
            SELECT id, provider_key
            FROM billing_gateway_accounts
            WHERE billing_scope_id = $1 AND gateway_configuration_id = $2
            "#,
        )
        .bind(billing_scope_id.as_uuid())
        .bind(gateway_configuration_id.as_uuid())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(SubscriptionEnrollmentServiceError::GatewayConfigurationChanged)?;
        let provider_key = GatewayProviderKey::new(&row.1)
            .map_err(|_| SubscriptionEnrollmentServiceError::InvalidState(INVALID_SERVICE_STATE))?;
        Ok(GatewayAccountSnapshot {
            account_id: GatewayAccountId::new(row.0),
            provider_key,
        })
    }

    async fn active_cooldown(
        &self,
        account: &GatewayAccountSnapshot,
    ) -> Result<Option<GatewayMutationCooldownScope>, SubscriptionEnrollmentServiceError> {
        let row = sqlx::query_as::<_, (bool, bool)>(
            r#"
            SELECT
                COALESCE(accounts.mutation_rate_limited_until > clock_timestamp(), false),
                provider.rate_limited_until > clock_timestamp()
            FROM billing_gateway_accounts AS accounts
            INNER JOIN billing_gateway_provider_rate_limits AS provider
                ON provider.provider_key = accounts.provider_key
            WHERE accounts.id = $1 AND accounts.provider_key = $2
            "#,
        )
        .bind(account.account_id.as_uuid())
        .bind(account.provider_key.as_str())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(SubscriptionEnrollmentServiceError::GatewayConfigurationChanged)?;
        Ok(if row.1 {
            Some(GatewayMutationCooldownScope::Provider)
        } else if row.0 {
            Some(GatewayMutationCooldownScope::Account)
        } else {
            None
        })
    }

    async fn resolve_cooldown(
        &self,
        reservation: &SubscriptionEnrollmentReservation,
        scope: GatewayMutationCooldownScope,
        boundary: OutcomeResolutionBoundary,
    ) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentServiceError> {
        let (message, code) = match scope {
            GatewayMutationCooldownScope::Account => (
                "gateway account mutation cooldown is active",
                PaymentResolutionCode::GatewayAccountMutationCooldownBeforeSubmission,
            ),
            GatewayMutationCooldownScope::Provider => (
                "gateway provider cooldown is active",
                PaymentResolutionCode::GatewayProviderRateLimitedBeforeSubmission,
            ),
        };
        let payment = self
            .resolve_readiness_failure(
                reservation,
                GatewayDiagnostic::new(message),
                code,
                None,
                boundary,
            )
            .await?;
        if payment.attempt().state().resolution_code() == Some(code) {
            Err(SubscriptionEnrollmentServiceError::GatewayMutationCooldown { scope })
        } else {
            Ok(payment)
        }
    }

    async fn resolve_provider_readiness_rate_limit(
        &self,
        reservation: &SubscriptionEnrollmentReservation,
        detail: GatewayDiagnostic,
    ) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentServiceError> {
        let code = PaymentResolutionCode::GatewayProviderRateLimitedBeforeSubmission;
        let payment = self
            .resolve_readiness_failure(
                reservation,
                detail,
                code,
                Some(RateLimitCooldown::Provider),
                OutcomeResolutionBoundary::Prepared,
            )
            .await?;
        if payment.attempt().state().resolution_code() == Some(code) {
            Err(
                SubscriptionEnrollmentServiceError::GatewayMutationCooldown {
                    scope: GatewayMutationCooldownScope::Provider,
                },
            )
        } else {
            Ok(payment)
        }
    }

    async fn resolve_readiness_failure(
        &self,
        reservation: &SubscriptionEnrollmentReservation,
        detail: GatewayDiagnostic,
        code: PaymentResolutionCode,
        cooldown: Option<RateLimitCooldown>,
        boundary: OutcomeResolutionBoundary,
    ) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentServiceError> {
        let evidence = ProcessorEvidence::new(
            None,
            None,
            None,
            None,
            Some(detail),
            Some(GatewayDiagnostic::new("failed")),
            GatewayPaymentDescriptor::default(),
        );
        resolve_non_approved_outcome(
            &self.pool,
            reservation,
            &evidence,
            PaymentAttemptStatus::Failed,
            Some(code),
            cooldown,
            boundary,
        )
        .await
        .map_err(SubscriptionEnrollmentServiceError::from)
    }

    async fn resolve_recovery_cooldown(
        &self,
        reservation: &SubscriptionRecoveryReservation,
        scope: GatewayMutationCooldownScope,
        boundary: OutcomeResolutionBoundary,
    ) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentServiceError> {
        let (message, code) = match scope {
            GatewayMutationCooldownScope::Account => (
                "gateway account mutation cooldown is active",
                PaymentResolutionCode::GatewayAccountMutationCooldownBeforeSubmission,
            ),
            GatewayMutationCooldownScope::Provider => (
                "gateway provider cooldown is active",
                PaymentResolutionCode::GatewayProviderRateLimitedBeforeSubmission,
            ),
        };
        let payment = self
            .resolve_recovery_readiness_failure(
                reservation,
                GatewayDiagnostic::new(message),
                code,
                None,
                boundary,
            )
            .await?;
        if payment.attempt().state().resolution_code() == Some(code) {
            Err(SubscriptionEnrollmentServiceError::GatewayMutationCooldown { scope })
        } else {
            Ok(payment)
        }
    }

    async fn resolve_recovery_provider_readiness_rate_limit(
        &self,
        reservation: &SubscriptionRecoveryReservation,
        detail: GatewayDiagnostic,
        boundary: OutcomeResolutionBoundary,
    ) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentServiceError> {
        let code = PaymentResolutionCode::GatewayProviderRateLimitedBeforeSubmission;
        let payment = self
            .resolve_recovery_readiness_failure(
                reservation,
                detail,
                code,
                Some(RateLimitCooldown::Provider),
                boundary,
            )
            .await?;
        if payment.attempt().state().resolution_code() == Some(code) {
            Err(
                SubscriptionEnrollmentServiceError::GatewayMutationCooldown {
                    scope: GatewayMutationCooldownScope::Provider,
                },
            )
        } else {
            Ok(payment)
        }
    }

    async fn resolve_recovery_readiness_failure(
        &self,
        reservation: &SubscriptionRecoveryReservation,
        detail: GatewayDiagnostic,
        code: PaymentResolutionCode,
        cooldown: Option<RateLimitCooldown>,
        boundary: OutcomeResolutionBoundary,
    ) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentServiceError> {
        let evidence = ProcessorEvidence::new(
            None,
            None,
            None,
            None,
            Some(detail),
            Some(GatewayDiagnostic::new("failed")),
            GatewayPaymentDescriptor::default(),
        );
        resolve_recovery_non_approved_outcome(
            &self.pool,
            reservation,
            &evidence,
            PaymentAttemptStatus::Failed,
            Some(code),
            cooldown,
            boundary,
        )
        .await
        .map_err(SubscriptionEnrollmentServiceError::from)
    }

    async fn resolve_payment_method_replacement_cooldown(
        &self,
        reservation: &SubscriptionPaymentMethodReplacement,
        scope: GatewayMutationCooldownScope,
        boundary: OutcomeResolutionBoundary,
    ) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentServiceError> {
        let (message, code) = match scope {
            GatewayMutationCooldownScope::Account => (
                "gateway account mutation cooldown is active",
                PaymentResolutionCode::GatewayAccountMutationCooldownBeforeSubmission,
            ),
            GatewayMutationCooldownScope::Provider => (
                "gateway provider cooldown is active",
                PaymentResolutionCode::GatewayProviderRateLimitedBeforeSubmission,
            ),
        };
        let payment = self
            .resolve_payment_method_replacement_readiness_failure(
                reservation,
                GatewayDiagnostic::new(message),
                code,
                None,
                boundary,
            )
            .await?;
        if payment.attempt().state().resolution_code() == Some(code) {
            Err(SubscriptionEnrollmentServiceError::GatewayMutationCooldown { scope })
        } else {
            Ok(payment)
        }
    }

    async fn resolve_payment_method_replacement_provider_readiness_rate_limit(
        &self,
        reservation: &SubscriptionPaymentMethodReplacement,
        detail: GatewayDiagnostic,
        boundary: OutcomeResolutionBoundary,
    ) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentServiceError> {
        let code = PaymentResolutionCode::GatewayProviderRateLimitedBeforeSubmission;
        let payment = self
            .resolve_payment_method_replacement_readiness_failure(
                reservation,
                detail,
                code,
                Some(RateLimitCooldown::Provider),
                boundary,
            )
            .await?;
        if payment.attempt().state().resolution_code() == Some(code) {
            Err(
                SubscriptionEnrollmentServiceError::GatewayMutationCooldown {
                    scope: GatewayMutationCooldownScope::Provider,
                },
            )
        } else {
            Ok(payment)
        }
    }

    async fn resolve_payment_method_replacement_readiness_failure(
        &self,
        reservation: &SubscriptionPaymentMethodReplacement,
        detail: GatewayDiagnostic,
        code: PaymentResolutionCode,
        cooldown: Option<RateLimitCooldown>,
        boundary: OutcomeResolutionBoundary,
    ) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentServiceError> {
        let evidence = ProcessorEvidence::new(
            None,
            None,
            None,
            None,
            Some(detail),
            Some(GatewayDiagnostic::new("failed")),
            GatewayPaymentDescriptor::default(),
        );
        resolve_payment_method_replacement_non_approved_outcome(
            &self.pool,
            reservation,
            &evidence,
            PaymentAttemptStatus::Failed,
            Some(code),
            cooldown,
            boundary,
        )
        .await
        .map_err(SubscriptionEnrollmentServiceError::from)
    }

    async fn renewal_gateway_account(
        &self,
        command: ChargeRenewal,
    ) -> Result<Option<RenewalGatewayAccountSnapshot>, SubscriptionEnrollmentServiceError> {
        let mut transaction = self.pool.begin().await?;
        let row = sqlx::query_as::<_, (uuid::Uuid, uuid::Uuid, String)>(
            r#"
            SELECT accounts.id, accounts.gateway_configuration_id, accounts.provider_key
            FROM billing_subscriptions AS subscriptions
            JOIN billing_gateway_accounts AS accounts
                ON accounts.billing_scope_id = subscriptions.billing_scope_id
                AND accounts.id = subscriptions.gateway_account_id
            WHERE subscriptions.billing_scope_id = $1 AND subscriptions.id = $2
                AND subscriptions.status IN ('active', 'past_due')
                AND subscriptions.next_renewal_at = $3
                AND subscriptions.next_renewal_at <= clock_timestamp()
            "#,
        )
        .bind(command.billing_scope_id().as_uuid())
        .bind(command.subscription_id().as_uuid())
        .bind(command.period_start_at())
        .fetch_optional(&mut *transaction)
        .await?;
        let Some((account_id, configuration_id, provider_key)) = row else {
            transaction.commit().await?;
            return Ok(None);
        };
        let attempt_state = crate::renewal_attempt_state(
            &mut transaction,
            command.subscription_id(),
            *command.period_start_at(),
            None,
        )
        .await
        .map_err(|error| match error {
            crate::RenewalStoreError::Sql(error) => SubscriptionEnrollmentServiceError::Sql(error),
            crate::RenewalStoreError::MissingProviderCooldown => {
                SubscriptionEnrollmentServiceError::InvalidState(INVALID_SERVICE_STATE)
            }
        })?;
        let now = sqlx::query_scalar("SELECT clock_timestamp()")
            .fetch_one(&mut *transaction)
            .await?;
        let has_payment_method_update: bool = sqlx::query_scalar(
            r#"
            SELECT EXISTS (
                SELECT 1 FROM billing_payment_attempts
                WHERE subscription_id = $1
                    AND attempt_kind = 'subscription_payment_method_update'
                    AND status IN ('pending', 'unknown', 'review_required')
                    AND NOT (
                        status = 'pending' AND submitted_at IS NULL
                        AND created_at <= clock_timestamp() - interval '3 minutes'
                    )
            )
            "#,
        )
        .bind(command.subscription_id().as_uuid())
        .fetch_one(&mut *transaction)
        .await?;
        transaction.commit().await?;
        if attempt_state.blocks_automatic_retry(now) || has_payment_method_update {
            if has_payment_method_update {
                return Err(
                    SubscriptionEnrollmentServiceError::RenewalReservationRejected(
                        SubscriptionRenewalReservationRejection::PaymentMethodUpdateInProgress,
                    ),
                );
            }
            return Ok(None);
        }
        Some((account_id, configuration_id, provider_key))
            .map(|(account_id, configuration_id, provider_key)| {
                Ok(RenewalGatewayAccountSnapshot {
                    account_id: GatewayAccountId::new(account_id),
                    configuration_id: syrup_rail::GatewayConfigurationId::new(configuration_id),
                    provider_key: GatewayProviderKey::new(provider_key).map_err(|_| {
                        SubscriptionEnrollmentServiceError::InvalidState(INVALID_SERVICE_STATE)
                    })?,
                })
            })
            .transpose()
    }

    async fn extend_provider_cooldown(
        &self,
        provider_key: &GatewayProviderKey,
    ) -> Result<(), SubscriptionEnrollmentServiceError> {
        let result = sqlx::query(
            r#"
            UPDATE billing_gateway_provider_rate_limits
            SET rate_limited_until = GREATEST(
                    rate_limited_until,
                    clock_timestamp() + make_interval(secs => $2)
                ),
                updated_at = clock_timestamp()
            WHERE provider_key = $1
            "#,
        )
        .bind(provider_key.as_str())
        .bind(syrup_rail::RENEWAL_PROVIDER_RATE_LIMIT_RETRY_AFTER_SECONDS)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() != 1 {
            return Err(SubscriptionEnrollmentServiceError::InvalidState(
                INVALID_SERVICE_STATE,
            ));
        }
        Ok(())
    }

    async fn resolve_renewal_cooldown(
        &self,
        reservation: &SubscriptionRenewalReservation,
        scope: GatewayMutationCooldownScope,
        boundary: OutcomeResolutionBoundary,
    ) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentServiceError> {
        let (message, code) = match scope {
            GatewayMutationCooldownScope::Account => (
                "gateway account mutation cooldown is active",
                PaymentResolutionCode::GatewayAccountMutationCooldownBeforeSubmission,
            ),
            GatewayMutationCooldownScope::Provider => (
                "gateway provider cooldown is active",
                PaymentResolutionCode::GatewayProviderRateLimitedBeforeSubmission,
            ),
        };
        self.resolve_renewal_readiness_failure(
            reservation,
            GatewayDiagnostic::new(message),
            code,
            None,
            boundary,
        )
        .await
    }

    async fn renewal_readiness_open(
        &self,
        reservation: &SubscriptionRenewalReservation,
        gateway: &syrup_rail::ResolvedGateway,
        boundary: OutcomeResolutionBoundary,
    ) -> Result<bool, SubscriptionEnrollmentServiceError> {
        match gateway.account_mode().await {
            Ok(GatewayAccountMode::Live) => Ok(true),
            Ok(GatewayAccountMode::Test) => {
                self.resolve_renewal_readiness_failure(
                    reservation,
                    GatewayDiagnostic::new(LIVE_READINESS_FAILED_TEXT),
                    PaymentResolutionCode::GatewayLiveReadinessFailedBeforeSubmission,
                    None,
                    boundary,
                )
                .await?;
                Ok(false)
            }
            Err(GatewayError::RateLimited(detail)) => {
                self.resolve_renewal_readiness_failure(
                    reservation,
                    detail,
                    PaymentResolutionCode::GatewayProviderRateLimitedBeforeSubmission,
                    Some(RateLimitCooldown::Provider),
                    boundary,
                )
                .await?;
                Ok(false)
            }
            Err(_) => {
                self.resolve_renewal_readiness_failure(
                    reservation,
                    GatewayDiagnostic::new(LIVE_READINESS_FAILED_TEXT),
                    PaymentResolutionCode::GatewayLiveReadinessFailedBeforeSubmission,
                    None,
                    boundary,
                )
                .await?;
                Ok(false)
            }
        }
    }

    async fn resolve_renewal_readiness_failure(
        &self,
        reservation: &SubscriptionRenewalReservation,
        detail: GatewayDiagnostic,
        code: PaymentResolutionCode,
        cooldown: Option<RateLimitCooldown>,
        boundary: OutcomeResolutionBoundary,
    ) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentServiceError> {
        let evidence = ProcessorEvidence::new(
            None,
            None,
            None,
            None,
            Some(detail),
            Some(GatewayDiagnostic::new("failed")),
            GatewayPaymentDescriptor::default(),
        );
        resolve_renewal_non_approved_outcome(
            self.coordinator.as_ref(),
            reservation,
            &evidence,
            PaymentAttemptStatus::Failed,
            Some(code),
            cooldown,
            boundary,
        )
        .await
        .map_err(SubscriptionEnrollmentServiceError::from)
    }
}

fn preserve_concurrent_terminal_payment(
    payment: SubscriptionEnrollmentPaymentResult,
    error: GatewayNotSubmittedError,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentServiceError> {
    if payment.attempt().state().resolution_code()
        == Some(crate::enrollment_application::not_submitted_resolution_code(&error))
    {
        Err(SubscriptionEnrollmentServiceError::GatewayNotSubmitted(
            error,
        ))
    } else {
        Ok(payment)
    }
}

fn is_retryable_renewal_admission_error(error: &SubscriptionEnrollmentApplicationError) -> bool {
    let sqlstate = match error {
        SubscriptionEnrollmentApplicationError::Sql(sqlx::Error::Database(error)) => error.code(),
        SubscriptionEnrollmentApplicationError::Attempt(PaymentAttemptStoreError::Sql(
            sqlx::Error::Database(error),
        )) => error.code(),
        _ => None,
    };
    matches!(
        sqlstate.as_deref(),
        Some("40001" | "40P01" | "55P03" | "57014")
    )
}

struct GatewayAccountSnapshot {
    account_id: GatewayAccountId,
    provider_key: GatewayProviderKey,
}

struct RenewalGatewayAccountSnapshot {
    account_id: GatewayAccountId,
    configuration_id: syrup_rail::GatewayConfigurationId,
    provider_key: GatewayProviderKey,
}

impl RenewalGatewayAccountSnapshot {
    fn as_gateway_snapshot(&self) -> GatewayAccountSnapshot {
        GatewayAccountSnapshot {
            account_id: self.account_id,
            provider_key: self.provider_key.clone(),
        }
    }
}

const fn map_reservation_build_error(
    error: SubscriptionEnrollmentReservationBuildError,
) -> SubscriptionEnrollmentServiceError {
    match error {
        SubscriptionEnrollmentReservationBuildError::GatewayIdentityMismatch => {
            SubscriptionEnrollmentServiceError::ResolvedGatewayIdentityMismatch
        }
        SubscriptionEnrollmentReservationBuildError::AttemptKindMismatch
        | SubscriptionEnrollmentReservationBuildError::InvalidCharge => {
            SubscriptionEnrollmentServiceError::InvalidState(INVALID_SERVICE_STATE)
        }
    }
}
