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
const PENDING_MAX_CHECKS: i32 = 24;
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
    StagedAmbiguous,
    StagedNoMatch,
}

impl GatewayLifecycleApplyOutcome {
    pub const fn applied_count(self) -> u64 {
        if matches!(self, Self::Applied) { 1 } else { 0 }
    }

    pub const fn staged_count(self) -> u64 {
        if matches!(self, Self::StagedAmbiguous | Self::StagedNoMatch) {
            1
        } else {
            0
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GatewayLifecycleReconciliationSummary {
    applied: u64,
    staged: u64,
    quarantined: u64,
    cleaned: u64,
}

impl GatewayLifecycleReconciliationSummary {
    pub const fn applied(self) -> u64 {
        self.applied
    }

    pub const fn staged(self) -> u64 {
        self.staged
    }

    pub const fn quarantined(self) -> u64 {
        self.quarantined
    }

    pub const fn cleaned(self) -> u64 {
        self.cleaned
    }

    fn record_outcome(&mut self, outcome: GatewayLifecycleApplyOutcome) {
        self.applied += outcome.applied_count();
        self.staged += outcome.staged_count();
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
            effective_at: evidence.effective_at().copied(),
        }
    }
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
                summary.quarantined += 1;
            }
            GatewayTransactionReport::Evidence(evidence) => {
                let outcome = apply_or_stage_evidence(
                    pool,
                    host_charge_targets,
                    account,
                    StoredEvidence::from(&evidence),
                    None,
                )
                .await?;
                summary.record_outcome(outcome);
            }
        }
    }
    Ok(summary)
}

pub async fn apply_gateway_lifecycle_evidence(
    pool: &PgPool,
    host_charge_targets: &dyn HostChargeTargetStore,
    account: &GatewayLifecycleAccount,
    evidence: &GatewayLifecycleEvidence,
) -> Result<GatewayLifecycleApplyOutcome, GatewayLifecycleReconciliationError> {
    apply_or_stage_evidence(
        pool,
        host_charge_targets,
        account,
        StoredEvidence::from(evidence),
        None,
    )
    .await
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
    record_unactionable_checks(pool, account).await?;
    summary.cleaned += cleanup_pending(pool, account).await?;

    for (pending_id, evidence) in actionable_pending(pool, account).await? {
        let outcome = apply_or_stage_evidence(
            pool,
            host_charge_targets,
            account,
            evidence,
            Some(pending_id),
        )
        .await?;
        summary.applied += outcome.applied_count();
    }
    Ok(summary)
}

async fn apply_or_stage_evidence(
    pool: &PgPool,
    host_charge_targets: &dyn HostChargeTargetStore,
    account: &GatewayLifecycleAccount,
    evidence: StoredEvidence,
    pending_id: Option<Uuid>,
) -> Result<GatewayLifecycleApplyOutcome, GatewayLifecycleReconciliationError> {
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
        if pending_id.is_none() {
            stage_evidence(&mut transaction, account, &evidence).await?;
        }
        transaction.commit().await?;
        return Ok(if candidate_count == 0 {
            GatewayLifecycleApplyOutcome::StagedNoMatch
        } else {
            GatewayLifecycleApplyOutcome::StagedAmbiguous
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
            if let Some(kind) = evidence.state.full_reversal_kind()
                && candidate.kind == PaymentAttemptKind::HostCharge
            {
                let target_id = candidate.host_charge_target_id.ok_or(
                    GatewayLifecycleReconciliationError::InvalidState(INVALID_STORED_STATE),
                )?;
                let reversed_at =
                    latest_time(candidate.current_lifecycle_at, evidence.effective_at)
                        .unwrap_or(reconciled_at);
                host_charge_targets
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
            }
            GatewayLifecycleApplyOutcome::Applied
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
    Ok(outcome)
}

async fn attempt_candidates(
    transaction: &mut Transaction<'_, Postgres>,
    account: &GatewayLifecycleAccount,
    evidence: &StoredEvidence,
) -> Result<Vec<AttemptCandidate>, GatewayLifecycleReconciliationError> {
    let rows = sqlx::query(
        r#"
        SELECT attempts.id,
            attempts.billing_scope_id,
            attempts.subscriber_id,
            attempts.attempt_kind,
            attempts.host_charge_target_id,
            attempts.amount_cents,
            attempts.gateway_lifecycle_status,
            attempts.gateway_lifecycle_at,
            attempts.refunded_amount_cents,
            COALESCE((
                $1::text IS NOT NULL
                AND public.billing_canonical_gateway_transaction_id(
                    attempts.gateway_transaction_id
                ) = $1
            ), false) AS matched_transaction_id,
            COALESCE((
                $2::text IS NOT NULL
                AND (
                    $1::text IS NULL
                    OR public.billing_canonical_gateway_transaction_id(
                        attempts.gateway_transaction_id
                    ) IS NULL
                )
                AND attempts.gateway_order_id = $2
            ), false) AS matched_order_id
        FROM billing_payment_attempts attempts
        WHERE attempts.billing_scope_id = $3
            AND attempts.gateway_account_id = $4
            AND attempts.status = 'approved'
            AND (
                (
                    $1::text IS NOT NULL
                    AND public.billing_canonical_gateway_transaction_id(
                        attempts.gateway_transaction_id
                    ) = $1
                )
                OR (
                    $2::text IS NOT NULL
                    AND (
                        $1::text IS NULL
                        OR public.billing_canonical_gateway_transaction_id(
                            attempts.gateway_transaction_id
                        ) IS NULL
                    )
                    AND attempts.gateway_order_id = $2
                )
            )
        ORDER BY attempts.created_at, attempts.id
        FOR UPDATE OF attempts
        "#,
    )
    .bind(evidence.transaction_id.as_deref())
    .bind(evidence.order_id.as_deref())
    .bind(account.billing_scope_id().as_uuid())
    .bind(account.gateway_account_id().as_uuid())
    .fetch_all(&mut **transaction)
    .await?;
    rows.into_iter()
        .map(|row| {
            let current_refunded_amount_cents = row.try_get("refunded_amount_cents")?;
            let current_state = lifecycle_state_from_parts(
                row.try_get::<String, _>("gateway_lifecycle_status")?
                    .as_str(),
                Some(current_refunded_amount_cents),
                true,
            )?;
            Ok(AttemptCandidate {
                id: row.try_get("id")?,
                billing_scope_id: BillingScopeId::new(row.try_get("billing_scope_id")?),
                subscriber_id: SubscriberId::new(row.try_get("subscriber_id")?),
                kind: row
                    .try_get::<String, _>("attempt_kind")?
                    .parse()
                    .map_err(|_| {
                        GatewayLifecycleReconciliationError::InvalidState(INVALID_STORED_STATE)
                    })?,
                host_charge_target_id: row
                    .try_get::<Option<Uuid>, _>("host_charge_target_id")?
                    .map(HostChargeTargetId::new),
                amount_cents: row.try_get("amount_cents")?,
                current_state,
                current_lifecycle_at: row.try_get("gateway_lifecycle_at")?,
                current_refunded_amount_cents,
                matched_transaction_id: row.try_get("matched_transaction_id")?,
                matched_order_id: row.try_get("matched_order_id")?,
            })
        })
        .collect()
}

fn lifecycle_transition(
    current_state: &GatewayLifecycleState,
    current_refunded_amount_cents: i32,
    current_lifecycle_at: Option<DateTime<Utc>>,
    incoming_state: &GatewayLifecycleState,
    incoming_lifecycle_at: Option<DateTime<Utc>>,
    captured_amount_cents: i32,
) -> Result<LifecycleTransition, GatewayLifecycleReconciliationError> {
    if !stored_amount_is_valid(
        current_state,
        current_refunded_amount_cents,
        captured_amount_cents,
    ) {
        return Err(GatewayLifecycleReconciliationError::InvalidState(
            INVALID_STORED_STATE,
        ));
    }
    let incoming_refunded_amount_cents = lifecycle_refunded_amount(incoming_state);
    if !incoming_amount_is_valid(incoming_state, captured_amount_cents) {
        return Ok(LifecycleTransition::InvalidRefundEconomics);
    }
    let incoming_rank = lifecycle_rank(incoming_state);
    let current_rank = lifecycle_rank(current_state);
    let should_advance = incoming_rank > current_rank
        || (incoming_rank == current_rank
            && (current_lifecycle_at.is_none()
                || incoming_lifecycle_at
                    .zip(current_lifecycle_at)
                    .is_some_and(|(incoming, current)| incoming > current)
                || incoming_refunded_amount_cents
                    .is_some_and(|incoming| incoming > current_refunded_amount_cents)));
    if !should_advance {
        return Ok(LifecycleTransition::AlreadySuperseded);
    }
    let next_refunded_amount_cents = current_refunded_amount_cents
        .max(incoming_refunded_amount_cents.unwrap_or(current_refunded_amount_cents));
    if !stored_amount_is_valid(
        incoming_state,
        next_refunded_amount_cents,
        captured_amount_cents,
    ) {
        return Ok(LifecycleTransition::ConflictingLifecycleEvidence);
    }
    Ok(LifecycleTransition::Apply {
        refunded_amount_cents: next_refunded_amount_cents,
    })
}

const fn lifecycle_rank(state: &GatewayLifecycleState) -> i16 {
    match state {
        GatewayLifecycleState::Unknown => 0,
        GatewayLifecycleState::PendingSettlement => 1,
        GatewayLifecycleState::Settled { .. } => 2,
        GatewayLifecycleState::Voided => 3,
        GatewayLifecycleState::Refunded { .. } => 4,
        GatewayLifecycleState::Chargeback { .. } => 5,
    }
}

const fn lifecycle_status(state: &GatewayLifecycleState) -> &'static str {
    match state {
        GatewayLifecycleState::Unknown => "unknown",
        GatewayLifecycleState::PendingSettlement => "pending_settlement",
        GatewayLifecycleState::Settled { .. } => "settled",
        GatewayLifecycleState::Voided => "voided",
        GatewayLifecycleState::Refunded { .. } => "refunded",
        GatewayLifecycleState::Chargeback { .. } => "chargeback",
    }
}

const fn lifecycle_refunded_amount(state: &GatewayLifecycleState) -> Option<i32> {
    match state {
        GatewayLifecycleState::Settled {
            cumulative_refunded_cents,
        }
        | GatewayLifecycleState::Chargeback {
            cumulative_refunded_cents,
        } => match cumulative_refunded_cents {
            Some(amount) => Some(amount.get()),
            None => None,
        },
        GatewayLifecycleState::Refunded {
            cumulative_refunded_cents,
        } => Some(cumulative_refunded_cents.get()),
        GatewayLifecycleState::Unknown
        | GatewayLifecycleState::PendingSettlement
        | GatewayLifecycleState::Voided => None,
    }
}

fn incoming_amount_is_valid(state: &GatewayLifecycleState, captured: i32) -> bool {
    match state {
        GatewayLifecycleState::Unknown
        | GatewayLifecycleState::PendingSettlement
        | GatewayLifecycleState::Voided => true,
        GatewayLifecycleState::Settled {
            cumulative_refunded_cents,
        } => captured > 0 && cumulative_refunded_cents.is_none_or(|amount| amount.get() < captured),
        GatewayLifecycleState::Refunded {
            cumulative_refunded_cents,
        } => captured > 0 && cumulative_refunded_cents.get() == captured,
        GatewayLifecycleState::Chargeback {
            cumulative_refunded_cents,
        } => {
            captured > 0 && cumulative_refunded_cents.is_none_or(|amount| amount.get() <= captured)
        }
    }
}

fn stored_amount_is_valid(state: &GatewayLifecycleState, refunded: i32, captured: i32) -> bool {
    match state {
        GatewayLifecycleState::Unknown
        | GatewayLifecycleState::PendingSettlement
        | GatewayLifecycleState::Voided => refunded == 0,
        GatewayLifecycleState::Settled { .. } => {
            captured > 0 && refunded >= 0 && refunded < captured
        }
        GatewayLifecycleState::Refunded { .. } => captured > 0 && refunded == captured,
        GatewayLifecycleState::Chargeback { .. } => {
            captured > 0 && refunded >= 0 && refunded <= captured
        }
    }
}

fn lifecycle_state_from_parts(
    status: &str,
    refunded: Option<i32>,
    stored_attempt: bool,
) -> Result<GatewayLifecycleState, GatewayLifecycleReconciliationError> {
    let state = match (status, refunded) {
        ("unknown", None | Some(0)) => GatewayLifecycleState::Unknown,
        ("pending_settlement", None | Some(0)) => GatewayLifecycleState::PendingSettlement,
        ("voided", None | Some(0)) => GatewayLifecycleState::Voided,
        ("settled", None | Some(0)) => GatewayLifecycleState::Settled {
            cumulative_refunded_cents: None,
        },
        ("settled", Some(value)) if value > 0 => GatewayLifecycleState::Settled {
            cumulative_refunded_cents: Some(CumulativeRefundCents::new(value).map_err(|_| {
                GatewayLifecycleReconciliationError::InvalidState(INVALID_STORED_STATE)
            })?),
        },
        ("refunded", Some(value)) if value > 0 => GatewayLifecycleState::Refunded {
            cumulative_refunded_cents: CumulativeRefundCents::new(value).map_err(|_| {
                GatewayLifecycleReconciliationError::InvalidState(INVALID_STORED_STATE)
            })?,
        },
        ("chargeback", None | Some(0)) => GatewayLifecycleState::Chargeback {
            cumulative_refunded_cents: None,
        },
        ("chargeback", Some(value)) if value > 0 => GatewayLifecycleState::Chargeback {
            cumulative_refunded_cents: Some(CumulativeRefundCents::new(value).map_err(|_| {
                GatewayLifecycleReconciliationError::InvalidState(INVALID_STORED_STATE)
            })?),
        },
        _ => {
            return Err(GatewayLifecycleReconciliationError::InvalidState(
                INVALID_STORED_STATE,
            ));
        }
    };
    if stored_attempt && refunded.is_none() {
        return Err(GatewayLifecycleReconciliationError::InvalidState(
            INVALID_STORED_STATE,
        ));
    }
    Ok(state)
}

async fn stage_evidence(
    transaction: &mut Transaction<'_, Postgres>,
    account: &GatewayLifecycleAccount,
    evidence: &StoredEvidence,
) -> Result<(), GatewayLifecycleReconciliationError> {
    sqlx::query(
        r#"
        INSERT INTO billing_gateway_lifecycle_pending_updates (
            billing_scope_id,
            gateway_account_id,
            gateway_transaction_id,
            gateway_order_id,
            gateway_condition,
            gateway_lifecycle_status,
            gateway_lifecycle_action,
            gateway_lifecycle_at,
            refunded_amount_cents,
            expires_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9,
            now() + ($10::bigint * interval '1 second'))
        ON CONFLICT (
            gateway_account_id,
            COALESCE(gateway_transaction_id, ''),
            COALESCE(gateway_order_id, ''),
            COALESCE(gateway_condition, ''),
            gateway_lifecycle_status,
            COALESCE(gateway_lifecycle_action, ''),
            COALESCE(gateway_lifecycle_at, '-infinity'),
            COALESCE(refunded_amount_cents, -1)
        ) DO UPDATE SET updated_at = now()
        "#,
    )
    .bind(account.billing_scope_id().as_uuid())
    .bind(account.gateway_account_id().as_uuid())
    .bind(evidence.transaction_id.as_deref())
    .bind(evidence.order_id.as_deref())
    .bind(evidence.condition.as_deref())
    .bind(lifecycle_status(&evidence.state))
    .bind(evidence.action.as_deref())
    .bind(evidence.effective_at)
    .bind(lifecycle_refunded_amount(&evidence.state))
    .bind(PENDING_RETENTION_SECONDS)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn record_quarantine(
    pool: &PgPool,
    account: &GatewayLifecycleAccount,
    quarantine: &GatewayLifecycleQuarantine,
) -> Result<(), GatewayLifecycleReconciliationError> {
    let mut transaction = pool.begin().await?;
    set_timeouts(&mut transaction).await?;
    ensure_account(&mut transaction, account).await?;
    record_quarantine_parts(
        &mut transaction,
        account,
        quarantine
            .transaction_id()
            .map(GatewayTransactionId::expose),
        quarantine.order_id().map(GatewayOrderId::expose),
        quarantine.reason(),
    )
    .await?;
    transaction.commit().await?;
    Ok(())
}

async fn record_quarantine_parts(
    transaction: &mut Transaction<'_, Postgres>,
    account: &GatewayLifecycleAccount,
    transaction_id: Option<&str>,
    order_id: Option<&str>,
    reason: GatewayLifecycleQuarantineReason,
) -> Result<(), GatewayLifecycleReconciliationError> {
    sqlx::query(
        r#"
        INSERT INTO billing_gateway_lifecycle_quarantines (
            billing_scope_id,
            gateway_account_id,
            gateway_transaction_id,
            gateway_order_id,
            reason_code
        )
        VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT (
            gateway_account_id,
            COALESCE(gateway_transaction_id, ''),
            COALESCE(gateway_order_id, ''),
            reason_code
        ) DO UPDATE
        SET last_seen_at = now(),
            occurrence_count = billing_gateway_lifecycle_quarantines.occurrence_count + 1,
            resolved_at = NULL,
            last_operator_alerted_at = CASE
                WHEN billing_gateway_lifecycle_quarantines.resolved_at IS NULL
                    THEN billing_gateway_lifecycle_quarantines.last_operator_alerted_at
                ELSE NULL
            END
        "#,
    )
    .bind(account.billing_scope_id().as_uuid())
    .bind(account.gateway_account_id().as_uuid())
    .bind(transaction_id)
    .bind(order_id)
    .bind(reason.as_str())
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn resolve_matching_quarantines(
    transaction: &mut Transaction<'_, Postgres>,
    account: &GatewayLifecycleAccount,
    evidence: &StoredEvidence,
) -> Result<(), GatewayLifecycleReconciliationError> {
    sqlx::query(
        r#"
        UPDATE billing_gateway_lifecycle_quarantines
        SET resolved_at = now()
        WHERE billing_scope_id = $1
            AND gateway_account_id = $2
            AND resolved_at IS NULL
            AND (
                (
                    $3::text IS NOT NULL
                    AND public.billing_canonical_gateway_transaction_id(
                        gateway_transaction_id
                    ) = $3
                )
                OR (
                    gateway_transaction_id IS NULL
                    AND gateway_order_id IS NOT NULL
                    AND $4::text IS NOT NULL
                    AND gateway_order_id = $4
                )
            )
        "#,
    )
    .bind(account.billing_scope_id().as_uuid())
    .bind(account.gateway_account_id().as_uuid())
    .bind(evidence.transaction_id.as_deref())
    .bind(evidence.order_id.as_deref())
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn delete_pending(
    transaction: &mut Transaction<'_, Postgres>,
    account: &GatewayLifecycleAccount,
    pending_id: Uuid,
) -> Result<(), GatewayLifecycleReconciliationError> {
    sqlx::query(
        r#"
        DELETE FROM billing_gateway_lifecycle_pending_updates
        WHERE id = $1
            AND billing_scope_id = $2
            AND gateway_account_id = $3
        "#,
    )
    .bind(pending_id)
    .bind(account.billing_scope_id().as_uuid())
    .bind(account.gateway_account_id().as_uuid())
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn cleanup_pending(
    pool: &PgPool,
    account: &GatewayLifecycleAccount,
) -> Result<u64, GatewayLifecycleReconciliationError> {
    let mut transaction = pool.begin().await?;
    set_timeouts(&mut transaction).await?;
    ensure_account(&mut transaction, account).await?;
    let result = sqlx::query(
        r#"
        WITH stale AS MATERIALIZED (
            SELECT pending.id
            FROM billing_gateway_lifecycle_pending_updates pending
            WHERE pending.billing_scope_id = $1
                AND pending.gateway_account_id = $2
                AND pending.expires_at <= now()
            ORDER BY pending.expires_at, pending.first_seen_at, pending.id
            LIMIT $3
            FOR UPDATE SKIP LOCKED
        )
        DELETE FROM billing_gateway_lifecycle_pending_updates pending
        USING stale
        WHERE pending.id = stale.id
        "#,
    )
    .bind(account.billing_scope_id().as_uuid())
    .bind(account.gateway_account_id().as_uuid())
    .bind(PENDING_CLEANUP_BATCH_SIZE)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(result.rows_affected())
}

async fn record_unactionable_checks(
    pool: &PgPool,
    account: &GatewayLifecycleAccount,
) -> Result<(), GatewayLifecycleReconciliationError> {
    let mut transaction = pool.begin().await?;
    set_timeouts(&mut transaction).await?;
    ensure_account(&mut transaction, account).await?;
    sqlx::query(
        r#"
        WITH unactionable AS MATERIALIZED (
            SELECT pending.id
            FROM billing_gateway_lifecycle_pending_updates pending
            CROSS JOIN LATERAL (
                SELECT COUNT(*)::bigint AS candidate_count,
                    COALESCE(BOOL_OR(
                        pending.gateway_transaction_id IS NOT NULL
                        AND public.billing_canonical_gateway_transaction_id(
                            attempts.gateway_transaction_id
                        ) = pending.gateway_transaction_id
                    ), false) AS has_transaction_match
                FROM billing_payment_attempts attempts
                WHERE attempts.billing_scope_id = $1
                    AND attempts.gateway_account_id = $2
                    AND attempts.status = 'approved'
                    AND (
                        (
                            pending.gateway_transaction_id IS NOT NULL
                            AND public.billing_canonical_gateway_transaction_id(
                                attempts.gateway_transaction_id
                            ) = pending.gateway_transaction_id
                        )
                        OR (
                            pending.gateway_order_id IS NOT NULL
                            AND (
                                pending.gateway_transaction_id IS NULL
                                OR public.billing_canonical_gateway_transaction_id(
                                    attempts.gateway_transaction_id
                                ) IS NULL
                            )
                            AND attempts.gateway_order_id = pending.gateway_order_id
                        )
                    )
            ) matches
            WHERE pending.billing_scope_id = $1
                AND pending.gateway_account_id = $2
                AND pending.expires_at > now()
                AND pending.check_count < $3
                AND NOT (matches.has_transaction_match OR matches.candidate_count = 1)
            ORDER BY pending.last_checked_at NULLS FIRST,
                pending.first_seen_at,
                pending.id
            LIMIT $4
            FOR UPDATE OF pending SKIP LOCKED
        )
        UPDATE billing_gateway_lifecycle_pending_updates pending
        SET last_checked_at = now(),
            check_count = pending.check_count + 1
        FROM unactionable
        WHERE pending.id = unactionable.id
        "#,
    )
    .bind(account.billing_scope_id().as_uuid())
    .bind(account.gateway_account_id().as_uuid())
    .bind(PENDING_MAX_CHECKS)
    .bind(PENDING_CLEANUP_BATCH_SIZE)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(())
}

async fn actionable_pending(
    pool: &PgPool,
    account: &GatewayLifecycleAccount,
) -> Result<Vec<(Uuid, StoredEvidence)>, GatewayLifecycleReconciliationError> {
    let mut transaction = pool.begin().await?;
    set_timeouts(&mut transaction).await?;
    ensure_account(&mut transaction, account).await?;
    let rows = sqlx::query(
        r#"
        SELECT pending.id,
            pending.gateway_transaction_id,
            pending.gateway_order_id,
            pending.gateway_condition,
            pending.gateway_lifecycle_status,
            pending.gateway_lifecycle_action,
            pending.gateway_lifecycle_at,
            pending.refunded_amount_cents
        FROM billing_gateway_lifecycle_pending_updates pending
        CROSS JOIN LATERAL (
            SELECT COUNT(*)::bigint AS candidate_count,
                COALESCE(BOOL_OR(
                    pending.gateway_transaction_id IS NOT NULL
                    AND public.billing_canonical_gateway_transaction_id(
                        attempts.gateway_transaction_id
                    ) = pending.gateway_transaction_id
                ), false) AS has_transaction_match
            FROM billing_payment_attempts attempts
            WHERE attempts.billing_scope_id = $1
                AND attempts.gateway_account_id = $2
                AND attempts.status = 'approved'
                AND (
                    (
                        pending.gateway_transaction_id IS NOT NULL
                        AND public.billing_canonical_gateway_transaction_id(
                            attempts.gateway_transaction_id
                        ) = pending.gateway_transaction_id
                    )
                    OR (
                        pending.gateway_order_id IS NOT NULL
                        AND (
                            pending.gateway_transaction_id IS NULL
                            OR public.billing_canonical_gateway_transaction_id(
                                attempts.gateway_transaction_id
                            ) IS NULL
                        )
                        AND attempts.gateway_order_id = pending.gateway_order_id
                    )
                )
        ) matches
        WHERE pending.billing_scope_id = $1
            AND pending.gateway_account_id = $2
            AND pending.expires_at > now()
            AND (matches.has_transaction_match OR matches.candidate_count = 1)
        ORDER BY pending.first_seen_at, pending.id
        LIMIT $3
        FOR UPDATE OF pending SKIP LOCKED
        "#,
    )
    .bind(account.billing_scope_id().as_uuid())
    .bind(account.gateway_account_id().as_uuid())
    .bind(STAGED_APPLICATION_BATCH_SIZE)
    .fetch_all(&mut *transaction)
    .await?;
    let pending = rows
        .into_iter()
        .map(|row| {
            let status: String = row.try_get("gateway_lifecycle_status")?;
            let refunded = row.try_get("refunded_amount_cents")?;
            Ok((
                row.try_get("id")?,
                StoredEvidence {
                    transaction_id: row.try_get("gateway_transaction_id")?,
                    order_id: row.try_get("gateway_order_id")?,
                    state: lifecycle_state_from_parts(&status, refunded, false)?,
                    condition: row.try_get("gateway_condition")?,
                    action: row.try_get("gateway_lifecycle_action")?,
                    effective_at: row.try_get("gateway_lifecycle_at")?,
                },
            ))
        })
        .collect::<Result<Vec<_>, GatewayLifecycleReconciliationError>>()?;
    transaction.commit().await?;
    Ok(pending)
}

pub(crate) async fn ensure_account(
    transaction: &mut Transaction<'_, Postgres>,
    account: &GatewayLifecycleAccount,
) -> Result<(), GatewayLifecycleReconciliationError> {
    let exists: bool = sqlx::query_scalar(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM billing_gateway_accounts
            WHERE billing_scope_id = $1
                AND id = $2
                AND provider_key = $3
            FOR KEY SHARE
        )
        "#,
    )
    .bind(account.billing_scope_id().as_uuid())
    .bind(account.gateway_account_id().as_uuid())
    .bind(account.provider_key().as_str())
    .fetch_one(&mut **transaction)
    .await?;
    if exists {
        Ok(())
    } else {
        Err(GatewayLifecycleReconciliationError::AccountNotFound)
    }
}

pub(crate) async fn set_timeouts(
    transaction: &mut Transaction<'_, Postgres>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "SELECT set_config('lock_timeout', $1, true), set_config('statement_timeout', $2, true)",
    )
    .bind(ROW_LOCK_TIMEOUT)
    .bind(OPERATION_TIMEOUT)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

fn latest_time(left: Option<DateTime<Utc>>, right: Option<DateTime<Utc>>) -> Option<DateTime<Utc>> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        error::Error,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;
    use crate::test_support::{TestDatabase, create_gateway_account};
    use async_trait::async_trait;
    use sqlx::PgConnection;
    use syrup_rail::{
        GatewayAccountId, GatewayDiagnostic, GatewayProviderKey, GatewayReferenceValueError,
        HostChargeTargetNoChange, HostChargeTargetTransitionOutcome,
    };

    #[derive(Default)]
    struct ExactHostTargets {
        calls: AtomicU64,
    }

    #[async_trait]
    impl HostChargeTargetStore for ExactHostTargets {
        async fn preflight_target(
            &self,
            _connection: &mut PgConnection,
            _reservation: &crate::HostChargeTargetReservation,
        ) -> Result<crate::HostChargeReservationDecision, crate::HostChargeTargetError> {
            Ok(crate::HostChargeReservationDecision::Rejected {
                reason: syrup_rail::HostChargeTargetRejection::TargetUnavailable,
            })
        }

        async fn reserve_target(
            &self,
            _connection: &mut PgConnection,
            _reservation: &crate::HostChargeTargetReservation,
        ) -> Result<crate::HostChargeReservationDecision, crate::HostChargeTargetError> {
            Ok(crate::HostChargeReservationDecision::Rejected {
                reason: syrup_rail::HostChargeTargetRejection::TargetUnavailable,
            })
        }

        async fn admit_submission(
            &self,
            _connection: &mut PgConnection,
            _admission: &crate::HostChargeSubmissionAdmission,
        ) -> Result<crate::HostChargeSubmissionDecision, crate::HostChargeTargetError> {
            Ok(crate::HostChargeSubmissionDecision::Rejected {
                reason: syrup_rail::HostChargeTargetRejection::TargetUnavailable,
            })
        }

        async fn apply_transition(
            &self,
            connection: &mut PgConnection,
            transition: HostChargeTargetTransition,
        ) -> Result<HostChargeTargetTransitionOutcome, crate::HostChargeTargetError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let kind = match transition.kind() {
                HostChargeTargetTransitionKind::Reversed { kind } => match kind {
                    syrup_rail::PaymentReversalKind::Refunded => "refunded",
                    syrup_rail::PaymentReversalKind::Voided => "voided",
                    syrup_rail::PaymentReversalKind::Chargeback => "chargeback",
                },
                _ => {
                    return Ok(HostChargeTargetTransitionOutcome::Unchanged {
                        reason: HostChargeTargetNoChange::InapplicableState,
                    });
                }
            };
            let result = sqlx::query(
                r#"
                UPDATE host_charge_targets
                SET status = 'reversed',
                    reversal_kind = $4,
                    reversed_at = $5
                WHERE id = $1
                    AND billing_scope_id = $2
                    AND subscriber_id = $3
                    AND status = 'paid'
                "#,
            )
            .bind(transition.target_id().as_uuid())
            .bind(transition.billing_scope_id().as_uuid())
            .bind(transition.subscriber_id().as_uuid())
            .bind(kind)
            .bind(transition.effective_at())
            .execute(connection)
            .await
            .map_err(crate::HostChargeTargetError::new)?;
            Ok(if result.rows_affected() == 1 {
                HostChargeTargetTransitionOutcome::Applied
            } else {
                HostChargeTargetTransitionOutcome::Unchanged {
                    reason: HostChargeTargetNoChange::InapplicableState,
                }
            })
        }
    }

    fn lifecycle_account(
        fixture: crate::test_support::GatewayAccountFixture,
        provider: &str,
    ) -> GatewayLifecycleAccount {
        GatewayLifecycleAccount::new(
            BillingScopeId::new(fixture.billing_scope_id),
            GatewayAccountId::new(fixture.gateway_account_id),
            GatewayProviderKey::new(provider).unwrap(),
        )
    }

    fn evidence(
        transaction_id: &str,
        state: GatewayLifecycleState,
        effective_at: DateTime<Utc>,
    ) -> Result<GatewayTransactionReport, GatewayReferenceValueError> {
        Ok(GatewayTransactionReport::Evidence(
            GatewayLifecycleEvidence::new(
                Some(GatewayTransactionId::new(transaction_id)?),
                None,
                state,
                Some(GatewayDiagnostic::new("condition")),
                Some(GatewayDiagnostic::new("diagnostic action")),
                Some(effective_at),
            )
            .unwrap(),
        ))
    }

    async fn insert_host_attempt(
        pool: &PgPool,
        fixture: crate::test_support::GatewayAccountFixture,
        subscriber_id: Uuid,
        target_id: Uuid,
        transaction_id: &str,
        amount_cents: i32,
        resolved_at: DateTime<Utc>,
    ) -> Result<Uuid, sqlx::Error> {
        let attempt_id = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO billing_payment_attempts (
                id,
                billing_scope_id,
                subscriber_id,
                host_charge_target_id,
                attempt_kind,
                status,
                idempotency_key,
                request_fingerprint,
                amount_cents,
                gateway_account_id,
                gateway_configuration_id,
                gateway_order_id,
                gateway_transaction_id,
                submitted_at,
                resolved_at
            ) VALUES (
                $1, $2, $3, $4, 'host_charge', 'approved', $5, $6, $7,
                $8, $9, $10, $11, $12, $12
            )
            "#,
        )
        .bind(attempt_id)
        .bind(fixture.billing_scope_id)
        .bind(subscriber_id)
        .bind(target_id)
        .bind(format!("idempotency-{attempt_id}"))
        .bind(format!("fingerprint-{attempt_id}"))
        .bind(amount_cents)
        .bind(fixture.gateway_account_id)
        .bind(fixture.gateway_configuration_id)
        .bind(format!("order-{attempt_id}"))
        .bind(transaction_id)
        .bind(resolved_at)
        .execute(pool)
        .await?;
        Ok(attempt_id)
    }

    #[tokio::test]
    async fn report_lifecycle_is_crash_safe_monotonic_and_exact_targeted()
    -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("rail_lifecycle").await?;
        let fixture = create_gateway_account(&database.pool, "nmi").await?;
        let account = lifecycle_account(fixture, "nmi");
        let host_targets = ExactHostTargets::default();
        sqlx::query(
            r#"
            CREATE TABLE host_charge_targets (
                id uuid PRIMARY KEY,
                billing_scope_id uuid NOT NULL,
                subscriber_id uuid NOT NULL,
                status text NOT NULL,
                reversal_kind text,
                paid_at timestamptz,
                reversed_at timestamptz
            )
            "#,
        )
        .execute(&database.pool)
        .await?;

        let staged_at = Utc::now() - chrono::Duration::minutes(3);
        let staged = reconcile_gateway_transaction_reports(
            &database.pool,
            &host_targets,
            &account,
            vec![evidence(
                "txn-late",
                GatewayLifecycleState::Settled {
                    cumulative_refunded_cents: Some(CumulativeRefundCents::new(125)?),
                },
                staged_at,
            )?],
        )
        .await?;
        assert_eq!(staged.staged(), 1);
        let pending_state: (String, Option<i32>, String) = sqlx::query_as(
            r#"
            SELECT gateway_lifecycle_status, refunded_amount_cents,
                gateway_lifecycle_action
            FROM billing_gateway_lifecycle_pending_updates
            WHERE gateway_account_id = $1
            "#,
        )
        .bind(fixture.gateway_account_id)
        .fetch_one(&database.pool)
        .await?;
        assert_eq!(
            pending_state,
            (
                "settled".to_owned(),
                Some(125),
                "diagnostic action".to_owned()
            )
        );

        let late_subscriber = Uuid::now_v7();
        let late_target = Uuid::now_v7();
        insert_host_attempt(
            &database.pool,
            fixture,
            late_subscriber,
            late_target,
            "txn-late",
            1_000,
            staged_at - chrono::Duration::minutes(1),
        )
        .await?;
        let applied =
            apply_staged_gateway_lifecycle_evidence(&database.pool, &host_targets, &account)
                .await?;
        assert_eq!(applied.applied(), 1);
        let stored: (String, i32, String) = sqlx::query_as(
            r#"
            SELECT gateway_lifecycle_status, refunded_amount_cents,
                gateway_lifecycle_action
            FROM billing_payment_attempts
            WHERE gateway_transaction_id = 'txn-late'
            "#,
        )
        .fetch_one(&database.pool)
        .await?;
        assert_eq!(
            stored,
            ("settled".to_owned(), 125, "diagnostic action".to_owned())
        );
        assert_eq!(host_targets.calls.load(Ordering::SeqCst), 0);

        let subscriber_id = Uuid::now_v7();
        let target_id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO host_charge_targets (id, billing_scope_id, subscriber_id, status) VALUES ($1, $2, $3, 'paid')",
        )
        .bind(target_id)
        .bind(fixture.billing_scope_id)
        .bind(subscriber_id)
        .execute(&database.pool)
        .await?;
        insert_host_attempt(
            &database.pool,
            fixture,
            subscriber_id,
            target_id,
            "txn-refund",
            2_500,
            staged_at,
        )
        .await?;
        let refunded_at = Utc::now();
        let report = evidence(
            "txn-refund",
            GatewayLifecycleState::Refunded {
                cumulative_refunded_cents: CumulativeRefundCents::new(2_500)?,
            },
            refunded_at,
        )?;
        let first = reconcile_gateway_transaction_reports(
            &database.pool,
            &host_targets,
            &account,
            vec![report.clone()],
        )
        .await?;
        let replay = reconcile_gateway_transaction_reports(
            &database.pool,
            &host_targets,
            &account,
            vec![report],
        )
        .await?;
        assert_eq!(first.applied(), 1);
        assert_eq!(replay.applied(), 0);
        assert_eq!(host_targets.calls.load(Ordering::SeqCst), 1);
        let target: (String, String, DateTime<Utc>) = sqlx::query_as(
            "SELECT status, reversal_kind, reversed_at FROM host_charge_targets WHERE id = $1",
        )
        .bind(target_id)
        .fetch_one(&database.pool)
        .await?;
        assert_eq!(target.0, "reversed");
        assert_eq!(target.1, "refunded");
        assert_eq!(target.2, refunded_at);

        let cursor_key = GatewayLifecycleCursorKey::new("approved_lifecycle")?;
        let initial =
            gateway_lifecycle_reconciliation_start(&database.pool, &account, &cursor_key).await?;
        assert!(initial.is_some());
        let later = Utc::now() + chrono::Duration::minutes(5);
        save_gateway_lifecycle_reconciliation_cursor(&database.pool, &account, &cursor_key, later)
            .await?;
        save_gateway_lifecycle_reconciliation_cursor(
            &database.pool,
            &account,
            &cursor_key,
            staged_at,
        )
        .await?;
        assert_eq!(
            gateway_lifecycle_reconciliation_start(&database.pool, &account, &cursor_key).await?,
            Some(later)
        );

        let wrong_provider = lifecycle_account(fixture, "other");
        assert!(matches!(
            gateway_lifecycle_reconciliation_start(&database.pool, &wrong_provider, &cursor_key)
                .await,
            Err(GatewayLifecycleReconciliationError::AccountNotFound)
        ));

        database.cleanup().await?;
        Ok(())
    }
}
