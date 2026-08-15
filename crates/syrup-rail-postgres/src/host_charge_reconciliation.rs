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
    host_charge_application::HostChargeApplicationError,
    host_charges::HostChargeTargetStore,
    reconciliation::{RECONCILIATION_CLAIM_RETRY_AFTER_SECONDS, RECONCILIATION_PHASE_BATCH_SIZE},
};

pub(crate) const HOST_CHARGE_UNSUBMITTED_STALE_AFTER_SECONDS: i64 = 30 * 60;
const STALE_UNSUBMITTED_HOST_CHARGE_TEXT: &str =
    "Host charge was abandoned before gateway submission.";

/// Outcome of one bounded stale host-charge cleanup page.
///
/// A skipped candidate keeps its financial and target state because its host
/// target rejected the release transition, the attempt changed concurrently,
/// or its row was contended. Its reconciliation claim timestamp advances so
/// unclaimed work can progress before it is retried. The host target callback
/// owns incident reporting for rejected transitions.
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

    /// Candidates left financially unchanged after rejection or revalidation.
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
/// Candidate selection first makes a durable scheduling claim with
/// `FOR UPDATE SKIP LOCKED`. Previously claimed rows sort behind untouched work,
/// so a bounded page cannot be monopolized by target-local skips. Each claimed
/// candidate then transitions its host-owned target to `PaymentFailed` before
/// locking and revalidating the canonical attempt. Both financial changes
/// commit in one transaction. A concurrent submission, terminal outcome,
/// target-level `StaleTarget` or `Unchanged`, or contended attempt row rolls the
/// target transition back and counts as skipped. No gateway I/O is performed.
pub async fn fail_stale_unsubmitted_host_charges(
    pool: &PgPool,
    targets: &dyn HostChargeTargetStore,
    gateway_account_id: GatewayAccountId,
) -> Result<StaleHostChargeCleanupSummary, HostChargeApplicationError> {
    let candidates = claim_stale_host_charge_candidates(pool, gateway_account_id).await?;

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
        .await;
        let result = match result {
            Ok(result) => result,
            Err(error) if is_lock_not_available(&error) => {
                transaction.rollback().await?;
                summary.skipped += 1;
                continue;
            }
            Err(error) => return Err(error.into()),
        };
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

async fn claim_stale_host_charge_candidates(
    pool: &PgPool,
    gateway_account_id: GatewayAccountId,
) -> Result<Vec<StaleHostChargeCandidate>, sqlx::Error> {
    let mut transaction = pool.begin().await?;
    set_application_timeouts(&mut transaction).await?;
    let candidates = sqlx::query_as::<_, (Uuid, Uuid, Uuid, Uuid)>(
        r#"
        WITH candidate_attempts AS MATERIALIZED (
            SELECT attempts.id AS attempt_id,
                attempts.billing_scope_id,
                attempts.subscriber_id,
                attempts.host_charge_target_id AS target_id,
                attempts.created_at,
                attempts.updated_at AS claimed_order_at
            FROM billing_payment_attempts AS attempts
            WHERE attempts.gateway_account_id = $1
                AND attempts.attempt_kind = 'host_charge'
                AND attempts.status IN ('pending', 'review_required')
                AND attempts.submitted_at IS NULL
                AND attempts.created_at <= clock_timestamp()
                    - ($2::bigint * interval '1 second')
                AND attempts.updated_at <= clock_timestamp()
                    - ($3::bigint * interval '1 second')
            ORDER BY attempts.updated_at, attempts.created_at, attempts.id
            LIMIT $4
            FOR UPDATE OF attempts SKIP LOCKED
        ), claimed_attempts AS (
            UPDATE billing_payment_attempts AS attempts
            SET updated_at = clock_timestamp()
            FROM candidate_attempts
            WHERE attempts.id = candidate_attempts.attempt_id
            RETURNING attempts.id
        )
        SELECT candidate_attempts.attempt_id,
            candidate_attempts.billing_scope_id,
            candidate_attempts.subscriber_id,
            candidate_attempts.target_id
        FROM candidate_attempts
        INNER JOIN claimed_attempts
            ON claimed_attempts.id = candidate_attempts.attempt_id
        ORDER BY candidate_attempts.claimed_order_at,
            candidate_attempts.created_at,
            candidate_attempts.attempt_id
        "#,
    )
    .bind(gateway_account_id.as_uuid())
    .bind(HOST_CHARGE_UNSUBMITTED_STALE_AFTER_SECONDS)
    .bind(RECONCILIATION_CLAIM_RETRY_AFTER_SECONDS)
    .bind(RECONCILIATION_PHASE_BATCH_SIZE)
    .fetch_all(&mut *transaction)
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
    transaction.commit().await?;
    Ok(candidates)
}

fn is_lock_not_available(error: &sqlx::Error) -> bool {
    matches!(
        error,
        sqlx::Error::Database(error) if error.code().as_deref() == Some("55P03")
    )
}

#[cfg(test)]
mod tests;
