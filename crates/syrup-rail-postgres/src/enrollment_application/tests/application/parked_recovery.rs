use super::*;

#[tokio::test]
async fn initial_approval_recovers_after_grant_conflict_clears() -> Result<(), Box<dyn Error>> {
    let fixture = application_fixture("initial_park_clear").build().await?;
    let result = async {
        let grant_id = Uuid::now_v7();
        sqlx::query("INSERT INTO billing_subscription_grants (id, billing_scope_id, subscriber_id, plan_key, grant_kind, reason, starts_at, ends_at, granted_by_actor_id) VALUES ($1, $2, $3, $4, 'promotion', 'test grant', clock_timestamp() - interval '1 hour', clock_timestamp() + interval '1 day', $5)")
            .bind(grant_id).bind(fixture.command.billing_scope_id().as_uuid())
            .bind(fixture.command.subscriber_id().as_uuid()).bind(fixture.command.plan_key().as_str())
            .bind(Uuid::now_v7()).execute(&fixture.database.pool).await?;
        let outcome = approved_outcome("grant-conflict-recovery");
        let parked = apply_subscription_enrollment_gateway_outcome(
            &fixture.database.pool, &fixture.coordinator, &fixture.reservation, &outcome,
        ).await?;
        assert_eq!(parked.attempt().status(), PaymentAttemptStatus::ReviewRequired);
        assert_eq!(parked.attempt().state().resolution_code(), Some(PaymentResolutionCode::SubscriptionInitialCurrentGrantConflict));
        assert!(parked.subscription().is_none());
        sqlx::query("UPDATE billing_subscription_grants SET ends_at = clock_timestamp() - interval '1 second' WHERE id = $1")
            .bind(grant_id).execute(&fixture.database.pool).await?;
        for _ in 0..2 {
            let recovered = apply_reconciled_subscription_enrollment_gateway_outcome(
                &fixture.database.pool, &fixture.coordinator, fixture.command.billing_scope_id(),
                fixture.reservation.identity().attempt_id(), &outcome,
            ).await?;
            assert_eq!(recovered.attempt().status(), PaymentAttemptStatus::Approved);
            assert!(recovered.subscription().is_some());
        }
        let state: (i64, String) = sqlx::query_as("SELECT count(*), min(progression_state) FROM billing_processor_charges WHERE attempt_id = $1")
            .bind(fixture.reservation.identity().attempt_id().as_uuid()).fetch_one(&fixture.database.pool).await?;
        assert_eq!(state, (1, "applied".to_owned()));
        assert_eq!(fixture.coordinator.events.lock().await.iter().filter(|e| matches!(e, BillingEvent::SubscriptionStarted { .. })).count(), 1);
        Ok::<_, Box<dyn Error>>(())
    }.await;
    let cleanup = fixture.cleanup().await;
    result?;
    cleanup
}
