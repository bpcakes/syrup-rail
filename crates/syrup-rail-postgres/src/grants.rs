use chrono::{DateTime, Utc};
use sqlx::{Postgres, Row, Transaction, postgres::PgRow};
use syrup_rail::{
    ActorId, BillingScopeId, PlanKey, SubscriberId, SubscriptionGrant, SubscriptionGrantCreation,
    SubscriptionGrantCreationOutcome, SubscriptionGrantId, SubscriptionGrantKind,
    SubscriptionGrantReason, SubscriptionGrantRecord, SubscriptionGrantRevocation,
    SubscriptionGrantRevocationAudit, SubscriptionGrantRevocationOutcome,
    SubscriptionGrantRevocationState, SubscriptionStatus,
};
use thiserror::Error;
use uuid::Uuid;

use crate::attempts::lock_subscription_aggregate;

const BILLING_ROW_LOCK_TIMEOUT: &str = "250ms";
const INVALID_GRANT_STATE: &str = "canonical subscription grant state is invalid";

type GrantTimeState = (DateTime<Utc>, DateTime<Utc>, Option<DateTime<Utc>>);

#[derive(Debug, Error)]
pub enum SubscriptionGrantMutationError {
    #[error("subscription grant mutation failed")]
    Sql(#[from] sqlx::Error),
    #[error("{0}")]
    InvalidState(&'static str),
}

/// Creates a grant inside the caller's transaction.
///
/// The aggregate lock, canonical row locks, admission checks, database clock,
/// and insert are one transaction. Authentication and actor presentation stay
/// with the host application.
pub async fn create_subscription_grant(
    transaction: &mut Transaction<'_, Postgres>,
    creation: &SubscriptionGrantCreation,
) -> Result<SubscriptionGrantCreationOutcome, SubscriptionGrantMutationError> {
    set_lock_timeout(transaction).await?;
    lock_subscription_aggregate(transaction, creation.subscriber_id(), creation.plan_key()).await?;
    lock_initial_attempts(
        transaction,
        creation.billing_scope_id().as_uuid(),
        creation.subscriber_id().as_uuid(),
        creation.plan_key().as_str(),
    )
    .await?;

    let subscriptions: Vec<(String, DateTime<Utc>)> = sqlx::query_as(
        r#"
        SELECT status, current_period_end_at
        FROM billing_subscriptions
        WHERE billing_scope_id = $1
            AND subscriber_id = $2
            AND plan_key = $3
        ORDER BY id
        FOR NO KEY UPDATE
        "#,
    )
    .bind(creation.billing_scope_id().as_uuid())
    .bind(creation.subscriber_id().as_uuid())
    .bind(creation.plan_key().as_str())
    .fetch_all(&mut **transaction)
    .await?;
    let existing_grants: Vec<GrantTimeState> = sqlx::query_as(
        r#"
        SELECT starts_at, ends_at, revoked_at
        FROM billing_subscription_grants
        WHERE billing_scope_id = $1
            AND subscriber_id = $2
            AND plan_key = $3
        ORDER BY id
        FOR UPDATE
        "#,
    )
    .bind(creation.billing_scope_id().as_uuid())
    .bind(creation.subscriber_id().as_uuid())
    .bind(creation.plan_key().as_str())
    .fetch_all(&mut **transaction)
    .await?;
    let mutation_now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut **transaction)
        .await?;

    if creation.ends_at() <= &mutation_now {
        return Ok(SubscriptionGrantCreationOutcome::EndsAtNotFuture);
    }
    if subscriptions.iter().any(|(status, period_end)| {
        matches!(
            status.parse(),
            Ok(SubscriptionStatus::Active | SubscriptionStatus::PastDue)
        ) || (status == SubscriptionStatus::Canceled.as_str() && *period_end > mutation_now)
    }) {
        return Ok(SubscriptionGrantCreationOutcome::CurrentPaidSubscription);
    }
    if existing_grants
        .iter()
        .any(|(starts_at, ends_at, revoked_at)| {
            *starts_at <= mutation_now && *ends_at > mutation_now && revoked_at.is_none()
        })
    {
        return Ok(SubscriptionGrantCreationOutcome::ActiveGrant);
    }
    if blocking_initial_attempt_exists(transaction, creation).await? {
        return Ok(SubscriptionGrantCreationOutcome::BlockingInitialAttempt);
    }
    if pending_processor_evidence_exists(transaction, creation).await? {
        return Ok(SubscriptionGrantCreationOutcome::PendingApprovedProcessorEvidence);
    }

    sqlx::query(
        r#"
        INSERT INTO billing_subscription_grants (
            id,
            billing_scope_id,
            subscriber_id,
            plan_key,
            grant_kind,
            reason,
            starts_at,
            ends_at,
            granted_by_actor_id,
            created_at,
            updated_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $7, $7)
        "#,
    )
    .bind(creation.id().as_uuid())
    .bind(creation.billing_scope_id().as_uuid())
    .bind(creation.subscriber_id().as_uuid())
    .bind(creation.plan_key().as_str())
    .bind(creation.kind().as_str())
    .bind(creation.reason().as_str())
    .bind(mutation_now)
    .bind(creation.ends_at())
    .bind(creation.granted_by_actor_id().as_uuid())
    .execute(&mut **transaction)
    .await?;

    let record = grant_by_id(transaction, creation.id().as_uuid())
        .await?
        .ok_or(SubscriptionGrantMutationError::InvalidState(
            INVALID_GRANT_STATE,
        ))?;
    Ok(SubscriptionGrantCreationOutcome::Created(Box::new(record)))
}

/// Revokes an exact grant inside the caller's transaction.
pub async fn revoke_subscription_grant(
    transaction: &mut Transaction<'_, Postgres>,
    revocation: &SubscriptionGrantRevocation,
) -> Result<SubscriptionGrantRevocationOutcome, SubscriptionGrantMutationError> {
    set_lock_timeout(transaction).await?;
    lock_subscription_aggregate(
        transaction,
        revocation.subscriber_id(),
        revocation.plan_key(),
    )
    .await?;

    let row = sqlx::query(
        r#"
        SELECT id,
            billing_scope_id,
            subscriber_id,
            plan_key,
            grant_kind,
            reason,
            starts_at,
            ends_at,
            granted_by_actor_id,
            revoked_at,
            revoked_by_actor_id,
            revocation_reason,
            created_at,
            updated_at
        FROM billing_subscription_grants
        WHERE id = $1
            AND billing_scope_id = $2
            AND subscriber_id = $3
            AND plan_key = $4
        FOR UPDATE
        "#,
    )
    .bind(revocation.id().as_uuid())
    .bind(revocation.billing_scope_id().as_uuid())
    .bind(revocation.subscriber_id().as_uuid())
    .bind(revocation.plan_key().as_str())
    .fetch_optional(&mut **transaction)
    .await?;
    let Some(row) = row else {
        return Ok(SubscriptionGrantRevocationOutcome::NotFound);
    };
    let record = grant_from_row(&row)?;
    let mutation_now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut **transaction)
        .await?;
    if record.revoked_at().is_some() {
        return Ok(SubscriptionGrantRevocationOutcome::AlreadyRevoked(
            Box::new(record),
        ));
    }
    if record.grant().ends_at() <= &mutation_now {
        return Ok(SubscriptionGrantRevocationOutcome::Expired);
    }

    sqlx::query(
        r#"
        UPDATE billing_subscription_grants
        SET revoked_at = $2,
            revoked_by_actor_id = $3,
            revocation_reason = $4,
            updated_at = $2
        WHERE id = $1
        "#,
    )
    .bind(revocation.id().as_uuid())
    .bind(mutation_now)
    .bind(revocation.revoked_by_actor_id().as_uuid())
    .bind(revocation.reason().as_str())
    .execute(&mut **transaction)
    .await?;

    let record = grant_by_id(transaction, revocation.id().as_uuid())
        .await?
        .ok_or(SubscriptionGrantMutationError::InvalidState(
            INVALID_GRANT_STATE,
        ))?;
    Ok(SubscriptionGrantRevocationOutcome::Revoked(Box::new(
        record,
    )))
}

async fn set_lock_timeout(transaction: &mut Transaction<'_, Postgres>) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT set_config('lock_timeout', $1, true)")
        .bind(BILLING_ROW_LOCK_TIMEOUT)
        .execute(&mut **transaction)
        .await?;
    Ok(())
}

async fn lock_initial_attempts(
    transaction: &mut Transaction<'_, Postgres>,
    billing_scope_id: &Uuid,
    subscriber_id: &Uuid,
    plan_key: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT id
        FROM billing_payment_attempts
        WHERE billing_scope_id = $1
            AND subscriber_id = $2
            AND plan_key = $3
            AND attempt_kind = 'subscription_initial'
        ORDER BY created_at, id
        FOR UPDATE
        "#,
    )
    .bind(billing_scope_id)
    .bind(subscriber_id)
    .bind(plan_key)
    .fetch_all(&mut **transaction)
    .await?;
    Ok(())
}

async fn blocking_initial_attempt_exists(
    transaction: &mut Transaction<'_, Postgres>,
    creation: &SubscriptionGrantCreation,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        r#"
        SELECT EXISTS (
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
                AND NOT EXISTS (
                    SELECT 1
                    FROM billing_subscriptions subscriptions
                    WHERE subscriptions.billing_scope_id = attempts.billing_scope_id
                        AND subscriptions.subscriber_id = attempts.subscriber_id
                        AND subscriptions.plan_key = attempts.plan_key
                        AND subscriptions.created_at >= attempts.created_at
                )
        )
        "#,
    )
    .bind(creation.billing_scope_id().as_uuid())
    .bind(creation.subscriber_id().as_uuid())
    .bind(creation.plan_key().as_str())
    .fetch_one(&mut **transaction)
    .await
}

async fn pending_processor_evidence_exists(
    transaction: &mut Transaction<'_, Postgres>,
    creation: &SubscriptionGrantCreation,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM billing_processor_charges evidence
            INNER JOIN billing_payment_attempts attempts
                ON attempts.id = evidence.attempt_id
            WHERE attempts.billing_scope_id = $1
                AND attempts.subscriber_id = $2
                AND attempts.plan_key = $3
                AND attempts.attempt_kind = 'subscription_initial'
                AND evidence.progression_state IN (
                    'pending',
                    'reconciliation_required',
                    'external_reversal_required'
                )
        )
        "#,
    )
    .bind(creation.billing_scope_id().as_uuid())
    .bind(creation.subscriber_id().as_uuid())
    .bind(creation.plan_key().as_str())
    .fetch_one(&mut **transaction)
    .await
}

async fn grant_by_id(
    transaction: &mut Transaction<'_, Postgres>,
    grant_id: &Uuid,
) -> Result<Option<SubscriptionGrantRecord>, SubscriptionGrantMutationError> {
    let row = sqlx::query(
        r#"
        SELECT id,
            billing_scope_id,
            subscriber_id,
            plan_key,
            grant_kind,
            reason,
            starts_at,
            ends_at,
            granted_by_actor_id,
            revoked_at,
            revoked_by_actor_id,
            revocation_reason,
            created_at,
            updated_at
        FROM billing_subscription_grants
        WHERE id = $1
        "#,
    )
    .bind(grant_id)
    .fetch_optional(&mut **transaction)
    .await?;
    row.as_ref().map(grant_from_row).transpose()
}

fn grant_from_row(row: &PgRow) -> Result<SubscriptionGrantRecord, SubscriptionGrantMutationError> {
    let plan_key = row
        .try_get::<String, _>("plan_key")?
        .parse::<PlanKey>()
        .map_err(|_| SubscriptionGrantMutationError::InvalidState(INVALID_GRANT_STATE))?;
    let grant = SubscriptionGrant::new(
        SubscriptionGrantId::new(row.try_get("id")?),
        plan_key,
        row.try_get::<String, _>("grant_kind")?
            .parse::<SubscriptionGrantKind>()
            .map_err(|_| SubscriptionGrantMutationError::InvalidState(INVALID_GRANT_STATE))?,
        row.try_get("starts_at")?,
        row.try_get("ends_at")?,
        ActorId::new(row.try_get("granted_by_actor_id")?),
    )
    .map_err(|_| SubscriptionGrantMutationError::InvalidState(INVALID_GRANT_STATE))?;
    SubscriptionGrantRecord::from_revocation_state(
        BillingScopeId::new(row.try_get("billing_scope_id")?),
        SubscriberId::new(row.try_get("subscriber_id")?),
        grant,
        grant_reason(row.try_get("reason")?)?,
        grant_revocation_state(row)?,
        row.try_get("created_at")?,
        row.try_get("updated_at")?,
    )
    .map_err(|_| SubscriptionGrantMutationError::InvalidState(INVALID_GRANT_STATE))
}

fn grant_revocation_state(
    row: &PgRow,
) -> Result<SubscriptionGrantRevocationState, SubscriptionGrantMutationError> {
    let revoked_at = row.try_get::<Option<DateTime<Utc>>, _>("revoked_at")?;
    let revoked_by_actor_id = row.try_get::<Option<Uuid>, _>("revoked_by_actor_id")?;
    let reason = row.try_get::<Option<String>, _>("revocation_reason")?;
    match (revoked_at, revoked_by_actor_id, reason) {
        (None, None, None) => Ok(SubscriptionGrantRevocationState::Active),
        (Some(revoked_at), Some(revoked_by_actor_id), Some(reason)) => Ok(
            SubscriptionGrantRevocationState::Revoked(SubscriptionGrantRevocationAudit::new(
                revoked_at,
                ActorId::new(revoked_by_actor_id),
                grant_reason(reason)?,
            )),
        ),
        _ => Err(SubscriptionGrantMutationError::InvalidState(
            INVALID_GRANT_STATE,
        )),
    }
}

fn grant_reason(value: String) -> Result<SubscriptionGrantReason, SubscriptionGrantMutationError> {
    SubscriptionGrantReason::new(value)
        .map_err(|_| SubscriptionGrantMutationError::InvalidState(INVALID_GRANT_STATE))
}

#[cfg(test)]
mod tests {
    use std::{error::Error, io};

    use chrono::{Duration, Utc};
    use syrup_rail::{
        ActorId, BillingScopeId, PlanKey, SubscriberId, SubscriptionGrantCreation,
        SubscriptionGrantCreationOutcome, SubscriptionGrantId, SubscriptionGrantKind,
        SubscriptionGrantReason, SubscriptionGrantRevocation, SubscriptionGrantRevocationOutcome,
    };
    use uuid::Uuid;

    use super::{
        SubscriptionGrantMutationError, create_subscription_grant, revoke_subscription_grant,
    };
    use crate::test_support::{TestDatabase, create_gateway_account};

    #[tokio::test]
    async fn discount_and_grant_workflows_contend_on_the_canonical_subscription_aggregate()
    -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("sr_gr_agg_lock").await?;
        let result = async {
            let subscriber_id = SubscriberId::new(Uuid::now_v7());
            let plan_key = PlanKey::new("base_subscription")?;
            let mut holder = database.pool.begin().await?;
            let held = crate::clear_subscription_discount_in_transaction(
                &mut holder,
                BillingScopeId::new(Uuid::now_v7()),
                subscriber_id,
                &plan_key,
            )
            .await?;
            if held != syrup_rail::SubscriptionDiscountClearOutcome::NotFound {
                return Err(io::Error::other(
                    "discount workflow did not retain its empty aggregate transaction",
                )
                .into());
            }

            let creation = SubscriptionGrantCreation::new(
                SubscriptionGrantId::new(Uuid::now_v7()),
                BillingScopeId::new(Uuid::now_v7()),
                subscriber_id,
                plan_key,
                SubscriptionGrantKind::Testing,
                SubscriptionGrantReason::new("aggregate contention")?,
                Utc::now() + Duration::days(1),
                ActorId::new(Uuid::now_v7()),
            );
            let mut contender = database.pool.begin().await?;
            let error = create_subscription_grant(&mut contender, &creation)
                .await
                .expect_err("canonical aggregate holder must block grant creation");
            contender.rollback().await?;
            holder.rollback().await?;
            let SubscriptionGrantMutationError::Sql(sqlx::Error::Database(error)) = error else {
                return Err(io::Error::other(format!(
                    "expected grant lock timeout, got {error:?}"
                ))
                .into());
            };
            if error.code().as_deref() != Some("55P03") {
                return Err(io::Error::other(format!(
                    "expected grant lock timeout SQLSTATE 55P03, got {:?}",
                    error.code()
                ))
                .into());
            }
            Ok::<_, Box<dyn Error>>(())
        }
        .await;
        let cleanup = database.cleanup().await;
        result?;
        cleanup
    }

    #[tokio::test]
    async fn grant_lifecycle_is_auditable_and_owned_by_the_caller_transaction()
    -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("sr_grants_v1").await?;
        let result = async {
            let scope = BillingScopeId::new(Uuid::now_v7());
            let subscriber = SubscriberId::new(Uuid::now_v7());
            let plan = PlanKey::new("base_subscription")?;
            let granting_actor = ActorId::new(Uuid::now_v7());
            let grant_id = SubscriptionGrantId::new(Uuid::now_v7());
            let creation = SubscriptionGrantCreation::new(
                grant_id,
                scope,
                subscriber,
                plan.clone(),
                SubscriptionGrantKind::Promotion,
                SubscriptionGrantReason::new("  launch partner  ")?,
                Utc::now() + Duration::days(30),
                granting_actor,
            );

            let mut rolled_back = database.pool.begin().await?;
            if !matches!(
                create_subscription_grant(&mut rolled_back, &creation).await?,
                SubscriptionGrantCreationOutcome::Created(_)
            ) {
                return Err(io::Error::other("grant was not created before rollback").into());
            }
            rolled_back.rollback().await?;
            let count: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM billing_subscription_grants WHERE id = $1",
            )
            .bind(grant_id.as_uuid())
            .fetch_one(&database.pool)
            .await?;
            if count != 0 {
                return Err(io::Error::other("grant escaped caller rollback").into());
            }

            let mut created_transaction = database.pool.begin().await?;
            let created = create_subscription_grant(&mut created_transaction, &creation).await?;
            created_transaction.commit().await?;
            let SubscriptionGrantCreationOutcome::Created(created) = created else {
                return Err(io::Error::other("grant was not created").into());
            };
            if created.reason().as_str() != "launch partner"
                || created.grant().id() != grant_id
                || created.grant().granted_by_actor_id() != granting_actor
                || created.revoked_at().is_some()
            {
                return Err(io::Error::other("created grant audit was not lossless").into());
            }

            let revoking_actor = ActorId::new(Uuid::now_v7());
            let revocation = SubscriptionGrantRevocation::new(
                grant_id,
                scope,
                subscriber,
                plan,
                revoking_actor,
                SubscriptionGrantReason::new("access no longer required")?,
            );
            let mut revoked_transaction = database.pool.begin().await?;
            let revoked = revoke_subscription_grant(&mut revoked_transaction, &revocation).await?;
            revoked_transaction.commit().await?;
            let SubscriptionGrantRevocationOutcome::Revoked(revoked) = revoked else {
                return Err(io::Error::other("active grant was not revoked").into());
            };
            if revoked.revoked_by_actor_id() != Some(revoking_actor)
                || revoked.revocation_reason().map(|reason| reason.as_str())
                    != Some("access no longer required")
                || revoked.revoked_at().is_none()
            {
                return Err(io::Error::other("revocation audit was not lossless").into());
            }

            let mut repeated_transaction = database.pool.begin().await?;
            let repeated =
                revoke_subscription_grant(&mut repeated_transaction, &revocation).await?;
            repeated_transaction.commit().await?;
            if !matches!(
                repeated,
                SubscriptionGrantRevocationOutcome::AlreadyRevoked(record)
                    if record == revoked
            ) {
                return Err(io::Error::other("repeated revocation was not idempotent").into());
            }
            Ok::<_, Box<dyn Error>>(())
        }
        .await;
        let cleanup = database.cleanup().await;
        result?;
        cleanup
    }

    #[tokio::test]
    async fn persisted_partial_revocation_audit_is_rejected() -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("sr_grant_partial").await?;
        let result = async {
            let scope = BillingScopeId::new(Uuid::now_v7());
            let subscriber = SubscriberId::new(Uuid::now_v7());
            let plan = PlanKey::new("base_subscription")?;
            let grant_id = SubscriptionGrantId::new(Uuid::now_v7());
            let creation = SubscriptionGrantCreation::new(
                grant_id,
                scope,
                subscriber,
                plan.clone(),
                SubscriptionGrantKind::Promotion,
                SubscriptionGrantReason::new("partial audit regression")?,
                Utc::now() + Duration::days(30),
                ActorId::new(Uuid::now_v7()),
            );
            let mut creation_transaction = database.pool.begin().await?;
            if !matches!(
                create_subscription_grant(&mut creation_transaction, &creation).await?,
                SubscriptionGrantCreationOutcome::Created(_)
            ) {
                return Err(io::Error::other("grant was not created").into());
            }
            creation_transaction.commit().await?;

            sqlx::query(
                "ALTER TABLE billing_subscription_grants DROP CONSTRAINT billing_subscription_grants_revocation_check",
            )
            .execute(&database.pool)
            .await?;
            sqlx::query(
                "UPDATE billing_subscription_grants SET revoked_at = clock_timestamp() WHERE id = $1",
            )
            .bind(grant_id.as_uuid())
            .execute(&database.pool)
            .await?;

            let revocation = SubscriptionGrantRevocation::new(
                grant_id,
                scope,
                subscriber,
                plan,
                ActorId::new(Uuid::now_v7()),
                SubscriptionGrantReason::new("must reject partial audit")?,
            );
            let mut transaction = database.pool.begin().await?;
            let error = revoke_subscription_grant(&mut transaction, &revocation)
                .await
                .expect_err("partial persisted revocation audit must fail closed");
            transaction.rollback().await?;
            if !matches!(error, SubscriptionGrantMutationError::InvalidState(_)) {
                return Err(io::Error::other(format!(
                    "expected invalid grant state, got {error:?}"
                ))
                .into());
            }
            Ok::<_, Box<dyn Error>>(())
        }
        .await;
        let cleanup = database.cleanup().await;
        result?;
        cleanup
    }

    #[tokio::test]
    async fn grant_admission_is_exactly_scope_subscriber_and_plan_scoped()
    -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("sr_grant_scope").await?;
        let result = async {
            let gateway = create_gateway_account(&database.pool, "test_gateway").await?;
            let plan_a = PlanKey::new("plan_a")?;
            let actor = ActorId::new(Uuid::now_v7());
            let subscriber_with_other_plan_attempt = SubscriberId::new(Uuid::now_v7());
            insert_pending_initial_attempt(
                &database.pool,
                gateway.billing_scope_id,
                subscriber_with_other_plan_attempt.into_uuid(),
                "plan_b",
                gateway.gateway_account_id,
                gateway.gateway_configuration_id,
            )
            .await?;
            let creation = SubscriptionGrantCreation::new(
                SubscriptionGrantId::new(Uuid::now_v7()),
                BillingScopeId::new(gateway.billing_scope_id),
                subscriber_with_other_plan_attempt,
                plan_a.clone(),
                SubscriptionGrantKind::Testing,
                SubscriptionGrantReason::new("plan isolation")?,
                Utc::now() + Duration::days(1),
                actor,
            );
            let mut transaction = database.pool.begin().await?;
            let outcome = create_subscription_grant(&mut transaction, &creation).await?;
            transaction.commit().await?;
            if !matches!(outcome, SubscriptionGrantCreationOutcome::Created(_)) {
                return Err(io::Error::other("another plan blocked grant creation").into());
            }

            let blocked_subscriber = SubscriberId::new(Uuid::now_v7());
            insert_pending_initial_attempt(
                &database.pool,
                gateway.billing_scope_id,
                blocked_subscriber.into_uuid(),
                plan_a.as_str(),
                gateway.gateway_account_id,
                gateway.gateway_configuration_id,
            )
            .await?;
            let blocked_creation = SubscriptionGrantCreation::new(
                SubscriptionGrantId::new(Uuid::now_v7()),
                BillingScopeId::new(gateway.billing_scope_id),
                blocked_subscriber,
                plan_a,
                SubscriptionGrantKind::Testing,
                SubscriptionGrantReason::new("same plan must block")?,
                Utc::now() + Duration::days(1),
                actor,
            );
            let mut blocked_transaction = database.pool.begin().await?;
            let blocked =
                create_subscription_grant(&mut blocked_transaction, &blocked_creation).await?;
            blocked_transaction.commit().await?;
            if blocked != SubscriptionGrantCreationOutcome::BlockingInitialAttempt {
                return Err(io::Error::other("same-plan initial attempt did not block").into());
            }
            Ok::<_, Box<dyn Error>>(())
        }
        .await;
        let cleanup = database.cleanup().await;
        result?;
        cleanup
    }

    async fn insert_pending_initial_attempt(
        pool: &sqlx::PgPool,
        scope: Uuid,
        subscriber: Uuid,
        plan: &str,
        gateway_account: Uuid,
        gateway_configuration: Uuid,
    ) -> Result<(), sqlx::Error> {
        let attempt = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO billing_payment_attempts (
                id,
                billing_scope_id,
                subscriber_id,
                plan_key,
                attempt_kind,
                status,
                idempotency_key,
                request_fingerprint,
                amount_cents,
                currency,
                gateway_account_id,
                gateway_configuration_id,
                gateway_order_id,
                subscription_initial_terms_version,
                subscription_initial_start_kind,
                subscription_initial_recurring_base_amount_cents,
                subscription_initial_recurring_period_kind,
                subscription_initial_recurring_period_count,
                subscription_initial_dunning_retry_delays_seconds,
                subscription_initial_dunning_exhaustion,
                subscription_initial_past_due_access
            ) VALUES (
                $1, $2, $3, $4, 'subscription_initial', 'pending', $5, $6,
                100, 'USD', $7, $8, $9, 2, 'recurring_immediately', 100,
                'calendar_months', 1, ARRAY[]::bigint[],
                'remain_past_due', 'suspend_immediately'
            )
            "#,
        )
        .bind(attempt)
        .bind(scope)
        .bind(subscriber)
        .bind(plan)
        .bind(format!("grant-{}", attempt.simple()))
        .bind(format!("initial:{plan}:100:USD"))
        .bind(gateway_account)
        .bind(gateway_configuration)
        .bind(format!("grant-order-{}", attempt.simple()))
        .execute(pool)
        .await?;
        Ok(())
    }
}
