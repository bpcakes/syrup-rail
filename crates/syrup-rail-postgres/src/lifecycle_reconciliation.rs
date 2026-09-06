use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, Row, Transaction};
use syrup_rail::{
    BillingScopeId, CumulativeRefundCents, GatewayLifecycleAccount, GatewayLifecycleCursorKey,
    GatewayLifecycleEvidence, GatewayLifecycleQuarantine, GatewayLifecycleQuarantineReason,
    GatewayLifecycleState, GatewayOrderId, GatewayTransactionId, GatewayTransactionReport,
    HostChargeTargetId, HostChargeTargetTransition, HostChargeTargetTransitionKind,
    PaymentAttemptId, PaymentAttemptKind, SubscriberId,
};
use thiserror::Error;
use uuid::Uuid;

const ROW_LOCK_TIMEOUT: &str = "250ms";
const OPERATION_TIMEOUT: &str = "5s";
const PENDING_RETENTION_SECONDS: i64 = 7 * 24 * 60 * 60;
const PENDING_CLEANUP_BATCH_SIZE: i64 = 500;
const STAGED_APPLICATION_BATCH_SIZE: i64 = 100;
const INVALID_STORED_STATE: &str = "canonical gateway lifecycle state is invalid";

use crate::{HostChargeTargetError, HostChargeTargetStore};

#[derive(Debug, Error)]
pub enum GatewayLifecycleReconciliationError {
    #[error("gateway lifecycle storage operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("gateway lifecycle account was not found")]
    AccountNotFound,
    #[error("{0}")]
    InvalidState(&'static str),
    #[error(transparent)]
    HostChargeTarget(#[from] HostChargeTargetError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GatewayLifecycleApplyOutcome {
    Applied,
    AlreadySuperseded,
    InvalidRefundEconomics,
    ConflictingLifecycleEvidence,
    HostTargetTransitionSkipped,
    StagedAmbiguous,
    StagedNoMatch,
}

impl GatewayLifecycleApplyOutcome {
    /// Returns one when this outcome applied evidence to a canonical attempt.
    pub const fn applied_count(self) -> u64 {
        if matches!(self, Self::Applied) { 1 } else { 0 }
    }

    /// Returns one for a no-match or ambiguous-match staging outcome, including
    /// redelivery of an already pending row. This classifies the outcome; it does
    /// not count newly inserted pending rows as
    /// [`GatewayLifecycleReconciliationSummary::staged`] does. Host-target
    /// refusals are classified separately by [`Self::skipped_count`].
    pub const fn staged_count(self) -> u64 {
        if matches!(self, Self::StagedAmbiguous | Self::StagedNoMatch) {
            1
        } else {
            0
        }
    }

    pub const fn skipped_count(self) -> u64 {
        if matches!(self, Self::HostTargetTransitionSkipped) {
            1
        } else {
            0
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct EvidenceApplication {
    outcome: GatewayLifecycleApplyOutcome,
    newly_staged: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GatewayLifecycleSummaryOutcome {
    Evidence(GatewayLifecycleApplyOutcome),
    ExplicitQuarantine,
}

impl From<GatewayLifecycleApplyOutcome> for GatewayLifecycleSummaryOutcome {
    fn from(outcome: GatewayLifecycleApplyOutcome) -> Self {
        Self::Evidence(outcome)
    }
}

/// Per-call accounting for lifecycle report reconciliation.
///
/// Applied evidence increments `applied`; invalid or conflicting evidence and
/// explicit quarantine reports increment `quarantined`. Only newly inserted
/// pending rows increment `staged`. Redelivery of an existing pending row does
/// not increment `staged`; superseded evidence adds no counts. A refused
/// host-target transition increments `skipped` and also `staged` if it inserts
/// a pending row. During staged draining, `cleaned` counts expired pending
/// rows removed independently of evidence outcomes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GatewayLifecycleReconciliationSummary {
    applied: u64,
    staged: u64,
    quarantined: u64,
    cleaned: u64,
    skipped: u64,
}

impl GatewayLifecycleReconciliationSummary {
    /// Evidence outcomes applied to canonical payment attempts.
    pub const fn applied(self) -> u64 {
        self.applied
    }

    /// Newly inserted pending evidence rows, including refused host-target
    /// transitions. Redelivery of an existing pending row adds nothing.
    pub const fn staged(self) -> u64 {
        self.staged
    }

    /// Explicit reports and evidence outcomes that wrote or reopened quarantine.
    pub const fn quarantined(self) -> u64 {
        self.quarantined
    }

    /// Expired pending evidence rows removed during a staged-drain call.
    pub const fn cleaned(self) -> u64 {
        self.cleaned
    }

    /// Host-target transitions that were refused and left evidence staged.
    pub const fn skipped(self) -> u64 {
        self.skipped
    }

    fn record_outcome(&mut self, outcome: GatewayLifecycleSummaryOutcome) {
        match outcome {
            GatewayLifecycleSummaryOutcome::Evidence(GatewayLifecycleApplyOutcome::Applied) => {
                self.applied += 1;
            }
            GatewayLifecycleSummaryOutcome::Evidence(
                GatewayLifecycleApplyOutcome::StagedAmbiguous
                | GatewayLifecycleApplyOutcome::StagedNoMatch,
            ) => {
                self.staged += 1;
            }
            GatewayLifecycleSummaryOutcome::Evidence(
                GatewayLifecycleApplyOutcome::InvalidRefundEconomics
                | GatewayLifecycleApplyOutcome::ConflictingLifecycleEvidence,
            )
            | GatewayLifecycleSummaryOutcome::ExplicitQuarantine => {
                self.quarantined += 1;
            }
            GatewayLifecycleSummaryOutcome::Evidence(
                GatewayLifecycleApplyOutcome::AlreadySuperseded,
            ) => {}
            GatewayLifecycleSummaryOutcome::Evidence(
                GatewayLifecycleApplyOutcome::HostTargetTransitionSkipped,
            ) => {
                self.skipped += 1;
            }
        }
    }

    fn record_application(&mut self, application: EvidenceApplication) {
        if matches!(
            application.outcome,
            GatewayLifecycleApplyOutcome::StagedAmbiguous
                | GatewayLifecycleApplyOutcome::StagedNoMatch
        ) {
            self.staged += u64::from(application.newly_staged);
        } else {
            self.record_outcome(application.outcome.into());
            if application.outcome == GatewayLifecycleApplyOutcome::HostTargetTransitionSkipped {
                self.staged += u64::from(application.newly_staged);
            }
        }
    }
}

#[derive(Clone, Debug)]
struct StoredEvidence {
    transaction_id: Option<String>,
    order_id: Option<String>,
    state: GatewayLifecycleState,
    condition: Option<String>,
    action: Option<String>,
    effective_at: Option<DateTime<Utc>>,
}

impl From<&GatewayLifecycleEvidence> for StoredEvidence {
    fn from(evidence: &GatewayLifecycleEvidence) -> Self {
        Self {
            transaction_id: evidence
                .transaction_id()
                .map(|identifier| identifier.expose().to_owned()),
            order_id: evidence
                .order_id()
                .map(|identifier| identifier.expose().to_owned()),
            state: evidence.state().clone(),
            condition: evidence
                .condition()
                .map(|diagnostic| diagnostic.expose().to_owned()),
            action: evidence
                .action()
                .map(|diagnostic| diagnostic.expose().to_owned()),
            effective_at: evidence
                .effective_at()
                .copied()
                .map(postgres_timestamp_precision),
        }
    }
}

fn postgres_timestamp_precision(value: DateTime<Utc>) -> DateTime<Utc> {
    value - chrono::Duration::nanoseconds(i64::from(value.timestamp_subsec_nanos() % 1_000))
}

#[derive(Debug)]
struct AttemptCandidate {
    id: Uuid,
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    kind: PaymentAttemptKind,
    host_charge_target_id: Option<HostChargeTargetId>,
    amount_cents: i32,
    current_state: GatewayLifecycleState,
    current_lifecycle_at: Option<DateTime<Utc>>,
    current_refunded_amount_cents: i32,
    matched_transaction_id: bool,
    matched_order_id: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LifecycleTransition {
    Apply { refunded_amount_cents: i32 },
    AlreadySuperseded,
    InvalidRefundEconomics,
    ConflictingLifecycleEvidence,
}

pub async fn gateway_lifecycle_reconciliation_start(
    pool: &PgPool,
    account: &GatewayLifecycleAccount,
    cursor_key: &GatewayLifecycleCursorKey,
) -> Result<Option<DateTime<Utc>>, GatewayLifecycleReconciliationError> {
    let mut transaction = pool.begin().await?;
    set_timeouts(&mut transaction).await?;
    ensure_account(&mut transaction, account).await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!(
            "billing_reconciliation_cursor:{}:{}:{}",
            account.gateway_account_id(),
            account.provider_key(),
            cursor_key
        ))
        .execute(&mut *transaction)
        .await?;
    let row = sqlx::query(
        r#"
        WITH cursor_value AS (
            SELECT (
                SELECT cursors.last_successful_end_at
                FROM billing_reconciliation_cursors cursors
                WHERE cursors.billing_scope_id = $1
                    AND cursors.gateway_account_id = $2
                    AND cursors.provider_key = $3
                    AND cursors.cursor_key = $4
                FOR UPDATE
            ) AS cursor_at
        )
        SELECT cursor_at,
            CASE WHEN cursor_at IS NULL THEN (
                SELECT MIN(COALESCE(attempts.resolved_at, attempts.updated_at))
                FROM billing_payment_attempts attempts
                WHERE attempts.billing_scope_id = $1
                    AND attempts.gateway_account_id = $2
                    AND attempts.status = 'approved'
                    AND (
                        public.billing_canonical_gateway_transaction_id(
                            attempts.gateway_transaction_id
                        ) IS NOT NULL
                        OR attempts.gateway_order_id IS NOT NULL
                    )
            ) END AS first_approved_at
        FROM cursor_value
        "#,
    )
    .bind(account.billing_scope_id().as_uuid())
    .bind(account.gateway_account_id().as_uuid())
    .bind(account.provider_key().as_str())
    .bind(cursor_key.as_str())
    .fetch_one(&mut *transaction)
    .await?;
    let cursor_at: Option<DateTime<Utc>> = row.try_get("cursor_at")?;
    let first_approved_at: Option<DateTime<Utc>> = row.try_get("first_approved_at")?;
    transaction.commit().await?;
    Ok(cursor_at.or(first_approved_at))
}

pub async fn save_gateway_lifecycle_reconciliation_cursor(
    pool: &PgPool,
    account: &GatewayLifecycleAccount,
    cursor_key: &GatewayLifecycleCursorKey,
    last_successful_end_at: DateTime<Utc>,
) -> Result<(), GatewayLifecycleReconciliationError> {
    let mut transaction = pool.begin().await?;
    set_timeouts(&mut transaction).await?;
    ensure_account(&mut transaction, account).await?;
    sqlx::query(
        r#"
        INSERT INTO billing_reconciliation_cursors (
            billing_scope_id,
            gateway_account_id,
            provider_key,
            cursor_key,
            last_successful_end_at
        )
        VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT (gateway_account_id, provider_key, cursor_key) DO UPDATE
        SET last_successful_end_at = GREATEST(
                billing_reconciliation_cursors.last_successful_end_at,
                EXCLUDED.last_successful_end_at
            ),
            updated_at = now()
        "#,
    )
    .bind(account.billing_scope_id().as_uuid())
    .bind(account.gateway_account_id().as_uuid())
    .bind(account.provider_key().as_str())
    .bind(cursor_key.as_str())
    .bind(last_successful_end_at)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(())
}

pub async fn reconcile_gateway_transaction_reports(
    pool: &PgPool,
    host_charge_targets: &dyn HostChargeTargetStore,
    account: &GatewayLifecycleAccount,
    reports: Vec<GatewayTransactionReport>,
) -> Result<GatewayLifecycleReconciliationSummary, GatewayLifecycleReconciliationError> {
    let mut summary = GatewayLifecycleReconciliationSummary::default();
    for report in reports {
        match report {
            GatewayTransactionReport::Ignore => {}
            GatewayTransactionReport::Quarantine(quarantine) => {
                record_quarantine(pool, account, &quarantine).await?;
                summary.record_outcome(GatewayLifecycleSummaryOutcome::ExplicitQuarantine);
            }
            GatewayTransactionReport::Evidence(evidence) => {
                let application = apply_or_stage_evidence(
                    pool,
                    host_charge_targets,
                    account,
                    StoredEvidence::from(&evidence),
                    None,
                )
                .await?;
                summary.record_application(application);
            }
        }
    }
    Ok(summary)
}

/// Applies one lifecycle observation.
///
/// If the matched host target refuses a full reversal, the canonical attempt
/// update is rolled back and this first-seen evidence is durably staged before
/// `HostTargetTransitionSkipped` is returned.
pub async fn apply_gateway_lifecycle_evidence(
    pool: &PgPool,
    host_charge_targets: &dyn HostChargeTargetStore,
    account: &GatewayLifecycleAccount,
    evidence: &GatewayLifecycleEvidence,
) -> Result<GatewayLifecycleApplyOutcome, GatewayLifecycleReconciliationError> {
    Ok(apply_or_stage_evidence(
        pool,
        host_charge_targets,
        account,
        StoredEvidence::from(evidence),
        None,
    )
    .await?
    .outcome)
}

pub async fn stage_gateway_lifecycle_evidence(
    pool: &PgPool,
    account: &GatewayLifecycleAccount,
    evidence: &GatewayLifecycleEvidence,
) -> Result<(), GatewayLifecycleReconciliationError> {
    let mut transaction = pool.begin().await?;
    set_timeouts(&mut transaction).await?;
    ensure_account(&mut transaction, account).await?;
    stage_evidence(&mut transaction, account, &StoredEvidence::from(evidence)).await?;
    transaction.commit().await?;
    Ok(())
}

pub async fn record_gateway_lifecycle_quarantines(
    pool: &PgPool,
    account: &GatewayLifecycleAccount,
    quarantines: &[GatewayLifecycleQuarantine],
) -> Result<(), GatewayLifecycleReconciliationError> {
    for quarantine in quarantines {
        record_quarantine(pool, account, quarantine).await?;
    }
    Ok(())
}

pub async fn apply_staged_gateway_lifecycle_evidence(
    pool: &PgPool,
    host_charge_targets: &dyn HostChargeTargetStore,
    account: &GatewayLifecycleAccount,
) -> Result<GatewayLifecycleReconciliationSummary, GatewayLifecycleReconciliationError> {
    let mut summary = GatewayLifecycleReconciliationSummary::default();
    summary.cleaned += cleanup_pending(pool, account).await?;
    // Preserve the established two bounded cleanup passes and their public
    // cleaned count while expiry becomes the only pending-evidence clock.
    summary.cleaned += cleanup_pending(pool, account).await?;

    for (pending_id, evidence) in actionable_pending(pool, account).await? {
        let application = apply_or_stage_evidence(
            pool,
            host_charge_targets,
            account,
            evidence,
            Some(pending_id),
        )
        .await?;
        summary.record_application(application);
    }
    Ok(summary)
}

async fn apply_or_stage_evidence(
    pool: &PgPool,
    host_charge_targets: &dyn HostChargeTargetStore,
    account: &GatewayLifecycleAccount,
    evidence: StoredEvidence,
    pending_id: Option<Uuid>,
) -> Result<EvidenceApplication, GatewayLifecycleReconciliationError> {
    let mut transaction = pool.begin().await?;
    set_timeouts(&mut transaction).await?;
    ensure_account(&mut transaction, account).await?;
    let mut candidates = attempt_candidates(&mut transaction, account, &evidence).await?;
    let candidate_count = candidates.len();
    let selected_index = candidates
        .iter()
        .position(|candidate| candidate.matched_transaction_id)
        .or_else(|| (candidate_count == 1 && candidates[0].matched_order_id).then_some(0));
    let Some(selected_index) = selected_index else {
        let newly_staged = if pending_id.is_none() {
            stage_evidence(&mut transaction, account, &evidence).await?
        } else {
            false
        };
        transaction.commit().await?;
        return Ok(EvidenceApplication {
            outcome: if candidate_count == 0 {
                GatewayLifecycleApplyOutcome::StagedNoMatch
            } else {
                GatewayLifecycleApplyOutcome::StagedAmbiguous
            },
            newly_staged,
        });
    };
    let candidate = candidates.swap_remove(selected_index);
    let transition = lifecycle_transition(
        &candidate.current_state,
        candidate.current_refunded_amount_cents,
        candidate.current_lifecycle_at,
        &evidence.state,
        evidence.effective_at,
        candidate.amount_cents,
    )?;
    let reconciled_at: DateTime<Utc> = sqlx::query_scalar("SELECT now()")
        .fetch_one(&mut *transaction)
        .await?;
    let outcome = match transition {
        LifecycleTransition::Apply {
            refunded_amount_cents,
        } => {
            let result = sqlx::query(
                r#"
                UPDATE billing_payment_attempts
                SET gateway_condition = COALESCE($2, gateway_condition),
                    gateway_lifecycle_status = $3,
                    gateway_lifecycle_action = $4,
                    gateway_lifecycle_at = GREATEST(gateway_lifecycle_at, $5),
                    refunded_amount_cents = $6,
                    gateway_lifecycle_reconciled_at = $7,
                    updated_at = $7
                WHERE id = $1
                    AND billing_scope_id = $8
                    AND gateway_account_id = $9
                "#,
            )
            .bind(candidate.id)
            .bind(evidence.condition.as_deref())
            .bind(lifecycle_status(&evidence.state))
            .bind(evidence.action.as_deref())
            .bind(evidence.effective_at)
            .bind(refunded_amount_cents)
            .bind(reconciled_at)
            .bind(account.billing_scope_id().as_uuid())
            .bind(account.gateway_account_id().as_uuid())
            .execute(&mut *transaction)
            .await?;
            if result.rows_affected() != 1 {
                return Err(GatewayLifecycleReconciliationError::InvalidState(
                    INVALID_STORED_STATE,
                ));
            }
            let target_transition_applied = if let Some(kind) = evidence.state.full_reversal_kind()
                && candidate.kind == PaymentAttemptKind::HostCharge
            {
                let target_id = candidate.host_charge_target_id.ok_or(
                    GatewayLifecycleReconciliationError::InvalidState(INVALID_STORED_STATE),
                )?;
                let reversed_at =
                    latest_time(candidate.current_lifecycle_at, evidence.effective_at)
                        .unwrap_or(reconciled_at);
                let target_outcome = host_charge_targets
                    .apply_transition(
                        &mut transaction,
                        HostChargeTargetTransition::new(
                            candidate.billing_scope_id,
                            candidate.subscriber_id,
                            PaymentAttemptId::new(candidate.id),
                            target_id,
                            HostChargeTargetTransitionKind::Reversed { kind },
                            reversed_at,
                        ),
                    )
                    .await?;
                if !target_outcome.is_applied() {
                    tracing::warn!(
                        target: "syrup_rail::gateway_lifecycle_reconciliation",
                        billing_scope_id = %candidate.billing_scope_id.as_uuid(),
                        subscriber_id = %candidate.subscriber_id.as_uuid(),
                        attempt_id = %candidate.id,
                        target_id = %target_id.as_uuid(),
                        ?target_outcome,
                        "host target refused a full-reversal transition; leaving lifecycle evidence unapplied"
                    );
                }
                target_outcome.is_applied()
            } else {
                true
            };
            if target_transition_applied {
                GatewayLifecycleApplyOutcome::Applied
            } else {
                GatewayLifecycleApplyOutcome::HostTargetTransitionSkipped
            }
        }
        LifecycleTransition::AlreadySuperseded => GatewayLifecycleApplyOutcome::AlreadySuperseded,
        LifecycleTransition::InvalidRefundEconomics => {
            GatewayLifecycleApplyOutcome::InvalidRefundEconomics
        }
        LifecycleTransition::ConflictingLifecycleEvidence => {
            GatewayLifecycleApplyOutcome::ConflictingLifecycleEvidence
        }
    };

    match outcome {
        GatewayLifecycleApplyOutcome::Applied | GatewayLifecycleApplyOutcome::AlreadySuperseded => {
            resolve_matching_quarantines(&mut transaction, account, &evidence).await?;
        }
        GatewayLifecycleApplyOutcome::InvalidRefundEconomics
        | GatewayLifecycleApplyOutcome::ConflictingLifecycleEvidence => {
            record_quarantine_parts(
                &mut transaction,
                account,
                evidence.transaction_id.as_deref(),
                evidence.order_id.as_deref(),
                GatewayLifecycleQuarantineReason::InvalidRefundEconomics,
            )
            .await?;
        }
        GatewayLifecycleApplyOutcome::HostTargetTransitionSkipped => {
            transaction.rollback().await?;
            // A provider page may advance its cursor after this batch succeeds.
            // Preserve first-seen evidence before allowing later reports to
            // continue; an already-pending row was restored by the rollback.
            let newly_staged = if pending_id.is_none() {
                let mut staging = pool.begin().await?;
                set_timeouts(&mut staging).await?;
                ensure_account(&mut staging, account).await?;
                let newly_staged = stage_evidence(&mut staging, account, &evidence).await?;
                staging.commit().await?;
                newly_staged
            } else {
                false
            };
            return Ok(EvidenceApplication {
                outcome,
                newly_staged,
            });
        }
        GatewayLifecycleApplyOutcome::StagedAmbiguous
        | GatewayLifecycleApplyOutcome::StagedNoMatch => unreachable!("handled before selection"),
    }
    if matches!(
        outcome,
        GatewayLifecycleApplyOutcome::Applied
            | GatewayLifecycleApplyOutcome::AlreadySuperseded
            | GatewayLifecycleApplyOutcome::InvalidRefundEconomics
            | GatewayLifecycleApplyOutcome::ConflictingLifecycleEvidence
    ) && let Some(pending_id) = pending_id
    {
        delete_pending(&mut transaction, account, pending_id).await?;
    }
    transaction.commit().await?;
    Ok(EvidenceApplication {
        outcome,
        newly_staged: false,
    })
}

include!("lifecycle_reconciliation/application_support.rs");

#[cfg(test)]
mod tests {
    include!("lifecycle_reconciliation/tests/core.rs");
    include!("lifecycle_reconciliation/tests/persistence.rs");
}
