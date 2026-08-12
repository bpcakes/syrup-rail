use super::super::fixtures::*;
use super::super::*;

#[tokio::test]
async fn schema_v1_retry_reclassification_audit_reports_every_reactivated_history()
-> Result<(), Box<dyn Error>> {
    let normalized = V1_TO_V2_RETRY_RECLASSIFICATION_AUDIT_SQL
        .trim()
        .to_ascii_uppercase();
    if !normalized.starts_with("-- READ-ONLY RETRY RECLASSIFICATION AUDIT")
        || normalized.matches(';').count() != 1
        || !normalized.ends_with(';')
    {
        return Err(io::Error::other(
            "version-1 retry reclassification audit must be one checked-in result query",
        )
        .into());
    }
    for forbidden in [
        "\nINSERT ",
        "\nUPDATE ",
        "\nDELETE ",
        "\nALTER ",
        "\nCREATE ",
        "\nDROP ",
        "\nDO ",
        "\nBEGIN",
        "\nCOMMIT",
    ] {
        if normalized.contains(forbidden) {
            return Err(io::Error::other(format!(
                "version-1 retry reclassification audit contains mutation or transaction control {forbidden:?}"
            ))
            .into());
        }
    }

    let database = TestDatabase::start_v1("sr_retry_audit").await?;
    let result = async {
        let gateway = create_gateway_account(&database.pool, "test_gateway").await?;

        let active_subscriber_id = Uuid::now_v7();
        let active_recovery_only = create_v1_subscription_with_status(
            &database.pool,
            gateway,
            active_subscriber_id,
            "active",
        )
        .await?;
        insert_v1_recovery_failures(
            &database.pool,
            gateway,
            active_subscriber_id,
            &active_recovery_only,
            5,
            "active",
        )
        .await?;

        let past_due_subscriber_id = Uuid::now_v7();
        let past_due_recovery_only = create_v1_subscription_with_status(
            &database.pool,
            gateway,
            past_due_subscriber_id,
            "active",
        )
        .await?;
        apply_v1_manual_active_recovery_failure(
            &database.pool,
            gateway,
            past_due_subscriber_id,
            past_due_recovery_only.0,
            past_due_recovery_only.1,
            &past_due_recovery_only.2,
        )
        .await?;
        insert_v1_recovery_failures(
            &database.pool,
            gateway,
            past_due_subscriber_id,
            &past_due_recovery_only,
            4,
            "past_due",
        )
        .await?;

        let mixed_subscriber_id = Uuid::now_v7();
        let mixed = create_v1_subscription_with_status(
            &database.pool,
            gateway,
            mixed_subscriber_id,
            "past_due",
        )
        .await?;
        insert_v1_reclassified_exhausted_history(
            &database.pool,
            gateway,
            mixed_subscriber_id,
            mixed.0,
            mixed.1,
            &mixed.2,
        )
        .await?;

        let exhausted_subscriber_id = Uuid::now_v7();
        let exhausted = create_v1_subscription_with_status(
            &database.pool,
            gateway,
            exhausted_subscriber_id,
            "past_due",
        )
        .await?;
        for sequence in 0..5 {
            let day = sequence + 2;
            insert_v1_terminal_subscription_attempt(
                &database.pool,
                gateway,
                exhausted_subscriber_id,
                exhausted.0,
                exhausted.1,
                &exhausted.2,
                "subscription_renewal",
                "declined",
                &format!("retry-audit-exhausted-{sequence}"),
                Some(&format!("2026-02-{day:02} 00:00:00+00")),
                &format!("2026-02-{day:02} 00:01:00+00"),
                None,
            )
            .await?;
        }

        let below_ceiling_subscriber_id = Uuid::now_v7();
        let below_ceiling = create_v1_subscription_with_status(
            &database.pool,
            gateway,
            below_ceiling_subscriber_id,
            "active",
        )
        .await?;
        insert_v1_recovery_failures(
            &database.pool,
            gateway,
            below_ceiling_subscriber_id,
            &below_ceiling,
            4,
            "active",
        )
        .await?;

        let candidates = sqlx::query(V1_TO_V2_RETRY_RECLASSIFICATION_AUDIT_SQL)
            .fetch_all(&database.pool)
            .await?
            .into_iter()
            .map(|row| {
                Ok::<_, sqlx::Error>((
                    row.try_get::<Uuid, _>("subscription_id")?,
                    row.try_get::<String, _>("subscription_status")?,
                    row.try_get::<i64, _>("v1_terminal_failure_count")?,
                    row.try_get::<i64, _>("v2_automatic_failure_count")?,
                ))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let expected = [
            (active_recovery_only.1, "active".to_owned(), 5, 0),
            (past_due_recovery_only.1, "past_due".to_owned(), 5, 0),
            (mixed.1, "past_due".to_owned(), 5, 3),
        ];
        if candidates.len() != expected.len()
            || !expected
                .iter()
                .all(|candidate| candidates.contains(candidate))
        {
            return Err(io::Error::other(format!(
                "retry reclassification audit returned the wrong candidates: {candidates:?}"
            ))
            .into());
        }
        if candidates
            .iter()
            .any(|candidate| candidate.0 == exhausted.1 || candidate.0 == below_ceiling.1)
        {
            return Err(io::Error::other(
                "retry reclassification audit included an unchanged retry history",
            )
            .into());
        }

        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

async fn insert_v1_recovery_failures(
    pool: &PgPool,
    gateway: GatewayAccountFixture,
    subscriber_id: Uuid,
    subscription: &(Uuid, Uuid, String),
    failure_count: usize,
    expected_status: &str,
) -> Result<(), sqlx::Error> {
    debug_assert!(matches!(expected_status, "active" | "past_due"));
    for sequence in 0..failure_count {
        let day = sequence + 2;
        let attempt_id = insert_v1_terminal_subscription_attempt(
            pool,
            gateway,
            subscriber_id,
            subscription.0,
            subscription.1,
            &subscription.2,
            "subscription_recovery",
            "declined",
            &format!(
                "retry-audit-{expected_status}-recovery-{sequence}-{}",
                opaque_fixture_uuid(subscription.1)
            ),
            Some(&format!("2026-02-{day:02} 00:00:00+00")),
            &format!("2026-02-{day:02} 00:01:00+00"),
            None,
        )
        .await?;
        sqlx::query(
            "UPDATE billing_payment_attempts SET subscription_expected_status = $2 WHERE id = $1",
        )
        .bind(attempt_id)
        .bind(expected_status)
        .execute(pool)
        .await?;
    }
    Ok(())
}
