use sqlx::{PgConnection, Postgres, Transaction};
use syrup_rail::{
    GatewayDiagnostic, GatewayTransactionId, PaymentAttempt, PaymentAttemptStatus, PaymentMethodId,
    PaymentResolutionCode, ProcessorEvidence, SubscriptionId,
};

use super::{PaymentAttemptStoreError, invalid_state};
use crate::attempts::persistence::find_payment_attempt_by_id_in_transaction;

/// The non-approved statuses that application code may durably record.
///
/// `Approved` is deliberately absent because approval must also carry the
/// subscription and payment-method linkage created by the same transaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AttemptResolutionStatus {
    Declined,
    Failed,
    Unknown,
    ReviewRequired,
}

impl AttemptResolutionStatus {
    pub(crate) const fn payment_status(self) -> PaymentAttemptStatus {
        match self {
            Self::Declined => PaymentAttemptStatus::Declined,
            Self::Failed => PaymentAttemptStatus::Failed,
            Self::Unknown => PaymentAttemptStatus::Unknown,
            Self::ReviewRequired => PaymentAttemptStatus::ReviewRequired,
        }
    }

    pub(crate) const fn as_str(self) -> &'static str {
        self.payment_status().as_str()
    }
}

/// One legal durable application transition for a subscription payment
/// attempt. Each variant fixes both its required data and its allowed source
/// states, so callers cannot independently toggle terminal-race behavior or
/// supply only half of the approved subscription linkage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AttemptApproval {
    HostCharge,
    Subscription {
        subscription_id: SubscriptionId,
        payment_method_id: PaymentMethodId,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AttemptTransition {
    Approved(AttemptApproval),
    Resolved {
        status: AttemptResolutionStatus,
        resolution_code: Option<PaymentResolutionCode>,
    },
    LateApprovalReview {
        resolution_code: Option<PaymentResolutionCode>,
        message: &'static str,
    },
}

/// Persists processor evidence together with one typed attempt transition.
///
/// The ordinary transition retains the historical pending/unknown/review
/// compare-and-set predicate. Late approved evidence is the sole transition
/// allowed to reopen a declined or failed attempt as review-required.
pub(crate) async fn persist_attempt_transition(
    connection: &mut PgConnection,
    attempt: &PaymentAttempt,
    evidence: &ProcessorEvidence,
    transition: AttemptTransition,
) -> Result<(), PaymentAttemptStoreError> {
    let descriptor = evidence.descriptor();
    if let AttemptTransition::LateApprovalReview {
        resolution_code,
        message,
    } = transition
    {
        let result = sqlx::query(
            r#"
            UPDATE billing_payment_attempts
            SET status = 'review_required',
                gateway_transaction_id = $2,
                gateway_payment_method_reference = $3,
                gateway_response = $4, gateway_response_code = $5,
                gateway_response_text = CASE
                    WHEN $6 IS NULL THEN $14
                    ELSE left($14 || ' ' || $6, 512)
                END,
                gateway_condition = $7,
                payment_type = $8, card_brand = $9, card_last4 = $10,
                card_exp_month = $11, card_exp_year = $12,
                resolution_code = $13,
                review_required_at = COALESCE(review_required_at, clock_timestamp()),
                updated_at = clock_timestamp()
            WHERE id = $1
                AND status IN ('pending', 'unknown', 'review_required', 'declined', 'failed')
            "#,
        )
        .bind(attempt.identity().attempt_id().as_uuid())
        .bind(evidence.transaction_id().map(GatewayTransactionId::expose))
        .bind(
            evidence
                .payment_method_reference()
                .map(|value| value.expose()),
        )
        .bind(evidence.response().map(GatewayDiagnostic::expose))
        .bind(evidence.response_code().map(GatewayDiagnostic::expose))
        .bind(evidence.response_text().map(GatewayDiagnostic::expose))
        .bind(evidence.condition().map(GatewayDiagnostic::expose))
        .bind(descriptor.payment_type().map(GatewayDiagnostic::expose))
        .bind(descriptor.card_brand().map(GatewayDiagnostic::expose))
        .bind(descriptor.card_last_four().map(|value| value.expose()))
        .bind(descriptor.card_exp_month())
        .bind(descriptor.card_exp_year())
        .bind(resolution_code.map(PaymentResolutionCode::as_str))
        .bind(message)
        .execute(&mut *connection)
        .await?;
        if result.rows_affected() != 1 {
            return Err(invalid_state());
        }
        return Ok(());
    }

    let (status, resolution_code, subscription_id, payment_method_id) = match transition {
        AttemptTransition::Approved(approval) => {
            let (subscription_id, payment_method_id) = match approval {
                AttemptApproval::HostCharge => (None, None),
                AttemptApproval::Subscription {
                    subscription_id,
                    payment_method_id,
                } => (Some(subscription_id), Some(payment_method_id)),
            };
            (
                PaymentAttemptStatus::Approved,
                None,
                subscription_id,
                payment_method_id,
            )
        }
        AttemptTransition::Resolved {
            status,
            resolution_code,
        } => (status.payment_status(), resolution_code, None, None),
        AttemptTransition::LateApprovalReview { .. } => unreachable!(),
    };
    let result = sqlx::query(
        r#"
        UPDATE billing_payment_attempts
        SET status = $2,
            subscription_id = COALESCE($3, subscription_id),
            payment_method_id = COALESCE($4, payment_method_id),
            gateway_transaction_id = $5,
            gateway_payment_method_reference = $6,
            gateway_response = $7, gateway_response_code = $8,
            gateway_response_text = $9, gateway_condition = $10,
            payment_type = $11, card_brand = $12, card_last4 = $13,
            card_exp_month = $14, card_exp_year = $15,
            resolution_code = $16,
            resolved_at = CASE WHEN $2 IN ('approved', 'declined', 'failed')
                THEN clock_timestamp() ELSE resolved_at END,
            review_required_at = CASE WHEN $2 = 'review_required'
                THEN COALESCE(review_required_at, clock_timestamp()) ELSE review_required_at END,
            updated_at = clock_timestamp()
        WHERE id = $1 AND status IN ('pending', 'unknown', 'review_required')
        "#,
    )
    .bind(attempt.identity().attempt_id().as_uuid())
    .bind(status.as_str())
    .bind(subscription_id.map(SubscriptionId::into_uuid))
    .bind(payment_method_id.map(PaymentMethodId::into_uuid))
    .bind(evidence.transaction_id().map(GatewayTransactionId::expose))
    .bind(
        evidence
            .payment_method_reference()
            .map(|value| value.expose()),
    )
    .bind(evidence.response().map(GatewayDiagnostic::expose))
    .bind(evidence.response_code().map(GatewayDiagnostic::expose))
    .bind(evidence.response_text().map(GatewayDiagnostic::expose))
    .bind(evidence.condition().map(GatewayDiagnostic::expose))
    .bind(descriptor.payment_type().map(GatewayDiagnostic::expose))
    .bind(descriptor.card_brand().map(GatewayDiagnostic::expose))
    .bind(descriptor.card_last_four().map(|value| value.expose()))
    .bind(descriptor.card_exp_month())
    .bind(descriptor.card_exp_year())
    .bind(resolution_code.map(PaymentResolutionCode::as_str))
    .execute(connection)
    .await?;
    if result.rows_affected() != 1 {
        return Err(invalid_state());
    }
    Ok(())
}

/// Marks one still-pending prepared attempt as submitted and reloads its
/// canonical representation. The caller remains responsible for aggregate
/// locking and semantic revalidation before admission.
pub(crate) async fn admit_prepared_attempt(
    transaction: &mut Transaction<'_, Postgres>,
    attempt: &PaymentAttempt,
) -> Result<PaymentAttempt, PaymentAttemptStoreError> {
    let identity = attempt.identity();
    let updated = sqlx::query(
        "UPDATE billing_payment_attempts SET submitted_at = clock_timestamp(), updated_at = clock_timestamp() WHERE id = $1 AND status = 'pending' AND submitted_at IS NULL",
    )
    .bind(identity.attempt_id().as_uuid())
    .execute(&mut **transaction)
    .await?;
    if updated.rows_affected() != 1 {
        return Err(invalid_state());
    }
    find_payment_attempt_by_id_in_transaction(
        transaction,
        identity.billing_scope_id(),
        identity.attempt_id(),
    )
    .await?
    .ok_or_else(invalid_state)
}

/// Terminally rejects one later-operation attempt before provider submission
/// and reloads its canonical representation.
pub(super) async fn reject_prepared_attempt(
    transaction: &mut Transaction<'_, Postgres>,
    attempt: &PaymentAttempt,
    resolution_code: PaymentResolutionCode,
    message: &'static str,
) -> Result<PaymentAttempt, PaymentAttemptStoreError> {
    sqlx::query(
        r#"
        UPDATE billing_payment_attempts
        SET status = 'failed', resolution_code = $2,
            gateway_response_text = $3,
            resolved_at = COALESCE(resolved_at, clock_timestamp()),
            updated_at = clock_timestamp()
        WHERE id = $1 AND status = 'pending' AND submitted_at IS NULL
        "#,
    )
    .bind(attempt.identity().attempt_id().as_uuid())
    .bind(resolution_code.as_str())
    .bind(message)
    .execute(&mut **transaction)
    .await?;
    find_payment_attempt_by_id_in_transaction(
        transaction,
        attempt.identity().billing_scope_id(),
        attempt.identity().attempt_id(),
    )
    .await?
    .ok_or_else(invalid_state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolution_status_excludes_approval_and_preserves_exact_storage_values() {
        for (status, expected) in [
            (
                AttemptResolutionStatus::Declined,
                PaymentAttemptStatus::Declined,
            ),
            (
                AttemptResolutionStatus::Failed,
                PaymentAttemptStatus::Failed,
            ),
            (
                AttemptResolutionStatus::Unknown,
                PaymentAttemptStatus::Unknown,
            ),
            (
                AttemptResolutionStatus::ReviewRequired,
                PaymentAttemptStatus::ReviewRequired,
            ),
        ] {
            assert_eq!(status.payment_status(), expected);
            assert_eq!(status.as_str(), expected.as_str());
        }
    }
}
