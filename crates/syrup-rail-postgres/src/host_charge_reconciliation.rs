use chrono::{DateTime, Utc};
use sqlx::PgPool;
use syrup_rail::{
    BillingScopeId, GatewayAccountId, HostChargeTargetId, HostChargeTargetTransition,
    HostChargeTargetTransitionKind, HostChargeTargetTransitionOutcome, PaymentAttemptId,
    SubscriberId,
};
use uuid::Uuid;

use crate::{
    enrollment_application::set_application_timeouts,
    host_charge_application::HostChargeApplicationError, host_charges::HostChargeTargetStore,
    reconciliation::RECONCILIATION_PHASE_BATCH_SIZE,
};

pub(crate) const HOST_CHARGE_UNSUBMITTED_STALE_AFTER_SECONDS: i64 = 30 * 60;
const STALE_UNSUBMITTED_HOST_CHARGE_TEXT: &str =
    "Host charge was abandoned before gateway submission.";

/// Outcome of one bounded stale host-charge cleanup page.
///
/// A skipped candidate is left unchanged because its host target rejected the
/// release transition or because the canonical attempt changed concurrently.
/// The host target callback owns incident reporting for rejected transitions.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StaleHostChargeCleanupSummary {
    failed: u64,
    skipped: u64,
}

impl StaleHostChargeCleanupSummary {
    /// Attempts whose target release and local failure committed atomically.
    pub const fn failed(self) -> u64 {
        self.failed
    }

    /// Candidates left untouched after target rejection or revalidation.
    pub const fn skipped(self) -> u64 {
        self.skipped
    }
}

#[derive(Clone, Copy)]
struct StaleHostChargeCandidate {
    attempt_id: Uuid,
    billing_scope_id: Uuid,
    subscriber_id: Uuid,
    target_id: Uuid,
}

/// Fails one bounded account-scoped batch of stale local host charges.
///
/// Each candidate transitions its host-owned target to `PaymentFailed` before
/// locking and revalidating the canonical attempt. Both changes commit in one
/// transaction. A concurrent submission or terminal outcome rolls the host
/// transition back and counts as skipped. A target-level `StaleTarget` or
/// `Unchanged` outcome is likewise rolled back and skipped so unrelated targets
/// continue through the page. No gateway I/O is performed.
pub async fn fail_stale_unsubmitted_host_charges(
    pool: &PgPool,
    targets: &dyn HostChargeTargetStore,
    gateway_account_id: GatewayAccountId,
) -> Result<StaleHostChargeCleanupSummary, HostChargeApplicationError> {
    let candidates = sqlx::query_as::<_, (Uuid, Uuid, Uuid, Uuid)>(
        r#"
        SELECT id, billing_scope_id, subscriber_id, host_charge_target_id
        FROM billing_payment_attempts
        WHERE gateway_account_id = $1
            AND attempt_kind = 'host_charge'
            AND status IN ('pending', 'review_required')
            AND submitted_at IS NULL
            AND created_at <= clock_timestamp()
                - ($2::bigint * interval '1 second')
        ORDER BY created_at, id
        LIMIT $3
        "#,
    )
    .bind(gateway_account_id.as_uuid())
    .bind(HOST_CHARGE_UNSUBMITTED_STALE_AFTER_SECONDS)
    .bind(RECONCILIATION_PHASE_BATCH_SIZE)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(
        |(attempt_id, billing_scope_id, subscriber_id, target_id)| StaleHostChargeCandidate {
            attempt_id,
            billing_scope_id,
            subscriber_id,
            target_id,
        },
    )
    .collect::<Vec<_>>();

    let mut summary = StaleHostChargeCleanupSummary::default();
    for candidate in candidates {
        let mut transaction = pool.begin().await?;
        set_application_timeouts(&mut transaction).await?;
        let effective_at: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
            .fetch_one(&mut *transaction)
            .await?;
        let target_outcome = targets
            .apply_transition(
                &mut transaction,
                HostChargeTargetTransition::new(
                    BillingScopeId::new(candidate.billing_scope_id),
                    SubscriberId::new(candidate.subscriber_id),
                    PaymentAttemptId::new(candidate.attempt_id),
                    HostChargeTargetId::new(candidate.target_id),
                    HostChargeTargetTransitionKind::PaymentFailed,
                    effective_at,
                ),
            )
            .await?;
        if !matches!(
            target_outcome,
            HostChargeTargetTransitionOutcome::Applied
                | HostChargeTargetTransitionOutcome::ExactReplay
        ) {
            transaction.rollback().await?;
            summary.skipped += 1;
            continue;
        }

        let result = sqlx::query(
            r#"
            UPDATE billing_payment_attempts
            SET status = 'failed',
                gateway_response_text = $6,
                gateway_condition = COALESCE(gateway_condition, 'failed'),
                resolved_at = COALESCE(resolved_at, clock_timestamp()),
                updated_at = clock_timestamp()
            WHERE id = $1 AND billing_scope_id = $2 AND subscriber_id = $3
                AND host_charge_target_id = $4 AND gateway_account_id = $5
                AND attempt_kind = 'host_charge'
                AND status IN ('pending', 'review_required')
                AND submitted_at IS NULL
                AND created_at <= clock_timestamp()
                    - ($7::bigint * interval '1 second')
            "#,
        )
        .bind(candidate.attempt_id)
        .bind(candidate.billing_scope_id)
        .bind(candidate.subscriber_id)
        .bind(candidate.target_id)
        .bind(gateway_account_id.as_uuid())
        .bind(STALE_UNSUBMITTED_HOST_CHARGE_TEXT)
        .bind(HOST_CHARGE_UNSUBMITTED_STALE_AFTER_SECONDS)
        .execute(&mut *transaction)
        .await?;
        if result.rows_affected() == 0 {
            transaction.rollback().await?;
            summary.skipped += 1;
            continue;
        }
        transaction.commit().await?;
        summary.failed += 1;
    }
    Ok(summary)
}

#[cfg(test)]
mod tests;
