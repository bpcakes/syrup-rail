use std::{
    error::Error,
    io,
    sync::atomic::{AtomicU64, Ordering},
};

use super::*;
use crate::test_support::{
    TestDatabase, create_gateway_account, explain_plan_root, plan_has_node_type,
};
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

    async fn ensure_submission_admitted(
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
                required_gateway_account_mode,
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
                'live',
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
    .bind(format!("order-{}", attempt_id.simple()))
    .bind(transaction_id)
    .bind(resolved_at)
    .execute(pool)
    .await?;
    Ok(attempt_id)
}

#[test]
fn summary_reducer_counts_every_outcome_once() {
    for (outcome, expected) in [
        (
            GatewayLifecycleSummaryOutcome::Evidence(GatewayLifecycleApplyOutcome::Applied),
            (1, 0, 0),
        ),
        (
            GatewayLifecycleSummaryOutcome::Evidence(
                GatewayLifecycleApplyOutcome::AlreadySuperseded,
            ),
            (0, 0, 0),
        ),
        (
            GatewayLifecycleSummaryOutcome::Evidence(
                GatewayLifecycleApplyOutcome::InvalidRefundEconomics,
            ),
            (0, 0, 1),
        ),
        (
            GatewayLifecycleSummaryOutcome::Evidence(
                GatewayLifecycleApplyOutcome::ConflictingLifecycleEvidence,
            ),
            (0, 0, 1),
        ),
        (
            GatewayLifecycleSummaryOutcome::Evidence(GatewayLifecycleApplyOutcome::StagedAmbiguous),
            (0, 1, 0),
        ),
        (
            GatewayLifecycleSummaryOutcome::Evidence(GatewayLifecycleApplyOutcome::StagedNoMatch),
            (0, 1, 0),
        ),
        (
            GatewayLifecycleSummaryOutcome::ExplicitQuarantine,
            (0, 0, 1),
        ),
    ] {
        let mut summary = GatewayLifecycleReconciliationSummary::default();
        summary.record_outcome(outcome);
        assert_eq!(
            (summary.applied(), summary.staged(), summary.quarantined()),
            expected,
        );
    }
}

#[tokio::test]
async fn summary_accounts_for_incoming_and_staged_quarantine_and_superseded_evidence()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("life_summary").await?;
    let fixture = create_gateway_account(&database.pool, "nmi").await?;
    let account = lifecycle_account(fixture, "nmi");
    let host_targets = ExactHostTargets::default();
    let observed_at = Utc::now() - chrono::Duration::minutes(1);

    insert_host_attempt(
        &database.pool,
        fixture,
        Uuid::now_v7(),
        Uuid::now_v7(),
        "txn-invalid-economics",
        1_000,
        observed_at,
    )
    .await?;
    let conflicting_attempt_id = insert_host_attempt(
        &database.pool,
        fixture,
        Uuid::now_v7(),
        Uuid::now_v7(),
        "txn-conflicting-evidence",
        1_000,
        observed_at,
    )
    .await?;
    sqlx::query(
        r#"
            UPDATE billing_payment_attempts
            SET gateway_lifecycle_status = 'settled',
                gateway_lifecycle_at = $2,
                refunded_amount_cents = 100
            WHERE id = $1
            "#,
    )
    .bind(conflicting_attempt_id)
    .bind(observed_at)
    .execute(&database.pool)
    .await?;

    let incoming = reconcile_gateway_transaction_reports(
        &database.pool,
        &host_targets,
        &account,
        vec![
            evidence(
                "txn-invalid-economics",
                GatewayLifecycleState::Refunded {
                    cumulative_refunded_cents: CumulativeRefundCents::new(500)?,
                },
                observed_at,
            )?,
            evidence(
                "txn-conflicting-evidence",
                GatewayLifecycleState::Voided,
                observed_at + chrono::Duration::seconds(1),
            )?,
            GatewayTransactionReport::Quarantine(GatewayLifecycleQuarantine::new(
                Some(GatewayTransactionId::new("txn-explicit-quarantine")?),
                None,
                GatewayLifecycleQuarantineReason::MalformedReportStructure,
            )?),
            GatewayTransactionReport::Ignore,
        ],
    )
    .await?;
    assert_eq!(incoming.applied(), 0);
    assert_eq!(incoming.staged(), 0);
    assert_eq!(incoming.quarantined(), 3);
    assert_eq!(incoming.cleaned(), 0);

    insert_host_attempt(
        &database.pool,
        fixture,
        Uuid::now_v7(),
        Uuid::now_v7(),
        "txn-superseded",
        1_000,
        observed_at,
    )
    .await?;
    let superseded_report = evidence(
        "txn-superseded",
        GatewayLifecycleState::PendingSettlement,
        observed_at,
    )?;
    let applied = reconcile_gateway_transaction_reports(
        &database.pool,
        &host_targets,
        &account,
        vec![superseded_report.clone()],
    )
    .await?;
    assert_eq!(applied.applied(), 1);
    let superseded = reconcile_gateway_transaction_reports(
        &database.pool,
        &host_targets,
        &account,
        vec![superseded_report],
    )
    .await?;
    assert_eq!(superseded, GatewayLifecycleReconciliationSummary::default());

    let staged = reconcile_gateway_transaction_reports(
        &database.pool,
        &host_targets,
        &account,
        vec![evidence(
            "txn-staged-invalid",
            GatewayLifecycleState::Refunded {
                cumulative_refunded_cents: CumulativeRefundCents::new(500)?,
            },
            observed_at,
        )?],
    )
    .await?;
    assert_eq!(staged.staged(), 1);
    insert_host_attempt(
        &database.pool,
        fixture,
        Uuid::now_v7(),
        Uuid::now_v7(),
        "txn-staged-invalid",
        1_000,
        observed_at,
    )
    .await?;
    let drained =
        apply_staged_gateway_lifecycle_evidence(&database.pool, &host_targets, &account).await?;
    assert_eq!(drained.applied(), 0);
    assert_eq!(drained.staged(), 0);
    assert_eq!(drained.quarantined(), 1);
    assert_eq!(drained.cleaned(), 0);
    let pending_invalid: i64 = sqlx::query_scalar(
        r#"
            SELECT COUNT(*)
            FROM billing_gateway_lifecycle_pending_updates
            WHERE gateway_account_id = $1
                AND gateway_transaction_id = 'txn-staged-invalid'
            "#,
    )
    .bind(fixture.gateway_account_id)
    .fetch_one(&database.pool)
    .await?;
    assert_eq!(pending_invalid, 0);

    database.cleanup().await
}
