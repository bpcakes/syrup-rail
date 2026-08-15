use super::*;

#[tokio::test]
async fn stale_unsubmitted_charge_does_not_block_renewal_dispatch() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("renew_local").await?;
    let account = create_gateway_account(&database.pool, "nmi").await?;
    let due_at = Utc::now() - Duration::minutes(10);
    let subscription = insert_due_subscription_at(
        &database.pool,
        account,
        "stale-local-charge-plan",
        Uuid::from_u128(14),
        due_at,
    )
    .await?;
    insert_renewal_attempt(
        &database.pool,
        account,
        &subscription,
        due_at,
        "subscription_recovery",
        "review_required",
        None,
        None,
    )
    .await?;
    sqlx::query(
        r#"
        UPDATE billing_payment_attempts
        SET created_at = clock_timestamp() - interval '31 minutes',
            updated_at = clock_timestamp() - interval '31 minutes'
        WHERE subscription_id = $1
        "#,
    )
    .bind(subscription.subscription_id)
    .execute(&database.pool)
    .await?;

    let page = due_renewals_page(&database.pool, None).await?;
    let dispatch = page
        .dispatches()
        .iter()
        .find(|dispatch| dispatch.subscription_id().into_uuid() == subscription.subscription_id)
        .expect("the stale local attempt no longer blocks renewal dispatch");
    assert_eq!(dispatch.attempt_sequence_count(), 1);

    database.cleanup().await
}
