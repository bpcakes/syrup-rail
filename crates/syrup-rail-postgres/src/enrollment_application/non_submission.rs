//! A definite non-submission result is bound to one admission generation.
use super::*;
use syrup_rail::{PaymentAttemptIdentity, PaymentAttemptPhase};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct NonSubmissionProof {
    identity: PaymentAttemptIdentity,
    admitted_at: DateTime<Utc>,
}

impl NonSubmissionProof {
    /// Only a definite not-submitted provider result can produce this proof.
    /// The snapshot must come from the consumed committed-admission capability.
    pub(crate) fn for_error(
        admitted: &PaymentAttempt,
        _error: &GatewayNotSubmittedError,
    ) -> Result<Self, SubscriptionEnrollmentApplicationError> {
        Self::before_provider_call(admitted)
    }

    /// Bind a local control-plane decision while the committed capability has
    /// not yet been consumed by provider I/O.
    pub(crate) fn before_provider_call(
        admitted: &PaymentAttempt,
    ) -> Result<Self, SubscriptionEnrollmentApplicationError> {
        let PaymentAttemptPhase::Admitted { admitted_at } = admitted.phase() else {
            return Err(SubscriptionEnrollmentApplicationError::SubmissionIdentityMismatch);
        };
        Ok(Self {
            identity: admitted.identity(),
            admitted_at,
        })
    }

    pub(crate) fn matches(self, attempt: &PaymentAttempt) -> bool {
        attempt.identity() == self.identity
            && attempt.phase()
                == PaymentAttemptPhase::Admitted {
                    admitted_at: self.admitted_at,
                }
    }
}

/// Restores only the pending admission whose non-submission was proved.
/// An old result cannot clear a later admission, uncertain outcome, or resolution.
pub(crate) async fn restore_prepared_attempt_submission(
    connection: &mut PgConnection,
    attempt: &PaymentAttempt,
    proof: NonSubmissionProof,
) -> Result<(), SubscriptionEnrollmentApplicationError> {
    if !proof.matches(attempt) {
        return Err(SubscriptionEnrollmentApplicationError::SubmissionIdentityMismatch);
    }
    let result = sqlx::query(
        "UPDATE billing_payment_attempts SET submitted_at = NULL, updated_at = clock_timestamp() \
         WHERE id = $1 AND billing_scope_id = $2 AND status = 'pending' AND submitted_at = $3",
    )
    .bind(proof.identity.attempt_id().as_uuid())
    .bind(proof.identity.billing_scope_id().as_uuid())
    .bind(proof.admitted_at)
    .execute(connection)
    .await?;
    if result.rows_affected() != 1 {
        return Err(SubscriptionEnrollmentApplicationError::InvalidState(
            INVALID_APPLICATION_STATE,
        ));
    }
    Ok(())
}
