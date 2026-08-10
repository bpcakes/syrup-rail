use super::*;

#[tokio::test]
async fn schema_v1_catalog_conformance_accepts_host_extensions_and_rejects_canonical_drift()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start_v1("sr_catalog_v1").await?;
    let result = async {
        sqlx::raw_sql(
            r#"
            CREATE TABLE example_host_billing_scopes (
                id uuid PRIMARY KEY
            );

            ALTER TABLE billing_gateway_accounts
                ADD CONSTRAINT example_host_gateway_accounts_scope_fk
                FOREIGN KEY (billing_scope_id)
                REFERENCES example_host_billing_scopes(id)
                ON DELETE RESTRICT;

            CREATE INDEX example_host_gateway_accounts_scope_idx
            ON billing_gateway_accounts (billing_scope_id, id);

            CREATE FUNCTION example_host_gateway_account_noop()
            RETURNS trigger
            LANGUAGE plpgsql
            SET search_path = pg_catalog, public
            AS $$
            BEGIN
                RETURN NEW;
            END
            $$;

            CREATE TRIGGER example_host_gateway_account_noop
            BEFORE UPDATE ON billing_gateway_accounts
            FOR EACH ROW
            EXECUTE FUNCTION example_host_gateway_account_noop();
            "#,
        )
        .execute(&database.pool)
        .await?;
        assert_v1_conforms(&database.pool).await?;

        sqlx::query("ALTER TABLE billing_gateway_accounts ADD COLUMN host_drift text")
            .execute(&database.pool)
            .await?;
        expect_conformance_rejection(&database.pool, "canonical column drift").await?;
        sqlx::query("ALTER TABLE billing_gateway_accounts DROP COLUMN host_drift")
            .execute(&database.pool)
            .await?;
        assert_v1_conforms(&database.pool).await?;

        sqlx::query(
            r#"
            ALTER TABLE billing_gateway_accounts
            ADD CONSTRAINT billing_gateway_accounts_host_drift_check
            CHECK (billing_scope_id IS NOT NULL)
            "#,
        )
        .execute(&database.pool)
        .await?;
        expect_conformance_rejection(&database.pool, "canonical constraint drift").await?;
        sqlx::query(
            r#"
            ALTER TABLE billing_gateway_accounts
            DROP CONSTRAINT billing_gateway_accounts_host_drift_check
            "#,
        )
        .execute(&database.pool)
        .await?;
        assert_v1_conforms(&database.pool).await?;

        sqlx::query(
            r#"
            CREATE INDEX billing_gateway_accounts_host_drift_idx
            ON billing_gateway_accounts (updated_at)
            "#,
        )
        .execute(&database.pool)
        .await?;
        expect_conformance_rejection(&database.pool, "canonical index drift").await?;
        sqlx::query("DROP INDEX billing_gateway_accounts_host_drift_idx")
            .execute(&database.pool)
            .await?;
        assert_v1_conforms(&database.pool).await?;

        let payment_facts_definition = sqlx::query_scalar::<_, String>(
            "SELECT pg_catalog.pg_get_viewdef('billing_payment_facts'::regclass, true)",
        )
        .fetch_one(&database.pool)
        .await?;
        let payment_facts_definition = payment_facts_definition.trim_end_matches(';');
        let drifted_view = format!(
            "CREATE OR REPLACE VIEW billing_payment_facts AS SELECT * FROM ({payment_facts_definition}) AS canonical_payment_facts WHERE false"
        );
        sqlx::raw_sql(&drifted_view)
            .execute(&database.pool)
            .await?;
        expect_conformance_rejection(&database.pool, "canonical view drift").await?;
        let restored_view = format!(
            "CREATE OR REPLACE VIEW billing_payment_facts AS {payment_facts_definition}"
        );
        sqlx::raw_sql(&restored_view)
            .execute(&database.pool)
            .await?;
        assert_v1_conforms(&database.pool).await?;

        let canonical_function = sqlx::query_scalar::<_, String>(
            r#"
            SELECT pg_catalog.pg_get_functiondef(
                'billing_canonical_gateway_transaction_id(text)'::regprocedure
            )
            "#,
        )
        .fetch_one(&database.pool)
        .await?;
        sqlx::query(
            "ALTER FUNCTION billing_canonical_gateway_transaction_id(text) COST 101",
        )
        .execute(&database.pool)
        .await?;
        expect_conformance_rejection(&database.pool, "canonical function drift").await?;
        sqlx::raw_sql(&canonical_function)
            .execute(&database.pool)
            .await?;
        assert_v1_conforms(&database.pool).await?;

        sqlx::query(
            r#"
            ALTER TABLE billing_processor_charges
            DISABLE TRIGGER billing_processor_charge_evidence_immutable
            "#,
        )
        .execute(&database.pool)
        .await?;
        expect_conformance_rejection(&database.pool, "disabled canonical trigger").await?;
        sqlx::query(
            r#"
            ALTER TABLE billing_processor_charges
            ENABLE TRIGGER billing_processor_charge_evidence_immutable
            "#,
        )
        .execute(&database.pool)
        .await?;
        assert_v1_conforms(&database.pool).await?;
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn schema_v1_gateway_diagnostics_are_bounded() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start_v1("sr_text_v1").await?;
    let result = async {
        let gateway = create_gateway_account(&database.pool, "test_gateway").await?;
        let subscriber_id = Uuid::now_v7();
        let ascii_boundary = "a".repeat(512);
        let multibyte_boundary = "é".repeat(256);
        let oversized = "é".repeat(257);

        for (position, field) in ["payment_type", "card_brand"].iter().enumerate() {
            let boundary = if position.is_multiple_of(2) {
                &ascii_boundary
            } else {
                &multibyte_boundary
            };
            insert_payment_method_gateway_text(
                &database.pool,
                gateway,
                subscriber_id,
                field,
                boundary,
            )
            .await?;
            let rejected = insert_payment_method_gateway_text(
                &database.pool,
                gateway,
                subscriber_id,
                field,
                &oversized,
            )
            .await;
            expect_database_constraint(rejected, "billing_payment_methods_gateway_text_check")?;
        }

        for (position, field) in [
            "gateway_response",
            "gateway_response_code",
            "gateway_response_text",
            "gateway_condition",
            "payment_type",
            "card_brand",
            "gateway_lifecycle_action",
        ]
        .iter()
        .enumerate()
        {
            let boundary = if position.is_multiple_of(2) {
                &ascii_boundary
            } else {
                &multibyte_boundary
            };
            insert_attempt_gateway_text(&database.pool, gateway, subscriber_id, field, boundary)
                .await?;
            let rejected = insert_attempt_gateway_text(
                &database.pool,
                gateway,
                subscriber_id,
                field,
                &oversized,
            )
            .await;
            expect_database_constraint(rejected, "billing_payment_attempts_gateway_text_check")?;
        }

        for (position, field) in [
            "gateway_response",
            "gateway_response_code",
            "gateway_response_text",
            "gateway_condition",
            "payment_type",
            "card_brand",
        ]
        .iter()
        .enumerate()
        {
            let boundary = if position.is_multiple_of(2) {
                &ascii_boundary
            } else {
                &multibyte_boundary
            };
            insert_charge_gateway_text(&database.pool, gateway, subscriber_id, field, boundary)
                .await?;
            let rejected = insert_charge_gateway_text(
                &database.pool,
                gateway,
                subscriber_id,
                field,
                &oversized,
            )
            .await;
            expect_database_constraint(rejected, "billing_processor_charges_gateway_text_check")?;
        }

        for (position, field) in [
            "gateway_response",
            "gateway_response_code",
            "gateway_response_text",
            "gateway_condition",
            "payment_type",
            "card_brand",
        ]
        .iter()
        .enumerate()
        {
            let boundary = if position.is_multiple_of(2) {
                &ascii_boundary
            } else {
                &multibyte_boundary
            };
            insert_attestation_gateway_text(
                &database.pool,
                gateway,
                subscriber_id,
                field,
                boundary,
            )
            .await?;
            let rejected = insert_attestation_gateway_text(
                &database.pool,
                gateway,
                subscriber_id,
                field,
                &oversized,
            )
            .await;
            expect_database_constraint(
                rejected,
                "billing_external_reversal_attestations_gateway_text_check",
            )?;
        }

        for (position, field) in ["gateway_condition", "gateway_lifecycle_action"]
            .iter()
            .enumerate()
        {
            let boundary = if position.is_multiple_of(2) {
                &ascii_boundary
            } else {
                &multibyte_boundary
            };
            insert_lifecycle_gateway_text(&database.pool, gateway, field, boundary).await?;
            let rejected =
                insert_lifecycle_gateway_text(&database.pool, gateway, field, &oversized).await;
            expect_database_constraint(
                rejected,
                "billing_gateway_lifecycle_pending_gateway_text_check",
            )?;
        }
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn schema_v1_nullable_shapes_reject_missing_required_fields() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start_v1("sr_shapes_v1").await?;
    let result = async {
        let gateway = create_gateway_account(&database.pool, "test_gateway").await?;
        let subscriber_id = Uuid::now_v7();
        let (payment_method_id, subscription_id, initial_transaction_id) =
            create_subscription_fixture(&database.pool, gateway, subscriber_id).await?;

        let missing_host_target = sqlx::query(
            r#"
            INSERT INTO billing_payment_attempts (
                id, billing_scope_id, subscriber_id, attempt_kind, status,
                idempotency_key, request_fingerprint, amount_cents, currency,
                gateway_account_id, gateway_configuration_id, gateway_order_id
            ) VALUES (
                $1, $2, $3, 'host_charge', 'pending', $4, $5, 100, 'USD',
                $6, $7, $8
            )
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(gateway.billing_scope_id)
        .bind(subscriber_id)
        .bind("shape-host-target")
        .bind(format!("host_charge:{}:100:USD", Uuid::now_v7()))
        .bind(gateway.gateway_account_id)
        .bind(gateway.gateway_configuration_id)
        .bind("shape-host-target-order")
        .execute(&database.pool)
        .await;
        expect_database_constraint(
            missing_host_target,
            "billing_payment_attempts_plan_target_shape_check",
        )?;

        let missing_method_snapshot = sqlx::query(
            r#"
            INSERT INTO billing_payment_attempts (
                id, billing_scope_id, subscriber_id, plan_key,
                subscription_id, payment_method_id, attempt_kind, status,
                idempotency_key, request_fingerprint, amount_cents, currency,
                gateway_account_id, gateway_configuration_id, gateway_order_id
            ) VALUES (
                $1, $2, $3, 'base_subscription', $4, $5,
                'subscription_payment_method_update', 'pending',
                $6, $7, 0, 'USD', $8, $9, $10
            )
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(gateway.billing_scope_id)
        .bind(subscriber_id)
        .bind(subscription_id)
        .bind(payment_method_id)
        .bind("shape-method-snapshot")
        .bind(format!(
            "subscription_payment_method_update:base_subscription:{subscription_id}:{payment_method_id}:{initial_transaction_id}"
        ))
        .bind(gateway.gateway_account_id)
        .bind(gateway.gateway_configuration_id)
        .bind("shape-method-snapshot-order")
        .execute(&database.pool)
        .await;
        expect_database_constraint(
            missing_method_snapshot,
            "billing_payment_attempts_method_update_snapshot_check",
        )?;

        let missing_subscription_snapshot = sqlx::query(
            r#"
            INSERT INTO billing_payment_attempts (
                id, billing_scope_id, subscriber_id, plan_key,
                subscription_id, payment_method_id, attempt_kind, status,
                idempotency_key, request_fingerprint, amount_cents, currency,
                billing_period_start_at, billing_period_end_at,
                gateway_account_id, gateway_configuration_id, gateway_order_id,
                subscription_expected_payment_method_id,
                subscription_expected_initial_transaction_id
            ) VALUES (
                $1, $2, $3, 'base_subscription', $4, $5,
                'subscription_renewal', 'pending', $6, $7, 100, 'USD',
                '2026-02-01 00:00:00+00', '2026-03-01 00:00:00+00',
                $8, $9, $10, $5, $11
            )
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(gateway.billing_scope_id)
        .bind(subscriber_id)
        .bind(subscription_id)
        .bind(payment_method_id)
        .bind("shape-renewal-snapshot")
        .bind(format!(
            "subscription_renewal:base_subscription:{subscription_id}:{payment_method_id}:2026-02-01:100:USD"
        ))
        .bind(gateway.gateway_account_id)
        .bind(gateway.gateway_configuration_id)
        .bind("shape-renewal-snapshot-order")
        .bind(&initial_transaction_id)
        .execute(&database.pool)
        .await;
        expect_database_constraint(
            missing_subscription_snapshot,
            "billing_payment_attempts_subscription_snapshot_check",
        )?;

        let missing_recurring_period = sqlx::query(
            r#"
            INSERT INTO billing_payment_attempts (
                id, billing_scope_id, subscriber_id, plan_key,
                subscription_id, payment_method_id, attempt_kind, status,
                idempotency_key, request_fingerprint, amount_cents, currency,
                gateway_account_id, gateway_configuration_id, gateway_order_id,
                subscription_expected_payment_method_id,
                subscription_expected_initial_transaction_id,
                subscription_expected_status
            ) VALUES (
                $1, $2, $3, 'base_subscription', $4, $5,
                'subscription_recovery', 'pending', $6, $7, 100, 'USD',
                $8, $9, $10, $5, $11, 'past_due'
            )
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(gateway.billing_scope_id)
        .bind(subscriber_id)
        .bind(subscription_id)
        .bind(payment_method_id)
        .bind("shape-recovery-period")
        .bind(format!(
            "subscription_recovery:base_subscription:{subscription_id}:{payment_method_id}:missing:100:USD"
        ))
        .bind(gateway.gateway_account_id)
        .bind(gateway.gateway_configuration_id)
        .bind("shape-recovery-period-order")
        .bind(&initial_transaction_id)
        .execute(&database.pool)
        .await;
        expect_database_constraint(
            missing_recurring_period,
            "billing_payment_attempts_relationship_shape_check",
        )?;

        let partial_initial_discount = sqlx::query(
            r#"
            INSERT INTO billing_payment_attempts (
                id, billing_scope_id, subscriber_id, plan_key,
                attempt_kind, status, idempotency_key, request_fingerprint,
                amount_cents, currency, gateway_account_id,
                gateway_configuration_id, gateway_order_id,
                subscription_initial_discount_claim_id
            ) VALUES (
                $1, $2, $3, 'base_subscription', 'subscription_initial',
                'pending', $4, $5, 100, 'USD', $6, $7, $8, $9
            )
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(gateway.billing_scope_id)
        .bind(subscriber_id)
        .bind("shape-initial-discount")
        .bind("subscription_initial:base_subscription:100:USD:discount:partial")
        .bind(gateway.gateway_account_id)
        .bind(gateway.gateway_configuration_id)
        .bind("shape-initial-discount-order")
        .bind(Uuid::now_v7())
        .execute(&database.pool)
        .await;
        expect_database_constraint(
            partial_initial_discount,
            "billing_payment_attempts_initial_discount_snapshot_check",
        )?;

        let missing_discount_value = sqlx::query(
            r#"
            INSERT INTO billing_subscription_discount_codes (
                id, billing_scope_id, plan_key, code_normalized,
                display_code, status, discount_kind, currency, duration
            ) VALUES (
                $1, $2, 'base_subscription', 'NOVALUE', 'NOVALUE',
                'active', 'amount_off', 'USD', 'indefinite'
            )
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(gateway.billing_scope_id)
        .execute(&database.pool)
        .await;
        expect_database_constraint(
            missing_discount_value,
            "billing_subscription_discount_codes_value_check",
        )?;

        let missing_discount_duration = sqlx::query(
            r#"
            INSERT INTO billing_subscription_discount_codes (
                id, billing_scope_id, plan_key, code_normalized,
                display_code, status, discount_kind, amount_off_cents,
                currency, duration
            ) VALUES (
                $1, $2, 'base_subscription', 'NODURATION', 'NODURATION',
                'active', 'amount_off', 10, 'USD', 'limited_months'
            )
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(gateway.billing_scope_id)
        .execute(&database.pool)
        .await;
        expect_database_constraint(
            missing_discount_duration,
            "billing_subscription_discount_codes_duration_check",
        )?;

        let discount_code_id = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO billing_subscription_discount_codes (
                id, billing_scope_id, plan_key, code_normalized,
                display_code, status, discount_kind, amount_off_cents,
                currency, duration
            ) VALUES (
                $1, $2, 'base_subscription', 'VALIDCODE', 'VALIDCODE',
                'active', 'amount_off', 10, 'USD', 'indefinite'
            )
            "#,
        )
        .bind(discount_code_id)
        .bind(gateway.billing_scope_id)
        .execute(&database.pool)
        .await?;
        let incomplete_applied_claim = sqlx::query(
            r#"
            INSERT INTO billing_subscription_discount_claims (
                id, billing_scope_id, subscriber_id, plan_key,
                discount_code_id, code_snapshot, discount_kind,
                amount_off_cents, currency, duration, base_amount_cents,
                discounted_amount_cents, status
            ) VALUES (
                $1, $2, $3, 'base_subscription', $4, 'VALIDCODE',
                'amount_off', 10, 'USD', 'indefinite', 100, 90, 'applied'
            )
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(gateway.billing_scope_id)
        .bind(subscriber_id)
        .bind(discount_code_id)
        .execute(&database.pool)
        .await;
        expect_database_constraint(
            incomplete_applied_claim,
            "billing_subscription_discount_claims_status_fields_check",
        )?;

        let incomplete_limited_discount = sqlx::query(
            r#"
            INSERT INTO billing_subscription_discounts (
                subscription_id, billing_scope_id, subscriber_id, plan_key,
                code_snapshot, discount_kind, amount_off_cents, currency,
                duration, base_amount_cents, discounted_amount_cents,
                periods_applied, status
            ) VALUES (
                $1, $2, $3, 'base_subscription', 'VALIDCODE',
                'amount_off', 10, 'USD', 'limited_months', 100, 90,
                1, 'active'
            )
            "#,
        )
        .bind(subscription_id)
        .bind(gateway.billing_scope_id)
        .bind(subscriber_id)
        .execute(&database.pool)
        .await;
        expect_database_constraint(
            incomplete_limited_discount,
            "billing_subscription_discounts_duration_periods_check",
        )?;

        let incomplete_revocation = sqlx::query(
            r#"
            INSERT INTO billing_subscription_grants (
                id, billing_scope_id, subscriber_id, plan_key, grant_kind,
                reason, starts_at, ends_at, granted_by_actor_id, revoked_at
            ) VALUES (
                $1, $2, $3, 'base_subscription', 'testing', 'test grant',
                '2026-01-01 00:00:00+00', '2026-02-01 00:00:00+00',
                $4, '2026-01-15 00:00:00+00'
            )
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(gateway.billing_scope_id)
        .bind(subscriber_id)
        .bind(Uuid::now_v7())
        .execute(&database.pool)
        .await;
        expect_database_constraint(
            incomplete_revocation,
            "billing_subscription_grants_revocation_check",
        )?;

        let charge_attempt_id = insert_host_charge_attempt_record(
            &database.pool,
            gateway,
            subscriber_id,
            Uuid::now_v7(),
            "shape-charge-order",
            "shape-charge-idempotency",
        )
        .await?;
        let incomplete_charge_state = sqlx::query(
            r#"
            INSERT INTO billing_processor_charges (
                attempt_id, billing_scope_id, gateway_account_id,
                gateway_order_id, gateway_transaction_id,
                progression_state, attempt_kind, amount_cents, currency
            ) VALUES (
                $1, $2, $3, 'shape-charge-order', 'txn_shape_charge',
                'applied', 'host_charge', 100, 'USD'
            )
            "#,
        )
        .bind(charge_attempt_id)
        .bind(gateway.billing_scope_id)
        .bind(gateway.gateway_account_id)
        .execute(&database.pool)
        .await;
        expect_database_constraint(
            incomplete_charge_state,
            "billing_processor_charges_state_timestamps_check",
        )?;
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}
