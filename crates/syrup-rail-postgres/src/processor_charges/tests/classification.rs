use super::*;

#[tokio::test]
async fn pending_additional_approval_is_classified_without_changing_its_owner()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("chg_class_extra").await?;
    let result = async {
        let gateway = create_gateway_account(&database.pool, "test_gateway").await?;
        let kind = PaymentAttemptKind::SubscriptionRecovery;
        let fixture = insert_subscription_attempt(&database.pool, gateway, kind, "classify-extra-order").await?;
        let attempt = fixture.identity.attempt_id();
        sqlx::query("UPDATE billing_payment_attempts SET status = 'approved', resolved_at = clock_timestamp(), gateway_transaction_id = 'classify-primary' WHERE id = $1")
            .bind(attempt.as_uuid()).execute(&database.pool).await?;
        for id in ["classify-primary", "classify-additional"] {
            assert_eq!(
                persist_approved_evidence_without_attempt_lock(
                    &database.pool, lock_free_terms(&fixture, kind), &approved_evidence(id),
                ).await?,
                LockFreeApprovedEvidenceOutcome::Persisted,
            );
        }
        let before = owner_snapshot(&database.pool, attempt).await?;
        let summary = crate::classify_pending_processor_charges(
            &database.pool, fixture.identity.gateway_account_id(), 100,
        ).await?;
        assert_eq!(summary.transitioned(), 2);
        assert_eq!(summary.remaining_pending(), 0);
        let charges: Vec<(String, String, Option<String>)> = sqlx::query_as(
            "SELECT charge_role, progression_state, state_code FROM billing_processor_charges WHERE attempt_id = $1 ORDER BY charge_role",
        ).bind(attempt.as_uuid()).fetch_all(&database.pool).await?;
        assert_eq!(charges, vec![
            ("additional".to_owned(), "external_reversal_required".to_owned(), Some("additional_approved_charge_identified".to_owned())),
            ("primary".to_owned(), "applied".to_owned(), None),
        ]);
        assert_eq!(owner_snapshot(&database.pool, attempt).await?, before);
        let replay = crate::classify_pending_processor_charges(
            &database.pool, fixture.identity.gateway_account_id(), 100,
        ).await?;
        assert_eq!(replay.transitioned(), 0);
        assert_eq!(replay.remaining_pending(), 0);
        let retained: Vec<(String, String, Option<String>)> = sqlx::query_as(
            "SELECT charge_role, progression_state, state_code FROM billing_processor_charges WHERE attempt_id = $1 ORDER BY charge_role",
        ).bind(attempt.as_uuid()).fetch_all(&database.pool).await?;
        assert_eq!(retained, charges);
        assert_eq!(owner_snapshot(&database.pool, attempt).await?, before);
        Ok::<_, Box<dyn Error>>(())
    }.await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

async fn owner_snapshot(
    pool: &PgPool,
    attempt: PaymentAttemptId,
) -> Result<serde_json::Value, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT jsonb_build_object('attempt', to_jsonb(a), 'subscription', to_jsonb(s)) FROM billing_payment_attempts a JOIN billing_subscriptions s ON s.id = a.subscription_id WHERE a.id = $1",
    ).bind(attempt.as_uuid()).fetch_one(pool).await
}
