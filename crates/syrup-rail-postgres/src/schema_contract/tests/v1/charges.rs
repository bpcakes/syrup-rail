use super::*;

#[tokio::test]
async fn schema_v1_processor_charge_triggers_preserve_dimensions_and_evidence()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start_v1("sr_charge_v1").await?;
    let result = async {
        let gateway = create_gateway_account(&database.pool, "test_gateway").await?;
        let subscriber_id = Uuid::now_v7();
        let target_id = Uuid::now_v7();
        let attempt_id = insert_host_charge_attempt_record(
            &database.pool,
            gateway,
            subscriber_id,
            target_id,
            "charge-trigger-order",
            "charge-trigger-idempotency",
        )
        .await?;

        let charge_id = Uuid::now_v7();
        let dimensions = sqlx::query_as::<
            _,
            (String, Option<String>, Option<Uuid>, i32, String, Option<String>),
        >(
            r#"
            INSERT INTO billing_processor_charges (
                id,
                attempt_id,
                billing_scope_id,
                gateway_account_id,
                gateway_order_id,
                gateway_transaction_id,
                gateway_response,
                attempt_kind,
                plan_key,
                host_charge_target_id,
                amount_cents,
                currency
            ) VALUES (
                $1, $2, $3, $4, $5, 'txn_charge_trigger', 'approved',
                'subscription_initial', 'wrong_plan', NULL, 999, 'EUR'
            )
            RETURNING
                attempt_kind,
                plan_key,
                host_charge_target_id,
                amount_cents,
                currency,
                gateway_response
            "#,
        )
        .bind(charge_id)
        .bind(attempt_id)
        .bind(gateway.billing_scope_id)
        .bind(gateway.gateway_account_id)
        .bind("charge-trigger-order")
        .fetch_one(&database.pool)
        .await?;
        if dimensions
            != (
                "host_charge".to_owned(),
                None,
                Some(target_id),
                100,
                "USD".to_owned(),
                Some("approved".to_owned()),
            )
        {
            return Err(io::Error::other(format!(
                "processor-charge trigger did not copy attempt dimensions: {dimensions:?}"
            ))
            .into());
        }

        let changed_evidence = sqlx::query(
            "UPDATE billing_processor_charges SET gateway_response = 'different' WHERE id = $1",
        )
        .bind(charge_id)
        .execute(&database.pool)
        .await;
        expect_database_rejection(changed_evidence, "mutable processor response evidence")?;

        let changed_dimension = sqlx::query(
            "UPDATE billing_processor_charges SET amount_cents = 101 WHERE id = $1",
        )
        .bind(charge_id)
        .execute(&database.pool)
        .await;
        expect_database_rejection(changed_dimension, "mutable processor charge dimension")?;

        let upgrade_attempt = insert_host_charge_attempt_record(
            &database.pool,
            gateway,
            subscriber_id,
            Uuid::now_v7(),
            "charge-upgrade-order",
            "charge-upgrade-idempotency",
        )
        .await?;
        let upgrade_charge = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO billing_processor_charges (
                id,
                attempt_id,
                billing_scope_id,
                gateway_account_id,
                gateway_order_id,
                attempt_kind,
                amount_cents,
                currency
            ) VALUES ($1, $2, $3, $4, $5, 'host_charge', 100, 'USD')
            "#,
        )
        .bind(upgrade_charge)
        .bind(upgrade_attempt)
        .bind(gateway.billing_scope_id)
        .bind(gateway.gateway_account_id)
        .bind("charge-upgrade-order")
        .execute(&database.pool)
        .await?;
        sqlx::query(
            "UPDATE billing_processor_charges SET gateway_transaction_id = 'txn_upgraded' WHERE id = $1",
        )
        .bind(upgrade_charge)
        .execute(&database.pool)
        .await?;
        let second_upgrade = sqlx::query(
            "UPDATE billing_processor_charges SET gateway_transaction_id = 'txn_changed' WHERE id = $1",
        )
        .bind(upgrade_charge)
        .execute(&database.pool)
        .await;
        expect_database_rejection(second_upgrade, "second transaction identity upgrade")?;

        let competing_attempt = insert_host_charge_attempt_record(
            &database.pool,
            gateway,
            subscriber_id,
            Uuid::now_v7(),
            "charge-owner-order",
            "charge-owner-idempotency",
        )
        .await?;
        let conflicting_owner = sqlx::query(
            r#"
            INSERT INTO billing_processor_charges (
                attempt_id,
                billing_scope_id,
                gateway_account_id,
                gateway_order_id,
                gateway_transaction_id,
                attempt_kind,
                amount_cents,
                currency
            ) VALUES ($1, $2, $3, $4, 'txn_charge_trigger', 'host_charge', 100, 'USD')
            "#,
        )
        .bind(competing_attempt)
        .bind(gateway.billing_scope_id)
        .bind(gateway.gateway_account_id)
        .bind("charge-owner-order")
        .execute(&database.pool)
        .await;
        expect_database_constraint(
            conflicting_owner,
            "billing_processor_charges_gateway_transaction_idx",
        )?;
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn schema_v1_host_charge_ledger_admission_matches_legacy_modes() -> Result<(), Box<dyn Error>>
{
    let database = TestDatabase::start_v1("sr_ledger_v1").await?;
    let result = async {
        let gateway = create_gateway_account(&database.pool, "test_gateway").await?;
        let subscriber_id = Uuid::now_v7();
        let target_id = Uuid::now_v7();

        assert_admission(
            &database.pool,
            gateway,
            subscriber_id,
            target_id,
            ("reserve", Some("ledger-key"), None, "safe"),
        )
        .await?;
        assert_admission(
            &database.pool,
            gateway,
            subscriber_id,
            target_id,
            ("release", None, None, "safe"),
        )
        .await?;
        assert_admission(
            &database.pool,
            gateway,
            subscriber_id,
            target_id,
            ("submit", None, Some(Uuid::now_v7()), "unsafe"),
        )
        .await?;

        let attempt_id = insert_host_charge_attempt_record(
            &database.pool,
            gateway,
            subscriber_id,
            target_id,
            "ledger-order",
            "ledger-key",
        )
        .await?;
        assert_admission(
            &database.pool,
            gateway,
            subscriber_id,
            target_id,
            ("reserve", Some("ledger-key"), None, "idempotent_contender"),
        )
        .await?;
        assert_admission(
            &database.pool,
            gateway,
            subscriber_id,
            target_id,
            ("reserve", Some("different-key"), None, "unsafe"),
        )
        .await?;
        assert_admission(
            &database.pool,
            gateway,
            subscriber_id,
            target_id,
            ("submit", None, Some(attempt_id), "safe"),
        )
        .await?;
        assert_admission(
            &database.pool,
            gateway,
            subscriber_id,
            target_id,
            ("release", None, None, "unsafe"),
        )
        .await?;

        sqlx::query(
            r#"
            UPDATE billing_payment_attempts
            SET status = 'failed', resolved_at = clock_timestamp()
            WHERE id = $1
            "#,
        )
        .bind(attempt_id)
        .execute(&database.pool)
        .await?;
        assert_admission(
            &database.pool,
            gateway,
            subscriber_id,
            target_id,
            ("release", None, None, "safe"),
        )
        .await?;
        assert_admission(
            &database.pool,
            gateway,
            subscriber_id,
            target_id,
            ("reserve", Some("replacement-key"), None, "safe"),
        )
        .await?;

        let charge_id = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO billing_processor_charges (
                id,
                attempt_id,
                billing_scope_id,
                gateway_account_id,
                gateway_order_id,
                gateway_transaction_id,
                attempt_kind,
                amount_cents,
                currency
            ) VALUES (
                $1, $2, $3, $4, 'ledger-order', 'txn_ledger',
                'host_charge', 100, 'USD'
            )
            "#,
        )
        .bind(charge_id)
        .bind(attempt_id)
        .bind(gateway.billing_scope_id)
        .bind(gateway.gateway_account_id)
        .execute(&database.pool)
        .await?;
        assert_admission(
            &database.pool,
            gateway,
            subscriber_id,
            target_id,
            ("release", None, None, "unsafe"),
        )
        .await?;

        sqlx::query(
            r#"
            UPDATE billing_processor_charges
            SET
                progression_state = 'externally_reversed',
                state_code = 'processor_charge_external_reversal_required',
                externally_reversed_at = clock_timestamp()
            WHERE id = $1
            "#,
        )
        .bind(charge_id)
        .execute(&database.pool)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO billing_external_reversal_attestations (
                attempt_id,
                processor_charge_id,
                actor_id,
                reversal_kind,
                reason,
                prior_resolution_code,
                final_resolution_code,
                gateway_account_id,
                gateway_configuration_id,
                gateway_order_id,
                amount_cents,
                currency,
                gateway_transaction_id,
                attested_at
            ) VALUES (
                $1, $2, $3, 'refund', 'operator confirmed refund',
                'processor_charge_external_reversal_required',
                'processor_charge_externally_refunded',
                $4, $5, 'ledger-order', 100, 'USD', 'txn_ledger',
                clock_timestamp()
            )
            "#,
        )
        .bind(attempt_id)
        .bind(charge_id)
        .bind(Uuid::now_v7())
        .bind(gateway.gateway_account_id)
        .bind(gateway.gateway_configuration_id)
        .execute(&database.pool)
        .await?;
        assert_admission(
            &database.pool,
            gateway,
            subscriber_id,
            target_id,
            ("release", None, None, "safe"),
        )
        .await?;

        for invalid in [
            ("reserve", None, None),
            ("submit", Some("unexpected"), Some(attempt_id)),
            ("release", None, Some(attempt_id)),
            ("unsupported", None, None),
        ] {
            let rejected = host_charge_admission(
                &database.pool,
                gateway,
                subscriber_id,
                target_id,
                invalid.0,
                invalid.1,
                invalid.2,
            )
            .await;
            if rejected.is_ok() {
                return Err(io::Error::other(format!(
                    "host charge ledger admitted invalid arguments: {invalid:?}"
                ))
                .into());
            }
        }
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}
