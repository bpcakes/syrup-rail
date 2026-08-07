use std::{fmt, sync::Arc, time::Duration};

use sqlx::PgPool;
use syrup_rail::{
    EndUserMutationAdmission, EndUserMutationAdmissionResult, EndUserMutationCommand,
    EndUserMutationOperation, EnrollSubscription, GatewayAccountId, GatewayAccountMode,
    GatewayDiagnostic, GatewayError, GatewayNotSubmittedError, GatewayPaymentDescriptor,
    GatewayProviderKey, GatewayResolutionError, GatewayResolver, PaymentAttempt,
    PaymentAttemptStatus, PaymentResolutionCode, ProcessorEvidence,
    SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentPreflightOutcome,
    SubscriptionEnrollmentReservation, SubscriptionEnrollmentReservationBuildError,
    SubscriptionEnrollmentReservationOutcome, SubscriptionEnrollmentReservationRejection,
    SubscriptionEnrollmentSubmissionRejection,
};
use thiserror::Error;

use crate::{
    BillingTransactionCoordinator, PaymentAttemptStoreError,
    SubscriptionEnrollmentAdmissionOutcome, SubscriptionEnrollmentApplicationError,
    SubscriptionEnrollmentProviderResult, SubscriptionOfferStore,
    admit_subscription_enrollment_submission,
    enrollment_application::{
        RateLimitCooldown, payment_result_for_attempt, resolve_non_approved_outcome,
    },
    preflight_subscription_enrollment_in_transaction,
    reserve_subscription_enrollment_in_transaction, submit_admitted_subscription_enrollment,
};

const INVALID_SERVICE_STATE: &str = "canonical subscription enrollment service state is invalid";

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
    #[error("gateway mutation was not submitted")]
    GatewayNotSubmitted(#[source] GatewayNotSubmittedError),
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
            Self::GatewayNotSubmitted(error) => formatter
                .debug_tuple("SubscriptionEnrollmentServiceError::GatewayNotSubmitted")
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

        let account = self.gateway_account(&command).await?;
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
            return self.resolve_cooldown(&reservation, scope).await;
        }
        match gateway.account_mode().await {
            Ok(GatewayAccountMode::Live) => {}
            Ok(GatewayAccountMode::Test) => {
                return self
                    .resolve_readiness_failure(
                        &reservation,
                        GatewayDiagnostic::new("gateway account is in test mode"),
                        PaymentResolutionCode::GatewayLiveReadinessFailedBeforeSubmission,
                        None,
                    )
                    .await;
            }
            Err(GatewayError::RateLimited(detail)) => {
                return self
                    .resolve_provider_readiness_rate_limit(&reservation, detail)
                    .await;
            }
            Err(error) => {
                return self
                    .resolve_readiness_failure(
                        &reservation,
                        error.detail().clone(),
                        PaymentResolutionCode::GatewayLiveReadinessFailedBeforeSubmission,
                        None,
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
            return self.resolve_cooldown(&reservation, scope).await;
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
            SubscriptionEnrollmentProviderResult::NotSubmitted { error, .. } => Err(
                SubscriptionEnrollmentServiceError::GatewayNotSubmitted(error),
            ),
        }
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
        command: &EnrollSubscription,
    ) -> Result<GatewayAccountSnapshot, SubscriptionEnrollmentServiceError> {
        let row = sqlx::query_as::<_, (uuid::Uuid, String)>(
            r#"
            SELECT id, provider_key
            FROM billing_gateway_accounts
            WHERE billing_scope_id = $1 AND gateway_configuration_id = $2
            "#,
        )
        .bind(command.billing_scope_id().as_uuid())
        .bind(command.gateway_configuration_id().as_uuid())
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
            .resolve_readiness_failure(reservation, GatewayDiagnostic::new(message), code, None)
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
            .resolve_readiness_failure(reservation, detail, code, Some(RateLimitCooldown::Provider))
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
    ) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentServiceError> {
        let evidence = ProcessorEvidence::new(
            None,
            None,
            None,
            None,
            Some(detail),
            None,
            GatewayPaymentDescriptor::default(),
        );
        resolve_non_approved_outcome(
            &self.pool,
            reservation,
            &evidence,
            PaymentAttemptStatus::Failed,
            Some(code),
            cooldown,
        )
        .await
        .map_err(SubscriptionEnrollmentServiceError::from)
    }
}

struct GatewayAccountSnapshot {
    account_id: GatewayAccountId,
    provider_key: GatewayProviderKey,
}

const fn map_reservation_build_error(
    error: SubscriptionEnrollmentReservationBuildError,
) -> SubscriptionEnrollmentServiceError {
    match error {
        SubscriptionEnrollmentReservationBuildError::GatewayIdentityMismatch => {
            SubscriptionEnrollmentServiceError::ResolvedGatewayIdentityMismatch
        }
    }
}
