//! Version-1 schema installation and host-conformance support.

use std::collections::BTreeSet;

use sqlx::PgPool;
use thiserror::Error;

/// The immutable version-1 fresh-install artifact.
pub const V1_INSTALL_SQL: &str = include_str!("../schema/v1/install.sql");

// Non-cryptographic drift fingerprint over the canonical PostgreSQL catalog.
// Host objects use host-prefixed names and are deliberately excluded.
const V1_CATALOG_FINGERPRINT: u64 = 0xc949_7313_2b48_83d9;

const REQUIRED_TABLES: &[&str] = &[
    "billing_external_reversal_attestations",
    "billing_gateway_accounts",
    "billing_gateway_lifecycle_pending_updates",
    "billing_gateway_lifecycle_quarantine_resolutions",
    "billing_gateway_lifecycle_quarantines",
    "billing_gateway_provider_rate_limits",
    "billing_payment_attempts",
    "billing_payment_methods",
    "billing_processor_charges",
    "billing_reconciliation_cursors",
    "billing_subscription_discount_claims",
    "billing_subscription_discount_codes",
    "billing_subscription_discounts",
    "billing_subscription_grants",
    "billing_subscriptions",
];

const REQUIRED_VIEWS: &[&str] = &[
    "billing_active_discount_facts",
    "billing_current_subscriptions",
    "billing_payment_facts",
];

const REQUIRED_FUNCTIONS: &[&str] = &[
    "billing_canonical_gateway_transaction_id",
    "billing_guard_processor_charge_evidence_update",
    "billing_host_charge_ledger_admission",
    "billing_set_attempt_review_required_at",
    "billing_set_processor_charge_attempt_dimensions",
];

const REQUIRED_TRIGGERS: &[&str] = &[
    "billing_payment_attempt_review_required_at",
    "billing_processor_charge_attempt_dimensions",
    "billing_processor_charge_evidence_immutable",
];

const PAYMENT_FACT_COLUMNS: &[&str] = &[
    "attempt_id",
    "billing_scope_id",
    "subscriber_id",
    "plan_key",
    "host_charge_target_id",
    "attempt_kind",
    "status",
    "amount_cents",
    "currency",
    "created_at",
    "resolved_at",
    "gateway_lifecycle_status",
    "refunded_amount_cents",
];

/// Why a host database does not satisfy the immutable version-1 contract.
#[derive(Debug, Error)]
pub enum SchemaConformanceError {
    #[error("schema conformance query failed: {0}")]
    Database(#[from] sqlx::Error),
    #[error("schema version 1 does not conform: {0}")]
    Contract(String),
}

/// Asserts that an already-migrated host database contains the canonical v1
/// objects without re-running or exposing a production migrator.
///
/// Separately named host objects are permitted. Canonical relations, views,
/// functions, triggers, validated constraints, and bounded host read surfaces
/// must remain present and retain their neutral vocabulary.
pub async fn assert_v1_conforms(pool: &PgPool) -> Result<(), SchemaConformanceError> {
    require_relations(pool, 'r', REQUIRED_TABLES).await?;
    require_relations(pool, 'v', REQUIRED_VIEWS).await?;
    require_functions(pool).await?;
    require_triggers(pool).await?;
    require_payment_fact_columns(pool).await?;
    reject_legacy_columns(pool).await?;
    require_validated_constraints(pool).await?;
    require_account_scoped_order_index(pool).await?;
    require_catalog_fingerprint(pool).await?;
    Ok(())
}

async fn require_relations(
    pool: &PgPool,
    relation_kind: char,
    expected: &[&str],
) -> Result<(), SchemaConformanceError> {
    let expected_names = expected
        .iter()
        .map(|name| (*name).to_owned())
        .collect::<Vec<_>>();
    let actual = sqlx::query_scalar::<_, String>(
        r#"
        SELECT relation.relname
        FROM pg_catalog.pg_class AS relation
        INNER JOIN pg_catalog.pg_namespace AS namespace
            ON namespace.oid = relation.relnamespace
        WHERE namespace.nspname = 'public'
            AND relation.relkind = $1
            AND relation.relname = ANY($2)
        ORDER BY relation.relname
        "#,
    )
    .bind(relation_kind.to_string())
    .bind(&expected_names)
    .fetch_all(pool)
    .await?;
    require_exact_set("relations", expected, actual)
}

async fn require_functions(pool: &PgPool) -> Result<(), SchemaConformanceError> {
    let expected = REQUIRED_FUNCTIONS
        .iter()
        .map(|name| (*name).to_owned())
        .collect::<Vec<_>>();
    let actual = sqlx::query_scalar::<_, String>(
        r#"
        SELECT DISTINCT function.proname
        FROM pg_catalog.pg_proc AS function
        INNER JOIN pg_catalog.pg_namespace AS namespace
            ON namespace.oid = function.pronamespace
        WHERE namespace.nspname = 'public'
            AND function.prokind = 'f'
            AND function.proname = ANY($1)
        ORDER BY function.proname
        "#,
    )
    .bind(&expected)
    .fetch_all(pool)
    .await?;
    require_exact_set("functions", REQUIRED_FUNCTIONS, actual)
}

async fn require_triggers(pool: &PgPool) -> Result<(), SchemaConformanceError> {
    let expected = REQUIRED_TRIGGERS
        .iter()
        .map(|name| (*name).to_owned())
        .collect::<Vec<_>>();
    let actual = sqlx::query_scalar::<_, String>(
        r#"
        SELECT trigger.tgname
        FROM pg_catalog.pg_trigger AS trigger
        INNER JOIN pg_catalog.pg_class AS relation
            ON relation.oid = trigger.tgrelid
        INNER JOIN pg_catalog.pg_namespace AS namespace
            ON namespace.oid = relation.relnamespace
        WHERE namespace.nspname = 'public'
            AND NOT trigger.tgisinternal
            AND trigger.tgenabled = 'O'
            AND trigger.tgname = ANY($1)
        ORDER BY trigger.tgname
        "#,
    )
    .bind(&expected)
    .fetch_all(pool)
    .await?;
    require_exact_set("triggers", REQUIRED_TRIGGERS, actual)
}

async fn require_payment_fact_columns(pool: &PgPool) -> Result<(), SchemaConformanceError> {
    let actual = sqlx::query_scalar::<_, String>(
        r#"
        SELECT column_name
        FROM information_schema.columns
        WHERE table_schema = 'public'
            AND table_name = 'billing_payment_facts'
        ORDER BY ordinal_position
        "#,
    )
    .fetch_all(pool)
    .await?;
    let expected = PAYMENT_FACT_COLUMNS
        .iter()
        .map(|column| (*column).to_owned())
        .collect::<Vec<_>>();
    if actual == expected {
        Ok(())
    } else {
        Err(SchemaConformanceError::Contract(format!(
            "billing_payment_facts columns differ: expected {expected:?}, found {actual:?}"
        )))
    }
}

async fn reject_legacy_columns(pool: &PgPool) -> Result<(), SchemaConformanceError> {
    let tables = REQUIRED_TABLES
        .iter()
        .map(|name| (*name).to_owned())
        .collect::<Vec<_>>();
    let legacy = sqlx::query_as::<_, (String, String)>(
        r#"
        SELECT table_name, column_name
        FROM information_schema.columns
        WHERE table_schema = 'public'
            AND table_name = ANY($1)
            AND (
                column_name IN (
                    'tenant_id',
                    'user_id',
                    'product',
                    'admin_user_id',
                    'granted_by_admin_user_id',
                    'revoked_by_admin_user_id',
                    'app_resolution_code',
                    'acquisition_channel'
                )
                OR column_name LIKE 'nmi\_%' ESCAPE '\'
                OR column_name LIKE 'base\_subscription\_%' ESCAPE '\'
                OR column_name LIKE 'google\_ads\_%' ESCAPE '\'
            )
        ORDER BY table_name, column_name
        "#,
    )
    .bind(&tables)
    .fetch_all(pool)
    .await?;
    if legacy.is_empty() {
        Ok(())
    } else {
        Err(SchemaConformanceError::Contract(format!(
            "legacy columns remain on canonical relations: {legacy:?}"
        )))
    }
}

async fn require_validated_constraints(pool: &PgPool) -> Result<(), SchemaConformanceError> {
    let tables = REQUIRED_TABLES
        .iter()
        .map(|name| (*name).to_owned())
        .collect::<Vec<_>>();
    let invalid = sqlx::query_as::<_, (String, String)>(
        r#"
        SELECT relation.relname, catalog_constraint.conname
        FROM pg_catalog.pg_constraint AS catalog_constraint
        INNER JOIN pg_catalog.pg_class AS relation
            ON relation.oid = catalog_constraint.conrelid
        INNER JOIN pg_catalog.pg_namespace AS namespace
            ON namespace.oid = relation.relnamespace
        WHERE namespace.nspname = 'public'
            AND relation.relname = ANY($1)
            AND catalog_constraint.conname LIKE 'billing\_%' ESCAPE '\'
            AND NOT catalog_constraint.convalidated
        ORDER BY relation.relname, catalog_constraint.conname
        "#,
    )
    .bind(&tables)
    .fetch_all(pool)
    .await?;
    if invalid.is_empty() {
        Ok(())
    } else {
        Err(SchemaConformanceError::Contract(format!(
            "canonical constraints are not validated: {invalid:?}"
        )))
    }
}

async fn require_account_scoped_order_index(pool: &PgPool) -> Result<(), SchemaConformanceError> {
    let definition = sqlx::query_scalar::<_, String>(
        r#"
        SELECT pg_catalog.pg_get_indexdef(index_relation.oid)
        FROM pg_catalog.pg_class AS index_relation
        INNER JOIN pg_catalog.pg_namespace AS namespace
            ON namespace.oid = index_relation.relnamespace
        WHERE namespace.nspname = 'public'
            AND index_relation.relname =
                'billing_payment_attempts_gateway_order_idx'
        "#,
    )
    .fetch_optional(pool)
    .await?;
    match definition {
        Some(definition) if definition.contains("(gateway_account_id, gateway_order_id)") => Ok(()),
        Some(definition) => Err(SchemaConformanceError::Contract(format!(
            "gateway-order uniqueness is not account-scoped: {definition}"
        ))),
        None => Err(SchemaConformanceError::Contract(
            "billing_payment_attempts_gateway_order_idx is missing".to_owned(),
        )),
    }
}

async fn require_catalog_fingerprint(pool: &PgPool) -> Result<(), SchemaConformanceError> {
    let tables = REQUIRED_TABLES
        .iter()
        .map(|name| (*name).to_owned())
        .collect::<Vec<_>>();
    let views = REQUIRED_VIEWS
        .iter()
        .map(|name| (*name).to_owned())
        .collect::<Vec<_>>();
    let functions = REQUIRED_FUNCTIONS
        .iter()
        .map(|name| (*name).to_owned())
        .collect::<Vec<_>>();
    let triggers = REQUIRED_TRIGGERS
        .iter()
        .map(|name| (*name).to_owned())
        .collect::<Vec<_>>();

    let columns = sqlx::query_scalar::<_, String>(
        r#"
        SELECT concat_ws(
            '|',
            table_name,
            ordinal_position::text,
            column_name,
            data_type,
            udt_name,
            is_nullable,
            COALESCE(column_default, '')
        )
        FROM information_schema.columns
        WHERE table_schema = 'public'
            AND table_name = ANY($1)
        ORDER BY table_name, ordinal_position
        "#,
    )
    .bind(&tables)
    .fetch_all(pool)
    .await?;
    let constraints = sqlx::query_scalar::<_, String>(
        r#"
        SELECT concat_ws(
            '|',
            relation.relname,
            catalog_constraint.conname,
            catalog_constraint.contype::text,
            pg_catalog.pg_get_constraintdef(catalog_constraint.oid, true)
        )
        FROM pg_catalog.pg_constraint AS catalog_constraint
        INNER JOIN pg_catalog.pg_class AS relation
            ON relation.oid = catalog_constraint.conrelid
        INNER JOIN pg_catalog.pg_namespace AS namespace
            ON namespace.oid = relation.relnamespace
        WHERE namespace.nspname = 'public'
            AND relation.relname = ANY($1)
            AND catalog_constraint.contype <> 'n'
            AND catalog_constraint.conname LIKE 'billing\_%' ESCAPE '\'
        ORDER BY relation.relname, catalog_constraint.conname
        "#,
    )
    .bind(&tables)
    .fetch_all(pool)
    .await?;
    let indexes = sqlx::query_scalar::<_, String>(
        r#"
        SELECT concat_ws(
            '|',
            table_relation.relname,
            index_relation.relname,
            pg_catalog.pg_get_indexdef(index_relation.oid)
        )
        FROM pg_catalog.pg_index AS catalog_index
        INNER JOIN pg_catalog.pg_class AS table_relation
            ON table_relation.oid = catalog_index.indrelid
        INNER JOIN pg_catalog.pg_class AS index_relation
            ON index_relation.oid = catalog_index.indexrelid
        INNER JOIN pg_catalog.pg_namespace AS namespace
            ON namespace.oid = table_relation.relnamespace
        WHERE namespace.nspname = 'public'
            AND table_relation.relname = ANY($1)
            AND index_relation.relname LIKE 'billing\_%' ESCAPE '\'
        ORDER BY table_relation.relname, index_relation.relname
        "#,
    )
    .bind(&tables)
    .fetch_all(pool)
    .await?;
    let view_definitions = sqlx::query_scalar::<_, String>(
        r#"
        SELECT concat_ws(
            '|',
            relation.relname,
            pg_catalog.pg_get_viewdef(relation.oid, true)
        )
        FROM pg_catalog.pg_class AS relation
        INNER JOIN pg_catalog.pg_namespace AS namespace
            ON namespace.oid = relation.relnamespace
        WHERE namespace.nspname = 'public'
            AND relation.relkind = 'v'
            AND relation.relname = ANY($1)
        ORDER BY relation.relname
        "#,
    )
    .bind(&views)
    .fetch_all(pool)
    .await?;
    let function_definitions = sqlx::query_scalar::<_, String>(
        r#"
        SELECT concat_ws(
            '|',
            catalog_function.proname,
            pg_catalog.pg_get_function_identity_arguments(
                catalog_function.oid
            ),
            pg_catalog.pg_get_functiondef(catalog_function.oid)
        )
        FROM pg_catalog.pg_proc AS catalog_function
        INNER JOIN pg_catalog.pg_namespace AS namespace
            ON namespace.oid = catalog_function.pronamespace
        WHERE namespace.nspname = 'public'
            AND catalog_function.prokind = 'f'
            AND catalog_function.proname = ANY($1)
        ORDER BY
            catalog_function.proname,
            pg_catalog.pg_get_function_identity_arguments(
                catalog_function.oid
            )
        "#,
    )
    .bind(&functions)
    .fetch_all(pool)
    .await?;
    let trigger_definitions = sqlx::query_scalar::<_, String>(
        r#"
        SELECT concat_ws(
            '|',
            relation.relname,
            catalog_trigger.tgname,
            catalog_trigger.tgenabled::text,
            pg_catalog.pg_get_triggerdef(catalog_trigger.oid, true)
        )
        FROM pg_catalog.pg_trigger AS catalog_trigger
        INNER JOIN pg_catalog.pg_class AS relation
            ON relation.oid = catalog_trigger.tgrelid
        INNER JOIN pg_catalog.pg_namespace AS namespace
            ON namespace.oid = relation.relnamespace
        WHERE namespace.nspname = 'public'
            AND NOT catalog_trigger.tgisinternal
            AND catalog_trigger.tgname = ANY($1)
        ORDER BY relation.relname, catalog_trigger.tgname
        "#,
    )
    .bind(&triggers)
    .fetch_all(pool)
    .await?;

    let actual = catalog_fingerprint([
        ("columns", columns.as_slice()),
        ("constraints", constraints.as_slice()),
        ("indexes", indexes.as_slice()),
        ("views", view_definitions.as_slice()),
        ("functions", function_definitions.as_slice()),
        ("triggers", trigger_definitions.as_slice()),
    ]);
    if actual == V1_CATALOG_FINGERPRINT {
        Ok(())
    } else {
        Err(SchemaConformanceError::Contract(format!(
            "canonical catalog fingerprint differs: expected {V1_CATALOG_FINGERPRINT:#018x}, found {actual:#018x}"
        )))
    }
}

fn catalog_fingerprint<'a>(categories: impl IntoIterator<Item = (&'a str, &'a [String])>) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    fn add_bytes(mut state: u64, bytes: &[u8]) -> u64 {
        for byte in bytes {
            state ^= u64::from(*byte);
            state = state.wrapping_mul(PRIME);
        }
        state
    }

    let mut state = OFFSET_BASIS;
    for (category, rows) in categories {
        state = add_bytes(state, category.as_bytes());
        state = add_bytes(state, &rows.len().to_be_bytes());
        for row in rows {
            state = add_bytes(state, &row.len().to_be_bytes());
            state = add_bytes(state, row.as_bytes());
        }
    }
    state
}

fn require_exact_set(
    category: &str,
    expected: &[&str],
    actual: Vec<String>,
) -> Result<(), SchemaConformanceError> {
    let expected = expected.iter().copied().collect::<BTreeSet<_>>();
    let actual = actual.iter().map(String::as_str).collect::<BTreeSet<_>>();
    if actual == expected {
        Ok(())
    } else {
        let missing = expected.difference(&actual).copied().collect::<Vec<_>>();
        Err(SchemaConformanceError::Contract(format!(
            "missing {category}: {missing:?}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use std::{error::Error, io};

    use sqlx::PgPool;
    use uuid::Uuid;

    use super::{V1_INSTALL_SQL, assert_v1_conforms};
    use crate::test_support::{GatewayAccountFixture, TestDatabase, create_gateway_account};

    #[tokio::test]
    async fn schema_v1_contains_no_host_or_cutover_vocabulary() -> Result<(), Box<dyn Error>> {
        if V1_INSTALL_SQL.trim().is_empty() {
            return Err(io::Error::other("version-1 install artifact is empty").into());
        }
        for forbidden in [
            "tenant_id",
            "user_id",
            "nmi_",
            "admin_user_id",
            "granted_by_admin_user_id",
            "revoked_by_admin_user_id",
            "app_resolution_code",
            "base_subscription_",
            "order_sale",
            "acquisition_channel",
            "google_ads_",
            "billing_pending_approved_",
            "2026-",
        ] {
            if V1_INSTALL_SQL.contains(forbidden) {
                return Err(io::Error::other(format!(
                    "version-1 install artifact contains forbidden vocabulary {forbidden:?}"
                ))
                .into());
            }
        }

        let database = TestDatabase::start("sr_schema_v1").await?;
        let result = assert_v1_conforms(&database.pool).await;
        let cleanup = database.cleanup().await;
        result?;
        cleanup
    }

    #[tokio::test]
    async fn schema_v1_gateway_order_identity_is_account_scoped() -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("sr_order_v1").await?;
        let result = async {
            let provider_key = "test_gateway";
            sqlx::query(
                "INSERT INTO billing_gateway_provider_rate_limits (provider_key) VALUES ($1)",
            )
            .bind(provider_key)
            .execute(&database.pool)
            .await?;

            let scope_a = Uuid::now_v7();
            let scope_b = Uuid::now_v7();
            let account_a = Uuid::now_v7();
            let account_b = Uuid::now_v7();
            for (account, scope) in [(account_a, scope_a), (account_b, scope_b)] {
                sqlx::query(
                    r#"
                    INSERT INTO billing_gateway_accounts (
                        id,
                        billing_scope_id,
                        provider_key,
                        gateway_configuration_id
                    ) VALUES ($1, $2, $3, $4)
                    "#,
                )
                .bind(account)
                .bind(scope)
                .bind(provider_key)
                .bind(Uuid::now_v7())
                .execute(&database.pool)
                .await?;
            }

            let subscriber = Uuid::now_v7();
            insert_host_charge_attempt(
                &database.pool,
                scope_a,
                subscriber,
                account_a,
                "shared-order-reference",
            )
            .await?;
            insert_host_charge_attempt(
                &database.pool,
                scope_b,
                subscriber,
                account_b,
                "shared-order-reference",
            )
            .await?;

            let duplicate = insert_host_charge_attempt(
                &database.pool,
                scope_a,
                Uuid::now_v7(),
                account_a,
                "shared-order-reference",
            )
            .await;
            let error =
                duplicate.expect_err("one gateway account must reject a duplicate order reference");
            let constraint = error
                .as_database_error()
                .and_then(|error| error.constraint());
            if constraint != Some("billing_payment_attempts_gateway_order_idx") {
                return Err(
                    io::Error::other(format!("unexpected duplicate-order error: {error}")).into(),
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
    async fn schema_v1_payment_facts_supports_skip_locked_row_locks() -> Result<(), Box<dyn Error>>
    {
        let database = TestDatabase::start("sr_view_lock_v1").await?;
        let result = async {
            let gateway = create_gateway_account(&database.pool, "test_gateway").await?;
            let attempt_id = insert_host_charge_attempt_record(
                &database.pool,
                gateway,
                Uuid::now_v7(),
                Uuid::now_v7(),
                "view-lock-order",
                "view-lock-idempotency",
            )
            .await?;

            let mut locker = database.pool.begin().await?;
            let locked = sqlx::query_scalar::<_, Uuid>(
                r#"
                SELECT attempt_id
                FROM billing_payment_facts
                WHERE attempt_id = $1
                FOR UPDATE
                "#,
            )
            .bind(attempt_id)
            .fetch_one(&mut *locker)
            .await?;
            if locked != attempt_id {
                return Err(
                    io::Error::other("payment-facts view returned the wrong attempt").into(),
                );
            }

            let mut contender = database.pool.begin().await?;
            let skipped = sqlx::query_scalar::<_, Uuid>(
                r#"
                SELECT attempt_id
                FROM billing_payment_facts
                WHERE attempt_id = $1
                FOR UPDATE SKIP LOCKED
                "#,
            )
            .bind(attempt_id)
            .fetch_optional(&mut *contender)
            .await?;
            if skipped.is_some() {
                return Err(io::Error::other(
                    "payment-facts SKIP LOCKED did not observe the underlying attempt lock",
                )
                .into());
            }

            locker.rollback().await?;
            let acquired = sqlx::query_scalar::<_, Uuid>(
                r#"
                SELECT attempt_id
                FROM billing_payment_facts
                WHERE attempt_id = $1
                FOR UPDATE SKIP LOCKED
                "#,
            )
            .bind(attempt_id)
            .fetch_one(&mut *contender)
            .await?;
            contender.rollback().await?;
            if acquired != attempt_id {
                return Err(io::Error::other(
                    "payment-facts row was not lockable after the competing lock released",
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

    #[tokio::test]
    async fn schema_v1_gateway_lifecycle_state_round_trips() -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("sr_lifecycle_v1").await?;
        let result = async {
            let provider_key = "test_gateway";
            let scope = Uuid::now_v7();
            let account = Uuid::now_v7();
            sqlx::query(
                "INSERT INTO billing_gateway_provider_rate_limits (provider_key) VALUES ($1)",
            )
            .bind(provider_key)
            .execute(&database.pool)
            .await?;
            sqlx::query(
                r#"
                INSERT INTO billing_gateway_accounts (
                    id,
                    billing_scope_id,
                    provider_key,
                    gateway_configuration_id
                ) VALUES ($1, $2, $3, $4)
                "#,
            )
            .bind(account)
            .bind(scope)
            .bind(provider_key)
            .bind(Uuid::now_v7())
            .execute(&database.pool)
            .await?;

            let valid = [
                ("unknown", None),
                ("pending_settlement", None),
                ("settled", None),
                ("settled", Some(1)),
                ("voided", None),
                ("refunded", Some(100)),
                ("chargeback", None),
                ("chargeback", Some(100)),
            ];
            for (position, (status, refunded_amount_cents)) in valid.iter().enumerate() {
                let order = format!("valid-lifecycle-{position}");
                let row = sqlx::query_as::<_, (String, Option<i32>)>(
                    r#"
                    INSERT INTO billing_gateway_lifecycle_pending_updates (
                        billing_scope_id,
                        gateway_account_id,
                        gateway_order_id,
                        gateway_lifecycle_status,
                        refunded_amount_cents
                    ) VALUES ($1, $2, $3, $4, $5)
                    RETURNING gateway_lifecycle_status, refunded_amount_cents
                    "#,
                )
                .bind(scope)
                .bind(account)
                .bind(order)
                .bind(*status)
                .bind(*refunded_amount_cents)
                .fetch_one(&database.pool)
                .await?;
                if row != (status.to_string(), *refunded_amount_cents) {
                    return Err(io::Error::other(format!(
                        "lifecycle state changed during staging: expected {status:?}/{refunded_amount_cents:?}, found {row:?}"
                    ))
                    .into());
                }
            }

            let invalid = [
                ("unknown", Some(1)),
                ("pending_settlement", Some(1)),
                ("voided", Some(1)),
                ("settled", Some(0)),
                ("refunded", None),
                ("chargeback", Some(0)),
                ("not_a_state", None),
            ];
            for (position, (status, refunded_amount_cents)) in invalid.iter().enumerate() {
                let inserted = sqlx::query(
                    r#"
                    INSERT INTO billing_gateway_lifecycle_pending_updates (
                        billing_scope_id,
                        gateway_account_id,
                        gateway_order_id,
                        gateway_lifecycle_status,
                        refunded_amount_cents
                    ) VALUES ($1, $2, $3, $4, $5)
                    "#,
                )
                .bind(scope)
                .bind(account)
                .bind(format!("invalid-lifecycle-{position}"))
                .bind(*status)
                .bind(*refunded_amount_cents)
                .execute(&database.pool)
                .await;
                if inserted.is_ok() {
                    return Err(io::Error::other(format!(
                        "invalid lifecycle state was accepted: {status:?}/{refunded_amount_cents:?}"
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

    #[tokio::test]
    async fn schema_v1_processor_charge_triggers_preserve_dimensions_and_evidence()
    -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("sr_charge_v1").await?;
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
    async fn schema_v1_host_charge_ledger_admission_matches_legacy_modes()
    -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("sr_ledger_v1").await?;
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

    #[tokio::test]
    async fn schema_v1_catalog_conformance_accepts_host_extensions_and_rejects_canonical_drift()
    -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("sr_catalog_v1").await?;
        let result = async {
            sqlx::raw_sql(
                r#"
                CREATE TABLE creditkit_billing_scopes (
                    id uuid PRIMARY KEY
                );

                ALTER TABLE billing_gateway_accounts
                    ADD CONSTRAINT creditkit_gateway_accounts_scope_fk
                    FOREIGN KEY (billing_scope_id)
                    REFERENCES creditkit_billing_scopes(id)
                    ON DELETE RESTRICT;

                CREATE INDEX creditkit_gateway_accounts_scope_idx
                ON billing_gateway_accounts (billing_scope_id, id);

                CREATE FUNCTION creditkit_gateway_account_noop()
                RETURNS trigger
                LANGUAGE plpgsql
                SET search_path = pg_catalog, public
                AS $$
                BEGIN
                    RETURN NEW;
                END
                $$;

                CREATE TRIGGER creditkit_gateway_account_noop
                BEFORE UPDATE ON billing_gateway_accounts
                FOR EACH ROW
                EXECUTE FUNCTION creditkit_gateway_account_noop();
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
        let database = TestDatabase::start("sr_text_v1").await?;
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
                insert_attempt_gateway_text(
                    &database.pool,
                    gateway,
                    subscriber_id,
                    field,
                    boundary,
                )
                .await?;
                let rejected = insert_attempt_gateway_text(
                    &database.pool,
                    gateway,
                    subscriber_id,
                    field,
                    &oversized,
                )
                .await;
                expect_database_constraint(
                    rejected,
                    "billing_payment_attempts_gateway_text_check",
                )?;
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
                expect_database_constraint(
                    rejected,
                    "billing_processor_charges_gateway_text_check",
                )?;
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
    async fn schema_v1_nullable_shapes_reject_missing_required_fields() -> Result<(), Box<dyn Error>>
    {
        let database = TestDatabase::start("sr_shapes_v1").await?;
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

    async fn create_subscription_fixture(
        pool: &PgPool,
        gateway: GatewayAccountFixture,
        subscriber_id: Uuid,
    ) -> Result<(Uuid, Uuid, String), sqlx::Error> {
        let payment_method_id = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO billing_payment_methods (
                id, billing_scope_id, subscriber_id, gateway_account_id,
                gateway_payment_method_reference, status
            ) VALUES ($1, $2, $3, $4, $5, 'active')
            "#,
        )
        .bind(payment_method_id)
        .bind(gateway.billing_scope_id)
        .bind(subscriber_id)
        .bind(gateway.gateway_account_id)
        .bind(format!("vault_{}", payment_method_id.simple()))
        .execute(pool)
        .await?;

        let subscription_id = Uuid::now_v7();
        let initial_transaction_id = format!("txn_{}", subscription_id.simple());
        sqlx::query(
            r#"
            INSERT INTO billing_subscriptions (
                id, billing_scope_id, subscriber_id, plan_key, status,
                gateway_account_id, payment_method_id, amount_cents, currency,
                current_period_start_at, current_period_end_at, next_renewal_at,
                initial_transaction_id
            ) VALUES (
                $1, $2, $3, 'base_subscription', 'active', $4, $5, 100, 'USD',
                '2026-01-01 00:00:00+00', '2026-02-01 00:00:00+00',
                '2026-02-01 00:00:00+00', $6
            )
            "#,
        )
        .bind(subscription_id)
        .bind(gateway.billing_scope_id)
        .bind(subscriber_id)
        .bind(gateway.gateway_account_id)
        .bind(payment_method_id)
        .bind(&initial_transaction_id)
        .execute(pool)
        .await?;
        Ok((payment_method_id, subscription_id, initial_transaction_id))
    }

    async fn insert_payment_method_gateway_text(
        pool: &PgPool,
        gateway: GatewayAccountFixture,
        subscriber_id: Uuid,
        field: &str,
        value: &str,
    ) -> Result<sqlx::postgres::PgQueryResult, sqlx::Error> {
        debug_assert!(["payment_type", "card_brand"].contains(&field));
        let payment_method_id = Uuid::now_v7();
        let query = format!(
            r#"
            INSERT INTO billing_payment_methods (
                id,
                billing_scope_id,
                subscriber_id,
                gateway_account_id,
                gateway_payment_method_reference,
                status,
                {field}
            ) VALUES ($1, $2, $3, $4, $5, 'active', $6)
            "#
        );
        sqlx::query(&query)
            .bind(payment_method_id)
            .bind(gateway.billing_scope_id)
            .bind(subscriber_id)
            .bind(gateway.gateway_account_id)
            .bind(format!("vault_{}", payment_method_id.simple()))
            .bind(value)
            .execute(pool)
            .await
    }

    async fn insert_attempt_gateway_text(
        pool: &PgPool,
        gateway: GatewayAccountFixture,
        subscriber_id: Uuid,
        field: &str,
        value: &str,
    ) -> Result<sqlx::postgres::PgQueryResult, sqlx::Error> {
        debug_assert!(
            [
                "gateway_response",
                "gateway_response_code",
                "gateway_response_text",
                "gateway_condition",
                "payment_type",
                "card_brand",
                "gateway_lifecycle_action",
            ]
            .contains(&field)
        );
        let attempt_id = Uuid::now_v7();
        let target_id = Uuid::now_v7();
        let query = format!(
            r#"
            INSERT INTO billing_payment_attempts (
                id,
                billing_scope_id,
                subscriber_id,
                host_charge_target_id,
                attempt_kind,
                status,
                idempotency_key,
                request_fingerprint,
                amount_cents,
                currency,
                gateway_account_id,
                gateway_configuration_id,
                gateway_order_id,
                {field}
            ) VALUES (
                $1, $2, $3, $4, 'host_charge', 'pending', $5, $6,
                100, 'USD', $7, $8, $9, $10
            )
            "#
        );
        sqlx::query(&query)
            .bind(attempt_id)
            .bind(gateway.billing_scope_id)
            .bind(subscriber_id)
            .bind(target_id)
            .bind(format!("diagnostic-{}", attempt_id.simple()))
            .bind(format!("host_charge:{target_id}:100:USD"))
            .bind(gateway.gateway_account_id)
            .bind(gateway.gateway_configuration_id)
            .bind(format!("diagnostic-order-{}", attempt_id.simple()))
            .bind(value)
            .execute(pool)
            .await
    }

    async fn insert_charge_gateway_text(
        pool: &PgPool,
        gateway: GatewayAccountFixture,
        subscriber_id: Uuid,
        field: &str,
        value: &str,
    ) -> Result<sqlx::postgres::PgQueryResult, sqlx::Error> {
        debug_assert!(
            [
                "gateway_response",
                "gateway_response_code",
                "gateway_response_text",
                "gateway_condition",
                "payment_type",
                "card_brand",
            ]
            .contains(&field)
        );
        let target_id = Uuid::now_v7();
        let order_id = format!("charge-text-order-{}", Uuid::now_v7().simple());
        let attempt_id = insert_host_charge_attempt_record(
            pool,
            gateway,
            subscriber_id,
            target_id,
            &order_id,
            &format!("charge-text-{}", Uuid::now_v7().simple()),
        )
        .await?;
        let query = format!(
            r#"
            INSERT INTO billing_processor_charges (
                attempt_id,
                billing_scope_id,
                gateway_account_id,
                gateway_order_id,
                attempt_kind,
                amount_cents,
                currency,
                {field}
            ) VALUES ($1, $2, $3, $4, 'host_charge', 100, 'USD', $5)
            "#
        );
        sqlx::query(&query)
            .bind(attempt_id)
            .bind(gateway.billing_scope_id)
            .bind(gateway.gateway_account_id)
            .bind(order_id)
            .bind(value)
            .execute(pool)
            .await
    }

    async fn insert_attestation_gateway_text(
        pool: &PgPool,
        gateway: GatewayAccountFixture,
        subscriber_id: Uuid,
        field: &str,
        value: &str,
    ) -> Result<sqlx::postgres::PgQueryResult, sqlx::Error> {
        debug_assert!(
            [
                "gateway_response",
                "gateway_response_code",
                "gateway_response_text",
                "gateway_condition",
                "payment_type",
                "card_brand",
            ]
            .contains(&field)
        );
        let target_id = Uuid::now_v7();
        let order_id = format!("attestation-order-{}", Uuid::now_v7().simple());
        let transaction_id = format!("txn_{}", Uuid::now_v7().simple());
        let attempt_id = insert_host_charge_attempt_record(
            pool,
            gateway,
            subscriber_id,
            target_id,
            &order_id,
            &format!("attestation-{}", Uuid::now_v7().simple()),
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
            ) VALUES ($1, $2, $3, $4, $5, $6, 'host_charge', 100, 'USD')
            "#,
        )
        .bind(charge_id)
        .bind(attempt_id)
        .bind(gateway.billing_scope_id)
        .bind(gateway.gateway_account_id)
        .bind(&order_id)
        .bind(&transaction_id)
        .execute(pool)
        .await?;

        let query = format!(
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
                attested_at,
                {field}
            ) VALUES (
                $1, $2, $3, 'refund', 'operator confirmed refund',
                'processor_charge_external_reversal_required',
                'processor_charge_externally_refunded',
                $4, $5, $6, 100, 'USD', $7, clock_timestamp(), $8
            )
            "#
        );
        sqlx::query(&query)
            .bind(attempt_id)
            .bind(charge_id)
            .bind(Uuid::now_v7())
            .bind(gateway.gateway_account_id)
            .bind(gateway.gateway_configuration_id)
            .bind(order_id)
            .bind(transaction_id)
            .bind(value)
            .execute(pool)
            .await
    }

    async fn insert_lifecycle_gateway_text(
        pool: &PgPool,
        gateway: GatewayAccountFixture,
        field: &str,
        value: &str,
    ) -> Result<sqlx::postgres::PgQueryResult, sqlx::Error> {
        debug_assert!(["gateway_condition", "gateway_lifecycle_action"].contains(&field));
        let query = format!(
            r#"
            INSERT INTO billing_gateway_lifecycle_pending_updates (
                billing_scope_id,
                gateway_account_id,
                gateway_order_id,
                gateway_lifecycle_status,
                {field}
            ) VALUES ($1, $2, $3, 'unknown', $4)
            "#
        );
        sqlx::query(&query)
            .bind(gateway.billing_scope_id)
            .bind(gateway.gateway_account_id)
            .bind(format!("lifecycle-text-{}", Uuid::now_v7().simple()))
            .bind(value)
            .execute(pool)
            .await
    }

    async fn expect_conformance_rejection(
        pool: &PgPool,
        context: &str,
    ) -> Result<(), Box<dyn Error>> {
        if assert_v1_conforms(pool).await.is_ok() {
            Err(io::Error::other(format!("catalog conformance accepted {context}")).into())
        } else {
            Ok(())
        }
    }

    async fn assert_admission(
        pool: &PgPool,
        gateway: GatewayAccountFixture,
        subscriber_id: Uuid,
        target_id: Uuid,
        expectation: (&str, Option<&str>, Option<Uuid>, &str),
    ) -> Result<(), Box<dyn Error>> {
        let (mode, idempotency_key, attempt_id, expected) = expectation;
        let actual = host_charge_admission(
            pool,
            gateway,
            subscriber_id,
            target_id,
            mode,
            idempotency_key,
            attempt_id,
        )
        .await?;
        if actual == expected {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "host charge admission {mode:?} returned {actual:?}, expected {expected:?}"
            ))
            .into())
        }
    }

    async fn host_charge_admission(
        pool: &PgPool,
        gateway: GatewayAccountFixture,
        subscriber_id: Uuid,
        target_id: Uuid,
        mode: &str,
        idempotency_key: Option<&str>,
        attempt_id: Option<Uuid>,
    ) -> Result<String, sqlx::Error> {
        sqlx::query_scalar(
            r#"
            SELECT billing_host_charge_ledger_admission(
                $1, $2, $3, $4, $5, $6
            )
            "#,
        )
        .bind(gateway.billing_scope_id)
        .bind(subscriber_id)
        .bind(target_id)
        .bind(mode)
        .bind(idempotency_key)
        .bind(attempt_id)
        .fetch_one(pool)
        .await
    }

    fn expect_database_rejection(
        result: Result<sqlx::postgres::PgQueryResult, sqlx::Error>,
        context: &str,
    ) -> Result<(), Box<dyn Error>> {
        if result.is_ok() {
            Err(io::Error::other(format!("database accepted {context}")).into())
        } else {
            Ok(())
        }
    }

    fn expect_database_constraint(
        result: Result<sqlx::postgres::PgQueryResult, sqlx::Error>,
        expected_constraint: &str,
    ) -> Result<(), Box<dyn Error>> {
        let error = result.expect_err("database operation should violate a constraint");
        let actual = error
            .as_database_error()
            .and_then(|database_error| database_error.constraint());
        if actual == Some(expected_constraint) {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "expected constraint {expected_constraint:?}, found {actual:?}: {error}"
            ))
            .into())
        }
    }

    async fn insert_host_charge_attempt(
        pool: &PgPool,
        billing_scope_id: Uuid,
        subscriber_id: Uuid,
        gateway_account_id: Uuid,
        gateway_order_id: &str,
    ) -> Result<Uuid, sqlx::Error> {
        insert_host_charge_attempt_record(
            pool,
            GatewayAccountFixture {
                billing_scope_id,
                gateway_account_id,
                gateway_configuration_id: Uuid::now_v7(),
            },
            subscriber_id,
            Uuid::now_v7(),
            gateway_order_id,
            &format!("idempotency-{}", Uuid::now_v7()),
        )
        .await
    }

    async fn insert_host_charge_attempt_record(
        pool: &PgPool,
        gateway: GatewayAccountFixture,
        subscriber_id: Uuid,
        host_charge_target_id: Uuid,
        gateway_order_id: &str,
        idempotency_key: &str,
    ) -> Result<Uuid, sqlx::Error> {
        let attempt_id = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO billing_payment_attempts (
                id,
                billing_scope_id,
                subscriber_id,
                host_charge_target_id,
                attempt_kind,
                status,
                idempotency_key,
                request_fingerprint,
                amount_cents,
                currency,
                gateway_account_id,
                gateway_configuration_id,
                gateway_order_id
            ) VALUES (
                $1,
                $2,
                $3,
                $4,
                'host_charge',
                'pending',
                $5,
                $6,
                100,
                'USD',
                $7,
                $8,
                $9
            )
            "#,
        )
        .bind(attempt_id)
        .bind(gateway.billing_scope_id)
        .bind(subscriber_id)
        .bind(host_charge_target_id)
        .bind(idempotency_key)
        .bind(format!("host_charge:{host_charge_target_id}:100:USD"))
        .bind(gateway.gateway_account_id)
        .bind(gateway.gateway_configuration_id)
        .bind(gateway_order_id)
        .execute(pool)
        .await?;
        Ok(attempt_id)
    }
}
