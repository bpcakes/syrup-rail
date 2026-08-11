//! Read-only canonical schema conformance and feature-gated artifact support.

use std::collections::BTreeSet;

use sqlx::{PgConnection, PgPool};
use thiserror::Error;

/// The only PostgreSQL major version supported by this crate and schema
/// contract.
pub const SUPPORTED_POSTGRES_MAJOR_VERSION: u16 = 18;

// These artifacts are intentionally unavailable to ordinary production
// dependencies. Hosts materialize them through their own migration system;
// the explicit feature is for contract fixtures and migration tests only.
#[cfg(any(test, feature = "schema-contract-test-support"))]
/// The immutable version-1 fresh-install artifact.
pub const V1_INSTALL_SQL: &str = include_str!("../schema/v1/install.sql");
#[cfg(any(test, feature = "schema-contract-test-support"))]
/// The immutable version-2 fresh-install artifact.
pub const V2_INSTALL_SQL: &str = include_str!("../schema/v2/install.sql");
#[cfg(any(test, feature = "schema-contract-test-support"))]
/// The read-only version-1-to-version-2 upgrade preflight.
pub const V1_TO_V2_PREFLIGHT_SQL: &str = include_str!("../schema/v2/preflight_from_v1.sql");
#[cfg(any(test, feature = "schema-contract-test-support"))]
/// The read-only audit of v1 retry histories reclassified by version 2.
pub const V1_TO_V2_RETRY_RECLASSIFICATION_AUDIT_SQL: &str =
    include_str!("../schema/v2/audit_retry_reclassification_from_v1.sql");
#[cfg(any(test, feature = "schema-contract-test-support"))]
/// The immutable forward-only version-1-to-version-2 upgrade artifact.
pub const V1_TO_V2_UPGRADE_SQL: &str = include_str!("../schema/v2/upgrade_from_v1.sql");

// Non-cryptographic drift fingerprint over the canonical PostgreSQL catalog.
// Host objects use host-prefixed names and are deliberately excluded.
#[cfg(any(test, feature = "schema-contract-test-support"))]
const V1_CATALOG_FINGERPRINT: u64 = 0xc949_7313_2b48_83d9;
const V2_CATALOG_FINGERPRINT: u64 = 0x0da8_83df_aab0_1e30;

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

#[cfg(any(test, feature = "schema-contract-test-support"))]
const V1_CURRENT_SUBSCRIPTION_COLUMNS: &[&str] = &[
    "id",
    "billing_scope_id",
    "gateway_account_id",
    "subscriber_id",
    "plan_key",
    "status",
    "payment_method_id",
    "amount_cents",
    "currency",
    "current_period_start_at",
    "current_period_end_at",
    "next_renewal_at",
    "initial_transaction_id",
    "canceled_at",
    "created_at",
    "updated_at",
    "current_subscription_rank",
];

const V2_CURRENT_SUBSCRIPTION_COLUMNS: &[&str] = &[
    "id",
    "billing_scope_id",
    "gateway_account_id",
    "subscriber_id",
    "plan_key",
    "status",
    "payment_method_id",
    "amount_cents",
    "currency",
    "current_period_start_at",
    "current_period_end_at",
    "next_renewal_at",
    "initial_transaction_id",
    "canceled_at",
    "created_at",
    "updated_at",
    "current_subscription_rank",
    "phase",
    "recurring_period_kind",
    "recurring_period_count",
    "trial_amount_cents",
    "trial_period_kind",
    "trial_period_count",
    "dunning_retry_delays_seconds",
    "dunning_exhaustion",
    "past_due_access",
    "next_payment_attempt_at",
    "unpaid_at",
];

/// Why a host database does not satisfy a canonical schema contract.
///
/// [`crate::assert_runtime_schema_v2_compatible`] reports version `2` in its
/// [`Self::Contract`] diagnostic. Database failures include inability to begin
/// or commit the read-only catalog snapshot.
#[non_exhaustive]
#[derive(Debug, Error)]
pub enum SchemaConformanceError {
    #[error("schema conformance query failed: {0}")]
    Database(#[from] sqlx::Error),
    #[error(
        "PostgreSQL major version {required_major} is required; connected server reported server_version_num={actual_server_version_num}"
    )]
    UnsupportedPostgresVersion {
        required_major: u16,
        actual_server_version_num: i32,
    },
    #[error("schema version {version} does not conform: {detail}")]
    Contract { version: u16, detail: String },
}

/// Asserts that a host database is compatible with the canonical schema-v2
/// contract before the host accepts billing work.
///
/// Call this after the host has applied its immutable Syrup Rail install or
/// forward-only upgrade migration through its normal migration deployment.
/// This function does not install, upgrade, preflight, audit, or otherwise
/// mutate the schema. It runs the same full canonical v2 catalog conformance
/// and fingerprint check used by the schema-contract tests in one
/// `REPEATABLE READ READ ONLY` PostgreSQL transaction. PostgreSQL major version
/// 18 is required; other majors are rejected before catalog comparison.
pub async fn assert_runtime_schema_v2_compatible(
    pool: &PgPool,
) -> Result<(), SchemaConformanceError> {
    assert_schema_conforms_in_read_only_snapshot(
        pool,
        2,
        V2_CURRENT_SUBSCRIPTION_COLUMNS,
        V2_CATALOG_FINGERPRINT,
    )
    .await
}

#[cfg(any(test, feature = "schema-contract-test-support"))]
/// Asserts that an already-migrated host database contains the canonical v1
/// objects without re-running or exposing a production migrator.
///
/// Separately named host objects are permitted. Canonical relations, views,
/// functions, triggers, validated constraints, and bounded host read surfaces
/// must remain present and retain their neutral vocabulary.
pub async fn assert_v1_conforms(pool: &PgPool) -> Result<(), SchemaConformanceError> {
    assert_schema_conforms_in_read_only_snapshot(
        pool,
        1,
        V1_CURRENT_SUBSCRIPTION_COLUMNS,
        V1_CATALOG_FINGERPRINT,
    )
    .await
}

#[cfg(any(test, feature = "schema-contract-test-support"))]
/// Asserts that an already-migrated host database contains the canonical v2
/// objects without exposing a production runtime migrator.
pub async fn assert_v2_conforms(pool: &PgPool) -> Result<(), SchemaConformanceError> {
    assert_runtime_schema_v2_compatible(pool).await
}

async fn assert_schema_conforms_in_read_only_snapshot(
    pool: &PgPool,
    version: u16,
    current_subscription_columns: &[&str],
    expected_fingerprint: u64,
) -> Result<(), SchemaConformanceError> {
    let mut transaction = pool
        .begin_with("BEGIN TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .await?;
    let result = async {
        let server_version_num =
            sqlx::query_scalar::<_, i32>("SELECT current_setting('server_version_num')::integer")
                .fetch_one(&mut *transaction)
                .await?;
        require_supported_postgres_version_num(server_version_num)?;
        assert_schema_conforms(
            &mut transaction,
            version,
            current_subscription_columns,
            expected_fingerprint,
        )
        .await
    }
    .await;
    match result {
        Ok(()) => transaction.commit().await?,
        Err(error) => {
            transaction.rollback().await?;
            return Err(error);
        }
    }
    Ok(())
}

fn require_supported_postgres_version_num(
    actual_server_version_num: i32,
) -> Result<(), SchemaConformanceError> {
    if actual_server_version_num / 10_000 == i32::from(SUPPORTED_POSTGRES_MAJOR_VERSION) {
        Ok(())
    } else {
        Err(SchemaConformanceError::UnsupportedPostgresVersion {
            required_major: SUPPORTED_POSTGRES_MAJOR_VERSION,
            actual_server_version_num,
        })
    }
}

async fn assert_schema_conforms(
    connection: &mut PgConnection,
    version: u16,
    current_subscription_columns: &[&str],
    expected_fingerprint: u64,
) -> Result<(), SchemaConformanceError> {
    require_relations(connection, version, 'r', REQUIRED_TABLES).await?;
    require_relations(connection, version, 'v', REQUIRED_VIEWS).await?;
    require_functions(connection, version).await?;
    require_triggers(connection, version).await?;
    require_view_columns(
        connection,
        version,
        "billing_payment_facts",
        PAYMENT_FACT_COLUMNS,
    )
    .await?;
    require_view_columns(
        connection,
        version,
        "billing_current_subscriptions",
        current_subscription_columns,
    )
    .await?;
    reject_legacy_columns(connection, version).await?;
    require_validated_constraints(connection, version).await?;
    require_account_scoped_order_index(connection, version).await?;
    require_catalog_fingerprint(connection, version, expected_fingerprint).await?;
    Ok(())
}

async fn require_relations(
    connection: &mut PgConnection,
    version: u16,
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
    .fetch_all(&mut *connection)
    .await?;
    require_exact_set(version, "relations", expected, actual)
}

async fn require_functions(
    connection: &mut PgConnection,
    version: u16,
) -> Result<(), SchemaConformanceError> {
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
    .fetch_all(&mut *connection)
    .await?;
    require_exact_set(version, "functions", REQUIRED_FUNCTIONS, actual)
}

async fn require_triggers(
    connection: &mut PgConnection,
    version: u16,
) -> Result<(), SchemaConformanceError> {
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
    .fetch_all(&mut *connection)
    .await?;
    require_exact_set(version, "triggers", REQUIRED_TRIGGERS, actual)
}

async fn require_view_columns(
    connection: &mut PgConnection,
    version: u16,
    view: &str,
    expected_columns: &[&str],
) -> Result<(), SchemaConformanceError> {
    let actual = sqlx::query_scalar::<_, String>(
        r#"
        SELECT column_name
        FROM information_schema.columns
        WHERE table_schema = 'public'
            AND table_name = $1
        ORDER BY ordinal_position
        "#,
    )
    .bind(view)
    .fetch_all(&mut *connection)
    .await?;
    let expected = expected_columns
        .iter()
        .map(|column| (*column).to_owned())
        .collect::<Vec<_>>();
    if actual == expected {
        Ok(())
    } else {
        Err(contract_error(
            version,
            format!("{view} columns differ: expected {expected:?}, found {actual:?}"),
        ))
    }
}

async fn reject_legacy_columns(
    connection: &mut PgConnection,
    version: u16,
) -> Result<(), SchemaConformanceError> {
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
    .fetch_all(&mut *connection)
    .await?;
    if legacy.is_empty() {
        Ok(())
    } else {
        Err(contract_error(
            version,
            format!("legacy columns remain on canonical relations: {legacy:?}"),
        ))
    }
}

async fn require_validated_constraints(
    connection: &mut PgConnection,
    version: u16,
) -> Result<(), SchemaConformanceError> {
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
    .fetch_all(&mut *connection)
    .await?;
    if invalid.is_empty() {
        Ok(())
    } else {
        Err(contract_error(
            version,
            format!("canonical constraints are not validated: {invalid:?}"),
        ))
    }
}

async fn require_account_scoped_order_index(
    connection: &mut PgConnection,
    version: u16,
) -> Result<(), SchemaConformanceError> {
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
    .fetch_optional(&mut *connection)
    .await?;
    match definition {
        Some(definition) if definition.contains("(gateway_account_id, gateway_order_id)") => Ok(()),
        Some(definition) => Err(contract_error(
            version,
            format!("gateway-order uniqueness is not account-scoped: {definition}"),
        )),
        None => Err(contract_error(
            version,
            "billing_payment_attempts_gateway_order_idx is missing",
        )),
    }
}

async fn require_catalog_fingerprint(
    connection: &mut PgConnection,
    version: u16,
    expected: u64,
) -> Result<(), SchemaConformanceError> {
    let actual = canonical_catalog_fingerprint(connection).await?;
    if actual == expected {
        Ok(())
    } else {
        Err(contract_error(
            version,
            format!(
                "canonical catalog fingerprint differs: expected {expected:#018x}, found {actual:#018x}"
            ),
        ))
    }
}

async fn canonical_catalog_fingerprint(
    connection: &mut PgConnection,
) -> Result<u64, SchemaConformanceError> {
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
    .fetch_all(&mut *connection)
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
    .fetch_all(&mut *connection)
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
    .fetch_all(&mut *connection)
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
    .fetch_all(&mut *connection)
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
    .fetch_all(&mut *connection)
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
    .fetch_all(&mut *connection)
    .await?;

    Ok(catalog_fingerprint([
        ("columns", columns.as_slice()),
        ("constraints", constraints.as_slice()),
        ("indexes", indexes.as_slice()),
        ("views", view_definitions.as_slice()),
        ("functions", function_definitions.as_slice()),
        ("triggers", trigger_definitions.as_slice()),
    ]))
}

#[cfg(test)]
async fn canonical_catalog_fingerprint_for_pool(
    pool: &PgPool,
) -> Result<u64, SchemaConformanceError> {
    let mut connection = pool.acquire().await?;
    canonical_catalog_fingerprint(&mut connection).await
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
    version: u16,
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
        Err(contract_error(
            version,
            format!("missing {category}: {missing:?}"),
        ))
    }
}

fn contract_error(version: u16, detail: impl Into<String>) -> SchemaConformanceError {
    SchemaConformanceError::Contract {
        version,
        detail: detail.into(),
    }
}

#[cfg(test)]
mod tests;
