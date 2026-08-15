use sqlx::{Postgres, Transaction};
use tokio::sync::Mutex;

use super::*;
use crate::reconciliation::RECONCILIATION_PHASE_BATCH_SIZE;

#[tokio::test]
async fn a_full_page_of_target_skips_does_not_starve_unclaimed_work() -> Result<(), Box<dyn Error>>
{
    let database = TestDatabase::start("host_skip_page").await?;
    let result = async {
        install_reconciliation_targets(&database.pool).await?;
        let account = create_gateway_account(&database.pool, "host_reconciliation").await?;
        let base = Utc::now() - Duration::minutes(40);
        let mut first_skipped_attempt = None;
        for offset in 0..RECONCILIATION_PHASE_BATCH_SIZE {
            let skipped =
                insert_host_charge(&database.pool, account, base + Duration::seconds(offset))
                    .await?;
            first_skipped_attempt.get_or_insert(skipped.0);
            sqlx::query("DELETE FROM host_reconciliation_targets WHERE id = $1")
                .bind(skipped.1)
                .execute(&database.pool)
                .await?;
        }
        let releasable = insert_host_charge(
            &database.pool,
            account,
            base + Duration::seconds(RECONCILIATION_PHASE_BATCH_SIZE),
        )
        .await?;

        let first = fail_stale_unsubmitted_host_charges(
            &database.pool,
            &ReconciliationTargets,
            GatewayAccountId::new(account.gateway_account_id),
        )
        .await?;
        assert_eq!(first.failed(), 0);
        assert_eq!(
            first.skipped(),
            u64::try_from(RECONCILIATION_PHASE_BATCH_SIZE)?
        );
        assert_eq!(
            attempt_status(&database.pool, releasable.0).await?,
            "pending"
        );

        let (created_at, claimed_at): (chrono::DateTime<Utc>, chrono::DateTime<Utc>) =
            sqlx::query_as(
                "SELECT created_at, updated_at FROM billing_payment_attempts WHERE id = $1",
            )
            .bind(first_skipped_attempt.expect("one skipped attempt"))
            .fetch_one(&database.pool)
            .await?;
        assert!(claimed_at > created_at);

        // Simulate a scheduler cadence longer than the retry interval. The
        // older unclaimed row must still sort before all previously skipped
        // rows once those claims are eligible again.
        sqlx::query(
            r#"
            UPDATE billing_payment_attempts
            SET updated_at = clock_timestamp() - interval '2 minutes'
            WHERE gateway_account_id = $1 AND id <> $2 AND status = 'pending'
            "#,
        )
        .bind(account.gateway_account_id)
        .bind(releasable.0)
        .execute(&database.pool)
        .await?;

        let second = fail_stale_unsubmitted_host_charges(
            &database.pool,
            &ReconciliationTargets,
            GatewayAccountId::new(account.gateway_account_id),
        )
        .await?;
        assert_eq!(second.failed(), 1);
        assert_eq!(
            second.skipped(),
            u64::try_from(RECONCILIATION_PHASE_BATCH_SIZE - 1)?
        );
        assert_eq!(
            attempt_status(&database.pool, releasable.0).await?,
            "failed"
        );
        assert_eq!(
            target_status(&database.pool, releasable.1).await?,
            "released"
        );
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn a_locked_oldest_attempt_does_not_block_a_later_candidate() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("host_lock_skip").await?;
    let result = async {
        install_reconciliation_targets(&database.pool).await?;
        let account = create_gateway_account(&database.pool, "host_reconciliation").await?;
        let oldest =
            insert_host_charge(&database.pool, account, Utc::now() - Duration::minutes(32)).await?;
        let later =
            insert_host_charge(&database.pool, account, Utc::now() - Duration::minutes(31)).await?;
        let mut blocker = database.pool.begin().await?;
        sqlx::query("SELECT id FROM billing_payment_attempts WHERE id = $1 FOR UPDATE")
            .bind(oldest.0)
            .execute(&mut *blocker)
            .await?;

        let summary = fail_stale_unsubmitted_host_charges(
            &database.pool,
            &ReconciliationTargets,
            GatewayAccountId::new(account.gateway_account_id),
        )
        .await?;
        assert_eq!(summary.failed(), 1);
        assert_eq!(summary.skipped(), 0);
        assert_eq!(attempt_status(&database.pool, oldest.0).await?, "pending");
        assert_eq!(attempt_status(&database.pool, later.0).await?, "failed");
        blocker.rollback().await?;

        let retry = fail_stale_unsubmitted_host_charges(
            &database.pool,
            &ReconciliationTargets,
            GatewayAccountId::new(account.gateway_account_id),
        )
        .await?;
        assert_eq!(retry.failed(), 1);
        assert_eq!(attempt_status(&database.pool, oldest.0).await?, "failed");
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

struct LockFirstAttemptTargets {
    pool: sqlx::PgPool,
    blocked_attempt_id: Uuid,
    blocker: Mutex<Option<Transaction<'static, Postgres>>>,
}

#[async_trait]
impl HostChargeTargetStore for LockFirstAttemptTargets {
    async fn preflight_target(
        &self,
        connection: &mut PgConnection,
        reservation: &HostChargeTargetReservation,
    ) -> Result<HostChargeReservationDecision, HostChargeTargetError> {
        ReconciliationTargets
            .preflight_target(connection, reservation)
            .await
    }

    async fn reserve_target(
        &self,
        connection: &mut PgConnection,
        reservation: &HostChargeTargetReservation,
    ) -> Result<HostChargeReservationDecision, HostChargeTargetError> {
        ReconciliationTargets
            .reserve_target(connection, reservation)
            .await
    }

    async fn admit_submission(
        &self,
        connection: &mut PgConnection,
        admission: &HostChargeSubmissionAdmission,
    ) -> Result<HostChargeSubmissionDecision, HostChargeTargetError> {
        ReconciliationTargets
            .admit_submission(connection, admission)
            .await
    }

    async fn apply_transition(
        &self,
        connection: &mut PgConnection,
        transition: HostChargeTargetTransition,
    ) -> Result<HostChargeTargetTransitionOutcome, HostChargeTargetError> {
        let attempt_id = transition.attempt_id().into_uuid();
        if attempt_id != self.blocked_attempt_id {
            let blocker = self.blocker.lock().await.take();
            if let Some(blocker) = blocker {
                blocker
                    .rollback()
                    .await
                    .map_err(HostChargeTargetError::new)?;
            }
        }
        let outcome = ReconciliationTargets
            .apply_transition(connection, transition)
            .await?;
        if attempt_id == self.blocked_attempt_id {
            let mut blocker = self
                .pool
                .begin()
                .await
                .map_err(HostChargeTargetError::new)?;
            sqlx::query("SELECT id FROM billing_payment_attempts WHERE id = $1 FOR UPDATE")
                .bind(attempt_id)
                .execute(&mut *blocker)
                .await
                .map_err(HostChargeTargetError::new)?;
            *self.blocker.lock().await = Some(blocker);
        }
        Ok(outcome)
    }
}

#[tokio::test]
async fn contention_after_claim_rolls_back_one_target_and_continues_the_page()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("host_claim_lock").await?;
    let result = async {
        install_reconciliation_targets(&database.pool).await?;
        let account = create_gateway_account(&database.pool, "host_reconciliation").await?;
        let contended =
            insert_host_charge(&database.pool, account, Utc::now() - Duration::minutes(32)).await?;
        let later =
            insert_host_charge(&database.pool, account, Utc::now() - Duration::minutes(31)).await?;
        let targets = LockFirstAttemptTargets {
            pool: database.pool.clone(),
            blocked_attempt_id: contended.0,
            blocker: Mutex::new(None),
        };

        let summary = fail_stale_unsubmitted_host_charges(
            &database.pool,
            &targets,
            GatewayAccountId::new(account.gateway_account_id),
        )
        .await?;
        assert_eq!(summary.failed(), 1);
        assert_eq!(summary.skipped(), 1);
        assert!(targets.blocker.lock().await.is_none());
        assert_eq!(
            attempt_status(&database.pool, contended.0).await?,
            "pending"
        );
        assert_eq!(
            target_status(&database.pool, contended.1).await?,
            "reserved"
        );
        assert_eq!(attempt_status(&database.pool, later.0).await?, "failed");
        assert_eq!(target_status(&database.pool, later.1).await?, "released");
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}
