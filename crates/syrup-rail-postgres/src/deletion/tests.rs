use std::{error::Error, io};

use sqlx::{PgPool, Postgres, Transaction};
use syrup_rail::{BillingScopeId, ScrubSubscriberBillingData, SubscriberId};
use uuid::Uuid;

use super::scrub_subscriber_billing_data;
use crate::test_support::{
    GatewayAccountFixture, TestDatabase, create_gateway_account, fixture_raw_sql,
};

mod blocker_matrix;
mod snapshots;
mod stale_attempts;

use snapshots::{nonnull_count, projection, row_snapshot, table_columns};

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
    // Schema v7. Like `gateway_response_code`, the address-verification
    // result is a closed provider code, not the address it was checked
    // against, so it is retained with the attempt's other decision codes.
    "gateway_avs_response",
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
        let attempt_before = projection(&database.pool, attempt, RETAINED_ATTEMPT_COLUMNS).await?;
        let retained: serde_json::Value = serde_json::from_str(&attempt_before)?;
        assert_eq!(retained["gateway_avs_response"], "Y");
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

        if projection(&database.pool, attempt, RETAINED_ATTEMPT_COLUMNS).await? != attempt_before {
            return Err(io::Error::other("billing scrub changed retained attempt truth").into());
        }
        if projection(&database.pool, method, RETAINED_METHOD_COLUMNS).await? != method_before {
            return Err(io::Error::other("billing scrub changed retained method truth").into());
        }
        if nonnull_count(&database.pool, attempt, SCRUBBED_ATTEMPT_COLUMNS).await? != 0 {
            return Err(io::Error::other("attempt billing projection was not cleared").into());
        }
        let method_rewritten: bool = checked_query_scalar!(
            r#"
            SELECT status = 'disabled'
                AND gateway_payment_method_reference = 'erased:' || id::text
            FROM billing_payment_methods
            WHERE id = $1
            "#,
            fixture.payment_method_id,
        )
        .fetch_one(&database.pool)
        .await?;
        if !method_rewritten
            || nonnull_count(&database.pool, method, SCRUBBED_METHOD_COLUMNS).await? != 0
        {
            return Err(io::Error::other("stored payment method was not scrubbed").into());
        }
        if processor_charge_snapshot(&database.pool, &fixture).await? != charge_before {
            return Err(io::Error::other("billing scrub changed processor charge evidence").into());
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
        fixture_raw_sql(
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
            return Err(io::Error::other("injected scrub failure unexpectedly committed").into());
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
        sqlx::query!("SET LOCAL lock_timeout = '100ms'")
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
            return Err(
                io::Error::other("scrub touched rows before acquiring its method domain").into(),
            );
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
        sqlx::query!("SET LOCAL lock_timeout = '100ms'")
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
    sqlx::query!(
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
        payment_method_id,
        account.billing_scope_id,
        subscriber_id,
        account.gateway_account_id,
        format!("vault_{}", payment_method_id.simple()),
    )
    .execute(pool)
    .await?;

    let payment_attempt_id = Uuid::now_v7();
    let host_charge_target_id = Uuid::now_v7();
    sqlx::query!(
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
            gateway_lifecycle_reconciled_at, gateway_avs_response
        ) VALUES (
            'live',
            $1, $2, $3, $4, 'host_charge', 'approved', $5, $6,
            1200, 'USD', $7, $8, $9, $10, $11,
            '1', '100', 'approved processor response', 'complete',
            'card', 'visa', '4242', 12, 2034,
            'Jordan', 'Lee', 'jordan@example.test',
            '1 Main St', 'Suite 2', 'Boston', 'MA', '02110', 'US',
            now(), now(), 'settled',
            'settled transaction', now(), now(), 'Y'
        )
        "#,
        payment_attempt_id,
        account.billing_scope_id,
        subscriber_id,
        host_charge_target_id,
        format!("scrub-{}", payment_attempt_id.simple()),
        format!("host_charge:{host_charge_target_id}:1200:USD"),
        account.gateway_account_id,
        account.gateway_configuration_id,
        format!("order-{}", payment_attempt_id.simple()),
        format!("transaction-{}", payment_attempt_id.simple()),
        format!("vault-{}", payment_attempt_id.simple()),
    )
    .execute(pool)
    .await?;

    let processor_charge_id = Uuid::now_v7();
    sqlx::query!(
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
        payment_attempt_id,
        processor_charge_id,
    )
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

async fn processor_charge_snapshot(
    pool: &PgPool,
    fixture: &ScrubFixture,
) -> Result<String, sqlx::Error> {
    checked_query_scalar!(
        "SELECT to_jsonb(charges)::text FROM billing_processor_charges AS charges WHERE id = $1",
        fixture.processor_charge_id,
    )
    .fetch_one(pool)
    .await
}

async fn lock_payment_method_domain(
    transaction: &mut Transaction<'_, Postgres>,
    fixture: &ScrubFixture,
) -> Result<(), sqlx::Error> {
    checked_query!(
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
        fixture.account.billing_scope_id,
        fixture.account.gateway_account_id,
        fixture.subscriber_id,
    )
    .execute(&mut **transaction)
    .await?;
    Ok(())
}
