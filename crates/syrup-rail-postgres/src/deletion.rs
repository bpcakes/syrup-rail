use sqlx::PgConnection;
use syrup_rail::{
    BillingDeletionBlockers, BillingScopeId, DeletionBlockerQuery, PaymentAttemptKind,
    ScrubSubscriberBillingData, ScrubbedBillingRows, SubscriberId,
};

use crate::attempts::LocalAttemptPolicy;

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
    let payment_method_update_policy =
        LocalAttemptPolicy::for_kind(PaymentAttemptKind::SubscriptionPaymentMethodUpdate);
    let initial_policy = LocalAttemptPolicy::for_kind(PaymentAttemptKind::SubscriptionInitial);
    let subscription_charge_policy =
        LocalAttemptPolicy::for_kind(PaymentAttemptKind::SubscriptionRenewal);
    let host_charge_policy = LocalAttemptPolicy::for_kind(PaymentAttemptKind::HostCharge);
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
                        AND status = ANY($7::text[])
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
        payment_method_update_policy.stale_after_seconds(),
        initial_policy.stale_after_seconds(),
        subscription_charge_policy.stale_after_seconds(),
        host_charge_policy.stale_after_seconds(),
        LocalAttemptPolicy::expirable_status_values(),
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
/// deletion transaction. Following the [`crate::BillingTransactionCoordinator`]
/// contract, the host takes its own subject lock before calling this function
/// and holds no Syrup Rail aggregate or billing row locks. The scrub then
/// enters each affected gateway account's scrub domain and approval domain, in
/// ascending account order, before updating any row, so a concurrent approval
/// or renewal reservation cannot restore contact or address data mid-scrub.
/// Names, email, billing addresses, provider references, response text, and
/// card display are cleared; immutable processor-charge observations are never
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
    for gateway_account_id in &gateway_account_ids {
        lock_payment_method_scrub_domain(
            connection,
            command.billing_scope_id(),
            command.subscriber_id(),
            gateway_account_id,
        )
        .await?;
    }
    // Approval writers and renewal reservation use the approval domain. Enter
    // it after the scrub domains and before any row lock, matching the global
    // order used by saved-card metadata repair.
    crate::enrollment_application::lock_payment_method_domains(
        connection,
        command.subscriber_id(),
        gateway_account_ids,
    )
    .await?;

    let payment_attempts = sqlx::query(
        r#"
        UPDATE billing_payment_attempts
        SET billing_first_name = NULL,
            billing_last_name = NULL,
            billing_email = NULL,
            billing_address_line1 = NULL,
            billing_address_line2 = NULL,
            billing_address_city = NULL,
            billing_address_region = NULL,
            billing_address_postal_code = NULL,
            billing_address_country = NULL,
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
            billing_address_line1 = NULL,
            billing_address_line2 = NULL,
            billing_address_city = NULL,
            billing_address_region = NULL,
            billing_address_postal_code = NULL,
            billing_address_country = NULL,
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

/// Enters the deployed scrub domain. Its key is intentionally preserved for
/// compatibility with v0.5.2 writers; callers coordinating both workflows also
/// enter the approval domain before acquiring row locks.
pub(crate) async fn lock_payment_method_scrub_domain(
    connection: &mut PgConnection,
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    gateway_account_id: &uuid::Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('syrup-rail:payment-method:' || $1::uuid::text || ':' || $2::uuid::text || ':' || $3::uuid::text, 0))")
        .bind(billing_scope_id.as_uuid())
        .bind(gateway_account_id)
        .bind(subscriber_id.as_uuid())
        .execute(connection).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{error::Error, io};

    use sqlx::{PgPool, Postgres, Transaction};
    use syrup_rail::{BillingScopeId, ScrubSubscriberBillingData, SubscriberId};
    use uuid::Uuid;

    use super::scrub_subscriber_billing_data;
    use crate::test_support::{GatewayAccountFixture, TestDatabase, create_gateway_account};

    mod blocker_matrix;
    mod stale_attempts;

    struct ScrubFixture {
        account: GatewayAccountFixture,
        subscriber_id: Uuid,
        payment_method_id: Uuid,
        payment_attempt_id: Uuid,
        processor_charge_id: Uuid,
    }

    /// Attempt columns the scrub clears. Every other attempt column except
    /// `updated_at` is retained unchanged.
    const SCRUBBED_ATTEMPT_COLUMNS: &[&str] = &[
        "billing_first_name",
        "billing_last_name",
        "billing_email",
        "billing_address_line1",
        "billing_address_line2",
        "billing_address_city",
        "billing_address_region",
        "billing_address_postal_code",
        "billing_address_country",
        "gateway_response",
        "gateway_response_text",
        "gateway_payment_method_reference",
        "payment_type",
        "card_brand",
        "card_last4",
        "card_exp_month",
        "card_exp_year",
    ];

    /// Every attempt column the scrub must leave unchanged. Adding a column to
    /// the table without classifying it here or above fails the scrub test.
    const RETAINED_ATTEMPT_COLUMNS: &[&str] = &[
        "id",
        "billing_scope_id",
        "subscriber_id",
        "plan_key",
        "host_charge_target_id",
        "subscription_id",
        "payment_method_id",
        "attempt_kind",
        "status",
        "idempotency_key",
        "request_fingerprint",
        "amount_cents",
        "currency",
        "billing_period_start_at",
        "billing_period_end_at",
        "gateway_account_id",
        "gateway_configuration_id",
        "gateway_order_id",
        "gateway_transaction_id",
        "gateway_response_code",
        "gateway_condition",
        "submitted_at",
        "resolved_at",
        "created_at",
        "gateway_lifecycle_status",
        "gateway_lifecycle_action",
        "gateway_lifecycle_at",
        "gateway_lifecycle_reconciled_at",
        "refunded_amount_cents",
        "resolution_code",
        "review_required_at",
        "payment_method_update_expected_payment_method_id",
        "payment_method_update_expected_initial_transaction_id",
        "subscription_expected_payment_method_id",
        "subscription_expected_initial_transaction_id",
        "subscription_expected_status",
        "subscription_initial_discount_claim_id",
        "subscription_initial_discount_code_id",
        "subscription_initial_discount_code_snapshot",
        "subscription_initial_discount_label_snapshot",
        "subscription_initial_discount_kind",
        "subscription_initial_discount_amount_off_cents",
        "subscription_initial_discount_percent_off_bps",
        "subscription_initial_discount_currency",
        "subscription_initial_discount_duration",
        "subscription_initial_discount_duration_months",
        "subscription_initial_discount_base_amount_cents",
        "subscription_initial_discount_discounted_amount_cents",
        "subscription_initial_terms_version",
        "subscription_initial_start_kind",
        "subscription_initial_trial_amount_cents",
        "subscription_initial_trial_period_kind",
        "subscription_initial_trial_period_count",
        "subscription_initial_recurring_base_amount_cents",
        "subscription_initial_recurring_period_kind",
        "subscription_initial_recurring_period_count",
        "subscription_initial_dunning_retry_delays_seconds",
        "subscription_initial_dunning_exhaustion",
        "subscription_initial_past_due_access",
        "required_gateway_account_mode",
    ];

    /// Method columns the scrub clears.
    const SCRUBBED_METHOD_COLUMNS: &[&str] = &[
        "billing_name",
        "billing_email",
        "billing_address_line1",
        "billing_address_line2",
        "billing_address_city",
        "billing_address_region",
        "billing_address_postal_code",
        "billing_address_country",
        "payment_type",
        "card_brand",
        "card_last4",
        "card_exp_month",
        "card_exp_year",
    ];

    /// Method columns the scrub rewrites to fixed non-identifying values.
    const REWRITTEN_METHOD_COLUMNS: &[&str] = &["status", "gateway_payment_method_reference"];

    /// Every method column the scrub must leave unchanged.
    const RETAINED_METHOD_COLUMNS: &[&str] = &[
        "id",
        "billing_scope_id",
        "subscriber_id",
        "gateway_account_id",
        "created_at",
    ];

    #[tokio::test]
    async fn billing_scrub_is_exact_scoped_and_preserves_processor_charges()
    -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("sr_scrub_exact").await?;
        let result = async {
            let fixture = insert_scrub_fixture(&database.pool).await?;
            for (table, groups) in [
                (
                    "billing_payment_attempts",
                    vec![
                        RETAINED_ATTEMPT_COLUMNS,
                        SCRUBBED_ATTEMPT_COLUMNS,
                        &["updated_at"],
                    ],
                ),
                (
                    "billing_payment_methods",
                    vec![
                        RETAINED_METHOD_COLUMNS,
                        SCRUBBED_METHOD_COLUMNS,
                        REWRITTEN_METHOD_COLUMNS,
                        &["updated_at"],
                    ],
                ),
            ] {
                let mut classified = groups.concat();
                classified.sort_unstable();
                let mut columns = table_columns(&database.pool, table).await?;
                columns.sort_unstable();
                if columns != classified {
                    return Err(io::Error::other(format!(
                        "{table} scrub classification is incomplete: {columns:?} != {classified:?}"
                    ))
                    .into());
                }
            }
            let attempt = ("billing_payment_attempts", fixture.payment_attempt_id);
            let method = ("billing_payment_methods", fixture.payment_method_id);
            if nonnull_count(&database.pool, attempt, SCRUBBED_ATTEMPT_COLUMNS).await?
                != SCRUBBED_ATTEMPT_COLUMNS.len() as i64
                || nonnull_count(&database.pool, method, SCRUBBED_METHOD_COLUMNS).await?
                    != SCRUBBED_METHOD_COLUMNS.len() as i64
            {
                return Err(io::Error::other("fixture must populate every scrubbed column").into());
            }
            let attempt_before =
                projection(&database.pool, attempt, RETAINED_ATTEMPT_COLUMNS).await?;
            let method_before = projection(&database.pool, method, RETAINED_METHOD_COLUMNS).await?;
            let attempt_row_before = row_snapshot(&database.pool, attempt.0, attempt.1).await?;
            let method_row_before = row_snapshot(&database.pool, method.0, method.1).await?;
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
            if row_snapshot(&database.pool, attempt.0, attempt.1).await? != attempt_row_before
                || row_snapshot(&database.pool, method.0, method.1).await? != method_row_before
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

            if projection(&database.pool, attempt, RETAINED_ATTEMPT_COLUMNS).await?
                != attempt_before
            {
                return Err(
                    io::Error::other("billing scrub changed retained attempt truth").into(),
                );
            }
            if projection(&database.pool, method, RETAINED_METHOD_COLUMNS).await? != method_before {
                return Err(io::Error::other("billing scrub changed retained method truth").into());
            }
            if nonnull_count(&database.pool, attempt, SCRUBBED_ATTEMPT_COLUMNS).await? != 0 {
                return Err(io::Error::other("attempt billing projection was not cleared").into());
            }
            let method_rewritten: bool = sqlx::query_scalar(
                r#"
                SELECT status = 'disabled'
                    AND gateway_payment_method_reference = 'erased:' || id::text
                FROM billing_payment_methods
                WHERE id = $1
                "#,
            )
            .bind(fixture.payment_method_id)
            .fetch_one(&database.pool)
            .await?;
            if !method_rewritten
                || nonnull_count(&database.pool, method, SCRUBBED_METHOD_COLUMNS).await? != 0
            {
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

    #[tokio::test]
    async fn billing_scrub_enters_the_approval_domain_before_rows() -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("sr_scrub_approve").await?;
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
            // Approval writers and renewal reservation hold this domain.
            let mut held = database.pool.begin().await?;
            crate::enrollment_application::lock_payment_method_domain(
                &mut held,
                SubscriberId::new(fixture.subscriber_id),
                &fixture.account.gateway_account_id,
            )
            .await?;

            let mut blocked = database.pool.begin().await?;
            sqlx::query("SET LOCAL lock_timeout = '100ms'")
                .execute(&mut *blocked)
                .await?;
            let error = scrub_subscriber_billing_data(&mut blocked, scrub_command(&fixture))
                .await
                .expect_err("a held approval domain must block subscriber scrub");
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
                || row_snapshot(
                    &database.pool,
                    "billing_payment_methods",
                    fixture.payment_method_id,
                )
                .await?
                    != method_before
            {
                return Err(io::Error::other(
                    "scrub touched rows before acquiring the approval domain",
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
                billing_name, billing_email, billing_address_line1,
                billing_address_line2, billing_address_city, billing_address_region,
                billing_address_postal_code, billing_address_country
            ) VALUES (
                $1, $2, $3, $4, $5, 'active', 'card', 'visa', '4242',
                12, 2034, 'Jordan Lee', 'jordan@example.test', '1 Main St',
                'Suite 2', 'Boston', 'MA', '02110', 'US'
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
                required_gateway_account_mode,
                id, billing_scope_id, subscriber_id, host_charge_target_id,
                attempt_kind, status, idempotency_key, request_fingerprint,
                amount_cents, currency, gateway_account_id,
                gateway_configuration_id, gateway_order_id,
                gateway_transaction_id, gateway_payment_method_reference,
                gateway_response, gateway_response_code, gateway_response_text,
                gateway_condition, payment_type, card_brand, card_last4,
                card_exp_month, card_exp_year,
                billing_first_name, billing_last_name, billing_email,
                billing_address_line1, billing_address_line2, billing_address_city,
                billing_address_region, billing_address_postal_code,
                billing_address_country,
                submitted_at, resolved_at, gateway_lifecycle_status,
                gateway_lifecycle_action, gateway_lifecycle_at,
                gateway_lifecycle_reconciled_at
            ) VALUES (
                'live',
                $1, $2, $3, $4, 'host_charge', 'approved', $5, $6,
                1200, 'USD', $7, $8, $9, $10, $11,
                '1', '100', 'approved processor response', 'complete',
                'card', 'visa', '4242', 12, 2034,
                'Jordan', 'Lee', 'jordan@example.test',
                '1 Main St', 'Suite 2', 'Boston', 'MA', '02110', 'US',
                now(), now(), 'settled',
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

    async fn table_columns(pool: &PgPool, table: &str) -> Result<Vec<String>, sqlx::Error> {
        sqlx::query_scalar(
            r#"
            SELECT column_name::text
            FROM information_schema.columns
            WHERE table_schema = 'public' AND table_name = $1
            "#,
        )
        .bind(table)
        .fetch_all(pool)
        .await
    }

    /// Serializes only the listed columns of one fixed billing row.
    async fn projection(
        pool: &PgPool,
        (table, id): (&str, Uuid),
        columns: &[&str],
    ) -> Result<String, sqlx::Error> {
        let query = format!(
            r#"
            SELECT COALESCE(jsonb_object_agg(fields.key, fields.value), '{{}}'::jsonb)::text
            FROM public.{} AS rows, jsonb_each(to_jsonb(rows)) AS fields
            WHERE rows.id = $1 AND fields.key = ANY($2)
            "#,
            fixed_table(table)
        );
        sqlx::query_scalar(&query)
            .bind(id)
            .bind(columns)
            .fetch_one(pool)
            .await
    }

    /// Counts the listed columns of one fixed billing row that are not NULL.
    async fn nonnull_count(
        pool: &PgPool,
        (table, id): (&str, Uuid),
        columns: &[&str],
    ) -> Result<i64, sqlx::Error> {
        let query = format!(
            r#"
            SELECT count(*)
            FROM public.{} AS rows, jsonb_each(to_jsonb(rows)) AS fields
            WHERE rows.id = $1 AND fields.key = ANY($2) AND fields.value <> 'null'::jsonb
            "#,
            fixed_table(table)
        );
        sqlx::query_scalar(&query)
            .bind(id)
            .bind(columns)
            .fetch_one(pool)
            .await
    }

    fn fixed_table(table: &str) -> &'static str {
        match table {
            "billing_payment_attempts" => "billing_payment_attempts",
            "billing_payment_methods" => "billing_payment_methods",
            _ => unreachable!("test projection table is fixed"),
        }
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
