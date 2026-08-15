use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row};
use syrup_rail::{
    BillingScopeId, GatewayAccountId, GatewayAccountReconciliationCandidate, PaymentAttempt,
    PaymentAttemptKind, PaymentAttemptStatus, PaymentResolutionCode, PlanKey, SubscriberId,
};
use uuid::Uuid;

use crate::PaymentAttemptStoreError;
use crate::attempts::{
    STALE_UNSUBMITTED_RECOVERY_TEXT, STALE_UNSUBMITTED_RENEWAL_TEXT,
    SUBSCRIPTION_CHARGE_UNSUBMITTED_STALE_AFTER_SECONDS, expire_stale_initial_attempts,
    lock_initial_attempt_rows, lock_initial_charge_rows, lock_payment_attempt_by_id_on_connection,
    payment_attempt_from_row, set_enrollment_timeouts, try_lock_subscription_aggregate,
};

use classification::{
    attempt_locator, classify_pending_charge, count_pending_processor_charges,
    invalid_reconciliation_state, lock_attempt_for_classification,
    lock_pending_charge_for_classification, transition_pending_charge,
};

mod classification;

const PAYMENT_METHOD_REPLACEMENT_STALE_AFTER_SECONDS: i64 = 3 * 60;
pub(crate) const RECONCILIATION_PHASE_BATCH_SIZE: i64 = 100;
const STALE_PAYMENT_METHOD_REPLACEMENT_RESPONSE_TEXT: &str =
    "Payment method update was abandoned before gateway submission.";
const PROCESSOR_CHARGE_CANDIDATE_PAGE_SIZE: i64 = 128;
const EXACT_REQUERY_AFTER_SECONDS: i64 = 60;
const EXACT_STALE_AFTER_SECONDS: i64 = 30 * 60;
const EXACT_RECENT_TERMINAL_SECONDS: i64 = 24 * 60 * 60;
const EXACT_EMPTY_REVIEW_KEEPALIVE_TEXT: &str =
    "Payment processor still has not returned a transaction during manual review.";
const EXACT_EMPTY_STALE_PAYMENT_METHOD_TEXT: &str = "Payment method update was submitted locally but no processor transaction appeared before the reconciliation deadline.";
const EXACT_EMPTY_STALE_REVIEW_TEXT: &str =
    "Payment processor did not return a transaction before the reconciliation deadline.";
const EXACT_EMPTY_UNKNOWN_TEXT: &str = "Payment processor has not returned a transaction yet.";
const EXACT_MALFORMED_STALE_REVIEW_TEXT: &str = "Payment processor returned a malformed exact-query response after the reconciliation deadline.";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExactQueryObservation {
    NoTransaction,
    MalformedResponse,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessorChargeClassificationSummary {
    transitioned: u64,
    skipped_locked: u64,
    remaining_pending: u64,
}

impl ProcessorChargeClassificationSummary {
    pub const fn transitioned(self) -> u64 {
        self.transitioned
    }

    pub const fn skipped_locked(self) -> u64 {
        self.skipped_locked
    }

    pub const fn remaining_pending(self) -> u64 {
        self.remaining_pending
    }
}

#[derive(Clone, Debug)]
struct PendingChargeCandidate {
    id: Uuid,
    attempt_id: Uuid,
    transaction_id: Option<String>,
    observed_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AttemptLocator {
    id: Uuid,
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    plan_key: Option<PlanKey>,
    gateway_account_id: GatewayAccountId,
    kind: PaymentAttemptKind,
}

#[derive(Clone, Debug)]
struct LockedAttempt {
    locator: AttemptLocator,
    status: PaymentAttemptStatus,
    resolution_code: Option<PaymentResolutionCode>,
    amount_cents: i32,
    transaction_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ChargeRole {
    Primary,
    Additional,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ChargeProgression {
    ReconciliationRequired,
    ExternalReversalRequired,
    Applied,
    ExternallyReversed,
}

impl ChargeProgression {
    const fn as_str(self) -> &'static str {
        match self {
            Self::ReconciliationRequired => "reconciliation_required",
            Self::ExternalReversalRequired => "external_reversal_required",
            Self::Applied => "applied",
            Self::ExternallyReversed => "externally_reversed",
        }
    }
}

/// Returns every registered gateway account in deterministic locator order.
///
/// This operation intentionally has no caller-selected or implicit limit.
/// Hosts must either dispatch the complete result or introduce a separately
/// designed durable progress cursor.
pub async fn reconciliation_gateway_accounts(
    pool: &PgPool,
) -> Result<Vec<GatewayAccountReconciliationCandidate>, sqlx::Error> {
    let rows = sqlx::query_as::<_, (Uuid, Uuid)>(
        r#"
        SELECT billing_scope_id, id
        FROM billing_gateway_accounts
        ORDER BY billing_scope_id, id
        "#,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(billing_scope_id, gateway_account_id)| {
            GatewayAccountReconciliationCandidate::new(
                BillingScopeId::new(billing_scope_id),
                GatewayAccountId::new(gateway_account_id),
            )
        })
        .collect())
}

/// Claims one bounded, account-scoped batch for authoritative exact queries.
///
/// Updating `updated_at` is the durable requery claim. The returned value is
/// the canonical shared attempt, so subscription reconciliation never passes
/// through a host persistence projection.
pub async fn claim_exact_reconciliation_attempts(
    pool: &PgPool,
    gateway_account_id: GatewayAccountId,
) -> Result<Vec<PaymentAttempt>, PaymentAttemptStoreError> {
    let mut transaction = pool.begin().await?;
    sqlx::query("SELECT set_config('lock_timeout', '250ms', true)")
        .execute(&mut *transaction)
        .await?;
    let rows = sqlx::query(
        r#"
        WITH candidate_attempts AS MATERIALIZED (
            SELECT attempts.id AS attempt_id, attempts.created_at
            FROM billing_payment_attempts AS attempts
            WHERE attempts.gateway_account_id = $1
                AND attempts.submitted_at IS NOT NULL
                AND (
                    (
                        attempts.status IN ('unknown', 'review_required')
                        AND attempts.updated_at <= clock_timestamp()
                            - ($2::bigint * interval '1 second')
                    )
                    OR (
                        attempts.status = 'pending'
                        AND attempts.submitted_at
                            <= clock_timestamp() - ($3::bigint * interval '1 second')
                        AND attempts.updated_at <= clock_timestamp()
                            - ($2::bigint * interval '1 second')
                    )
                    OR (
                        attempts.status IN ('declined', 'failed')
                        AND public.billing_canonical_gateway_transaction_id(
                            attempts.gateway_transaction_id
                        ) IS NOT NULL
                        AND attempts.resolved_at IS NOT NULL
                        AND attempts.resolved_at >= clock_timestamp()
                            - ($4::bigint * interval '1 second')
                        AND attempts.updated_at <= clock_timestamp()
                            - ($2::bigint * interval '1 second')
                    )
                )
                AND NOT (
                    attempts.attempt_kind = 'subscription_initial'
                    AND attempts.status = 'review_required'
                    AND (
                        attempts.resolution_code IS NOT DISTINCT FROM
                            'subscription_initial_current_subscription_conflict'
                        OR attempts.resolution_code IS NOT DISTINCT FROM
                            'subscription_initial_current_grant_conflict'
                    )
                )
                AND attempts.resolution_code IS DISTINCT FROM
                    'subscription_initial_externally_refunded'
                AND attempts.resolution_code IS DISTINCT FROM
                    'subscription_initial_externally_voided'
            ORDER BY attempts.created_at, attempts.id
            LIMIT $5
            FOR UPDATE OF attempts SKIP LOCKED
        ), updated_attempts AS (
            UPDATE billing_payment_attempts AS attempts
            SET updated_at = clock_timestamp()
            FROM candidate_attempts
            WHERE attempts.id = candidate_attempts.attempt_id
            RETURNING attempts.*
        )
        SELECT updated_attempts.*
        FROM updated_attempts
        INNER JOIN candidate_attempts
            ON candidate_attempts.attempt_id = updated_attempts.id
        ORDER BY candidate_attempts.created_at, candidate_attempts.attempt_id
        "#,
    )
    .bind(gateway_account_id.as_uuid())
    .bind(EXACT_REQUERY_AFTER_SECONDS)
    .bind(EXACT_STALE_AFTER_SECONDS)
    .bind(EXACT_RECENT_TERMINAL_SECONDS)
    .bind(RECONCILIATION_PHASE_BATCH_SIZE)
    .fetch_all(&mut *transaction)
    .await?;
    let attempts = rows
        .iter()
        .map(payment_attempt_from_row)
        .collect::<Result<Vec<_>, _>>()?;
    transaction.commit().await?;
    Ok(attempts)
}

/// Applies one authoritative negative exact-query observation.
///
/// The attempt is locked and its immutable shared locator is revalidated
/// before any transition. `true` means this observation changed the lifecycle
/// state and therefore counts against the exact-query transition budget.
pub async fn apply_exact_query_observation(
    pool: &PgPool,
    claimed: &PaymentAttempt,
    observation: ExactQueryObservation,
) -> Result<bool, PaymentAttemptStoreError> {
    let mut transaction = pool.begin().await?;
    sqlx::query("SELECT set_config('lock_timeout', '250ms', true)")
        .execute(&mut *transaction)
        .await?;
    let current = lock_payment_attempt_by_id_on_connection(
        &mut transaction,
        claimed.identity().billing_scope_id(),
        claimed.identity().attempt_id(),
    )
    .await?
    .ok_or(PaymentAttemptStoreError::InvalidState(
        "claimed exact-query attempt was not found",
    ))?;
    if current.identity() != claimed.identity() || current.request() != claimed.request() {
        return Err(PaymentAttemptStoreError::InvalidState(
            "claimed exact-query attempt identity changed",
        ));
    }
    let Some(submitted_at) = current.state().timestamps().submitted_at() else {
        transaction.commit().await?;
        return Ok(false);
    };
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *transaction)
        .await?;
    let stale = submitted_at <= now - chrono::Duration::seconds(EXACT_STALE_AFTER_SECONDS);
    let status = current.status();
    let evidence = current.state().processor_evidence();

    let (next_status, message, transitioned) = match observation {
        ExactQueryObservation::NoTransaction
            if status == PaymentAttemptStatus::ReviewRequired
                && evidence.has_gateway_reference() =>
        {
            (status, EXACT_EMPTY_REVIEW_KEEPALIVE_TEXT, false)
        }
        ExactQueryObservation::NoTransaction
            if stale
                && current.kind() == PaymentAttemptKind::SubscriptionPaymentMethodUpdate
                && matches!(
                    status,
                    PaymentAttemptStatus::Pending | PaymentAttemptStatus::ReviewRequired
                )
                && current.state().timestamps().submitted_at().is_some()
                && evidence.transaction_id().is_none()
                && evidence.condition().is_none() =>
        {
            (
                PaymentAttemptStatus::Failed,
                EXACT_EMPTY_STALE_PAYMENT_METHOD_TEXT,
                true,
            )
        }
        ExactQueryObservation::NoTransaction if stale => (
            PaymentAttemptStatus::ReviewRequired,
            EXACT_EMPTY_STALE_REVIEW_TEXT,
            status != PaymentAttemptStatus::ReviewRequired,
        ),
        ExactQueryObservation::NoTransaction if status == PaymentAttemptStatus::Unknown => {
            (status, EXACT_EMPTY_UNKNOWN_TEXT, false)
        }
        ExactQueryObservation::MalformedResponse if stale => (
            PaymentAttemptStatus::ReviewRequired,
            EXACT_MALFORMED_STALE_REVIEW_TEXT,
            status != PaymentAttemptStatus::ReviewRequired,
        ),
        _ => {
            transaction.commit().await?;
            return Ok(false);
        }
    };

    let result = if next_status == PaymentAttemptStatus::Failed {
        sqlx::query(
            r#"
            UPDATE billing_payment_attempts
            SET status = 'failed', gateway_response_text = $2,
                gateway_condition = COALESCE(gateway_condition, 'failed'),
                resolved_at = clock_timestamp(), updated_at = clock_timestamp()
            WHERE id = $1 AND status IN ('pending', 'review_required')
            "#,
        )
        .bind(current.identity().attempt_id().as_uuid())
        .bind(message)
        .execute(&mut *transaction)
        .await?
    } else if next_status == PaymentAttemptStatus::ReviewRequired {
        sqlx::query(
            r#"
            UPDATE billing_payment_attempts
            SET status = 'review_required',
                gateway_response_text = CASE
                    WHEN status = 'review_required'
                        AND NULLIF(BTRIM(gateway_response_text), '') IS NOT NULL
                    THEN gateway_response_text ELSE $2
                END,
                updated_at = clock_timestamp()
            WHERE id = $1 AND status IN ('pending', 'unknown', 'review_required')
            "#,
        )
        .bind(current.identity().attempt_id().as_uuid())
        .bind(message)
        .execute(&mut *transaction)
        .await?
    } else {
        sqlx::query(
            r#"
            UPDATE billing_payment_attempts
            SET gateway_response_text = $2, updated_at = clock_timestamp()
            WHERE id = $1 AND status = $3
            "#,
        )
        .bind(current.identity().attempt_id().as_uuid())
        .bind(message)
        .bind(status.as_str())
        .execute(&mut *transaction)
        .await?
    };
    transaction.commit().await?;
    Ok(transitioned && result.rows_affected() == 1)
}

/// Fails one bounded batch of stale payment-method replacements for an account.
///
/// These attempts have never crossed the provider boundary, so expiring them
/// is a local reconciliation phase and performs no gateway I/O.
pub async fn fail_stale_unsubmitted_payment_method_replacements(
    pool: &PgPool,
    gateway_account_id: GatewayAccountId,
) -> Result<u64, sqlx::Error> {
    let mut transaction = pool.begin().await?;
    sqlx::query("SELECT set_config('lock_timeout', '250ms', true)")
        .execute(&mut *transaction)
        .await?;
    let result = sqlx::query(
        r#"
        WITH stale_attempts AS (
            SELECT id
            FROM billing_payment_attempts
            WHERE attempt_kind = 'subscription_payment_method_update'
                AND status = 'pending'
                AND submitted_at IS NULL
                AND created_at <= clock_timestamp()
                    - ($1::bigint * interval '1 second')
                AND gateway_account_id = $2
            ORDER BY created_at, id
            LIMIT $3
            FOR UPDATE SKIP LOCKED
        )
        UPDATE billing_payment_attempts AS attempts
        SET status = 'failed',
            gateway_response_text = COALESCE(gateway_response_text, $4),
            gateway_condition = COALESCE(gateway_condition, 'failed'),
            resolved_at = clock_timestamp(),
            updated_at = clock_timestamp()
        FROM stale_attempts
        WHERE attempts.id = stale_attempts.id
        "#,
    )
    .bind(PAYMENT_METHOD_REPLACEMENT_STALE_AFTER_SECONDS)
    .bind(gateway_account_id.as_uuid())
    .bind(RECONCILIATION_PHASE_BATCH_SIZE)
    .bind(STALE_PAYMENT_METHOD_REPLACEMENT_RESPONSE_TEXT)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(result.rows_affected())
}

/// Fails one bounded batch of stale local renewal and recovery attempts.
///
/// Both `pending` reservations and `review_required` rows parked by older
/// exact-query behavior are eligible only when `submitted_at` proves that no
/// provider boundary was crossed. This phase performs no gateway I/O.
pub async fn fail_stale_unsubmitted_subscription_charges(
    pool: &PgPool,
    gateway_account_id: GatewayAccountId,
) -> Result<u64, sqlx::Error> {
    let mut transaction = pool.begin().await?;
    sqlx::query("SELECT set_config('lock_timeout', '250ms', true)")
        .execute(&mut *transaction)
        .await?;
    let result = sqlx::query(
        r#"
        WITH stale_attempts AS (
            SELECT id
            FROM billing_payment_attempts
            WHERE attempt_kind IN ('subscription_renewal', 'subscription_recovery')
                AND status IN ('pending', 'review_required')
                AND submitted_at IS NULL
                AND created_at <= clock_timestamp()
                    - ($1::bigint * interval '1 second')
                AND gateway_account_id = $2
            ORDER BY created_at, id
            LIMIT $3
            FOR UPDATE SKIP LOCKED
        )
        UPDATE billing_payment_attempts AS attempts
        SET status = 'failed',
            gateway_response_text = CASE attempts.attempt_kind
                WHEN 'subscription_renewal' THEN $4
                WHEN 'subscription_recovery' THEN $5
            END,
            gateway_condition = COALESCE(attempts.gateway_condition, 'failed'),
            resolved_at = COALESCE(attempts.resolved_at, clock_timestamp()),
            updated_at = clock_timestamp()
        FROM stale_attempts
        WHERE attempts.id = stale_attempts.id
        "#,
    )
    .bind(SUBSCRIPTION_CHARGE_UNSUBMITTED_STALE_AFTER_SECONDS)
    .bind(gateway_account_id.as_uuid())
    .bind(RECONCILIATION_PHASE_BATCH_SIZE)
    .bind(STALE_UNSUBMITTED_RENEWAL_TEXT)
    .bind(STALE_UNSUBMITTED_RECOVERY_TEXT)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(result.rows_affected())
}

/// Expires every stale prepared enrollment for one account.
///
/// Each candidate enters its persisted subscriber/plan aggregate without
/// waiting for a busy aggregate. This preserves progress for unrelated plans
/// while serializing with enrollment admission and charge observation.
pub async fn fail_stale_unsubmitted_subscription_enrollments(
    pool: &PgPool,
    gateway_account_id: GatewayAccountId,
) -> Result<u64, sqlx::Error> {
    let candidates = sqlx::query_as::<_, (Uuid, Uuid, String)>(
        r#"
        SELECT DISTINCT billing_scope_id, subscriber_id, plan_key
        FROM billing_payment_attempts
        WHERE gateway_account_id = $1
            AND attempt_kind = 'subscription_initial'
            AND status = 'pending'
            AND submitted_at IS NULL
            AND created_at <= clock_timestamp() - interval '30 minutes'
        ORDER BY billing_scope_id, subscriber_id, plan_key
        "#,
    )
    .bind(gateway_account_id.as_uuid())
    .fetch_all(pool)
    .await?;

    let mut failed = 0;
    for (billing_scope_id, subscriber_id, plan_key) in candidates {
        let plan_key = PlanKey::new(plan_key)
            .map_err(|_| sqlx::Error::Protocol("stored plan key is invalid".to_owned()))?;
        let billing_scope_id = BillingScopeId::new(billing_scope_id);
        let subscriber_id = SubscriberId::new(subscriber_id);
        let mut transaction = pool.begin().await?;
        set_enrollment_timeouts(&mut transaction).await?;
        if !try_lock_subscription_aggregate(&mut transaction, subscriber_id, &plan_key).await? {
            transaction.rollback().await?;
            continue;
        }
        lock_initial_attempt_rows(&mut transaction, billing_scope_id, subscriber_id, &plan_key)
            .await?;
        lock_initial_charge_rows(&mut transaction, billing_scope_id, subscriber_id, &plan_key)
            .await?;
        failed += expire_stale_initial_attempts(
            &mut transaction,
            billing_scope_id,
            subscriber_id,
            &plan_key,
        )
        .await?;
        transaction.commit().await?;
    }
    Ok(failed)
}

/// Classifies one bounded batch of durable pending processor charges.
///
/// Candidate reads are account-scoped and stable for the duration of this
/// pass. Each subscription candidate enters its persisted subscriber/plan
/// aggregate before locking the attempt and charge. Busy aggregate or row
/// locks are skipped without consuming the transition budget.
pub async fn classify_pending_processor_charges(
    pool: &PgPool,
    gateway_account_id: GatewayAccountId,
    max_transitions: u64,
) -> Result<ProcessorChargeClassificationSummary, sqlx::Error> {
    let transition_limit = max_transitions.min(RECONCILIATION_PHASE_BATCH_SIZE as u64);
    if transition_limit == 0 {
        return Ok(ProcessorChargeClassificationSummary {
            transitioned: 0,
            skipped_locked: 0,
            remaining_pending: count_pending_processor_charges(pool, gateway_account_id).await?,
        });
    }

    let upper_bound = sqlx::query_as::<_, (DateTime<Utc>, Uuid)>(
        r#"
        SELECT observed_at, id
        FROM billing_processor_charges
        WHERE gateway_account_id = $1 AND progression_state = 'pending'
        ORDER BY observed_at DESC, id DESC
        LIMIT 1
        "#,
    )
    .bind(gateway_account_id.as_uuid())
    .fetch_optional(pool)
    .await?;
    let Some((upper_observed_at, upper_id)) = upper_bound else {
        return Ok(ProcessorChargeClassificationSummary {
            transitioned: 0,
            skipped_locked: 0,
            remaining_pending: 0,
        });
    };

    let mut transitioned = 0;
    let mut skipped_locked = 0;
    let mut cursor: Option<(DateTime<Utc>, Uuid)> = None;
    while transitioned < transition_limit {
        let rows = sqlx::query(
            r#"
            SELECT charges.id, charges.attempt_id,
                billing_canonical_gateway_transaction_id(
                    charges.gateway_transaction_id
                ) AS transaction_id,
                charges.observed_at
            FROM billing_processor_charges charges
            INNER JOIN billing_payment_attempts attempts
                ON attempts.id = charges.attempt_id
                AND attempts.gateway_account_id = $1
            WHERE charges.gateway_account_id = $1
                AND charges.progression_state = 'pending'
                AND (
                    $2::timestamptz IS NULL
                    OR (charges.observed_at, charges.id)
                        > ($2::timestamptz, $3::uuid)
                )
                AND (charges.observed_at, charges.id) <= ($4, $5)
            ORDER BY charges.observed_at, charges.id
            LIMIT $6
            "#,
        )
        .bind(gateway_account_id.as_uuid())
        .bind(cursor.as_ref().map(|(observed_at, _)| observed_at))
        .bind(cursor.as_ref().map(|(_, id)| id))
        .bind(upper_observed_at)
        .bind(upper_id)
        .bind(PROCESSOR_CHARGE_CANDIDATE_PAGE_SIZE)
        .fetch_all(pool)
        .await?;
        let candidates = rows
            .into_iter()
            .map(|row| {
                Ok(PendingChargeCandidate {
                    id: row.try_get("id")?,
                    attempt_id: row.try_get("attempt_id")?,
                    transaction_id: row.try_get("transaction_id")?,
                    observed_at: row.try_get("observed_at")?,
                })
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()?;
        let Some(last) = candidates.last() else {
            break;
        };
        cursor = Some((last.observed_at, last.id));

        for candidate in candidates {
            if transitioned >= transition_limit {
                break;
            }
            let mut transaction = pool.begin().await?;
            set_enrollment_timeouts(&mut transaction).await?;
            let Some(locator) = attempt_locator(&mut transaction, candidate.attempt_id).await?
            else {
                transaction.rollback().await?;
                continue;
            };
            if locator.gateway_account_id != gateway_account_id {
                return Err(invalid_reconciliation_state());
            }
            if let Some(plan_key) = locator.plan_key.as_ref()
                && !try_lock_subscription_aggregate(
                    &mut transaction,
                    locator.subscriber_id,
                    plan_key,
                )
                .await?
            {
                skipped_locked += 1;
                transaction.rollback().await?;
                continue;
            }
            let Some(attempt) = lock_attempt_for_classification(&mut transaction, locator).await?
            else {
                skipped_locked += 1;
                transaction.rollback().await?;
                continue;
            };
            let Some((role, charge_transaction_id, same_charge, dimensions_match)) =
                lock_pending_charge_for_classification(
                    &mut transaction,
                    candidate.id,
                    candidate.attempt_id,
                )
                .await?
            else {
                skipped_locked += 1;
                transaction.rollback().await?;
                continue;
            };
            if !dimensions_match || charge_transaction_id != candidate.transaction_id {
                return Err(invalid_reconciliation_state());
            }

            let (progression, state_code) = classify_pending_charge(
                &mut transaction,
                &attempt,
                candidate.id,
                role,
                charge_transaction_id.as_deref(),
                same_charge,
            )
            .await?;
            transition_pending_charge(
                &mut transaction,
                candidate.id,
                progression,
                state_code.as_deref(),
            )
            .await?;
            transaction.commit().await?;
            transitioned += 1;
        }
    }

    Ok(ProcessorChargeClassificationSummary {
        transitioned,
        skipped_locked,
        remaining_pending: count_pending_processor_charges(pool, gateway_account_id).await?,
    })
}

#[cfg(test)]
mod tests;
