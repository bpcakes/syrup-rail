use std::fmt;

use chrono::{DateTime, Utc};
use sqlx::{Executor, PgConnection, PgPool, Postgres, Row, Transaction, postgres::PgRow};
use syrup_rail::{
    ActorId, AppliedSubscriptionDiscount, BillingPeriod, ChargeAmount, CurrencyCode,
    DiscountClaimId, Entitlement, EntitlementGuard, EntitlementQuery, LimitedDiscountMonths,
    MissingSubscriptionAction, PastDueAccess, PastDueAccessPolicy, PastDueAction, PaymentMethodId,
    PercentOffBasisPoints, PositiveDiscountCents, SavedSubscriptionDiscount, Subscription,
    SubscriptionDiscountCode, SubscriptionDiscountDuration, SubscriptionDiscountKind,
    SubscriptionDiscountSnapshot, SubscriptionGrant, SubscriptionGrantId, SubscriptionGrantKind,
    SubscriptionId, SubscriptionPhase, SubscriptionStatus, classify_past_due_access,
};
use thiserror::Error;
use uuid::Uuid;

use crate::attempts::{
    INITIAL_PREPARED_STALE_AFTER_SECONDS, SUBSCRIPTION_CHARGE_UNSUBMITTED_STALE_AFTER_SECONDS,
};
use crate::subscription_persistence::{
    RenewalFailurePolicyScalars, SubscriptionPeriodRuleScalars, SubscriptionPersistenceCodecError,
    renewal_failure_policy_from_scalars, subscription_period_rule_from_scalars,
};

const INVALID_ENTITLEMENT_STATE: &str =
    "canonical subscription state cannot be represented as one entitlement";
const ENTITLEMENT_GUARD_LOCK_TIMEOUT: &str = "250ms";

type GuardGrantTimeState = (DateTime<Utc>, DateTime<Utc>, Option<DateTime<Utc>>);
type GuardSubscriptionState = (String, DateTime<Utc>, String, Option<DateTime<Utc>>);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GuardAccess {
    Missing,
    Paid,
    PaidThroughCancellation,
    PastDue,
    Granted,
    Invalid,
}

#[derive(Debug, Error)]
pub enum EntitlementQueryError {
    #[error("subscription entitlement query failed")]
    Sql(#[from] sqlx::Error),
    #[error("{0}")]
    InvalidState(&'static str),
}

#[derive(Debug, Error)]
pub enum EntitlementGuardError {
    #[error("subscription entitlement guard failed")]
    Sql(#[from] sqlx::Error),
    #[error("subscription entitlement is required")]
    Required,
    #[error("subscription entitlement is past due")]
    PastDue,
    #[error("{0}")]
    InvalidState(&'static str),
}

/// A top-level PostgreSQL transaction awaiting entitlement admission.
///
/// Create this transaction directly from a pool with [`Self::begin`]. It can
/// carry host preparatory writes, but deliberately has no commit operation.
/// Passing it to [`require_entitlement_for_update`] either rolls it back or
/// transforms it into an [`AdmittedEntitlementWriteTransaction`].
pub struct EntitlementWriteTransaction {
    inner: Transaction<'static, Postgres>,
}

impl fmt::Debug for EntitlementWriteTransaction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EntitlementWriteTransaction")
            .finish_non_exhaustive()
    }
}

impl EntitlementWriteTransaction {
    /// Starts a top-level transaction reserved for an entitlement-protected write.
    pub async fn begin(pool: &PgPool) -> Result<Self, sqlx::Error> {
        Ok(Self {
            inner: pool.begin().await?,
        })
    }

    /// Borrows the transaction connection for preparatory host database work.
    ///
    /// Callers must finish any nested savepoint before returning this value to
    /// Syrup Rail. Leaking a savepoint would violate SQLx's transaction
    /// lifecycle contract.
    pub fn connection(&mut self) -> &mut PgConnection {
        &mut self.inner
    }

    /// Explicitly rolls back the pending transaction.
    pub async fn rollback(self) -> Result<(), sqlx::Error> {
        self.inner.rollback().await
    }
}

/// A top-level transaction that passed entitlement admission.
///
/// Its entitlement rows and aggregate advisory lock remain held until this
/// value is committed or rolled back. The protected host mutation must use
/// [`Self::connection`] on this value.
pub struct AdmittedEntitlementWriteTransaction {
    inner: Transaction<'static, Postgres>,
}

impl fmt::Debug for AdmittedEntitlementWriteTransaction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AdmittedEntitlementWriteTransaction")
            .finish_non_exhaustive()
    }
}

impl AdmittedEntitlementWriteTransaction {
    /// Borrows the admitted transaction connection for the host-owned protected mutation.
    ///
    /// Callers must finish any nested savepoint before committing or rolling
    /// back this outer transaction. Leaking a savepoint would violate SQLx's
    /// transaction lifecycle contract.
    pub fn connection(&mut self) -> &mut PgConnection {
        &mut self.inner
    }

    /// Commits the protected transaction and releases its entitlement locks.
    pub async fn commit(self) -> Result<(), sqlx::Error> {
        self.inner.commit().await
    }

    /// Rolls back the protected transaction and releases its entitlement locks.
    pub async fn rollback(self) -> Result<(), sqlx::Error> {
        self.inner.rollback().await
    }
}

fn map_subscription_persistence_error(
    error: SubscriptionPersistenceCodecError,
) -> EntitlementQueryError {
    match error {
        SubscriptionPersistenceCodecError::RowRead(error) => EntitlementQueryError::Sql(error),
        SubscriptionPersistenceCodecError::InvalidState => {
            EntitlementQueryError::InvalidState(INVALID_ENTITLEMENT_STATE)
        }
    }
}

/// Locks and revalidates one exact entitlement inside a top-level transaction.
///
/// This function consumes the transaction and returns it only after successful
/// admission. The returned transaction retains the accepted paid or grant rows
/// and their aggregate advisory domain until the caller commits or rolls back
/// its protected mutation. Every completed denial or storage failure awaits a
/// full rollback. Canceling the future drops the owned transaction and queues a
/// full rollback, so a caller cannot continue an unguarded write.
///
/// The guard temporarily applies a 250 millisecond `lock_timeout` and restores
/// the caller's prior transaction-local value before returning successfully.
pub async fn require_entitlement_for_update(
    transaction: EntitlementWriteTransaction,
    guard: &EntitlementGuard,
) -> Result<AdmittedEntitlementWriteTransaction, EntitlementGuardError> {
    require_entitlement_for_update_with_lock_timeout(
        transaction,
        guard,
        ENTITLEMENT_GUARD_LOCK_TIMEOUT,
    )
    .await
}

async fn require_entitlement_for_update_with_lock_timeout(
    transaction: EntitlementWriteTransaction,
    guard: &EntitlementGuard,
    lock_timeout: &str,
) -> Result<AdmittedEntitlementWriteTransaction, EntitlementGuardError> {
    let mut transaction = transaction.inner;
    let admission = async {
        let previous_lock_timeout: String =
            sqlx::query_scalar("SELECT current_setting('lock_timeout', true)")
                .fetch_one(&mut *transaction)
                .await?;
        sqlx::query("SELECT set_config('lock_timeout', $1, true)")
            .bind(lock_timeout)
            .execute(&mut *transaction)
            .await?;
        let access = lock_and_classify_entitlement(&mut transaction, guard).await?;
        Ok::<_, sqlx::Error>((access, previous_lock_timeout))
    }
    .await;
    let (access, previous_lock_timeout) = match admission {
        Ok(admission) => admission,
        Err(error) => {
            return rollback_guard_failure(transaction, EntitlementGuardError::Sql(error)).await;
        }
    };

    match access {
        GuardAccess::Paid | GuardAccess::PaidThroughCancellation | GuardAccess::Granted => {
            if let Err(error) = sqlx::query("SELECT set_config('lock_timeout', $1, true)")
                .bind(previous_lock_timeout)
                .execute(&mut *transaction)
                .await
            {
                return rollback_guard_failure(transaction, EntitlementGuardError::Sql(error))
                    .await;
            }
            Ok(AdmittedEntitlementWriteTransaction { inner: transaction })
        }
        GuardAccess::PastDue => {
            rollback_guard_failure(transaction, EntitlementGuardError::PastDue).await
        }
        GuardAccess::Missing => {
            rollback_guard_failure(transaction, EntitlementGuardError::Required).await
        }
        GuardAccess::Invalid => {
            rollback_guard_failure(
                transaction,
                EntitlementGuardError::InvalidState(INVALID_ENTITLEMENT_STATE),
            )
            .await
        }
    }
}

async fn rollback_guard_failure(
    transaction: Transaction<'static, Postgres>,
    error: EntitlementGuardError,
) -> Result<AdmittedEntitlementWriteTransaction, EntitlementGuardError> {
    match transaction.rollback().await {
        Ok(()) => Err(error),
        Err(rollback_error) => Err(EntitlementGuardError::Sql(rollback_error)),
    }
}

async fn lock_and_classify_entitlement(
    connection: &mut PgConnection,
    guard: &EntitlementGuard,
) -> Result<GuardAccess, sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::uuid::text || ':' || $2, 0))")
        .bind(guard.subscriber_id().as_uuid())
        .bind(guard.plan_key().as_str())
        .execute(&mut *connection)
        .await?;

    let subscriptions = sqlx::query_as::<_, GuardSubscriptionState>(
        r#"
        SELECT status, current_period_end_at, past_due_access, next_payment_attempt_at
        FROM billing_subscriptions
        WHERE billing_scope_id = $1
            AND subscriber_id = $2
            AND plan_key = $3
        ORDER BY id
        FOR SHARE
        "#,
    )
    .bind(guard.billing_scope_id().as_uuid())
    .bind(guard.subscriber_id().as_uuid())
    .bind(guard.plan_key().as_str())
    .fetch_all(&mut *connection)
    .await?;
    let grants = sqlx::query_as::<_, GuardGrantTimeState>(
        r#"
        SELECT starts_at, ends_at, revoked_at
        FROM billing_subscription_grants
        WHERE billing_scope_id = $1
            AND subscriber_id = $2
            AND plan_key = $3
        ORDER BY id
        FOR SHARE
        "#,
    )
    .bind(guard.billing_scope_id().as_uuid())
    .bind(guard.subscriber_id().as_uuid())
    .bind(guard.plan_key().as_str())
    .fetch_all(&mut *connection)
    .await?;
    let access_at: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *connection)
        .await?;

    Ok(classify_guard_access(&subscriptions, &grants, access_at))
}

fn classify_guard_access(
    subscriptions: &[GuardSubscriptionState],
    grants: &[GuardGrantTimeState],
    access_at: DateTime<Utc>,
) -> GuardAccess {
    let current_paid = subscriptions
        .iter()
        .filter(|(status, current_period_end_at, _, _)| {
            matches!(status.as_str(), "active" | "past_due")
                || (status == "canceled" && *current_period_end_at > access_at)
        })
        .collect::<Vec<_>>();
    let active_grant_count = grants
        .iter()
        .filter(|(starts_at, ends_at, revoked_at)| {
            *starts_at <= access_at && *ends_at > access_at && revoked_at.is_none()
        })
        .count();
    if current_paid.len() > 1
        || active_grant_count > 1
        || (!current_paid.is_empty() && active_grant_count > 0)
    {
        return GuardAccess::Invalid;
    }
    if active_grant_count == 1 {
        return GuardAccess::Granted;
    }
    let Some((status, _, past_due_access, next_payment_attempt_at)) = current_paid.first() else {
        return GuardAccess::Missing;
    };
    match status.as_str() {
        "active" => GuardAccess::Paid,
        "canceled" => GuardAccess::PaidThroughCancellation,
        "past_due" => match past_due_access.parse::<PastDueAccessPolicy>() {
            Ok(policy)
                if classify_past_due_access(policy, next_payment_attempt_at.is_some())
                    == PastDueAccess::AllowedDuringDunning =>
            {
                GuardAccess::Paid
            }
            Ok(_) => GuardAccess::PastDue,
            Err(_) => GuardAccess::Invalid,
        },
        _ => GuardAccess::Invalid,
    }
}

/// Loads one exact scope/subscriber/plan entitlement from a single database snapshot.
///
/// Gateway availability and host authentication are intentionally outside this query.
pub async fn entitlement(
    pool: &PgPool,
    query: &EntitlementQuery,
) -> Result<Entitlement, EntitlementQueryError> {
    entitlement_on_executor(pool, query).await
}

/// Runs the canonical entitlement projection on a caller-owned connection.
///
/// This is crate-visible so a composite read can retain the exact entitlement
/// semantics while sharing one PostgreSQL snapshot with its other projections.
pub(crate) async fn entitlement_on_connection(
    connection: &mut PgConnection,
    query: &EntitlementQuery,
) -> Result<Entitlement, EntitlementQueryError> {
    entitlement_on_executor(connection, query).await
}

async fn entitlement_on_executor<'e, E>(
    executor: E,
    query: &EntitlementQuery,
) -> Result<Entitlement, EntitlementQueryError>
where
    E: Executor<'e, Database = Postgres>,
{
    let row = sqlx::query(
        r#"
        WITH clock AS MATERIALIZED (
            SELECT clock_timestamp() AS observed_at
        ),
        paid_candidates AS MATERIALIZED (
            SELECT subscriptions.*
            FROM billing_subscriptions subscriptions
            CROSS JOIN clock
            WHERE subscriptions.billing_scope_id = $1
                AND subscriptions.subscriber_id = $2
                AND subscriptions.plan_key = $3
                AND (
                    subscriptions.status IN ('active', 'past_due')
                    OR (
                        subscriptions.status = 'canceled'
                        AND subscriptions.current_period_end_at > clock.observed_at
                    )
                )
        ),
        active_grants AS MATERIALIZED (
            SELECT grants.*
            FROM billing_subscription_grants grants
            CROSS JOIN clock
            WHERE grants.billing_scope_id = $1
                AND grants.subscriber_id = $2
                AND grants.plan_key = $3
                AND grants.starts_at <= clock.observed_at
                AND grants.ends_at > clock.observed_at
                AND grants.revoked_at IS NULL
        ),
        paid AS MATERIALIZED (
            SELECT *
            FROM paid_candidates
            ORDER BY
                CASE status WHEN 'active' THEN 0 WHEN 'past_due' THEN 1 ELSE 2 END,
                updated_at DESC,
                id DESC
            LIMIT 1
        ),
        active_grant AS MATERIALIZED (
            SELECT *
            FROM active_grants
            ORDER BY ends_at DESC, created_at DESC, id DESC
            LIMIT 1
        )
        SELECT
            (SELECT count(*) FROM paid_candidates) AS paid_count,
            (SELECT count(*) FROM active_grants) AS grant_count,
            paid.id AS paid_id,
            paid.plan_key AS paid_plan_key,
            paid.status AS paid_status,
            paid.payment_method_id AS paid_payment_method_id,
            paid.amount_cents AS paid_amount_cents,
            paid.currency AS paid_currency,
            paid.current_period_start_at AS paid_period_start_at,
            paid.current_period_end_at AS paid_period_end_at,
            paid.next_renewal_at AS paid_next_renewal_at,
            paid.phase AS paid_phase,
            paid.recurring_period_kind AS paid_recurring_period_kind,
            paid.recurring_period_count AS paid_recurring_period_count,
            paid.dunning_retry_delays_seconds AS paid_dunning_retry_delays_seconds,
            paid.dunning_exhaustion AS paid_dunning_exhaustion,
            paid.past_due_access AS paid_past_due_access,
            paid.next_payment_attempt_at AS paid_next_payment_attempt_at,
            active_grant.id AS grant_id,
            active_grant.plan_key AS grant_plan_key,
            active_grant.grant_kind AS grant_kind,
            active_grant.starts_at AS grant_starts_at,
            active_grant.ends_at AS grant_ends_at,
            active_grant.granted_by_actor_id AS grant_actor_id,
            EXISTS (
                SELECT 1
                FROM billing_payment_attempts attempts
                WHERE attempts.billing_scope_id = $1
                    AND attempts.subscriber_id = $2
                    AND attempts.plan_key = $3
                    AND attempts.attempt_kind = 'subscription_initial'
                    AND (
                        attempts.status IN ('pending', 'unknown')
                        OR (
                            attempts.status = 'review_required'
                            AND attempts.resolution_code IS DISTINCT FROM
                                'subscription_initial_current_subscription_conflict'
                        )
                    )
                    AND NOT (
                        attempts.status IN ('pending', 'review_required')
                        AND attempts.submitted_at IS NULL
                        AND attempts.created_at <= clock_timestamp()
                            - ($4::bigint * interval '1 second')
                    )
                    AND NOT EXISTS (
                        SELECT 1
                        FROM billing_subscriptions later_subscription
                        WHERE later_subscription.billing_scope_id = attempts.billing_scope_id
                            AND later_subscription.subscriber_id = attempts.subscriber_id
                            AND later_subscription.plan_key = attempts.plan_key
                            AND later_subscription.created_at >= attempts.created_at
                    )
            ) AS blocking_initial_attempt,
            EXISTS (
                SELECT 1
                FROM billing_payment_attempts attempts
                WHERE attempts.subscription_id = paid.id
                    AND attempts.attempt_kind IN (
                        'subscription_renewal',
                        'subscription_recovery'
                    )
                    AND attempts.status IN ('pending', 'unknown', 'review_required')
                    AND NOT (
                        attempts.status IN ('pending', 'review_required')
                        AND attempts.submitted_at IS NULL
                        AND attempts.created_at <= clock_timestamp()
                            - ($5::bigint * interval '1 second')
                    )
            ) AS pending_recovery_confirmation,
            saved.id AS saved_claim_id,
            saved.code_snapshot AS saved_code,
            saved.label_snapshot AS saved_label,
            saved.discount_kind AS saved_kind,
            saved.amount_off_cents AS saved_amount_off_cents,
            saved.percent_off_bps AS saved_percent_off_bps,
            saved.currency AS saved_currency,
            saved.duration AS saved_duration,
            saved.duration_months AS saved_duration_months,
            saved.base_amount_cents AS saved_base_amount_cents,
            saved.discounted_amount_cents AS saved_discounted_amount_cents,
            applied.discount_claim_id AS applied_claim_id,
            applied.code_snapshot AS applied_code,
            applied.label_snapshot AS applied_label,
            applied.discount_kind AS applied_kind,
            applied.amount_off_cents AS applied_amount_off_cents,
            applied.percent_off_bps AS applied_percent_off_bps,
            applied.currency AS applied_currency,
            applied.duration AS applied_duration,
            applied.duration_months AS applied_duration_months,
            applied.base_amount_cents AS applied_base_amount_cents,
            applied.discounted_amount_cents AS applied_discounted_amount_cents,
            applied.periods_total AS applied_periods_total,
            applied.periods_applied AS applied_periods_applied
        FROM (SELECT 1) seed
        LEFT JOIN paid ON true
        LEFT JOIN active_grant ON true
        LEFT JOIN LATERAL (
            SELECT claims.*
            FROM billing_subscription_discount_claims claims
            WHERE claims.billing_scope_id = $1
                AND claims.subscriber_id = $2
                AND claims.plan_key = $3
                AND claims.status = 'saved'
            ORDER BY claims.claimed_at DESC, claims.id DESC
            LIMIT 1
        ) saved ON true
        LEFT JOIN LATERAL (
            SELECT discounts.*
            FROM billing_subscription_discounts discounts
            WHERE discounts.subscription_id = paid.id
                AND discounts.billing_scope_id = $1
                AND discounts.subscriber_id = $2
                AND discounts.plan_key = $3
                AND discounts.status = 'active'
            LIMIT 1
        ) applied ON true
        "#,
    )
    .bind(query.billing_scope_id().as_uuid())
    .bind(query.subscriber_id().as_uuid())
    .bind(query.plan_key().as_str())
    .bind(INITIAL_PREPARED_STALE_AFTER_SECONDS)
    .bind(SUBSCRIPTION_CHARGE_UNSUBMITTED_STALE_AFTER_SECONDS)
    .fetch_one(executor)
    .await?;

    entitlement_from_row(&row)
}

fn entitlement_from_row(row: &PgRow) -> Result<Entitlement, EntitlementQueryError> {
    let paid_count: i64 = row.try_get("paid_count")?;
    let grant_count: i64 = row.try_get("grant_count")?;
    if paid_count > 1 || grant_count > 1 || (paid_count > 0 && grant_count > 0) {
        return Err(EntitlementQueryError::InvalidState(
            INVALID_ENTITLEMENT_STATE,
        ));
    }

    if grant_count == 1 {
        return Ok(Entitlement::Granted {
            grant: grant_from_row(row)?,
        });
    }

    let Some(subscription_id) = row.try_get::<Option<Uuid>, _>("paid_id")? else {
        let next_action = if row.try_get("blocking_initial_attempt")? {
            MissingSubscriptionAction::ConfirmInitialPayment
        } else {
            MissingSubscriptionAction::StartSubscription
        };
        return Ok(Entitlement::Missing {
            next_action,
            saved_discount: saved_discount_from_row(row)?,
        });
    };

    let status = row
        .try_get::<String, _>("paid_status")?
        .parse::<SubscriptionStatus>()
        .map_err(|_| EntitlementQueryError::InvalidState(INVALID_ENTITLEMENT_STATE))?;
    let phase = row
        .try_get::<String, _>("paid_phase")?
        .parse::<SubscriptionPhase>()
        .map_err(|_| EntitlementQueryError::InvalidState(INVALID_ENTITLEMENT_STATE))?;
    let recurring_period_kind: String = row.try_get("paid_recurring_period_kind")?;
    let recurring_period_count: i32 = row.try_get("paid_recurring_period_count")?;
    let recurring_period = subscription_period_rule_from_scalars(
        SubscriptionPeriodRuleScalars::new(&recurring_period_kind, recurring_period_count),
    )
    .map_err(map_subscription_persistence_error)?;
    let retry_delays_seconds: Vec<i64> = row.try_get("paid_dunning_retry_delays_seconds")?;
    let exhaustion: String = row.try_get("paid_dunning_exhaustion")?;
    let past_due_access: String = row.try_get("paid_past_due_access")?;
    let renewal_failure = renewal_failure_policy_from_scalars(RenewalFailurePolicyScalars::new(
        retry_delays_seconds,
        &exhaustion,
        &past_due_access,
    ))
    .map_err(map_subscription_persistence_error)?;
    let next_payment_attempt_at = row.try_get("paid_next_payment_attempt_at")?;
    let subscription = Subscription::new(
        SubscriptionId::new(subscription_id),
        row.try_get::<String, _>("paid_plan_key")?
            .parse()
            .map_err(|_| EntitlementQueryError::InvalidState(INVALID_ENTITLEMENT_STATE))?,
        status,
        phase,
        PaymentMethodId::new(row.try_get("paid_payment_method_id")?),
        ChargeAmount::new(
            row.try_get("paid_amount_cents")?,
            CurrencyCode::new(&row.try_get::<String, _>("paid_currency")?)
                .map_err(|_| EntitlementQueryError::InvalidState(INVALID_ENTITLEMENT_STATE))?,
        )
        .map_err(|_| EntitlementQueryError::InvalidState(INVALID_ENTITLEMENT_STATE))?,
        recurring_period,
        renewal_failure,
        BillingPeriod::new(
            row.try_get("paid_period_start_at")?,
            row.try_get("paid_period_end_at")?,
        )
        .map_err(|_| EntitlementQueryError::InvalidState(INVALID_ENTITLEMENT_STATE))?,
        row.try_get("paid_next_renewal_at")?,
        next_payment_attempt_at,
    );
    let applied_discount = applied_discount_from_row(row)?;

    Ok(match status {
        SubscriptionStatus::Active => Entitlement::PaidActive {
            subscription,
            applied_discount,
        },
        SubscriptionStatus::PastDue => Entitlement::PastDue {
            access: classify_past_due_access(
                subscription.renewal_failure().past_due_access(),
                subscription.next_payment_attempt_at().is_some(),
            ),
            subscription,
            next_action: if row.try_get("pending_recovery_confirmation")? {
                PastDueAction::ConfirmRecoveryPayment
            } else {
                PastDueAction::RecoverPayment
            },
            applied_discount,
        },
        SubscriptionStatus::Canceled => Entitlement::PaidThroughCancellation {
            subscription,
            applied_discount,
        },
        SubscriptionStatus::Unpaid => {
            return Err(EntitlementQueryError::InvalidState(
                INVALID_ENTITLEMENT_STATE,
            ));
        }
    })
}

fn grant_from_row(row: &PgRow) -> Result<SubscriptionGrant, EntitlementQueryError> {
    SubscriptionGrant::new(
        SubscriptionGrantId::new(required(row, "grant_id")?),
        required::<String>(row, "grant_plan_key")?
            .parse()
            .map_err(|_| EntitlementQueryError::InvalidState(INVALID_ENTITLEMENT_STATE))?,
        required::<String>(row, "grant_kind")?
            .parse::<SubscriptionGrantKind>()
            .map_err(|_| EntitlementQueryError::InvalidState(INVALID_ENTITLEMENT_STATE))?,
        required(row, "grant_starts_at")?,
        required(row, "grant_ends_at")?,
        ActorId::new(required(row, "grant_actor_id")?),
    )
    .map_err(|_| EntitlementQueryError::InvalidState(INVALID_ENTITLEMENT_STATE))
}

fn saved_discount_from_row(
    row: &PgRow,
) -> Result<Option<SavedSubscriptionDiscount>, EntitlementQueryError> {
    let Some(claim_id) = row.try_get::<Option<Uuid>, _>("saved_claim_id")? else {
        return Ok(None);
    };
    Ok(Some(SavedSubscriptionDiscount::new(
        DiscountClaimId::new(claim_id),
        discount_snapshot(row, "saved")?,
    )))
}

fn applied_discount_from_row(
    row: &PgRow,
) -> Result<Option<AppliedSubscriptionDiscount>, EntitlementQueryError> {
    let Some(code) = row.try_get::<Option<String>, _>("applied_code")? else {
        return Ok(None);
    };
    let duration = discount_duration(
        &required::<String>(row, "applied_duration")?,
        row.try_get("applied_duration_months")?,
    )?;
    let periods_remaining = match duration {
        SubscriptionDiscountDuration::Indefinite => None,
        SubscriptionDiscountDuration::LimitedMonths(_) => {
            let total: i32 = required(row, "applied_periods_total")?;
            let applied: i32 = required(row, "applied_periods_applied")?;
            Some(
                u8::try_from(total - applied)
                    .map_err(|_| EntitlementQueryError::InvalidState(INVALID_ENTITLEMENT_STATE))?,
            )
        }
    };
    let snapshot = discount_snapshot_from_values(row, "applied", code, duration)?;
    AppliedSubscriptionDiscount::new(
        row.try_get::<Option<Uuid>, _>("applied_claim_id")?
            .map(DiscountClaimId::new),
        snapshot,
        periods_remaining,
    )
    .map(Some)
    .map_err(|_| EntitlementQueryError::InvalidState(INVALID_ENTITLEMENT_STATE))
}

fn discount_snapshot(
    row: &PgRow,
    prefix: &str,
) -> Result<SubscriptionDiscountSnapshot, EntitlementQueryError> {
    let code = required::<String>(row, &format!("{prefix}_code"))?;
    let duration = discount_duration(
        &required::<String>(row, &format!("{prefix}_duration"))?,
        row.try_get(format!("{prefix}_duration_months").as_str())?,
    )?;
    discount_snapshot_from_values(row, prefix, code, duration)
}

fn discount_snapshot_from_values(
    row: &PgRow,
    prefix: &str,
    code: String,
    duration: SubscriptionDiscountDuration,
) -> Result<SubscriptionDiscountSnapshot, EntitlementQueryError> {
    let kind = match required::<String>(row, &format!("{prefix}_kind"))?.as_str() {
        "amount_off" => SubscriptionDiscountKind::AmountOffCents(
            PositiveDiscountCents::new(required(row, &format!("{prefix}_amount_off_cents"))?)
                .map_err(|_| EntitlementQueryError::InvalidState(INVALID_ENTITLEMENT_STATE))?,
        ),
        "percent_off" => SubscriptionDiscountKind::PercentOffBasisPoints(
            PercentOffBasisPoints::new(
                u16::try_from(required::<i32>(row, &format!("{prefix}_percent_off_bps"))?)
                    .map_err(|_| EntitlementQueryError::InvalidState(INVALID_ENTITLEMENT_STATE))?,
            )
            .map_err(|_| EntitlementQueryError::InvalidState(INVALID_ENTITLEMENT_STATE))?,
        ),
        _ => {
            return Err(EntitlementQueryError::InvalidState(
                INVALID_ENTITLEMENT_STATE,
            ));
        }
    };
    let currency = CurrencyCode::new(&required::<String>(row, &format!("{prefix}_currency"))?)
        .map_err(|_| EntitlementQueryError::InvalidState(INVALID_ENTITLEMENT_STATE))?;
    SubscriptionDiscountSnapshot::new(
        SubscriptionDiscountCode::new(&code)
            .map_err(|_| EntitlementQueryError::InvalidState(INVALID_ENTITLEMENT_STATE))?,
        row.try_get(format!("{prefix}_label").as_str())?,
        kind,
        duration,
        ChargeAmount::new(
            required(row, &format!("{prefix}_base_amount_cents"))?,
            currency,
        )
        .map_err(|_| EntitlementQueryError::InvalidState(INVALID_ENTITLEMENT_STATE))?,
        ChargeAmount::new(
            required(row, &format!("{prefix}_discounted_amount_cents"))?,
            currency,
        )
        .map_err(|_| EntitlementQueryError::InvalidState(INVALID_ENTITLEMENT_STATE))?,
    )
    .map_err(|_| EntitlementQueryError::InvalidState(INVALID_ENTITLEMENT_STATE))
}

fn discount_duration(
    duration: &str,
    duration_months: Option<i32>,
) -> Result<SubscriptionDiscountDuration, EntitlementQueryError> {
    match (duration, duration_months) {
        ("indefinite", None) => Ok(SubscriptionDiscountDuration::Indefinite),
        ("limited_months", Some(months)) => Ok(SubscriptionDiscountDuration::LimitedMonths(
            LimitedDiscountMonths::new(
                u8::try_from(months)
                    .map_err(|_| EntitlementQueryError::InvalidState(INVALID_ENTITLEMENT_STATE))?,
            )
            .map_err(|_| EntitlementQueryError::InvalidState(INVALID_ENTITLEMENT_STATE))?,
        )),
        _ => Err(EntitlementQueryError::InvalidState(
            INVALID_ENTITLEMENT_STATE,
        )),
    }
}

fn required<T>(row: &PgRow, column: &str) -> Result<T, EntitlementQueryError>
where
    for<'r> T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    row.try_get::<Option<T>, _>(column)?
        .ok_or(EntitlementQueryError::InvalidState(
            INVALID_ENTITLEMENT_STATE,
        ))
}

#[cfg(test)]
mod tests;
