use std::fmt;

use sqlx::{PgConnection, PgPool, Row};
use syrup_rail::{
    ApprovedProcessorEvidence, BillingEvent, BillingEventSubject, BillingScopeId,
    GatewayMutationError, GatewayNotSubmittedError, GatewayPaymentDescriptor,
    GatewayPaymentOutcome, GatewayPaymentStatus, GatewayProviderKey,
    GatewayStorePaymentMethodRequest, PaymentAttempt, PaymentAttemptId, PaymentAttemptStatus,
    PaymentCardDisplay, PaymentMethodId, PaymentResolutionCode, ProcessorChargeProgression,
    ProcessorChargeRole, ProcessorEvidence, ReplaceSubscriptionPaymentMethod, ResolvedGateway,
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
    AttemptResolutionStatus, BILLING_LOCK_TIMEOUT, INVALID_APPLICATION_STATE, OutcomeReservation,
    OutcomeResolutionBoundary, OutcomeResolutionCommand,
    PAYMENT_METHOD_REPLACEMENT_INCOMPLETE_APPROVAL_TEXT,
    PAYMENT_METHOD_REPLACEMENT_STALE_STATE_TEXT, PAYMENT_METHOD_REPLACEMENT_STORAGE_FAILURE_TEXT,
    RateLimitCooldown, SubscriptionEnrollmentApplicationError,
    disable_payment_method_if_unreferenced, finalize_approved_application,
    is_retryable_evidence_error, load_applied_subscription, load_subscription,
    lock_expected_reservation_attempt, lock_payment_method_domain, lock_subscription_aggregate,
    mark_attempt_approved, mutation_error_evidence, not_submitted_resolution_code,
    park_locked_attempt, payment_result_for_attempt,
    persist_approved_evidence_without_attempt_lock, resolve_pool_outcome, set_application_timeouts,
    upsert_payment_method,
};

mod approval;

use approval::apply_payment_method_replacement_approved_outcome;

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
    gateway: &ResolvedGateway,
) -> Result<
    SubscriptionPaymentMethodReplacementProviderResult,
    SubscriptionEnrollmentApplicationError,
> {
    let identity = admission.reservation.identity();
    if admission.attempt.identity() != identity
        || admission.attempt.request() != admission.reservation.request()
        || admission.attempt.status() != PaymentAttemptStatus::Pending
        || admission
            .attempt
            .state()
            .timestamps()
            .submitted_at()
            .is_none()
        || command.billing_scope_id() != identity.billing_scope_id()
        || command.subscriber_id() != identity.subscriber_id()
        || command.plan_key() != admission.reservation.plan_key()
        || command.gateway_configuration_id() != identity.gateway_configuration_id()
        || gateway.billing_scope_id() != identity.billing_scope_id()
        || gateway.gateway_account_id() != identity.gateway_account_id()
        || gateway.gateway_configuration_id() != identity.gateway_configuration_id()
        || gateway.provider_key() != admission.reservation.provider_key()
    {
        return Err(SubscriptionEnrollmentApplicationError::SubmissionIdentityMismatch);
    }
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
            let cooldown = matches!(error, GatewayNotSubmittedError::RateLimited(_))
                .then_some(RateLimitCooldown::Account);
            let payment = resolve_payment_method_replacement_non_approved_outcome(
                pool,
                &admission.reservation,
                &evidence,
                AttemptResolutionStatus::Failed,
                Some(not_submitted_resolution_code(&error)),
                cooldown,
                OutcomeResolutionBoundary::AdmittedNotSubmitted,
            )
            .await?;
            Ok(SubscriptionPaymentMethodReplacementProviderResult::NotSubmitted { payment, error })
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
    let outcome = reconciled_outcome_with_persisted_evidence(&attempt, outcome);
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
    apply_subscription_payment_method_replacement_gateway_outcome(
        pool,
        coordinator,
        &reservation,
        &outcome,
    )
    .await
}

fn reconciled_outcome_with_persisted_evidence(
    attempt: &PaymentAttempt,
    outcome: &GatewayPaymentOutcome,
) -> GatewayPaymentOutcome {
    let observed = outcome.evidence();
    let persisted = attempt.state().processor_evidence();
    let preserve_review_evidence = attempt.status() == PaymentAttemptStatus::ReviewRequired;
    let (primary, fallback) = if preserve_review_evidence {
        (persisted, observed)
    } else {
        (observed, persisted)
    };
    let primary_descriptor = primary.descriptor();
    let fallback_descriptor = fallback.descriptor();
    let descriptor = GatewayPaymentDescriptor::from_provider_parts(
        primary_descriptor
            .payment_type()
            .or_else(|| fallback_descriptor.payment_type())
            .cloned(),
        primary_descriptor
            .card_brand()
            .or_else(|| fallback_descriptor.card_brand())
            .cloned(),
        primary_descriptor
            .card_last_four()
            .or_else(|| fallback_descriptor.card_last_four())
            .map(|value| value.expose()),
        primary_descriptor
            .card_exp_month()
            .or_else(|| fallback_descriptor.card_exp_month()),
        primary_descriptor
            .card_exp_year()
            .or_else(|| fallback_descriptor.card_exp_year()),
    );
    GatewayPaymentOutcome::new(
        outcome.status(),
        ProcessorEvidence::new(
            primary
                .transaction_id()
                .or_else(|| fallback.transaction_id())
                .cloned(),
            primary
                .payment_method_reference()
                .or_else(|| fallback.payment_method_reference())
                .cloned(),
            primary.response().or_else(|| fallback.response()).cloned(),
            primary
                .response_code()
                .or_else(|| fallback.response_code())
                .cloned(),
            primary
                .response_text()
                .or_else(|| fallback.response_text())
                .cloned(),
            primary
                .condition()
                .or_else(|| fallback.condition())
                .cloned(),
            descriptor,
        ),
    )
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
            observe_payment_method_replacement_approved_evidence_with_retry(
                pool,
                reservation,
                evidence,
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
    }
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
        observe_processor_charge(
            &mut transaction,
            &attempt,
            evidence,
            ProcessorChargeProgression::ReconciliationRequired,
        )
        .await?;
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
