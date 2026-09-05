use super::*;

#[tokio::test]
async fn initial_attempt_row_locks_preserve_exact_scope_and_caller_transaction()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("sr_row_locks").await?;
    let result = async {
        install_host_offers(&database).await?;
        let account = create_gateway_account(&database.pool, "nmi").await?;
        let other_account = create_gateway_account(&database.pool, "nmi").await?;
        let subscriber_id = Uuid::now_v7();
        let plan_key = PlanKey::new("base_subscription")?;
        let mut attempt_ids = Vec::new();
        for (account, subscriber, plan) in [
            (account, subscriber_id, "base_subscription"),
            (account, Uuid::now_v7(), "base_subscription"),
            (account, subscriber_id, "premium"),
            (other_account, subscriber_id, "base_subscription"),
        ] {
            set_offer(&database, account.billing_scope_id, plan, 1_000).await?;
            let attempt_id = Uuid::now_v7();
            let command = enrollment_command(
                account,
                subscriber,
                attempt_id,
                &attempt_id.to_string(),
                full_price(plan, 1_000),
            );
            let reservation = SubscriptionEnrollmentReservation::from_command(
                &command,
                &resolved_gateway(account),
                GatewayAccountMode::Live,
            )?;
            let mut transaction = database.pool.begin().await?;
            assert!(matches!(
                reserve_subscription_enrollment_in_transaction(
                    &mut transaction,
                    &TestOfferStore,
                    &reservation,
                )
                .await?,
                SubscriptionEnrollmentReservationOutcome::Reserved(_)
            ));
            transaction.commit().await?;
            attempt_ids.push(attempt_id);
        }

        let mut holder = database.pool.begin().await?;
        lock_subscription_aggregate(&mut holder, SubscriberId::new(subscriber_id), &plan_key)
            .await?;
        lock_initial_attempt_rows(
            &mut holder,
            BillingScopeId::new(account.billing_scope_id),
            SubscriberId::new(subscriber_id),
            &plan_key,
        )
        .await?;
        for (index, id) in attempt_ids.iter().enumerate() {
            let mut contender = database.pool.begin().await?;
            let locked = sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM billing_payment_attempts WHERE id = $1 FOR UPDATE NOWAIT",
            )
            .bind(id)
            .fetch_one(&mut *contender)
            .await;
            contender.rollback().await?;
            if index == 0 {
                assert!(matches!(
                    locked,
                    Err(sqlx::Error::Database(error))
                        if error.code().as_deref() == Some("55P03")
                ));
            } else {
                assert_eq!(locked?, *id, "unrelated attempt must remain unlocked");
            }
        }
        holder.rollback().await?;
        let mut after_rollback = database.pool.begin().await?;
        let unlocked: Uuid = sqlx::query_scalar(
            "SELECT id FROM billing_payment_attempts WHERE id = $1 FOR UPDATE NOWAIT",
        )
        .bind(attempt_ids[0])
        .fetch_one(&mut *after_rollback)
        .await?;
        assert_eq!(unlocked, attempt_ids[0]);
        after_rollback.rollback().await?;
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}
