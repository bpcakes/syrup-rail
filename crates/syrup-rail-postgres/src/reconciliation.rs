use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row};
use syrup_rail::{
    BillingScopeId, GatewayAccountId, GatewayAccountReconciliationCandidate, PaymentAttempt,
    PaymentAttemptKind, PaymentAttemptStatus, PaymentResolutionCode, PlanKey, SubscriberId,
};
use uuid::Uuid;

use crate::PaymentAttemptStoreError;
use crate::attempts::{
    expire_stale_initial_attempts, lock_initial_attempt_rows, lock_initial_charge_rows,
    lock_payment_attempt_by_id_on_connection, payment_attempt_from_row, set_enrollment_timeouts,
    try_lock_subscription_aggregate,
};

const PAYMENT_METHOD_REPLACEMENT_STALE_AFTER_SECONDS: i64 = 3 * 60;
const RECONCILIATION_PHASE_BATCH_SIZE: i64 = 100;
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
                AND (
                    (
                        attempts.status IN ('unknown', 'review_required')
                        AND attempts.updated_at <= clock_timestamp()
                            - ($2::bigint * interval '1 second')
                    )
                    OR (
                        attempts.status = 'pending'
                        AND COALESCE(attempts.submitted_at, attempts.created_at)
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
                    AND attempts.resolution_code IN (
                        'subscription_initial_current_subscription_conflict',
                        'subscription_initial_current_grant_conflict'
                    )
                )
                AND NOT (
                    attempts.attempt_kind = 'subscription_payment_method_update'
                    AND attempts.submitted_at IS NULL
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
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *transaction)
        .await?;
    let stale = current.state().timestamps().submitted_or_created_at()
        <= now - chrono::Duration::seconds(EXACT_STALE_AFTER_SECONDS);
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

async fn count_pending_processor_charges(
    pool: &PgPool,
    gateway_account_id: GatewayAccountId,
) -> Result<u64, sqlx::Error> {
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM billing_processor_charges WHERE gateway_account_id = $1 AND progression_state = 'pending'",
    )
    .bind(gateway_account_id.as_uuid())
    .fetch_one(pool)
    .await?;
    u64::try_from(count).map_err(|_| invalid_reconciliation_state())
}

async fn attempt_locator(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    attempt_id: Uuid,
) -> Result<Option<AttemptLocator>, sqlx::Error> {
    let row = sqlx::query(
        r#"
        SELECT id, billing_scope_id, subscriber_id, plan_key,
            gateway_account_id, attempt_kind
        FROM billing_payment_attempts
        WHERE id = $1
        "#,
    )
    .bind(attempt_id)
    .fetch_optional(&mut **transaction)
    .await?;
    row.as_ref().map(attempt_locator_from_row).transpose()
}

fn attempt_locator_from_row(row: &sqlx::postgres::PgRow) -> Result<AttemptLocator, sqlx::Error> {
    let kind = row
        .try_get::<String, _>("attempt_kind")?
        .parse::<PaymentAttemptKind>()
        .map_err(|_| invalid_reconciliation_state())?;
    let plan_key = row
        .try_get::<Option<String>, _>("plan_key")?
        .map(PlanKey::new)
        .transpose()
        .map_err(|_| invalid_reconciliation_state())?;
    if (kind == PaymentAttemptKind::HostCharge) != plan_key.is_none() {
        return Err(invalid_reconciliation_state());
    }
    Ok(AttemptLocator {
        id: row.try_get("id")?,
        billing_scope_id: BillingScopeId::new(row.try_get("billing_scope_id")?),
        subscriber_id: SubscriberId::new(row.try_get("subscriber_id")?),
        plan_key,
        gateway_account_id: GatewayAccountId::new(row.try_get("gateway_account_id")?),
        kind,
    })
}

async fn lock_attempt_for_classification(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    locator: AttemptLocator,
) -> Result<Option<LockedAttempt>, sqlx::Error> {
    let row = sqlx::query(
        r#"
        SELECT id, billing_scope_id, subscriber_id, plan_key,
            gateway_account_id, attempt_kind, status, resolution_code,
            amount_cents,
            billing_canonical_gateway_transaction_id(
                gateway_transaction_id
            ) AS transaction_id
        FROM billing_payment_attempts
        WHERE id = $1
        FOR UPDATE SKIP LOCKED
        "#,
    )
    .bind(locator.id)
    .fetch_optional(&mut **transaction)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let locked_locator = attempt_locator_from_row(&row)?;
    if locked_locator != locator {
        return Err(invalid_reconciliation_state());
    }
    let status = row
        .try_get::<String, _>("status")?
        .parse::<PaymentAttemptStatus>()
        .map_err(|_| invalid_reconciliation_state())?;
    let resolution_code = row
        .try_get::<Option<String>, _>("resolution_code")?
        .as_deref()
        .map(PaymentResolutionCode::try_from)
        .transpose()
        .map_err(|_| invalid_reconciliation_state())?;
    Ok(Some(LockedAttempt {
        locator,
        status,
        resolution_code,
        amount_cents: row.try_get("amount_cents")?,
        transaction_id: row.try_get("transaction_id")?,
    }))
}

async fn lock_pending_charge_for_classification(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    charge_id: Uuid,
    attempt_id: Uuid,
) -> Result<Option<(ChargeRole, Option<String>, bool, bool)>, sqlx::Error> {
    let row = sqlx::query(
        r#"
        SELECT charges.charge_role,
            billing_canonical_gateway_transaction_id(
                charges.gateway_transaction_id
            ) AS transaction_id,
            CASE
                WHEN billing_canonical_gateway_transaction_id(
                    attempts.gateway_transaction_id
                ) IS NOT NULL
                    AND billing_canonical_gateway_transaction_id(
                        charges.gateway_transaction_id
                    ) IS NOT NULL
                THEN billing_canonical_gateway_transaction_id(
                    attempts.gateway_transaction_id
                ) = billing_canonical_gateway_transaction_id(
                    charges.gateway_transaction_id
                )
                WHEN billing_canonical_gateway_transaction_id(
                    attempts.gateway_transaction_id
                ) IS NULL
                    AND billing_canonical_gateway_transaction_id(
                        charges.gateway_transaction_id
                    ) IS NULL
                THEN attempts.gateway_order_id = charges.gateway_order_id
                    AND attempts.gateway_payment_method_reference
                        IS NOT DISTINCT FROM charges.gateway_payment_method_reference
                    AND attempts.gateway_response
                        IS NOT DISTINCT FROM charges.gateway_response
                    AND attempts.gateway_response_code
                        IS NOT DISTINCT FROM charges.gateway_response_code
                    AND attempts.gateway_response_text
                        IS NOT DISTINCT FROM charges.gateway_response_text
                    AND attempts.gateway_condition
                        IS NOT DISTINCT FROM charges.gateway_condition
                    AND attempts.payment_type IS NOT DISTINCT FROM charges.payment_type
                    AND attempts.card_brand IS NOT DISTINCT FROM charges.card_brand
                    AND attempts.card_last4 IS NOT DISTINCT FROM charges.card_last4
                    AND attempts.card_exp_month
                        IS NOT DISTINCT FROM charges.card_exp_month
                    AND attempts.card_exp_year
                        IS NOT DISTINCT FROM charges.card_exp_year
                ELSE false
            END AS same_charge,
            charges.attempt_id = attempts.id
                AND charges.billing_scope_id = attempts.billing_scope_id
                AND charges.gateway_account_id = attempts.gateway_account_id
                AND charges.gateway_order_id = attempts.gateway_order_id
                AND charges.attempt_kind = attempts.attempt_kind
                AND charges.plan_key IS NOT DISTINCT FROM attempts.plan_key
                AND charges.host_charge_target_id
                    IS NOT DISTINCT FROM attempts.host_charge_target_id
                AND charges.amount_cents = attempts.amount_cents
                AND charges.currency = attempts.currency AS dimensions_match
        FROM billing_processor_charges charges
        INNER JOIN billing_payment_attempts attempts
            ON attempts.id = charges.attempt_id
        WHERE charges.id = $1 AND attempts.id = $2
            AND charges.progression_state = 'pending'
        FOR UPDATE OF charges SKIP LOCKED
        "#,
    )
    .bind(charge_id)
    .bind(attempt_id)
    .fetch_optional(&mut **transaction)
    .await?;
    row.map(|row| {
        let role = match row.try_get::<String, _>("charge_role")?.as_str() {
            "primary" => ChargeRole::Primary,
            "additional" => ChargeRole::Additional,
            _ => return Err(invalid_reconciliation_state()),
        };
        Ok((
            role,
            row.try_get("transaction_id")?,
            row.try_get("same_charge")?,
            row.try_get("dimensions_match")?,
        ))
    })
    .transpose()
}

async fn classify_pending_charge(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    attempt: &LockedAttempt,
    charge_id: Uuid,
    role: ChargeRole,
    transaction_id: Option<&str>,
    same_charge: bool,
) -> Result<(ChargeProgression, Option<String>), sqlx::Error> {
    let Some(transaction_id) = transaction_id else {
        return Ok((
            ChargeProgression::ReconciliationRequired,
            Some("processor_charge_transaction_identity_required".to_owned()),
        ));
    };
    let attestation = sqlx::query_as::<_, (Uuid, String)>(
        r#"
        SELECT processor_charge_id, final_resolution_code
        FROM billing_external_reversal_attestations
        WHERE attempt_id = $1 AND gateway_transaction_id = $2
        FOR UPDATE
        "#,
    )
    .bind(attempt.locator.id)
    .bind(transaction_id)
    .fetch_optional(&mut **transaction)
    .await?;
    if let Some((attested_charge_id, final_resolution_code)) = attestation {
        if attested_charge_id != charge_id {
            return Err(invalid_reconciliation_state());
        }
        let final_resolution_code = PaymentResolutionCode::try_from(final_resolution_code.as_str())
            .map_err(|_| invalid_reconciliation_state())?;
        return Ok((
            ChargeProgression::ExternallyReversed,
            Some(final_resolution_code.as_str().to_owned()),
        ));
    }

    let terminal_external_reversal = attempt.status == PaymentAttemptStatus::Failed
        && matches!(
            attempt.resolution_code,
            Some(
                PaymentResolutionCode::SubscriptionInitialExternallyRefunded
                    | PaymentResolutionCode::SubscriptionInitialExternallyVoided
                    | PaymentResolutionCode::ProcessorChargeExternallyRefunded
                    | PaymentResolutionCode::ProcessorChargeExternallyVoided
            )
        );
    let initial_grant_conflict = attempt.locator.kind == PaymentAttemptKind::SubscriptionInitial
        && attempt.resolution_code
            == Some(PaymentResolutionCode::SubscriptionInitialCurrentGrantConflict);
    if attempt.amount_cents > 0
        && (role == ChargeRole::Additional
            || terminal_external_reversal
            || initial_grant_conflict
            || (attempt.transaction_id.is_some() && !same_charge))
    {
        return Ok((
            ChargeProgression::ExternalReversalRequired,
            Some(
                if role == ChargeRole::Additional {
                    "additional_approved_charge_identified"
                } else {
                    "processor_charge_external_reversal_required"
                }
                .to_owned(),
            ),
        ));
    }
    if same_charge && attempt.status == PaymentAttemptStatus::Approved {
        return Ok((ChargeProgression::Applied, None));
    }
    Ok((
        ChargeProgression::ReconciliationRequired,
        Some(
            if role == ChargeRole::Additional {
                "zero_amount_additional_approved_charge"
            } else {
                "approved_charge_waiting_for_application"
            }
            .to_owned(),
        ),
    ))
}

async fn transition_pending_charge(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    charge_id: Uuid,
    progression: ChargeProgression,
    state_code: Option<&str>,
) -> Result<(), sqlx::Error> {
    let result = sqlx::query(
        r#"
        UPDATE billing_processor_charges
        SET progression_state = $2,
            state_code = $3,
            reconciliation_required_at = CASE
                WHEN $2 = 'reconciliation_required'
                THEN COALESCE(reconciliation_required_at, clock_timestamp())
            END,
            external_reversal_required_at = CASE
                WHEN $2 = 'external_reversal_required'
                THEN COALESCE(external_reversal_required_at, clock_timestamp())
            END,
            applied_at = CASE WHEN $2 = 'applied'
                THEN COALESCE(applied_at, clock_timestamp()) END,
            externally_reversed_at = CASE WHEN $2 = 'externally_reversed'
                THEN COALESCE(externally_reversed_at, clock_timestamp()) END,
            updated_at = clock_timestamp()
        WHERE id = $1 AND progression_state = 'pending'
            AND (
                $2 NOT IN ('external_reversal_required', 'externally_reversed')
                OR (
                    billing_canonical_gateway_transaction_id(
                        gateway_transaction_id
                    ) IS NOT NULL
                    AND amount_cents > 0
                    AND attempt_kind <> 'subscription_payment_method_update'
                )
            )
            AND (
                $2 <> 'applied'
                OR (
                    charge_role = 'primary'
                    AND billing_canonical_gateway_transaction_id(
                        gateway_transaction_id
                    ) IS NOT NULL
                )
            )
        "#,
    )
    .bind(charge_id)
    .bind(progression.as_str())
    .bind(state_code)
    .execute(&mut **transaction)
    .await?;
    if result.rows_affected() != 1 {
        return Err(invalid_reconciliation_state());
    }
    Ok(())
}

fn invalid_reconciliation_state() -> sqlx::Error {
    sqlx::Error::Protocol("canonical reconciliation state is invalid".to_owned())
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use syrup_rail::{
        BillingScopeId, GatewayAccountId, GatewayAccountRegistration, GatewayConfigurationId,
        GatewayProviderKey, PaymentAttemptKind,
    };
    use uuid::Uuid;

    use super::{
        ExactQueryObservation, RECONCILIATION_PHASE_BATCH_SIZE, apply_exact_query_observation,
        claim_exact_reconciliation_attempts, classify_pending_processor_charges,
        fail_stale_unsubmitted_payment_method_replacements,
        fail_stale_unsubmitted_subscription_enrollments, reconciliation_gateway_accounts,
    };
    use crate::{
        register_gateway_account,
        test_support::{TestDatabase, create_gateway_account},
    };

    #[tokio::test]
    async fn reconciliation_candidate_scan_is_complete_unbounded_and_deterministic()
    -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("sr_recon_scan").await?;
        let result = async {
            let provider = GatewayProviderKey::new("test_gateway")?;
            let mut expected = Vec::new();
            let mut transaction = database.pool.begin().await?;
            for position in (0_u128..101).rev() {
                let scope = BillingScopeId::new(Uuid::from_u128(1 + position));
                let account = GatewayAccountId::new(Uuid::from_u128(2_000 - position));
                register_gateway_account(
                    &mut transaction,
                    &GatewayAccountRegistration::new(
                        scope,
                        account,
                        provider.clone(),
                        GatewayConfigurationId::new(Uuid::from_u128(2_000 + position)),
                    ),
                )
                .await?;
                expected.push((scope, account));
            }
            transaction.commit().await?;
            expected.sort_unstable();

            let candidates = reconciliation_gateway_accounts(&database.pool).await?;
            let actual: Vec<_> = candidates
                .into_iter()
                .map(|candidate| (candidate.billing_scope_id(), candidate.gateway_account_id()))
                .collect();

            assert_eq!(actual.len(), 101);
            assert_eq!(actual, expected);
            Ok::<_, Box<dyn Error>>(())
        }
        .await;
        let cleanup = database.cleanup().await;
        result?;
        cleanup
    }

    #[tokio::test]
    async fn exact_attempt_claim_is_canonical_bounded_and_account_scoped()
    -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("sr_exact_claim").await?;
        let result = async {
            let account = create_gateway_account(&database.pool, "test_gateway").await?;
            let sibling = create_gateway_account(&database.pool, "test_gateway").await?;
            let subscriber_id = Uuid::now_v7();
            let first =
                insert_stale_enrollment(&database.pool, account, subscriber_id, "plan_a").await?;
            let second =
                insert_stale_enrollment(&database.pool, account, subscriber_id, "plan_b").await?;
            for position in 2..=RECONCILIATION_PHASE_BATCH_SIZE {
                insert_stale_enrollment(
                    &database.pool,
                    account,
                    Uuid::now_v7(),
                    &format!("plan_{position}"),
                )
                .await?;
            }
            let sibling_attempt =
                insert_stale_enrollment(&database.pool, sibling, Uuid::now_v7(), "sibling_plan")
                    .await?;

            let claimed = claim_exact_reconciliation_attempts(
                &database.pool,
                GatewayAccountId::new(account.gateway_account_id),
            )
            .await?;
            assert_eq!(claimed.len(), RECONCILIATION_PHASE_BATCH_SIZE as usize);
            assert_eq!(*claimed[0].identity().attempt_id().as_uuid(), first);
            assert_eq!(*claimed[1].identity().attempt_id().as_uuid(), second);
            assert_eq!(claimed[0].kind(), PaymentAttemptKind::SubscriptionInitial);
            assert_eq!(
                claimed[0]
                    .request()
                    .target()
                    .plan_key()
                    .map(|key| key.as_str()),
                Some("plan_a"),
            );
            assert!(claimed.iter().all(|attempt| {
                *attempt.identity().gateway_account_id().as_uuid() == account.gateway_account_id
            }));
            assert!(
                apply_exact_query_observation(
                    &database.pool,
                    &claimed[0],
                    ExactQueryObservation::NoTransaction,
                )
                .await?
            );
            assert_eq!(
                attempt_status(&database.pool, first).await?,
                "review_required"
            );
            assert!(
                apply_exact_query_observation(
                    &database.pool,
                    &claimed[1],
                    ExactQueryObservation::MalformedResponse,
                )
                .await?
            );
            assert_eq!(
                attempt_status(&database.pool, second).await?,
                "review_required"
            );

            let remainder = claim_exact_reconciliation_attempts(
                &database.pool,
                GatewayAccountId::new(account.gateway_account_id),
            )
            .await?;
            assert_eq!(remainder.len(), 1);
            assert_eq!(
                *claim_exact_reconciliation_attempts(
                    &database.pool,
                    GatewayAccountId::new(sibling.gateway_account_id),
                )
                .await?[0]
                    .identity()
                    .attempt_id()
                    .as_uuid(),
                sibling_attempt,
            );
            Ok::<_, Box<dyn Error>>(())
        }
        .await;
        let cleanup = database.cleanup().await;
        result?;
        cleanup
    }

    #[tokio::test]
    async fn stale_payment_method_replacement_cleanup_is_bounded_and_account_scoped()
    -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("sr_recon_method").await?;
        let result = async {
            let account = create_gateway_account(&database.pool, "test_gateway").await?;
            let sibling = create_gateway_account(&database.pool, "test_gateway").await?;
            for position in 0..=RECONCILIATION_PHASE_BATCH_SIZE {
                insert_stale_payment_method_replacement(
                    &database.pool,
                    account.billing_scope_id,
                    account.gateway_account_id,
                    account.gateway_configuration_id,
                    position,
                )
                .await?;
            }
            insert_stale_payment_method_replacement(
                &database.pool,
                sibling.billing_scope_id,
                sibling.gateway_account_id,
                sibling.gateway_configuration_id,
                10_000,
            )
            .await?;

            assert_eq!(
                fail_stale_unsubmitted_payment_method_replacements(
                    &database.pool,
                    GatewayAccountId::new(account.gateway_account_id),
                )
                .await?,
                RECONCILIATION_PHASE_BATCH_SIZE as u64,
            );
            assert_eq!(
                fail_stale_unsubmitted_payment_method_replacements(
                    &database.pool,
                    GatewayAccountId::new(account.gateway_account_id),
                )
                .await?,
                1,
            );
            let sibling_status: String = sqlx::query_scalar(
                "SELECT status FROM billing_payment_attempts WHERE gateway_account_id = $1",
            )
            .bind(sibling.gateway_account_id)
            .fetch_one(&database.pool)
            .await?;
            assert_eq!(sibling_status, "pending");
            Ok::<_, Box<dyn Error>>(())
        }
        .await;
        let cleanup = database.cleanup().await;
        result?;
        cleanup
    }

    async fn insert_stale_payment_method_replacement(
        pool: &sqlx::PgPool,
        billing_scope_id: Uuid,
        gateway_account_id: Uuid,
        gateway_configuration_id: Uuid,
        position: i64,
    ) -> Result<(), sqlx::Error> {
        let subscriber_id = Uuid::now_v7();
        let payment_method_id = Uuid::now_v7();
        let subscription_id = Uuid::now_v7();
        let attempt_id = Uuid::now_v7();
        let transaction_id = format!("txn{position}ref");
        sqlx::query(
            r#"
            INSERT INTO billing_payment_methods (
                id, billing_scope_id, subscriber_id, gateway_account_id,
                gateway_payment_method_reference, status
            ) VALUES ($1, $2, $3, $4, $5, 'active')
            "#,
        )
        .bind(payment_method_id)
        .bind(billing_scope_id)
        .bind(subscriber_id)
        .bind(gateway_account_id)
        .bind(format!("method{position}ref"))
        .execute(pool)
        .await?;
        sqlx::query(
            r#"
            WITH clock AS MATERIALIZED (
                SELECT clock_timestamp() AS observed_at
            )
            INSERT INTO billing_subscriptions (
                id, billing_scope_id, subscriber_id, plan_key, status,
                gateway_account_id, payment_method_id, amount_cents, currency,
                current_period_start_at, current_period_end_at, next_renewal_at,
                initial_transaction_id
            ) SELECT
                $1, $2, $3, 'test_plan', 'active', $4, $5, 100, 'USD',
                observed_at - interval '1 day',
                observed_at + interval '1 day',
                observed_at + interval '1 day', $6
            FROM clock
            "#,
        )
        .bind(subscription_id)
        .bind(billing_scope_id)
        .bind(subscriber_id)
        .bind(gateway_account_id)
        .bind(payment_method_id)
        .bind(&transaction_id)
        .execute(pool)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO billing_payment_attempts (
                id, billing_scope_id, subscriber_id, plan_key, subscription_id,
                payment_method_id, attempt_kind, status, idempotency_key,
                request_fingerprint, amount_cents, currency, gateway_account_id,
                gateway_configuration_id, gateway_order_id,
                payment_method_update_expected_payment_method_id,
                payment_method_update_expected_initial_transaction_id, created_at,
                updated_at
            ) VALUES (
                $1, $2, $3, 'test_plan', $4, $5,
                'subscription_payment_method_update', 'pending', $6, $7, 0,
                'USD', $8, $9, $10, $5, $11,
                clock_timestamp() - interval '4 minutes',
                clock_timestamp() - interval '4 minutes'
            )
            "#,
        )
        .bind(attempt_id)
        .bind(billing_scope_id)
        .bind(subscriber_id)
        .bind(subscription_id)
        .bind(payment_method_id)
        .bind(format!("idem{position}"))
        .bind(format!("fingerprint{position}"))
        .bind(gateway_account_id)
        .bind(gateway_configuration_id)
        .bind(format!("order{position}ref"))
        .bind(transaction_id)
        .execute(pool)
        .await?;
        Ok(())
    }

    #[tokio::test]
    async fn stale_enrollment_cleanup_uses_the_persisted_plan_lock_and_account_scope()
    -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("sr_recon_initial").await?;
        let result = async {
            let account = create_gateway_account(&database.pool, "test_gateway").await?;
            let sibling = create_gateway_account(&database.pool, "test_gateway").await?;
            let subscriber_id = Uuid::now_v7();
            let plan_a = "plan_a";
            let plan_b = "plan_b";
            let plan_a_attempt =
                insert_stale_enrollment(&database.pool, account, subscriber_id, plan_a).await?;
            let plan_b_attempt =
                insert_stale_enrollment(&database.pool, account, subscriber_id, plan_b).await?;
            let sibling_attempt =
                insert_stale_enrollment(&database.pool, sibling, Uuid::now_v7(), "plan_c").await?;

            let mut lock_holder = database.pool.begin().await?;
            sqlx::query(
                "SELECT pg_advisory_xact_lock(hashtextextended($1::uuid::text || ':' || $2, 0))",
            )
            .bind(subscriber_id)
            .bind(plan_a)
            .execute(&mut *lock_holder)
            .await?;

            assert_eq!(
                fail_stale_unsubmitted_subscription_enrollments(
                    &database.pool,
                    GatewayAccountId::new(account.gateway_account_id),
                )
                .await?,
                1,
            );
            assert_eq!(
                attempt_status(&database.pool, plan_a_attempt).await?,
                "pending"
            );
            assert_eq!(
                attempt_status(&database.pool, plan_b_attempt).await?,
                "failed"
            );
            assert_eq!(
                attempt_status(&database.pool, sibling_attempt).await?,
                "pending"
            );

            lock_holder.rollback().await?;
            assert_eq!(
                fail_stale_unsubmitted_subscription_enrollments(
                    &database.pool,
                    GatewayAccountId::new(account.gateway_account_id),
                )
                .await?,
                1,
            );
            assert_eq!(
                attempt_status(&database.pool, plan_a_attempt).await?,
                "failed"
            );
            Ok::<_, Box<dyn Error>>(())
        }
        .await;
        let cleanup = database.cleanup().await;
        result?;
        cleanup
    }

    #[tokio::test]
    async fn pending_charge_classification_skips_a_busy_persisted_plan_without_starvation()
    -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("sr_charge_lock").await?;
        let result = async {
            let account = create_gateway_account(&database.pool, "test_gateway").await?;
            let sibling = create_gateway_account(&database.pool, "test_gateway").await?;
            let subscriber_id = Uuid::now_v7();
            let plan_a_charge = insert_pending_processor_charge(
                &database.pool,
                account,
                subscriber_id,
                "plan_a",
                30,
            )
            .await?;
            let plan_b_charge = insert_pending_processor_charge(
                &database.pool,
                account,
                subscriber_id,
                "plan_b",
                20,
            )
            .await?;
            let sibling_charge = insert_pending_processor_charge(
                &database.pool,
                sibling,
                Uuid::now_v7(),
                "plan_c",
                10,
            )
            .await?;

            let mut lock_holder = database.pool.begin().await?;
            sqlx::query(
                "SELECT pg_advisory_xact_lock(hashtextextended($1::uuid::text || ':' || $2, 0))",
            )
            .bind(subscriber_id)
            .bind("plan_a")
            .execute(&mut *lock_holder)
            .await?;

            let summary = classify_pending_processor_charges(
                &database.pool,
                GatewayAccountId::new(account.gateway_account_id),
                1,
            )
            .await?;
            assert_eq!(summary.transitioned(), 1);
            assert_eq!(summary.skipped_locked(), 1);
            assert_eq!(summary.remaining_pending(), 1);
            assert_eq!(
                charge_progression(&database.pool, plan_a_charge).await?,
                "pending"
            );
            assert_eq!(
                charge_progression(&database.pool, plan_b_charge).await?,
                "reconciliation_required"
            );
            assert_eq!(
                charge_progression(&database.pool, sibling_charge).await?,
                "pending"
            );

            lock_holder.rollback().await?;
            let summary = classify_pending_processor_charges(
                &database.pool,
                GatewayAccountId::new(account.gateway_account_id),
                1,
            )
            .await?;
            assert_eq!(summary.transitioned(), 1);
            assert_eq!(summary.skipped_locked(), 0);
            assert_eq!(summary.remaining_pending(), 0);
            assert_eq!(
                charge_progression(&database.pool, plan_a_charge).await?,
                "reconciliation_required"
            );
            Ok::<_, Box<dyn Error>>(())
        }
        .await;
        let cleanup = database.cleanup().await;
        result?;
        cleanup
    }

    #[tokio::test]
    async fn pending_charge_classification_caps_each_account_pass_at_one_hundred()
    -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("sr_charge_bound").await?;
        let result = async {
            let account = create_gateway_account(&database.pool, "test_gateway").await?;
            let sibling = create_gateway_account(&database.pool, "test_gateway").await?;
            for position in 0..=RECONCILIATION_PHASE_BATCH_SIZE {
                insert_pending_processor_charge(
                    &database.pool,
                    account,
                    Uuid::now_v7(),
                    "test_plan",
                    position,
                )
                .await?;
            }
            let sibling_charge = insert_pending_processor_charge(
                &database.pool,
                sibling,
                Uuid::now_v7(),
                "test_plan",
                10_000,
            )
            .await?;

            let first = classify_pending_processor_charges(
                &database.pool,
                GatewayAccountId::new(account.gateway_account_id),
                u64::MAX,
            )
            .await?;
            assert_eq!(first.transitioned(), RECONCILIATION_PHASE_BATCH_SIZE as u64);
            assert_eq!(first.remaining_pending(), 1);
            let second = classify_pending_processor_charges(
                &database.pool,
                GatewayAccountId::new(account.gateway_account_id),
                u64::MAX,
            )
            .await?;
            assert_eq!(second.transitioned(), 1);
            assert_eq!(second.remaining_pending(), 0);
            assert_eq!(
                charge_progression(&database.pool, sibling_charge).await?,
                "pending"
            );
            Ok::<_, Box<dyn Error>>(())
        }
        .await;
        let cleanup = database.cleanup().await;
        result?;
        cleanup
    }

    async fn insert_pending_processor_charge(
        pool: &sqlx::PgPool,
        account: crate::test_support::GatewayAccountFixture,
        subscriber_id: Uuid,
        plan_key: &str,
        age_seconds: i64,
    ) -> Result<Uuid, sqlx::Error> {
        let attempt_id = Uuid::now_v7();
        let charge_id = Uuid::now_v7();
        let order_id = format!("order-{attempt_id}");
        sqlx::query(
            r#"
            INSERT INTO billing_payment_attempts (
                id, billing_scope_id, subscriber_id, plan_key, attempt_kind,
                status, idempotency_key, request_fingerprint, amount_cents,
                currency, gateway_account_id, gateway_configuration_id,
                gateway_order_id
            ) VALUES (
                $1, $2, $3, $4, 'subscription_initial', 'pending', $5, $6,
                100, 'USD', $7, $8, $9
            )
            "#,
        )
        .bind(attempt_id)
        .bind(account.billing_scope_id)
        .bind(subscriber_id)
        .bind(plan_key)
        .bind(format!("idem-{attempt_id}"))
        .bind(format!("fingerprint-{attempt_id}"))
        .bind(account.gateway_account_id)
        .bind(account.gateway_configuration_id)
        .bind(&order_id)
        .execute(pool)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO billing_processor_charges (
                id, attempt_id, billing_scope_id, gateway_account_id,
                gateway_order_id, gateway_transaction_id, gateway_response,
                gateway_response_code, gateway_response_text,
                gateway_condition, charge_role, progression_state,
                observed_at, attempt_kind, plan_key, amount_cents, currency
            ) VALUES (
                $1, $2, $3, $4, $5, $6, '1', '100', 'Approved',
                'complete', 'primary', 'pending',
                clock_timestamp() - ($7::bigint * interval '1 second'),
                'subscription_initial', $8, 100, 'USD'
            )
            "#,
        )
        .bind(charge_id)
        .bind(attempt_id)
        .bind(account.billing_scope_id)
        .bind(account.gateway_account_id)
        .bind(order_id)
        .bind(format!("transaction-{attempt_id}"))
        .bind(age_seconds)
        .bind(plan_key)
        .execute(pool)
        .await?;
        Ok(charge_id)
    }

    async fn charge_progression(
        pool: &sqlx::PgPool,
        charge_id: Uuid,
    ) -> Result<String, sqlx::Error> {
        sqlx::query_scalar("SELECT progression_state FROM billing_processor_charges WHERE id = $1")
            .bind(charge_id)
            .fetch_one(pool)
            .await
    }

    async fn insert_stale_enrollment(
        pool: &sqlx::PgPool,
        account: crate::test_support::GatewayAccountFixture,
        subscriber_id: Uuid,
        plan_key: &str,
    ) -> Result<Uuid, sqlx::Error> {
        let attempt_id = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO billing_payment_attempts (
                id, billing_scope_id, subscriber_id, plan_key, attempt_kind,
                status, idempotency_key, request_fingerprint, amount_cents,
                currency, gateway_account_id, gateway_configuration_id,
                gateway_order_id, created_at, updated_at
            ) VALUES (
                $1, $2, $3, $4, 'subscription_initial', 'pending', $5, $6,
                100, 'USD', $7, $8, $9,
                clock_timestamp() - interval '31 minutes',
                clock_timestamp() - interval '31 minutes'
            )
            "#,
        )
        .bind(attempt_id)
        .bind(account.billing_scope_id)
        .bind(subscriber_id)
        .bind(plan_key)
        .bind(format!("idem-{attempt_id}"))
        .bind(format!("fingerprint-{attempt_id}"))
        .bind(account.gateway_account_id)
        .bind(account.gateway_configuration_id)
        .bind(format!("order_{}", attempt_id.simple()))
        .execute(pool)
        .await?;
        Ok(attempt_id)
    }

    async fn attempt_status(pool: &sqlx::PgPool, attempt_id: Uuid) -> Result<String, sqlx::Error> {
        sqlx::query_scalar("SELECT status FROM billing_payment_attempts WHERE id = $1")
            .bind(attempt_id)
            .fetch_one(pool)
            .await
    }
}
