use std::time::Duration;

use syrup_rail::{
    GatewayError, GatewayNotSubmittedError, GatewayResolutionError, HostChargeTargetRejection,
    SubscriptionEnrollmentReservationRejection, SubscriptionEnrollmentSubmissionRejection,
    SubscriptionPaymentMethodReplacementRejection,
    SubscriptionPaymentMethodReplacementSubmissionRejection,
    SubscriptionRecoveryReservationRejection, SubscriptionRecoverySubmissionRejection,
    SubscriptionRenewalReservationRejection,
};

use super::{
    GatewayMutationCooldownScope, SubscriptionBillingServiceError,
    SubscriptionBillingServiceErrorDisposition,
};

impl SubscriptionBillingServiceError {
    /// Returns the stable, conservative operational category for this error.
    ///
    /// The category intentionally does not expose provider diagnostics,
    /// payment values, or durable identifiers.
    pub const fn disposition(&self) -> SubscriptionBillingServiceErrorDisposition {
        match self {
            Self::StorageTemporarilyUnavailable(_) => {
                SubscriptionBillingServiceErrorDisposition::TemporarilyUnavailable
            }
            Self::Sql(_)
            | Self::Attempt(_)
            | Self::Application(_)
            | Self::HostChargeApplication(_)
            | Self::HostChargeStore(_)
            | Self::BillingTransaction(_)
            | Self::BillingEvent(_)
            | Self::ResolvedGatewayIdentityMismatch
            | Self::InvalidState(_) => SubscriptionBillingServiceErrorDisposition::Internal,
            Self::Cancellation(error) => cancellation_error_disposition(error),
            Self::Discount(error) => discount_error_disposition(error),
            Self::HostChargeUnavailable => {
                SubscriptionBillingServiceErrorDisposition::Misconfigured
            }
            Self::IdempotencyConflict => SubscriptionBillingServiceErrorDisposition::Conflict,
            Self::AdmissionDenied { .. } | Self::AdmissionTimeout | Self::AdmissionUnavailable => {
                SubscriptionBillingServiceErrorDisposition::TemporarilyUnavailable
            }
            Self::GatewayConfigurationChanged => {
                SubscriptionBillingServiceErrorDisposition::Conflict
            }
            Self::GatewayResolution(error) => gateway_resolution_disposition(*error),
            Self::GatewayMutationCooldown { scope } => match scope {
                GatewayMutationCooldownScope::Account | GatewayMutationCooldownScope::Provider => {
                    SubscriptionBillingServiceErrorDisposition::TemporarilyUnavailable
                }
            },
            Self::ReservationRejected(reason) => {
                enrollment_reservation_rejection_disposition(*reason)
            }
            Self::SubmissionRejected(reason) => {
                enrollment_submission_rejection_disposition(*reason)
            }
            Self::HostChargeReservationRejected(reason)
            | Self::HostChargeSubmissionRejected(reason) => {
                host_charge_rejection_disposition(*reason)
            }
            Self::RecoveryReservationRejected(reason) => {
                recovery_reservation_rejection_disposition(*reason)
            }
            Self::RecoverySubmissionRejected(reason) => {
                recovery_submission_rejection_disposition(*reason)
            }
            Self::RenewalReservationRejected(reason) => {
                renewal_reservation_rejection_disposition(*reason)
            }
            Self::PaymentMethodReplacementReservationRejected(reason) => {
                payment_method_replacement_reservation_rejection_disposition(*reason)
            }
            Self::PaymentMethodReplacementSubmissionRejected(reason) => {
                payment_method_replacement_submission_rejection_disposition(*reason)
            }
            Self::GatewayNotSubmitted(error) => gateway_not_submitted_disposition(error),
            Self::GatewayReadiness(error) => gateway_readiness_disposition(error),
        }
    }

    /// Returns whether it is safe to resubmit the **same idempotent command**.
    ///
    /// A `true` result means only that Syrup Rail can safely accept another
    /// submission of that unchanged command and idempotency key. It does not
    /// promise that the next attempt will succeed. Do not create a new command
    /// or idempotency key merely because this returns `true`.
    pub const fn is_retryable(&self) -> bool {
        matches!(
            self.disposition(),
            SubscriptionBillingServiceErrorDisposition::TemporarilyUnavailable
        )
    }

    /// Returns whether the command conflicts with an existing idempotency or
    /// authority snapshot.
    ///
    /// Conflicts are not retryable as-is; reload current authority or reconcile
    /// the existing idempotency key before constructing another command.
    pub const fn is_conflict(&self) -> bool {
        matches!(
            self.disposition(),
            SubscriptionBillingServiceErrorDisposition::Conflict
        )
    }

    /// Returns an exact retry delay when the service was given one.
    ///
    /// `None` does not imply that the error is non-retryable. For example,
    /// gateway/account cooldowns are temporarily unavailable but do not carry
    /// an exact delay that this API can safely fabricate.
    pub const fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::AdmissionDenied { retry_after } => Some(*retry_after),
            Self::Sql(_)
            | Self::StorageTemporarilyUnavailable(_)
            | Self::Attempt(_)
            | Self::Application(_)
            | Self::HostChargeApplication(_)
            | Self::HostChargeStore(_)
            | Self::Cancellation(_)
            | Self::Discount(_)
            | Self::BillingTransaction(_)
            | Self::BillingEvent(_)
            | Self::HostChargeUnavailable
            | Self::IdempotencyConflict
            | Self::AdmissionTimeout
            | Self::AdmissionUnavailable
            | Self::GatewayConfigurationChanged
            | Self::GatewayResolution(_)
            | Self::ResolvedGatewayIdentityMismatch
            | Self::GatewayMutationCooldown { .. }
            | Self::ReservationRejected(_)
            | Self::SubmissionRejected(_)
            | Self::HostChargeReservationRejected(_)
            | Self::HostChargeSubmissionRejected(_)
            | Self::RecoveryReservationRejected(_)
            | Self::RecoverySubmissionRejected(_)
            | Self::RenewalReservationRejected(_)
            | Self::PaymentMethodReplacementReservationRejected(_)
            | Self::PaymentMethodReplacementSubmissionRejected(_)
            | Self::GatewayNotSubmitted(_)
            | Self::GatewayReadiness(_)
            | Self::InvalidState(_) => None,
        }
    }
}

const fn cancellation_error_disposition(
    error: &crate::SubscriptionCancellationError,
) -> SubscriptionBillingServiceErrorDisposition {
    match error {
        crate::SubscriptionCancellationError::Sql(_)
        | crate::SubscriptionCancellationError::InvalidState(_) => {
            SubscriptionBillingServiceErrorDisposition::Internal
        }
    }
}

const fn discount_error_disposition(
    error: &crate::SubscriptionDiscountOperationError,
) -> SubscriptionBillingServiceErrorDisposition {
    match error {
        crate::SubscriptionDiscountOperationError::Sql(_)
        | crate::SubscriptionDiscountOperationError::InvalidState(_) => {
            SubscriptionBillingServiceErrorDisposition::Internal
        }
        crate::SubscriptionDiscountOperationError::OfferUnavailable
        | crate::SubscriptionDiscountOperationError::InvalidConfiguration
        | crate::SubscriptionDiscountOperationError::LimitedDiscountCadence => {
            SubscriptionBillingServiceErrorDisposition::Misconfigured
        }
        crate::SubscriptionDiscountOperationError::OfferPlanMismatch => {
            SubscriptionBillingServiceErrorDisposition::Conflict
        }
    }
}

const fn gateway_resolution_disposition(
    error: GatewayResolutionError,
) -> SubscriptionBillingServiceErrorDisposition {
    match error {
        GatewayResolutionError::NotFound | GatewayResolutionError::InvalidConfiguration => {
            SubscriptionBillingServiceErrorDisposition::Misconfigured
        }
        GatewayResolutionError::ConfigurationChanged => {
            SubscriptionBillingServiceErrorDisposition::Conflict
        }
        GatewayResolutionError::Unavailable => {
            SubscriptionBillingServiceErrorDisposition::TemporarilyUnavailable
        }
    }
}

const fn gateway_readiness_disposition(
    error: &GatewayError,
) -> SubscriptionBillingServiceErrorDisposition {
    match error {
        GatewayError::RequestRejected(_) => SubscriptionBillingServiceErrorDisposition::Rejected,
        // A malformed gateway response violates the adapter contract; retrying
        // the same command would not establish that provider I/O is safe.
        GatewayError::Malformed(_) => SubscriptionBillingServiceErrorDisposition::Internal,
        GatewayError::Configuration(_) => SubscriptionBillingServiceErrorDisposition::Misconfigured,
        GatewayError::Unavailable(_) | GatewayError::RateLimited(_) => {
            SubscriptionBillingServiceErrorDisposition::TemporarilyUnavailable
        }
    }
}

const fn gateway_not_submitted_disposition(
    error: &GatewayNotSubmittedError,
) -> SubscriptionBillingServiceErrorDisposition {
    match error {
        GatewayNotSubmittedError::RequestRejected(_) => {
            SubscriptionBillingServiceErrorDisposition::Rejected
        }
        // A malformed request was definitely not submitted, but it still
        // indicates a service/adapter contract fault rather than a retryable
        // transport condition.
        GatewayNotSubmittedError::Malformed(_) => {
            SubscriptionBillingServiceErrorDisposition::Internal
        }
        GatewayNotSubmittedError::Configuration(_) => {
            SubscriptionBillingServiceErrorDisposition::Misconfigured
        }
        GatewayNotSubmittedError::Unavailable(_) | GatewayNotSubmittedError::RateLimited(_) => {
            SubscriptionBillingServiceErrorDisposition::TemporarilyUnavailable
        }
    }
}

const fn enrollment_reservation_rejection_disposition(
    rejection: SubscriptionEnrollmentReservationRejection,
) -> SubscriptionBillingServiceErrorDisposition {
    match rejection {
        SubscriptionEnrollmentReservationRejection::CurrentSubscription
        | SubscriptionEnrollmentReservationRejection::ActiveGrant
        | SubscriptionEnrollmentReservationRejection::UnresolvedProcessorCharge
        | SubscriptionEnrollmentReservationRejection::AttemptInProgress => {
            SubscriptionBillingServiceErrorDisposition::Rejected
        }
        SubscriptionEnrollmentReservationRejection::EnrollmentTermsChanged
        | SubscriptionEnrollmentReservationRejection::GatewayConfigurationChanged => {
            SubscriptionBillingServiceErrorDisposition::Conflict
        }
    }
}

const fn enrollment_submission_rejection_disposition(
    rejection: SubscriptionEnrollmentSubmissionRejection,
) -> SubscriptionBillingServiceErrorDisposition {
    match rejection {
        SubscriptionEnrollmentSubmissionRejection::BillingStateChanged
        | SubscriptionEnrollmentSubmissionRejection::EnrollmentTermsChanged
        | SubscriptionEnrollmentSubmissionRejection::GatewayConfigurationChanged => {
            SubscriptionBillingServiceErrorDisposition::Conflict
        }
    }
}

const fn host_charge_rejection_disposition(
    rejection: HostChargeTargetRejection,
) -> SubscriptionBillingServiceErrorDisposition {
    match rejection {
        HostChargeTargetRejection::TargetUnavailable | HostChargeTargetRejection::LedgerUnsafe => {
            SubscriptionBillingServiceErrorDisposition::Rejected
        }
        HostChargeTargetRejection::ChargeChanged => {
            SubscriptionBillingServiceErrorDisposition::Conflict
        }
    }
}

const fn recovery_reservation_rejection_disposition(
    rejection: SubscriptionRecoveryReservationRejection,
) -> SubscriptionBillingServiceErrorDisposition {
    match rejection {
        SubscriptionRecoveryReservationRejection::SubscriptionNotFound
        | SubscriptionRecoveryReservationRejection::PaymentNotDue
        | SubscriptionRecoveryReservationRejection::AttemptInProgress
        | SubscriptionRecoveryReservationRejection::PaymentMethodUpdateInProgress => {
            SubscriptionBillingServiceErrorDisposition::Rejected
        }
        SubscriptionRecoveryReservationRejection::GatewayConfigurationChanged => {
            SubscriptionBillingServiceErrorDisposition::Conflict
        }
    }
}

const fn recovery_submission_rejection_disposition(
    rejection: SubscriptionRecoverySubmissionRejection,
) -> SubscriptionBillingServiceErrorDisposition {
    match rejection {
        SubscriptionRecoverySubmissionRejection::BillingStateChanged
        | SubscriptionRecoverySubmissionRejection::GatewayConfigurationChanged => {
            SubscriptionBillingServiceErrorDisposition::Conflict
        }
    }
}

const fn renewal_reservation_rejection_disposition(
    rejection: SubscriptionRenewalReservationRejection,
) -> SubscriptionBillingServiceErrorDisposition {
    match rejection {
        SubscriptionRenewalReservationRejection::SubscriptionNotFound
        | SubscriptionRenewalReservationRejection::PaymentNotDue
        | SubscriptionRenewalReservationRejection::AttemptInProgress
        | SubscriptionRenewalReservationRejection::PaymentMethodUpdateInProgress
        | SubscriptionRenewalReservationRejection::RetryBlocked => {
            SubscriptionBillingServiceErrorDisposition::Rejected
        }
        SubscriptionRenewalReservationRejection::GatewayConfigurationChanged => {
            SubscriptionBillingServiceErrorDisposition::Conflict
        }
    }
}

const fn payment_method_replacement_reservation_rejection_disposition(
    rejection: SubscriptionPaymentMethodReplacementRejection,
) -> SubscriptionBillingServiceErrorDisposition {
    match rejection {
        SubscriptionPaymentMethodReplacementRejection::SubscriptionNotFound
        | SubscriptionPaymentMethodReplacementRejection::SubscriptionIneligible
        | SubscriptionPaymentMethodReplacementRejection::ChargeAttemptInProgress
        | SubscriptionPaymentMethodReplacementRejection::PaymentMethodUpdateInProgress => {
            SubscriptionBillingServiceErrorDisposition::Rejected
        }
        SubscriptionPaymentMethodReplacementRejection::GatewayConfigurationChanged => {
            SubscriptionBillingServiceErrorDisposition::Conflict
        }
    }
}

const fn payment_method_replacement_submission_rejection_disposition(
    rejection: SubscriptionPaymentMethodReplacementSubmissionRejection,
) -> SubscriptionBillingServiceErrorDisposition {
    match rejection {
        SubscriptionPaymentMethodReplacementSubmissionRejection::BillingStateChanged
        | SubscriptionPaymentMethodReplacementSubmissionRejection::GatewayConfigurationChanged => {
            SubscriptionBillingServiceErrorDisposition::Conflict
        }
    }
}
