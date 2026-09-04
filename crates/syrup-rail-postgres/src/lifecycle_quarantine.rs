use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row};
use syrup_rail::{
    ActorId, BillingScopeId, GatewayAccountId, GatewayLifecycleAccount,
    GatewayLifecycleQuarantineReason, GatewayLifecycleQuarantineResolutionReason,
};
use uuid::Uuid;

use crate::lifecycle_reconciliation::{
    GatewayLifecycleReconciliationError, ensure_account, set_timeouts,
};

const MAX_REVIEW_PAGE_SIZE: i64 = 101;
const INVALID_QUARANTINE_REASON: &str = "canonical gateway lifecycle quarantine reason is invalid";
const INVALID_QUARANTINE_HISTORY: &str =
    "canonical gateway lifecycle quarantine resolution history is invalid";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GatewayLifecycleQuarantineAlert {
    unresolved_count: i64,
    oldest_first_seen_at: DateTime<Utc>,
    latest_last_seen_at: DateTime<Utc>,
}

impl GatewayLifecycleQuarantineAlert {
    pub const fn unresolved_count(self) -> i64 {
        self.unresolved_count
    }

    pub const fn oldest_first_seen_at(self) -> DateTime<Utc> {
        self.oldest_first_seen_at
    }

    pub const fn latest_last_seen_at(self) -> DateTime<Utc> {
        self.latest_last_seen_at
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GatewayLifecycleQuarantineResolutionRecord {
    quarantine_id: Uuid,
    actor_id: ActorId,
    reason: GatewayLifecycleQuarantineResolutionReason,
    observed_occurrence_count: i64,
    observed_last_seen_at: DateTime<Utc>,
    resolved_at: DateTime<Utc>,
}

impl GatewayLifecycleQuarantineResolutionRecord {
    pub const fn quarantine_id(&self) -> Uuid {
        self.quarantine_id
    }

    pub const fn actor_id(&self) -> ActorId {
        self.actor_id
    }

    pub const fn reason(&self) -> &GatewayLifecycleQuarantineResolutionReason {
        &self.reason
    }

    pub const fn observed_occurrence_count(&self) -> i64 {
        self.observed_occurrence_count
    }

    pub const fn observed_last_seen_at(&self) -> DateTime<Utc> {
        self.observed_last_seen_at
    }

    pub const fn resolved_at(&self) -> DateTime<Utc> {
        self.resolved_at
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GatewayLifecycleQuarantineReviewRecord {
    id: Uuid,
    billing_scope_id: BillingScopeId,
    gateway_account_id: GatewayAccountId,
    transaction_id: Option<String>,
    order_id: Option<String>,
    reason: GatewayLifecycleQuarantineReason,
    first_seen_at: DateTime<Utc>,
    last_seen_at: DateTime<Utc>,
    occurrence_count: i64,
    resolution_count: i64,
    latest_resolution: Option<GatewayLifecycleQuarantineResolutionRecord>,
}

impl GatewayLifecycleQuarantineReviewRecord {
    pub const fn id(&self) -> Uuid {
        self.id
    }

    pub const fn billing_scope_id(&self) -> BillingScopeId {
        self.billing_scope_id
    }

    pub const fn gateway_account_id(&self) -> GatewayAccountId {
        self.gateway_account_id
    }

    /// Persisted review evidence only; this is not a provider-command capability.
    pub fn transaction_id(&self) -> Option<&str> {
        self.transaction_id.as_deref()
    }

    /// Persisted review evidence only; this is not a provider-command capability.
    pub fn order_id(&self) -> Option<&str> {
        self.order_id.as_deref()
    }

    pub const fn reason(&self) -> GatewayLifecycleQuarantineReason {
        self.reason
    }

    pub const fn first_seen_at(&self) -> DateTime<Utc> {
        self.first_seen_at
    }

    pub const fn last_seen_at(&self) -> DateTime<Utc> {
        self.last_seen_at
    }

    pub const fn occurrence_count(&self) -> i64 {
        self.occurrence_count
    }

    pub const fn resolution_count(&self) -> i64 {
        self.resolution_count
    }

    pub const fn latest_resolution(&self) -> Option<&GatewayLifecycleQuarantineResolutionRecord> {
        self.latest_resolution.as_ref()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GatewayLifecycleQuarantineResolutionOutcome {
    Resolved(GatewayLifecycleQuarantineResolutionRecord),
    Replayed(GatewayLifecycleQuarantineResolutionRecord),
    AlreadyResolved,
    Stale,
    NotFound,
}

pub async fn claim_gateway_lifecycle_quarantine_alert(
    pool: &PgPool,
    account: &GatewayLifecycleAccount,
    alert_after: Duration,
) -> Result<Option<GatewayLifecycleQuarantineAlert>, GatewayLifecycleReconciliationError> {
    let alert_after_seconds = i64::try_from(alert_after.as_secs()).map_err(|_| {
        GatewayLifecycleReconciliationError::InvalidState(
            "gateway lifecycle quarantine alert cadence is too large",
        )
    })?;
    let mut transaction = pool.begin().await?;
    set_timeouts(&mut transaction).await?;
    ensure_account(&mut transaction, account).await?;
    let owns_claim: bool = sqlx::query_scalar(
        r#"
        SELECT pg_try_advisory_xact_lock(
            hashtextextended(
                'syrup-rail:lifecycle-quarantine-alert:'
                    || $1::uuid::text || ':' || $2::uuid::text,
                0
            )
        )
        "#,
    )
    .bind(account.billing_scope_id().as_uuid())
    .bind(account.gateway_account_id().as_uuid())
    .fetch_one(&mut *transaction)
    .await?;
    if !owns_claim {
        transaction.rollback().await?;
        return Ok(None);
    }
    let row = sqlx::query(
        r#"
        WITH due AS MATERIALIZED (
            SELECT EXISTS (
                SELECT 1
                FROM billing_gateway_lifecycle_quarantines
                WHERE billing_scope_id = $1
                    AND gateway_account_id = $2
                    AND resolved_at IS NULL
                    AND (
                        last_operator_alerted_at IS NULL
                        OR last_operator_alerted_at
                            <= now() - ($3::bigint * interval '1 second')
                    )
            ) AS should_claim
        ),
        claimed AS (
            UPDATE billing_gateway_lifecycle_quarantines
            SET last_operator_alerted_at = now()
            WHERE billing_scope_id = $1
                AND gateway_account_id = $2
                AND resolved_at IS NULL
                AND (SELECT should_claim FROM due)
            RETURNING id
        )
        SELECT
            COUNT(*)::bigint AS unresolved_count,
            MIN(first_seen_at) AS oldest_first_seen_at,
            MAX(last_seen_at) AS latest_last_seen_at
        FROM billing_gateway_lifecycle_quarantines
        WHERE billing_scope_id = $1
            AND gateway_account_id = $2
            AND resolved_at IS NULL
        HAVING EXISTS (SELECT 1 FROM claimed)
        "#,
    )
    .bind(account.billing_scope_id().as_uuid())
    .bind(account.gateway_account_id().as_uuid())
    .bind(alert_after_seconds)
    .fetch_optional(&mut *transaction)
    .await?;
    transaction.commit().await?;
    row.map(|row| {
        Ok(GatewayLifecycleQuarantineAlert {
            unresolved_count: row.try_get("unresolved_count")?,
            oldest_first_seen_at: row.try_get("oldest_first_seen_at")?,
            latest_last_seen_at: row.try_get("latest_last_seen_at")?,
        })
    })
    .transpose()
}

pub async fn gateway_lifecycle_quarantine_review_page(
    pool: &PgPool,
    limit: i64,
    cursor: Option<(DateTime<Utc>, Uuid)>,
) -> Result<Vec<GatewayLifecycleQuarantineReviewRecord>, GatewayLifecycleReconciliationError> {
    if !(1..=MAX_REVIEW_PAGE_SIZE).contains(&limit) {
        return Err(GatewayLifecycleReconciliationError::InvalidState(
            "gateway lifecycle quarantine review page size is invalid",
        ));
    }
    let rows = sqlx::query(
        r#"
        WITH page AS (
            SELECT quarantine.id,
                quarantine.billing_scope_id,
                quarantine.gateway_account_id,
                quarantine.gateway_transaction_id,
                quarantine.gateway_order_id,
                quarantine.reason_code,
                quarantine.first_seen_at,
                quarantine.last_seen_at,
                quarantine.occurrence_count
            FROM billing_gateway_lifecycle_quarantines quarantine
            WHERE quarantine.resolved_at IS NULL
                AND (
                    $1::timestamptz IS NULL
                    OR (quarantine.first_seen_at, quarantine.id)
                        > ($1::timestamptz, $2::uuid)
                )
            ORDER BY quarantine.first_seen_at, quarantine.id
            LIMIT $3
        )
        SELECT page.*,
            COALESCE(history.resolution_count, 0)::bigint AS resolution_count,
            history.actor_id AS latest_resolution_actor_id,
            history.reason AS latest_resolution_reason,
            history.observed_occurrence_count AS latest_observed_occurrence_count,
            history.observed_last_seen_at AS latest_observed_last_seen_at,
            history.resolved_at AS latest_resolved_at
        FROM page
        LEFT JOIN LATERAL (
            SELECT resolution.actor_id,
                resolution.reason,
                resolution.observed_occurrence_count,
                resolution.observed_last_seen_at,
                resolution.resolved_at,
                COUNT(*) OVER () AS resolution_count
            FROM billing_gateway_lifecycle_quarantine_resolutions resolution
            WHERE resolution.quarantine_id = page.id
            ORDER BY resolution.resolved_at DESC, resolution.id DESC
            LIMIT 1
        ) history ON TRUE
        ORDER BY page.first_seen_at, page.id
        "#,
    )
    .bind(cursor.as_ref().map(|(first_seen_at, _)| first_seen_at))
    .bind(cursor.as_ref().map(|(_, id)| id))
    .bind(limit)
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(review_record).collect()
}

pub async fn resolve_gateway_lifecycle_quarantine(
    pool: &PgPool,
    quarantine_id: Uuid,
    actor_id: ActorId,
    expected_occurrence_count: i64,
    expected_last_seen_at: DateTime<Utc>,
    reason: &GatewayLifecycleQuarantineResolutionReason,
) -> Result<GatewayLifecycleQuarantineResolutionOutcome, GatewayLifecycleReconciliationError> {
    if expected_occurrence_count <= 0 {
        return Err(GatewayLifecycleReconciliationError::InvalidState(
            "gateway lifecycle quarantine observation is invalid",
        ));
    }
    let mut transaction = pool.begin().await?;
    set_timeouts(&mut transaction).await?;
    let quarantine = sqlx::query(
        r#"
        SELECT occurrence_count, last_seen_at, resolved_at
        FROM billing_gateway_lifecycle_quarantines
        WHERE id = $1
        FOR UPDATE
        "#,
    )
    .bind(quarantine_id)
    .fetch_optional(&mut *transaction)
    .await?;
    let Some(quarantine) = quarantine else {
        transaction.commit().await?;
        return Ok(GatewayLifecycleQuarantineResolutionOutcome::NotFound);
    };
    let occurrence_count: i64 = quarantine.try_get("occurrence_count")?;
    let last_seen_at: DateTime<Utc> = quarantine.try_get("last_seen_at")?;
    if occurrence_count != expected_occurrence_count || last_seen_at != expected_last_seen_at {
        transaction.commit().await?;
        return Ok(GatewayLifecycleQuarantineResolutionOutcome::Stale);
    }
    if quarantine
        .try_get::<Option<DateTime<Utc>>, _>("resolved_at")?
        .is_some()
    {
        let existing = sqlx::query(
            r#"
            SELECT quarantine_id, actor_id, reason,
                observed_occurrence_count, observed_last_seen_at, resolved_at
            FROM billing_gateway_lifecycle_quarantine_resolutions
            WHERE quarantine_id = $1
                AND observed_occurrence_count = $2
                AND observed_last_seen_at = $3
            "#,
        )
        .bind(quarantine_id)
        .bind(expected_occurrence_count)
        .bind(expected_last_seen_at)
        .fetch_optional(&mut *transaction)
        .await?;
        transaction.commit().await?;
        return Ok(match existing {
            Some(existing)
                if existing.try_get::<Uuid, _>("actor_id")? == *actor_id.as_uuid()
                    && existing.try_get::<String, _>("reason")? == reason.expose() =>
            {
                GatewayLifecycleQuarantineResolutionOutcome::Replayed(resolution_record(existing)?)
            }
            _ => GatewayLifecycleQuarantineResolutionOutcome::AlreadyResolved,
        });
    }
    let resolution = sqlx::query(
        r#"
        INSERT INTO billing_gateway_lifecycle_quarantine_resolutions (
            quarantine_id, actor_id, reason,
            observed_occurrence_count, observed_last_seen_at
        ) VALUES ($1, $2, $3, $4, $5)
        RETURNING quarantine_id, actor_id, reason,
            observed_occurrence_count, observed_last_seen_at, resolved_at
        "#,
    )
    .bind(quarantine_id)
    .bind(actor_id.as_uuid())
    .bind(reason.expose())
    .bind(expected_occurrence_count)
    .bind(expected_last_seen_at)
    .fetch_one(&mut *transaction)
    .await?;
    let resolved_at: DateTime<Utc> = resolution.try_get("resolved_at")?;
    let updated = sqlx::query(
        r#"
        UPDATE billing_gateway_lifecycle_quarantines
        SET resolved_at = $2
        WHERE id = $1
            AND resolved_at IS NULL
            AND occurrence_count = $3
            AND last_seen_at = $4
        "#,
    )
    .bind(quarantine_id)
    .bind(resolved_at)
    .bind(expected_occurrence_count)
    .bind(expected_last_seen_at)
    .execute(&mut *transaction)
    .await?;
    if updated.rows_affected() != 1 {
        return Err(GatewayLifecycleReconciliationError::InvalidState(
            "locked gateway lifecycle quarantine did not accept its operator resolution",
        ));
    }
    transaction.commit().await?;
    Ok(GatewayLifecycleQuarantineResolutionOutcome::Resolved(
        resolution_record(resolution)?,
    ))
}

fn review_record(
    row: sqlx::postgres::PgRow,
) -> Result<GatewayLifecycleQuarantineReviewRecord, GatewayLifecycleReconciliationError> {
    let latest_resolution = match (
        row.try_get::<Option<Uuid>, _>("latest_resolution_actor_id")?,
        row.try_get::<Option<String>, _>("latest_resolution_reason")?,
        row.try_get::<Option<i64>, _>("latest_observed_occurrence_count")?,
        row.try_get::<Option<DateTime<Utc>>, _>("latest_observed_last_seen_at")?,
        row.try_get::<Option<DateTime<Utc>>, _>("latest_resolved_at")?,
    ) {
        (Some(actor_id), Some(reason), Some(count), Some(last_seen_at), Some(resolved_at)) => {
            Some(GatewayLifecycleQuarantineResolutionRecord {
                quarantine_id: row.try_get("id")?,
                actor_id: ActorId::new(actor_id),
                reason: GatewayLifecycleQuarantineResolutionReason::new(reason).map_err(|_| {
                    GatewayLifecycleReconciliationError::InvalidState(INVALID_QUARANTINE_HISTORY)
                })?,
                observed_occurrence_count: count,
                observed_last_seen_at: last_seen_at,
                resolved_at,
            })
        }
        (None, None, None, None, None) => None,
        _ => {
            return Err(GatewayLifecycleReconciliationError::InvalidState(
                INVALID_QUARANTINE_HISTORY,
            ));
        }
    };
    let resolution_count: i64 = row.try_get("resolution_count")?;
    if (resolution_count == 0) != latest_resolution.is_none() {
        return Err(GatewayLifecycleReconciliationError::InvalidState(
            INVALID_QUARANTINE_HISTORY,
        ));
    }
    Ok(GatewayLifecycleQuarantineReviewRecord {
        id: row.try_get("id")?,
        billing_scope_id: BillingScopeId::new(row.try_get("billing_scope_id")?),
        gateway_account_id: GatewayAccountId::new(row.try_get("gateway_account_id")?),
        transaction_id: row.try_get("gateway_transaction_id")?,
        order_id: row.try_get("gateway_order_id")?,
        reason: quarantine_reason(&row.try_get::<String, _>("reason_code")?)?,
        first_seen_at: row.try_get("first_seen_at")?,
        last_seen_at: row.try_get("last_seen_at")?,
        occurrence_count: row.try_get("occurrence_count")?,
        resolution_count,
        latest_resolution,
    })
}

fn resolution_record(
    row: sqlx::postgres::PgRow,
) -> Result<GatewayLifecycleQuarantineResolutionRecord, GatewayLifecycleReconciliationError> {
    Ok(GatewayLifecycleQuarantineResolutionRecord {
        quarantine_id: row.try_get("quarantine_id")?,
        actor_id: ActorId::new(row.try_get("actor_id")?),
        reason: GatewayLifecycleQuarantineResolutionReason::new(
            row.try_get::<String, _>("reason")?,
        )
        .map_err(|_| {
            GatewayLifecycleReconciliationError::InvalidState(INVALID_QUARANTINE_HISTORY)
        })?,
        observed_occurrence_count: row.try_get("observed_occurrence_count")?,
        observed_last_seen_at: row.try_get("observed_last_seen_at")?,
        resolved_at: row.try_get("resolved_at")?,
    })
}

fn quarantine_reason(
    value: &str,
) -> Result<GatewayLifecycleQuarantineReason, GatewayLifecycleReconciliationError> {
    match value {
        "ambiguous_reversal_success" => {
            Ok(GatewayLifecycleQuarantineReason::AmbiguousReversalSuccess)
        }
        "invalid_refund_economics" => Ok(GatewayLifecycleQuarantineReason::InvalidRefundEconomics),
        "malformed_report_structure" => {
            Ok(GatewayLifecycleQuarantineReason::MalformedReportStructure)
        }
        _ => Err(GatewayLifecycleReconciliationError::InvalidState(
            INVALID_QUARANTINE_REASON,
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::{error::Error, io};

    use super::*;
    use crate::{
        record_gateway_lifecycle_quarantines,
        test_support::{TestDatabase, create_gateway_account},
    };
    use syrup_rail::{GatewayLifecycleQuarantine, GatewayProviderKey};

    #[tokio::test]
    async fn concurrent_alert_claim_has_one_winner() -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("rail_quar_claim").await?;
        let first_fixture = create_gateway_account(&database.pool, "nmi").await?;
        let first_account = GatewayLifecycleAccount::new(
            BillingScopeId::new(first_fixture.billing_scope_id),
            GatewayAccountId::new(first_fixture.gateway_account_id),
            GatewayProviderKey::new("nmi")?,
        );
        let first_quarantine = GatewayLifecycleQuarantine::new(
            Some(syrup_rail::GatewayTransactionId::new("txn-contended")?),
            None,
            GatewayLifecycleQuarantineReason::MalformedReportStructure,
        )?;
        record_gateway_lifecycle_quarantines(
            &database.pool,
            &first_account,
            std::slice::from_ref(&first_quarantine),
        )
        .await?;

        let second_fixture = create_gateway_account(&database.pool, "nmi").await?;
        let second_account = GatewayLifecycleAccount::new(
            BillingScopeId::new(second_fixture.billing_scope_id),
            GatewayAccountId::new(second_fixture.gateway_account_id),
            GatewayProviderKey::new("nmi")?,
        );
        let second_quarantine = GatewayLifecycleQuarantine::new(
            Some(syrup_rail::GatewayTransactionId::new("txn-independent")?),
            None,
            GatewayLifecycleQuarantineReason::MalformedReportStructure,
        )?;
        record_gateway_lifecycle_quarantines(
            &database.pool,
            &second_account,
            std::slice::from_ref(&second_quarantine),
        )
        .await?;

        let mut blocker = database.pool.begin().await?;
        sqlx::query(
            r#"
            SELECT id
            FROM billing_gateway_lifecycle_quarantines
            WHERE billing_scope_id = $1
                AND gateway_account_id = $2
                AND resolved_at IS NULL
            FOR UPDATE
            "#,
        )
        .bind(first_account.billing_scope_id().as_uuid())
        .bind(first_account.gateway_account_id().as_uuid())
        .fetch_one(&mut *blocker)
        .await?;
        let mut observer = database.pool.acquire().await?;

        let claim_pool = database.pool.clone();
        let claim_account = first_account.clone();
        let first_claim = tokio::spawn(async move {
            claim_gateway_lifecycle_quarantine_alert(
                &claim_pool,
                &claim_account,
                Duration::from_secs(3_600),
            )
            .await
        });

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if waiting_alert_claims(&mut observer).await? >= 1 {
                    return Ok::<_, sqlx::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .map_err(|_| io::Error::other("first alert claim did not reach its row-lock wait"))??;

        let independent_claim = tokio::time::timeout(
            Duration::from_millis(200),
            claim_gateway_lifecycle_quarantine_alert(
                &database.pool,
                &second_account,
                Duration::from_secs(3_600),
            ),
        )
        .await
        .map_err(|_| io::Error::other("a different account's alert claim was blocked"))??;
        assert!(independent_claim.is_some());

        let claim_pool = database.pool.clone();
        let claim_account = first_account.clone();
        let second_claim = tokio::spawn(async move {
            claim_gateway_lifecycle_quarantine_alert(
                &claim_pool,
                &claim_account,
                Duration::from_secs(3_600),
            )
            .await
        });

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let waiting_claims = waiting_alert_claims(&mut observer).await?;
                if waiting_claims >= 2 || first_claim.is_finished() || second_claim.is_finished() {
                    return Ok::<_, sqlx::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .map_err(|_| io::Error::other("alert claims did not reach a deterministic outcome"))??;

        blocker.rollback().await?;
        let (first_result, second_result) = tokio::try_join!(first_claim, second_claim)?;
        let successful_claims = [first_result?, second_result?]
            .into_iter()
            .filter(Option::is_some)
            .count();
        assert_eq!(successful_claims, 1);
        assert!(
            claim_gateway_lifecycle_quarantine_alert(
                &database.pool,
                &first_account,
                Duration::from_secs(3_600),
            )
            .await?
            .is_none()
        );

        drop(observer);
        database.cleanup().await?;
        Ok(())
    }

    async fn waiting_alert_claims(observer: &mut sqlx::PgConnection) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar(
            r#"
            SELECT COUNT(*)::bigint
            FROM pg_catalog.pg_stat_activity
            WHERE datname = current_database()
                AND pid <> pg_backend_pid()
                AND wait_event_type = 'Lock'
                AND query LIKE '%WITH due AS MATERIALIZED%'
            "#,
        )
        .fetch_one(observer)
        .await
    }

    #[tokio::test]
    async fn alert_review_resolution_replay_and_reopen_are_exact_and_auditable()
    -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("rail_quarantine").await?;
        let fixture = create_gateway_account(&database.pool, "nmi").await?;
        let account = GatewayLifecycleAccount::new(
            BillingScopeId::new(fixture.billing_scope_id),
            GatewayAccountId::new(fixture.gateway_account_id),
            GatewayProviderKey::new("nmi")?,
        );
        let quarantine = GatewayLifecycleQuarantine::new(
            Some(syrup_rail::GatewayTransactionId::new("txn-review")?),
            None,
            GatewayLifecycleQuarantineReason::MalformedReportStructure,
        )?;
        record_gateway_lifecycle_quarantines(
            &database.pool,
            &account,
            std::slice::from_ref(&quarantine),
        )
        .await?;

        let alert = claim_gateway_lifecycle_quarantine_alert(
            &database.pool,
            &account,
            Duration::from_secs(3_600),
        )
        .await?
        .expect("first unresolved alert is due");
        assert_eq!(alert.unresolved_count(), 1);
        assert!(
            claim_gateway_lifecycle_quarantine_alert(
                &database.pool,
                &account,
                Duration::from_secs(3_600),
            )
            .await?
            .is_none()
        );

        let page = gateway_lifecycle_quarantine_review_page(&database.pool, 10, None).await?;
        assert_eq!(page.len(), 1);
        let incident = &page[0];
        assert_eq!(incident.billing_scope_id(), account.billing_scope_id());
        assert_eq!(incident.gateway_account_id(), account.gateway_account_id());
        assert_eq!(incident.transaction_id(), Some("txn-review"));
        assert_eq!(
            incident.reason(),
            GatewayLifecycleQuarantineReason::MalformedReportStructure
        );
        assert_eq!(incident.resolution_count(), 0);

        let actor = ActorId::new(Uuid::now_v7());
        let reason = GatewayLifecycleQuarantineResolutionReason::new("provider report reviewed")?;
        let resolved = resolve_gateway_lifecycle_quarantine(
            &database.pool,
            incident.id(),
            actor,
            incident.occurrence_count(),
            incident.last_seen_at(),
            &reason,
        )
        .await?;
        let GatewayLifecycleQuarantineResolutionOutcome::Resolved(resolution) = resolved else {
            panic!("expected a new resolution");
        };
        assert_eq!(resolution.actor_id(), actor);
        assert_eq!(resolution.reason(), &reason);
        assert!(
            gateway_lifecycle_quarantine_review_page(&database.pool, 10, None)
                .await?
                .is_empty()
        );

        assert!(matches!(
            resolve_gateway_lifecycle_quarantine(
                &database.pool,
                incident.id(),
                actor,
                incident.occurrence_count(),
                incident.last_seen_at(),
                &reason,
            )
            .await?,
            GatewayLifecycleQuarantineResolutionOutcome::Replayed(_)
        ));

        record_gateway_lifecycle_quarantines(&database.pool, &account, &[quarantine]).await?;
        let reopened = gateway_lifecycle_quarantine_review_page(&database.pool, 10, None).await?;
        assert_eq!(reopened.len(), 1);
        assert_eq!(reopened[0].resolution_count(), 1);
        assert_eq!(reopened[0].latest_resolution(), Some(&resolution));
        assert!(matches!(
            resolve_gateway_lifecycle_quarantine(
                &database.pool,
                reopened[0].id(),
                actor,
                incident.occurrence_count(),
                incident.last_seen_at(),
                &reason,
            )
            .await?,
            GatewayLifecycleQuarantineResolutionOutcome::Stale
        ));

        let wrong_provider = GatewayLifecycleAccount::new(
            account.billing_scope_id(),
            account.gateway_account_id(),
            GatewayProviderKey::new("other")?,
        );
        assert!(matches!(
            claim_gateway_lifecycle_quarantine_alert(
                &database.pool,
                &wrong_provider,
                Duration::ZERO,
            )
            .await,
            Err(GatewayLifecycleReconciliationError::AccountNotFound)
        ));

        database.cleanup().await?;
        Ok(())
    }
}
