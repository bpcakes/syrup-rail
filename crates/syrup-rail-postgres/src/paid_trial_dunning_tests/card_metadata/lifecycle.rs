use super::*;
use syrup_rail::{GatewayLifecycleAccount, GatewayLifecycleEvidence, GatewayLifecycleState};

#[tokio::test]
async fn card_metadata_lifecycle_reconciliation_during_query_preserves_repair()
-> Result<(), Box<dyn Error>> {
    let db = TestDatabase::start("meta_lifecycle").await?;
    let fixture = Fixture::new(&db.pool).await?;
    let before_lifecycle = fixture.financial_snapshot(&db.pool).await?;
    let mut provider = QueryGateway::new(Reply::observation(Some(metadata(
        "txn_metadata",
        None,
        "Visa",
        "1111",
        Some(10),
        Some(2029),
    ))));
    provider.block = true;
    let provider = Arc::new(provider);
    let resolver = fixture.resolver(provider.clone())?;
    let pool = db.pool.clone();
    let command = fixture.command();
    let pending =
        tokio::spawn(
            async move { refresh_payment_method_metadata(&pool, &resolver, command).await },
        );
    tokio::time::timeout(Duration::from_secs(3), provider.started.notified()).await?;
    let account = GatewayLifecycleAccount::new(
        fixture.scope(),
        GatewayAccountId::new(fixture.account.gateway_account_id),
        GatewayProviderKey::new("nmi")?,
    );
    let evidence = GatewayLifecycleEvidence::new(
        Some(GatewayTransactionId::new("txn_metadata")?),
        None,
        GatewayLifecycleState::Settled {
            cumulative_refunded_cents: None,
        },
        Some(GatewayDiagnostic::new("complete")),
        None,
        Some(Utc::now()),
    )?;
    crate::apply_gateway_lifecycle_evidence(&db.pool, &NoHostTargets, &account, &evidence).await?;
    let status: String = sqlx::query_scalar(
        "SELECT gateway_lifecycle_status FROM billing_payment_attempts WHERE id = $1",
    )
    .bind(fixture.attempt_id.as_uuid())
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(status, "settled");
    let financial = fixture.financial_snapshot(&db.pool).await?;
    assert_ne!(
        financial, before_lifecycle,
        "real reconciliation changed its observation"
    );
    provider.release.notify_one();
    assert_eq!(pending.await??, Outcome::Updated);
    assert_eq!(fixture.financial_snapshot(&db.pool).await?, financial);
    let portal = subscription_billing_portal(&db.pool, &fixture.portal_query()).await?;
    assert_eq!(
        portal.payment_method_display().unwrap().card_last_four(),
        Some("1111")
    );
    db.cleanup().await?;
    Ok(())
}

struct NoHostTargets;

#[async_trait]
impl crate::HostChargeTargetStore for NoHostTargets {
    async fn preflight_target(
        &self,
        _: &mut PgConnection,
        _: &crate::HostChargeTargetReservation,
    ) -> Result<crate::HostChargeReservationDecision, crate::HostChargeTargetError> {
        panic!("subscription settlement does not preflight host targets")
    }

    async fn reserve_target(
        &self,
        _: &mut PgConnection,
        _: &crate::HostChargeTargetReservation,
    ) -> Result<crate::HostChargeReservationDecision, crate::HostChargeTargetError> {
        panic!("subscription settlement does not reserve host targets")
    }

    async fn ensure_submission_admitted(
        &self,
        _: &mut PgConnection,
        _: &crate::HostChargeSubmissionAdmission,
    ) -> Result<crate::HostChargeSubmissionDecision, crate::HostChargeTargetError> {
        panic!("subscription settlement does not submit host charges")
    }

    async fn apply_transition(
        &self,
        _: &mut PgConnection,
        _: syrup_rail::HostChargeTargetTransition,
    ) -> Result<syrup_rail::HostChargeTargetTransitionOutcome, crate::HostChargeTargetError> {
        panic!("subscription settlement does not transition host targets")
    }
}
