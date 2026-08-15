use sqlx::PgConnection;
use syrup_rail::{
    BillingDeletionBlockers, DeletionBlockerQuery, ScrubSubscriberBillingData, ScrubbedBillingRows,
};

use crate::{
    attempts::{
        INITIAL_PREPARED_STALE_AFTER_SECONDS,
        PAYMENT_METHOD_UPDATE_UNSUBMITTED_STALE_AFTER_SECONDS,
        SUBSCRIPTION_CHARGE_UNSUBMITTED_STALE_AFTER_SECONDS,
    },
    host_charge_reconciliation::HOST_CHARGE_UNSUBMITTED_STALE_AFTER_SECONDS,
};

/// Reports the canonical financial rows that prevent host account deletion.
///
/// The query is subscriber-wide across plans and uses the caller's transaction.
/// Host-specific orders and fulfillment blockers remain the host's responsibility.
pub async fn billing_deletion_blockers(
    connection: &mut PgConnection,
    query: DeletionBlockerQuery,
) -> Result<BillingDeletionBlockers, sqlx::Error> {
    let billing_scope_id = query.billing_scope_id().into_uuid();
    let subscriber_id = query.subscriber_id().into_uuid();
    let row = sqlx::query!(
        r#"
        SELECT
            EXISTS (
                SELECT 1
                FROM billing_subscriptions
                WHERE billing_scope_id = $1
                    AND subscriber_id = $2
                    AND status IN ('active', 'past_due')
            ) AS "active_subscription!",
            EXISTS (
                SELECT 1
                FROM billing_payment_attempts
                WHERE billing_scope_id = $1
                    AND subscriber_id = $2
                    AND status IN ('pending', 'unknown', 'review_required')
                    AND NOT (
                        submitted_at IS NULL
                        AND status IN ('pending', 'review_required')
                        AND (
                            (
                                attempt_kind = 'subscription_payment_method_update'
                                AND created_at <= clock_timestamp()
                                    - ($3::bigint * interval '1 second')
                            )
                            OR (
                                attempt_kind = 'subscription_initial'
                                AND created_at <= clock_timestamp()
                                    - ($4::bigint * interval '1 second')
                            )
                            OR (
                                attempt_kind IN (
                                    'subscription_renewal',
                                    'subscription_recovery'
                                )
                                AND created_at <= clock_timestamp()
                                    - ($5::bigint * interval '1 second')
                            )
                            OR (
                                attempt_kind = 'host_charge'
                                AND created_at <= clock_timestamp()
                                    - ($6::bigint * interval '1 second')
                            )
                        )
                    )
            ) AS "unresolved_payment!"
        "#,
        billing_scope_id,
        subscriber_id,
        PAYMENT_METHOD_UPDATE_UNSUBMITTED_STALE_AFTER_SECONDS,
        INITIAL_PREPARED_STALE_AFTER_SECONDS,
        SUBSCRIPTION_CHARGE_UNSUBMITTED_STALE_AFTER_SECONDS,
        HOST_CHARGE_UNSUBMITTED_STALE_AFTER_SECONDS,
    )
    .fetch_one(connection)
    .await?;
    Ok(BillingDeletionBlockers::new(
        row.active_subscription,
        row.unresolved_payment,
    ))
}

/// Removes the canonical mutable billing PII for one subscriber.
///
/// The caller owns deletion admission, must stabilize creation of new billing
/// rows for the subscriber, and must commit or roll back the surrounding host
/// deletion transaction. Immutable processor-charge observations are never
/// selected or mutated by this operation.
pub async fn scrub_subscriber_billing_data(
    connection: &mut PgConnection,
    command: ScrubSubscriberBillingData,
) -> Result<ScrubbedBillingRows, sqlx::Error> {
    let billing_scope_id = command.billing_scope_id().into_uuid();
    let subscriber_id = command.subscriber_id().into_uuid();

    // Payment methods are shared across plan aggregates. Enter every affected
    // subscriber/account mutation domain in deterministic account order before
    // locking either the attempts or methods that carry its mutable projection.
    let gateway_account_ids = sqlx::query_scalar::<_, uuid::Uuid>(
        r#"
        SELECT gateway_account_id
        FROM (
            SELECT gateway_account_id
            FROM billing_payment_attempts
            WHERE billing_scope_id = $1 AND subscriber_id = $2

            UNION

            SELECT gateway_account_id
            FROM billing_payment_methods
            WHERE billing_scope_id = $1 AND subscriber_id = $2
        ) AS affected_accounts
        ORDER BY gateway_account_id
        "#,
    )
    .bind(billing_scope_id)
    .bind(subscriber_id)
    .fetch_all(&mut *connection)
    .await?;
    for gateway_account_id in gateway_account_ids {
        sqlx::query(
            r#"
            SELECT pg_advisory_xact_lock(
                hashtextextended(
                    'syrup-rail:payment-method:'
                    || $1::uuid::text || ':'
                    || $2::uuid::text || ':'
                    || $3::uuid::text,
                    0
                )
            )
            "#,
        )
        .bind(billing_scope_id)
        .bind(gateway_account_id)
        .bind(subscriber_id)
        .execute(&mut *connection)
        .await?;
    }

    let payment_attempts = sqlx::query(
        r#"
        UPDATE billing_payment_attempts
        SET billing_name = NULL,
            billing_email = NULL,
            gateway_response = NULL,
            gateway_response_text = NULL,
            gateway_payment_method_reference = NULL,
            payment_type = NULL,
            card_brand = NULL,
            card_last4 = NULL,
            card_exp_month = NULL,
            card_exp_year = NULL,
            updated_at = now()
        WHERE billing_scope_id = $1 AND subscriber_id = $2
        "#,
    )
    .bind(billing_scope_id)
    .bind(subscriber_id)
    .execute(&mut *connection)
    .await?
    .rows_affected();

    let payment_methods = sqlx::query(
        r#"
        UPDATE billing_payment_methods
        SET status = 'disabled',
            gateway_payment_method_reference = 'erased:' || id::text,
            billing_name = NULL,
            billing_email = NULL,
            payment_type = NULL,
            card_brand = NULL,
            card_last4 = NULL,
            card_exp_month = NULL,
            card_exp_year = NULL,
            updated_at = now()
        WHERE billing_scope_id = $1 AND subscriber_id = $2
        "#,
    )
    .bind(billing_scope_id)
    .bind(subscriber_id)
    .execute(&mut *connection)
    .await?
    .rows_affected();

    Ok(ScrubbedBillingRows::new(payment_attempts, payment_methods))
}

#[cfg(test)]
mod tests {
    use std::{error::Error, io};

    use sqlx::{PgPool, Postgres, Transaction};
    use syrup_rail::{BillingScopeId, ScrubSubscriberBillingData, SubscriberId};
    use uuid::Uuid;

    use super::scrub_subscriber_billing_data;
    use crate::test_support::{GatewayAccountFixture, TestDatabase, create_gateway_account};

    mod stale_attempts;

    struct ScrubFixture {
        account: GatewayAccountFixture,
        subscriber_id: Uuid,
        payment_method_id: Uuid,
        payment_attempt_id: Uuid,
        processor_charge_id: Uuid,
    }

    #[tokio::test]
    async fn billing_scrub_is_exact_scoped_and_preserves_processor_charges()
    -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("sr_scrub_exact").await?;
        let result = async {
            let fixture = insert_scrub_fixture(&database.pool).await?;
            let attempt_before = retained_attempt_projection(&database.pool, &fixture).await?;
            let method_before = retained_method_projection(&database.pool, &fixture).await?;
            let charge_before = processor_charge_snapshot(&database.pool, &fixture).await?;

            let mut transaction = database.pool.begin().await?;
            let wrong_scope = scrub_subscriber_billing_data(
                &mut transaction,
                ScrubSubscriberBillingData::new(
                    BillingScopeId::new(Uuid::now_v7()),
                    SubscriberId::new(fixture.subscriber_id),
                ),
            )
            .await?;
            transaction.commit().await?;
            if !wrong_scope.is_empty() {
                return Err(io::Error::other("billing scrub crossed scopes").into());
            }
            if retained_attempt_projection(&database.pool, &fixture).await? != attempt_before
                || retained_method_projection(&database.pool, &fixture).await? != method_before
            {
                return Err(io::Error::other("wrong-scope scrub changed billing rows").into());
            }

            let mut transaction = database.pool.begin().await?;
            let scrubbed =
                scrub_subscriber_billing_data(&mut transaction, scrub_command(&fixture)).await?;
            transaction.commit().await?;
            if scrubbed.payment_attempts() != 1 || scrubbed.payment_methods() != 1 {
                return Err(io::Error::other("billing scrub returned incorrect row counts").into());
            }

            if retained_attempt_projection(&database.pool, &fixture).await? != attempt_before {
                return Err(
                    io::Error::other("billing scrub changed retained attempt truth").into(),
                );
            }
            if retained_method_projection(&database.pool, &fixture).await? != method_before {
                return Err(io::Error::other("billing scrub changed retained method truth").into());
            }
            let attempt_cleared: bool = sqlx::query_scalar(
                r#"
                SELECT billing_name IS NULL
                    AND billing_email IS NULL
                    AND gateway_response IS NULL
                    AND gateway_response_text IS NULL
                    AND gateway_payment_method_reference IS NULL
                    AND payment_type IS NULL
                    AND card_brand IS NULL
                    AND card_last4 IS NULL
                    AND card_exp_month IS NULL
                    AND card_exp_year IS NULL
                FROM billing_payment_attempts
                WHERE id = $1
                "#,
            )
            .bind(fixture.payment_attempt_id)
            .fetch_one(&database.pool)
            .await?;
            if !attempt_cleared {
                return Err(io::Error::other("attempt billing projection was not cleared").into());
            }
            let method_scrubbed: bool = sqlx::query_scalar(
                r#"
                SELECT status = 'disabled'
                    AND gateway_payment_method_reference = 'erased:' || id::text
                    AND billing_name IS NULL
                    AND billing_email IS NULL
                    AND payment_type IS NULL
                    AND card_brand IS NULL
                    AND card_last4 IS NULL
                    AND card_exp_month IS NULL
                    AND card_exp_year IS NULL
                FROM billing_payment_methods
                WHERE id = $1
                "#,
            )
            .bind(fixture.payment_method_id)
            .fetch_one(&database.pool)
            .await?;
            if !method_scrubbed {
                return Err(io::Error::other("stored payment method was not scrubbed").into());
            }
            if processor_charge_snapshot(&database.pool, &fixture).await? != charge_before {
                return Err(
                    io::Error::other("billing scrub changed processor charge evidence").into(),
                );
            }
            Ok::<_, Box<dyn Error>>(())
        }
        .await;
        let cleanup = database.cleanup().await;
        result?;
        cleanup
    }

    #[tokio::test]
    async fn billing_scrub_rolls_back_attempts_when_method_scrubbing_fails()
    -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("sr_scrub_rb").await?;
        let result = async {
            let fixture = insert_scrub_fixture(&database.pool).await?;
            let attempt_before = row_snapshot(
                &database.pool,
                "billing_payment_attempts",
                fixture.payment_attempt_id,
            )
            .await?;
            let method_before = row_snapshot(
                &database.pool,
                "billing_payment_methods",
                fixture.payment_method_id,
            )
            .await?;
            let charge_before = processor_charge_snapshot(&database.pool, &fixture).await?;
            sqlx::raw_sql(
                r#"
                CREATE FUNCTION test_fail_billing_method_scrub()
                RETURNS trigger
                LANGUAGE plpgsql
                AS $function$
                BEGIN
                    RAISE EXCEPTION 'injected billing method scrub failure';
                END;
                $function$;

                CREATE TRIGGER test_fail_billing_method_scrub
                BEFORE UPDATE ON billing_payment_methods
                FOR EACH ROW
                EXECUTE FUNCTION test_fail_billing_method_scrub();
                "#,
            )
            .execute(&database.pool)
            .await?;

            let mut transaction = database.pool.begin().await?;
            if scrub_subscriber_billing_data(&mut transaction, scrub_command(&fixture))
                .await
                .is_ok()
            {
                return Err(
                    io::Error::other("injected scrub failure unexpectedly committed").into(),
                );
            }
            transaction.rollback().await?;

            if row_snapshot(
                &database.pool,
                "billing_payment_attempts",
                fixture.payment_attempt_id,
            )
            .await?
                != attempt_before
                || row_snapshot(
                    &database.pool,
                    "billing_payment_methods",
                    fixture.payment_method_id,
                )
                .await?
                    != method_before
                || processor_charge_snapshot(&database.pool, &fixture).await? != charge_before
            {
                return Err(
                    io::Error::other("failed billing scrub did not roll back atomically").into(),
                );
            }
            Ok::<_, Box<dyn Error>>(())
        }
        .await;
        let cleanup = database.cleanup().await;
        result?;
        cleanup
    }

    #[tokio::test]
    async fn billing_scrub_enters_the_subscriber_account_method_domain_before_rows()
    -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("sr_scrub_lock").await?;
        let result = async {
            let fixture = insert_scrub_fixture(&database.pool).await?;
            let attempt_before = row_snapshot(
                &database.pool,
                "billing_payment_attempts",
                fixture.payment_attempt_id,
            )
            .await?;
            let mut held = database.pool.begin().await?;
            lock_payment_method_domain(&mut held, &fixture).await?;

            let mut blocked = database.pool.begin().await?;
            sqlx::query("SET LOCAL lock_timeout = '100ms'")
                .execute(&mut *blocked)
                .await?;
            let error = scrub_subscriber_billing_data(&mut blocked, scrub_command(&fixture))
                .await
                .expect_err("held payment-method domain must block subscriber scrub");
            let code = error
                .as_database_error()
                .and_then(|error| error.code())
                .map(|code| code.into_owned());
            if code.as_deref() != Some("55P03") {
                return Err(io::Error::other(format!("unexpected lock failure: {error}")).into());
            }
            blocked.rollback().await?;
            if row_snapshot(
                &database.pool,
                "billing_payment_attempts",
                fixture.payment_attempt_id,
            )
            .await?
                != attempt_before
            {
                return Err(io::Error::other(
                    "scrub touched rows before acquiring its method domain",
                )
                .into());
            }
            held.rollback().await?;
            Ok::<_, Box<dyn Error>>(())
        }
        .await;
        let cleanup = database.cleanup().await;
        result?;
        cleanup
    }

    fn scrub_command(fixture: &ScrubFixture) -> ScrubSubscriberBillingData {
        ScrubSubscriberBillingData::new(
            BillingScopeId::new(fixture.account.billing_scope_id),
            SubscriberId::new(fixture.subscriber_id),
        )
    }

    async fn insert_scrub_fixture(pool: &PgPool) -> Result<ScrubFixture, sqlx::Error> {
        let account = create_gateway_account(pool, "test_gateway").await?;
        let subscriber_id = Uuid::now_v7();
        let payment_method_id = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO billing_payment_methods (
                id, billing_scope_id, subscriber_id, gateway_account_id,
                gateway_payment_method_reference, status, payment_type,
                card_brand, card_last4, card_exp_month, card_exp_year,
                billing_name, billing_email
            ) VALUES (
                $1, $2, $3, $4, $5, 'active', 'card', 'visa', '4242',
                12, 2034, 'Jordan Lee', 'jordan@example.test'
            )
            "#,
        )
        .bind(payment_method_id)
        .bind(account.billing_scope_id)
        .bind(subscriber_id)
        .bind(account.gateway_account_id)
        .bind(format!("vault_{}", payment_method_id.simple()))
        .execute(pool)
        .await?;

        let payment_attempt_id = Uuid::now_v7();
        let host_charge_target_id = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO billing_payment_attempts (
                id, billing_scope_id, subscriber_id, host_charge_target_id,
                attempt_kind, status, idempotency_key, request_fingerprint,
                amount_cents, currency, gateway_account_id,
                gateway_configuration_id, gateway_order_id,
                gateway_transaction_id, gateway_payment_method_reference,
                gateway_response, gateway_response_code, gateway_response_text,
                gateway_condition, payment_type, card_brand, card_last4,
                card_exp_month, card_exp_year, billing_name, billing_email,
                submitted_at, resolved_at, gateway_lifecycle_status,
                gateway_lifecycle_action, gateway_lifecycle_at,
                gateway_lifecycle_reconciled_at
            ) VALUES (
                $1, $2, $3, $4, 'host_charge', 'approved', $5, $6,
                1200, 'USD', $7, $8, $9, $10, $11,
                '1', '100', 'approved processor response', 'complete',
                'card', 'visa', '4242', 12, 2034,
                'Jordan Lee', 'jordan@example.test', now(), now(), 'settled',
                'settled transaction', now(), now()
            )
            "#,
        )
        .bind(payment_attempt_id)
        .bind(account.billing_scope_id)
        .bind(subscriber_id)
        .bind(host_charge_target_id)
        .bind(format!("scrub-{}", payment_attempt_id.simple()))
        .bind(format!("host_charge:{host_charge_target_id}:1200:USD"))
        .bind(account.gateway_account_id)
        .bind(account.gateway_configuration_id)
        .bind(format!("order-{}", payment_attempt_id.simple()))
        .bind(format!("transaction-{}", payment_attempt_id.simple()))
        .bind(format!("vault-{}", payment_attempt_id.simple()))
        .execute(pool)
        .await?;

        let processor_charge_id = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO billing_processor_charges (
                id, attempt_id, billing_scope_id, gateway_account_id,
                gateway_order_id, gateway_transaction_id,
                gateway_payment_method_reference, gateway_response,
                gateway_response_code, gateway_response_text,
                gateway_condition, payment_type, card_brand, card_last4,
                card_exp_month, card_exp_year, charge_role, progression_state,
                state_code, observed_at, applied_at, attempt_kind, plan_key,
                host_charge_target_id, amount_cents, currency
            )
            SELECT
                $2, id, billing_scope_id, gateway_account_id,
                gateway_order_id, gateway_transaction_id,
                gateway_payment_method_reference, gateway_response,
                gateway_response_code, gateway_response_text,
                gateway_condition, payment_type, card_brand, card_last4,
                card_exp_month, card_exp_year, 'primary', 'applied',
                'billing_scrub_fixture', now(), now(), attempt_kind, plan_key,
                host_charge_target_id, amount_cents, currency
            FROM billing_payment_attempts
            WHERE id = $1
            "#,
        )
        .bind(payment_attempt_id)
        .bind(processor_charge_id)
        .execute(pool)
        .await?;

        Ok(ScrubFixture {
            account,
            subscriber_id,
            payment_method_id,
            payment_attempt_id,
            processor_charge_id,
        })
    }

    async fn retained_attempt_projection(
        pool: &PgPool,
        fixture: &ScrubFixture,
    ) -> Result<String, sqlx::Error> {
        sqlx::query_scalar(
            r#"
            SELECT (
                to_jsonb(attempts) - ARRAY[
                    'billing_name', 'billing_email', 'gateway_response',
                    'gateway_response_text', 'gateway_payment_method_reference',
                    'payment_type', 'card_brand', 'card_last4',
                    'card_exp_month', 'card_exp_year', 'updated_at'
                ]::text[]
            )::text
            FROM billing_payment_attempts AS attempts
            WHERE id = $1
            "#,
        )
        .bind(fixture.payment_attempt_id)
        .fetch_one(pool)
        .await
    }

    async fn retained_method_projection(
        pool: &PgPool,
        fixture: &ScrubFixture,
    ) -> Result<String, sqlx::Error> {
        sqlx::query_scalar(
            r#"
            SELECT (
                to_jsonb(methods) - ARRAY[
                    'status', 'gateway_payment_method_reference',
                    'billing_name', 'billing_email', 'payment_type',
                    'card_brand', 'card_last4', 'card_exp_month',
                    'card_exp_year', 'updated_at'
                ]::text[]
            )::text
            FROM billing_payment_methods AS methods
            WHERE id = $1
            "#,
        )
        .bind(fixture.payment_method_id)
        .fetch_one(pool)
        .await
    }

    async fn processor_charge_snapshot(
        pool: &PgPool,
        fixture: &ScrubFixture,
    ) -> Result<String, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT to_jsonb(charges)::text FROM billing_processor_charges AS charges WHERE id = $1",
        )
        .bind(fixture.processor_charge_id)
        .fetch_one(pool)
        .await
    }

    async fn row_snapshot(pool: &PgPool, table: &str, id: Uuid) -> Result<String, sqlx::Error> {
        let query = match table {
            "billing_payment_attempts" => {
                "SELECT to_jsonb(rows)::text FROM billing_payment_attempts AS rows WHERE id = $1"
            }
            "billing_payment_methods" => {
                "SELECT to_jsonb(rows)::text FROM billing_payment_methods AS rows WHERE id = $1"
            }
            _ => unreachable!("test snapshot table is fixed"),
        };
        sqlx::query_scalar(query).bind(id).fetch_one(pool).await
    }

    async fn lock_payment_method_domain(
        transaction: &mut Transaction<'_, Postgres>,
        fixture: &ScrubFixture,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            SELECT pg_advisory_xact_lock(
                hashtextextended(
                    'syrup-rail:payment-method:'
                    || $1::uuid::text || ':'
                    || $2::uuid::text || ':'
                    || $3::uuid::text,
                    0
                )
            )
            "#,
        )
        .bind(fixture.account.billing_scope_id)
        .bind(fixture.account.gateway_account_id)
        .bind(fixture.subscriber_id)
        .execute(&mut **transaction)
        .await?;
        Ok(())
    }
}
