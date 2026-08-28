use std::fmt;

use sqlx::{PgConnection, PgPool, Row};
use syrup_rail::{
    ApprovedProcessorEvidence, BillingEvent, BillingEventSubject, BillingScopeId,
    GatewayMutationError, GatewayNotSubmittedError, GatewayPaymentDescriptor,
    GatewayPaymentOutcome, GatewayPaymentStatus, GatewayStorePaymentMethodRequest, PaymentAttempt,
    PaymentAttemptId, PaymentAttemptStatus, PaymentCardDisplay, PaymentMethodId,
    PaymentResolutionCode, ProcessorChargeProgression, ProcessorChargeRole, ProcessorEvidence,
    ReplaceSubscriptionPaymentMethod, SubscriptionEnrollmentPaymentResult,
    SubscriptionPaymentMethodReplacement, SubscriptionPaymentMethodReplacementSubmissionOutcome,
    SubscriptionPaymentMethodReplacementSubmissionRejection,
};

use crate::{
    BillingTransactionCoordinator, BillingTransactionSubjectState,
    attempts::find_payment_attempt_by_id_on_connection,
    processor_charges::{ObservedCharge, observe_processor_charge, transition_charge},
};

use super::{
    APPROVED_APPLICATION_ATTEMPTS, APPROVED_EVIDENCE_RETRY_DELAY, AttemptResolutionStatus,
    BILLING_LOCK_TIMEOUT, GatewayNotSubmittedPolicy, INVALID_APPLICATION_STATE, OutcomeApplication,
    OutcomeReservation, OutcomeResolutionBoundary, OutcomeResolutionCommand,
    PAYMENT_METHOD_REPLACEMENT_INCOMPLETE_APPROVAL_TEXT,
    PAYMENT_METHOD_REPLACEMENT_STALE_STATE_TEXT, PAYMENT_METHOD_REPLACEMENT_STORAGE_FAILURE_TEXT,
    RateLimitCooldown, SubscriptionEnrollmentApplicationError,
    apply_resumable_not_submitted_policy, disable_payment_method_if_unreferenced,
    finalize_approved_application, load_applied_subscription, load_subscription,
    lock_expected_reservation_attempt, lock_payment_method_domain, lock_subscription_aggregate,
    mark_attempt_approved, mutation_error_evidence, park_locked_attempt, resolve_pool_outcome,
    set_application_timeouts, upsert_payment_method,
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
    match outcome.status() {
        GatewayPaymentStatus::Approved => {
            let approved_evidence = outcome.approved_evidence().ok_or(
                SubscriptionEnrollmentApplicationError::InvalidState(INVALID_APPLICATION_STATE),
            )?;
            if outcome.transaction_id().is_none() || outcome.payment_method_reference().is_none() {
                return super::park_approved_outcome(
                    pool,
                    OutcomeReservation::PaymentMethodReplacement(reservation),
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
            super::park_approved_outcome(
                pool,
                OutcomeReservation::PaymentMethodReplacement(reservation),
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
    super::apply_reconciled_gateway_outcome_for(
        pool,
        coordinator,
        billing_scope_id,
        attempt_id,
        outcome,
        super::ReconciledApplicationEntry::Exact(
            super::ReservationOperation::PaymentMethodReplacement,
        ),
    )
    .await
}

pub(super) fn reconciled_outcome_with_persisted_evidence(
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
