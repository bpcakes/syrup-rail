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
/// The immutable version-3 fresh-install artifact.
pub const V3_INSTALL_SQL: &str = include_str!("../schema/v3/install.sql");
#[cfg(any(test, feature = "schema-contract-test-support"))]
/// The immutable version-4 fresh-install artifact.
pub const V4_INSTALL_SQL: &str = include_str!("../schema/v4/install.sql");
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
#[cfg(any(test, feature = "schema-contract-test-support"))]
/// The immutable forward-only version-2-to-version-3 upgrade artifact.
pub const V2_TO_V3_UPGRADE_SQL: &str = include_str!("../schema/v3/upgrade_from_v2.sql");
#[cfg(any(test, feature = "schema-contract-test-support"))]
/// The first, fast-lock version-3-to-version-4 preparation artifact.
pub const V3_TO_V4_PREPARE_SQL: &str = include_str!("../schema/v4/prepare_from_v3.sql");
#[cfg(any(test, feature = "schema-contract-test-support"))]
/// The separately committed, lower-lock version-3-to-version-4 validation artifact.
pub const V3_TO_V4_VALIDATE_SQL: &str = include_str!("../schema/v4/validate_from_v3.sql");
#[cfg(any(test, feature = "schema-contract-test-support"))]
/// The non-transactional concurrent index stage for the version-3-to-version-4 cutover.
pub const V3_TO_V4_INDEX_SQL: &str = include_str!("../schema/v4/index_from_v3.sql");
#[cfg(any(test, feature = "schema-contract-test-support"))]
/// The final version-3-to-version-4 cutover artifact.
pub const V3_TO_V4_UPGRADE_SQL: &str = include_str!("../schema/v4/upgrade_from_v3.sql");

// Non-cryptographic drift fingerprint over the canonical PostgreSQL catalog.
// Host objects use host-prefixed names and are deliberately excluded.
#[cfg(any(test, feature = "schema-contract-test-support"))]
const V1_CATALOG_FINGERPRINT: u64 = 0xc949_7313_2b48_83d9;
#[cfg(any(test, feature = "schema-contract-test-support"))]
const V2_CATALOG_FINGERPRINT: u64 = 0x373b_9c1c_8b27_5be0;
#[cfg(any(test, feature = "schema-contract-test-support"))]
const V3_CATALOG_FINGERPRINT: u64 = 0x475d_91d1_6525_a966;
const V4_CATALOG_FINGERPRINT: u64 = 0x0931_8e66_2d53_c5b6;
const CONCURRENT_REINDEX_SHADOW_INDEX_PATTERN: &str = r"_cc(new|old)[0-9]*$";
const REINDEX_TRANSITION_DETAIL: &str = "concurrent reindex state changed during schema validation";
const REINDEX_TRANSITION_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(25);

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

// Reader-facing indexes are semantic schema contracts, not incidental planner
// hints. Keep these complete shapes aligned with the owning Rust queries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum IndexKeyOrdering {
    AscNullsLast,
    DescNullsFirst,
}

impl IndexKeyOrdering {
    const fn catalog_label(self) -> &'static str {
        match self {
            Self::AscNullsLast => "ASC NULLS LAST",
            Self::DescNullsFirst => "DESC NULLS FIRST",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct IndexKeyContract {
    expression: &'static str,
    ordering: IndexKeyOrdering,
    opclass: &'static str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct IndexContract {
    purpose: &'static str,
    name: &'static str,
    table: &'static str,
    unique: bool,
    keys: &'static [IndexKeyContract],
    included_expressions: &'static [&'static str],
    predicate: Option<&'static str>,
}

const GATEWAY_ORDER_INDEX_CONTRACT: IndexContract = IndexContract {
    purpose: "gateway-order uniqueness",
    name: "billing_payment_attempts_gateway_order_idx",
    table: "billing_payment_attempts",
    unique: true,
    keys: &[
        IndexKeyContract {
            expression: "gateway_account_id",
            ordering: IndexKeyOrdering::AscNullsLast,
            opclass: "pg_catalog.uuid_ops",
        },
        IndexKeyContract {
            expression: "gateway_order_id",
            ordering: IndexKeyOrdering::AscNullsLast,
            opclass: "pg_catalog.text_ops",
        },
    ],
    included_expressions: &[],
    predicate: None,
};

const RENEWAL_DISPATCH_INDEX_CONTRACT: IndexContract = IndexContract {
    purpose: "renewal-dispatch keyset",
    name: "billing_subscriptions_due_idx",
    table: "billing_subscriptions",
    unique: false,
    keys: &[
        IndexKeyContract {
            expression: "next_payment_attempt_at",
            ordering: IndexKeyOrdering::AscNullsLast,
            opclass: "pg_catalog.timestamptz_ops",
        },
        IndexKeyContract {
            expression: "id",
            ordering: IndexKeyOrdering::AscNullsLast,
            opclass: "pg_catalog.uuid_ops",
        },
    ],
    included_expressions: &["billing_scope_id", "gateway_account_id", "next_renewal_at"],
    predicate: Some(
        "(status = ANY (ARRAY['active'::text, 'past_due'::text])) AND next_payment_attempt_at IS NOT NULL",
    ),
};

const V4_RENEWAL_DISPATCH_INDEX_CONTRACT: IndexContract = IndexContract {
    included_expressions: &[
        "billing_scope_id",
        "gateway_account_id",
        "next_renewal_at",
        "required_gateway_account_mode",
    ],
    ..RENEWAL_DISPATCH_INDEX_CONTRACT
};

const MODE_RENEWAL_DISPATCH_INDEX_CONTRACT: IndexContract = IndexContract {
    purpose: "mode-specific renewal-dispatch keyset",
    name: "billing_subscriptions_due_mode_idx",
    table: "billing_subscriptions",
    unique: false,
    keys: &[
        IndexKeyContract {
            expression: "required_gateway_account_mode",
            ordering: IndexKeyOrdering::AscNullsLast,
            opclass: "pg_catalog.text_ops",
        },
        IndexKeyContract {
            expression: "next_payment_attempt_at",
            ordering: IndexKeyOrdering::AscNullsLast,
            opclass: "pg_catalog.timestamptz_ops",
        },
        IndexKeyContract {
            expression: "id",
            ordering: IndexKeyOrdering::AscNullsLast,
            opclass: "pg_catalog.uuid_ops",
        },
    ],
    included_expressions: &["billing_scope_id", "gateway_account_id", "next_renewal_at"],
    predicate: Some(
        "(status = ANY (ARRAY['active'::text, 'past_due'::text])) AND next_payment_attempt_at IS NOT NULL",
    ),
};

const SUBSCRIPTION_HISTORY_INDEX_CONTRACT: IndexContract = IndexContract {
    purpose: "subscription-history keyset",
    name: "billing_payment_attempts_subscription_history_idx",
    table: "billing_payment_attempts",
    unique: false,
    keys: &[
        IndexKeyContract {
            expression: "billing_scope_id",
            ordering: IndexKeyOrdering::AscNullsLast,
            opclass: "pg_catalog.uuid_ops",
        },
        IndexKeyContract {
            expression: "subscriber_id",
            ordering: IndexKeyOrdering::AscNullsLast,
            opclass: "pg_catalog.uuid_ops",
        },
        IndexKeyContract {
            expression: "plan_key",
            ordering: IndexKeyOrdering::AscNullsLast,
            opclass: "pg_catalog.text_ops",
        },
        IndexKeyContract {
            expression: "created_at",
            ordering: IndexKeyOrdering::DescNullsFirst,
            opclass: "pg_catalog.timestamptz_ops",
        },
        IndexKeyContract {
            expression: "id",
            ordering: IndexKeyOrdering::DescNullsFirst,
            opclass: "pg_catalog.uuid_ops",
        },
    ],
    included_expressions: &[],
    predicate: Some("attempt_kind <> 'host_charge'::text"),
};

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

#[cfg(any(test, feature = "schema-contract-test-support"))]
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

#[cfg(any(test, feature = "schema-contract-test-support"))]
const V3_CURRENT_SUBSCRIPTION_COLUMNS: &[&str] = V2_CURRENT_SUBSCRIPTION_COLUMNS;
const V4_CURRENT_SUBSCRIPTION_COLUMNS: &[&str] = &[
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
    "required_gateway_account_mode",
];

/// Why a host database does not satisfy a canonical schema contract.
///
/// [`crate::assert_runtime_schema_v4_compatible`] reports version `4` in its
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

#[derive(Debug)]
enum SchemaConformanceAttemptError {
    Final(SchemaConformanceError),
    RetryableReindexTransition { fallback: SchemaConformanceError },
}

impl From<SchemaConformanceError> for SchemaConformanceAttemptError {
    fn from(error: SchemaConformanceError) -> Self {
        Self::Final(error)
    }
}

impl From<sqlx::Error> for SchemaConformanceAttemptError {
    fn from(error: sqlx::Error) -> Self {
        Self::Final(error.into())
    }
}

/// Asserts that a historical host database matches the canonical schema-v3
/// contract in tests or migration tooling built with the
/// `schema-contract-test-support` feature.
///
/// This function is not part of the ordinary production facade because the
/// 0.4 runtime requires schema v4. It does not install, upgrade, preflight,
/// audit, or otherwise mutate the schema. It runs the full canonical v3 catalog conformance
/// and fingerprint check used by the schema-contract tests in one
/// `REPEATABLE READ READ ONLY` PostgreSQL transaction. PostgreSQL major version
/// 18 is required; other majors are rejected before catalog comparison.
/// A concurrent-reindex transition mismatch is retried once in a fresh
/// transaction so a reindex that commits between catalog and live-operation
/// observations cannot cause a stale result. Other contract failures are not
/// retried.
///
/// Invalid `_ccnew` and `_ccold` shadows are tolerated only when the validating
/// role can see matching non-initializing `REINDEX CONCURRENTLY` details in
/// `pg_stat_progress_create_index` and the backend retains the expected table
/// and index locks. PostgreSQL hides those details from unrelated roles without
/// statistics privileges, for which this check deliberately fails closed.
#[cfg(any(test, feature = "schema-contract-test-support"))]
pub async fn assert_runtime_schema_v3_compatible(
    pool: &PgPool,
) -> Result<(), SchemaConformanceError> {
    assert_schema_conforms_in_read_only_snapshot(
        pool,
        3,
        V3_CURRENT_SUBSCRIPTION_COLUMNS,
        V3_CATALOG_FINGERPRINT,
    )
    .await
}

/// Asserts that a host database is compatible with the canonical schema-v4
/// contract before the host accepts billing work.
///
/// Call this after the host has applied its immutable Syrup Rail install or
/// forward-only upgrade migration through its normal migration deployment.
/// This function is read-only and requires PostgreSQL major version 18.
pub async fn assert_runtime_schema_v4_compatible(
    pool: &PgPool,
) -> Result<(), SchemaConformanceError> {
    assert_schema_conforms_in_read_only_snapshot(
        pool,
        4,
        V4_CURRENT_SUBSCRIPTION_COLUMNS,
        V4_CATALOG_FINGERPRINT,
    )
    .await
}

#[cfg(any(test, feature = "schema-contract-test-support"))]
async fn assert_runtime_schema_v2_compatible(pool: &PgPool) -> Result<(), SchemaConformanceError> {
    assert_schema_conforms_in_read_only_snapshot(
        pool,
        2,
        V2_CURRENT_SUBSCRIPTION_COLUMNS,
        V2_CATALOG_FINGERPRINT,
    )
    .await
}

#[cfg(any(test, feature = "schema-contract-test-support"))]
/// Asserts that an already-migrated host database contains the canonical v2
/// objects without exposing a production runtime migrator.
pub async fn assert_v2_conforms(pool: &PgPool) -> Result<(), SchemaConformanceError> {
    assert_runtime_schema_v2_compatible(pool).await
}

#[cfg(any(test, feature = "schema-contract-test-support"))]
/// Asserts that an already-migrated host database contains the canonical v3
/// objects without exposing a production runtime migrator.
pub async fn assert_v3_conforms(pool: &PgPool) -> Result<(), SchemaConformanceError> {
    assert_runtime_schema_v3_compatible(pool).await
}

#[cfg(any(test, feature = "schema-contract-test-support"))]
/// Asserts that an already-migrated host database contains the canonical v4
/// objects without exposing a production runtime migrator.
pub async fn assert_v4_conforms(pool: &PgPool) -> Result<(), SchemaConformanceError> {
    assert_runtime_schema_v4_compatible(pool).await
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

async fn assert_schema_conforms_in_read_only_snapshot(
    pool: &PgPool,
    version: u16,
    current_subscription_columns: &[&str],
    expected_fingerprint: u64,
) -> Result<(), SchemaConformanceError> {
    retry_reindex_transition_once(|| {
        assert_schema_conforms_in_one_read_only_snapshot(
            pool,
            version,
            current_subscription_columns,
            expected_fingerprint,
        )
    })
    .await
}

async fn retry_reindex_transition_once<F, Fut>(
    mut validate: F,
) -> Result<(), SchemaConformanceError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(), SchemaConformanceAttemptError>>,
{
    match validate().await {
        Ok(()) => return Ok(()),
        Err(SchemaConformanceAttemptError::Final(error)) => return Err(error),
        Err(SchemaConformanceAttemptError::RetryableReindexTransition { .. }) => {}
    }
    tokio::time::sleep(REINDEX_TRANSITION_RETRY_DELAY).await;
    match validate().await {
        Ok(()) => Ok(()),
        Err(SchemaConformanceAttemptError::Final(error)) => Err(error),
        Err(SchemaConformanceAttemptError::RetryableReindexTransition { fallback }) => {
            Err(fallback)
        }
    }
}

async fn assert_schema_conforms_in_one_read_only_snapshot(
    pool: &PgPool,
    version: u16,
    current_subscription_columns: &[&str],
    expected_fingerprint: u64,
) -> Result<(), SchemaConformanceAttemptError> {
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
) -> Result<(), SchemaConformanceAttemptError> {
    let billing_indexes = load_billing_index_catalog(connection).await?;
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
    require_ready_canonical_indexes(version, &billing_indexes)?;
    require_index_contract(connection, version, GATEWAY_ORDER_INDEX_CONTRACT).await?;
    if version >= 2 {
        let renewal_dispatch_contract = if version >= 4 {
            V4_RENEWAL_DISPATCH_INDEX_CONTRACT
        } else {
            RENEWAL_DISPATCH_INDEX_CONTRACT
        };
        require_index_contract(connection, version, renewal_dispatch_contract).await?;
        require_index_contract(connection, version, SUBSCRIPTION_HISTORY_INDEX_CONTRACT).await?;
    }
    if version >= 4 {
        require_index_contract(connection, version, MODE_RENEWAL_DISPATCH_INDEX_CONTRACT).await?;
    }
    require_catalog_fingerprint(connection, version, expected_fingerprint, &billing_indexes)
        .await?;
    require_unchanged_active_reindex_shadows(connection, version, &billing_indexes).await
}

async fn require_unchanged_active_reindex_shadows(
    connection: &mut PgConnection,
    version: u16,
    initial_indexes: &[BillingIndexCatalogEntry],
) -> Result<(), SchemaConformanceAttemptError> {
    let initial_active_reindex_shadows = active_reindex_shadows(initial_indexes);
    if initial_active_reindex_shadows.is_empty() {
        return Ok(());
    }
    let rechecked_indexes = load_billing_index_catalog(connection).await?;
    if initial_active_reindex_shadows == active_reindex_shadows(&rechecked_indexes) {
        Ok(())
    } else {
        Err(SchemaConformanceAttemptError::RetryableReindexTransition {
            fallback: contract_error(version, REINDEX_TRANSITION_DETAIL),
        })
    }
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

fn require_ready_canonical_indexes(
    version: u16,
    billing_indexes: &[BillingIndexCatalogEntry],
) -> Result<(), SchemaConformanceAttemptError> {
    let unavailable = billing_indexes
        .iter()
        .filter(|index| {
            index.active_concurrent_reindex_pid.is_none()
                && !(index.is_valid && index.is_ready && index.is_live)
        })
        .collect::<Vec<_>>();
    if unavailable.is_empty() {
        return Ok(());
    }
    let retryable_reindex_transition = unavailable
        .iter()
        .all(|index| !index.is_valid && has_concurrent_reindex_shadow_suffix(&index.index_name));
    let unavailable_detail = unavailable
        .iter()
        .map(|index| {
            (
                index.table_name.clone(),
                index.index_name.clone(),
                index.is_valid,
                index.is_ready,
                index.is_live,
            )
        })
        .collect::<Vec<_>>();
    let fallback = contract_error(
        version,
        format!(
            "canonical indexes are not planner/write ready (table, index, valid, ready, live): {unavailable_detail:?}; invalid _ccnew/_ccold indexes are tolerated only while matching REINDEX CONCURRENTLY progress is visible to the validating role, and stale shadows left by failed maintenance must be dropped"
        ),
    );
    if retryable_reindex_transition {
        Err(SchemaConformanceAttemptError::RetryableReindexTransition { fallback })
    } else {
        Err(fallback.into())
    }
}

fn has_concurrent_reindex_shadow_suffix(index_name: &str) -> bool {
    ["_ccnew", "_ccold"].iter().any(|marker| {
        index_name
            .rsplit_once(marker)
            .is_some_and(|(base, counter)| {
                !base.is_empty() && counter.bytes().all(|byte| byte.is_ascii_digit())
            })
    })
}

#[derive(Clone, Debug, sqlx::FromRow)]
struct BillingIndexCatalogEntry {
    table_name: String,
    index_name: String,
    definition: String,
    is_valid: bool,
    is_ready: bool,
    is_live: bool,
    active_concurrent_reindex_pid: Option<i32>,
}

fn active_reindex_shadows(indexes: &[BillingIndexCatalogEntry]) -> Vec<(&str, &str, i32)> {
    indexes
        .iter()
        .filter_map(|index| {
            index
                .active_concurrent_reindex_pid
                .map(|pid| (index.table_name.as_str(), index.index_name.as_str(), pid))
        })
        .collect()
}

async fn load_billing_index_catalog(
    connection: &mut PgConnection,
) -> Result<Vec<BillingIndexCatalogEntry>, sqlx::Error> {
    let tables = REQUIRED_TABLES
        .iter()
        .map(|name| (*name).to_owned())
        .collect::<Vec<_>>();
    sqlx::query_as::<_, BillingIndexCatalogEntry>(
        r#"
        SELECT
            table_relation.relname AS table_name,
            index_relation.relname AS index_name,
            concat_ws(
                '|',
                table_relation.relname,
                index_relation.relname,
                pg_catalog.pg_get_indexdef(index_relation.oid)
            ) AS definition,
            catalog_index.indisvalid AS is_valid,
            catalog_index.indisready AS is_ready,
            catalog_index.indislive AS is_live,
            CASE
                WHEN NOT catalog_index.indisvalid
                    AND index_relation.relname ~ $2
                THEN (
                    SELECT shadow_lock.pid
                    FROM pg_catalog.pg_class AS canonical_index_relation
                    INNER JOIN pg_catalog.pg_index AS canonical_index
                        ON canonical_index.indexrelid = canonical_index_relation.oid
                    INNER JOIN pg_catalog.pg_locks AS shadow_lock
                        ON shadow_lock.locktype = 'relation'
                        AND shadow_lock.relation = index_relation.oid
                        AND shadow_lock.database = (
                            SELECT database.oid
                            FROM pg_catalog.pg_database AS database
                            WHERE database.datname = current_database()
                        )
                        AND shadow_lock.mode = 'ShareUpdateExclusiveLock'
                        AND shadow_lock.granted
                    INNER JOIN pg_catalog.pg_stat_progress_create_index AS progress
                        ON progress.pid = shadow_lock.pid
                        AND progress.datid = shadow_lock.database
                        AND progress.relid = table_relation.oid
                        AND (
                            -- PostgreSQL 18 reports the transient index before
                            -- the swap and the new canonical index afterward.
                            progress.index_relid IN (
                                index_relation.oid,
                                canonical_index_relation.oid
                            )
                            -- Table-wide reindex reports only its current
                            -- index while retaining session locks for every
                            -- index it is rebuilding on this table.
                            OR 1 < (
                                SELECT count(*)
                                FROM pg_catalog.pg_index AS scope_index
                                INNER JOIN pg_catalog.pg_locks AS scope_lock
                                    ON scope_lock.pid = shadow_lock.pid
                                    AND scope_lock.locktype = 'relation'
                                    AND scope_lock.database = shadow_lock.database
                                    AND scope_lock.relation = scope_index.indexrelid
                                    AND scope_lock.mode = 'ShareUpdateExclusiveLock'
                                    AND scope_lock.granted
                                WHERE scope_index.indrelid = table_relation.oid
                                    AND scope_index.indisvalid
                                    AND scope_index.indisready
                                    AND scope_index.indislive
                            )
                        )
                        AND progress.command = 'REINDEX CONCURRENTLY'
                        -- Table-wide reindex gathering locks skipped invalid
                        -- indexes only before progress leaves initialization.
                        AND progress.phase <> 'initializing'
                    INNER JOIN pg_catalog.pg_locks AS canonical_index_lock
                        ON canonical_index_lock.pid = shadow_lock.pid
                        AND canonical_index_lock.locktype = 'relation'
                        AND canonical_index_lock.database = shadow_lock.database
                        AND canonical_index_lock.relation = canonical_index_relation.oid
                        AND canonical_index_lock.mode = 'ShareUpdateExclusiveLock'
                        AND canonical_index_lock.granted
                    INNER JOIN pg_catalog.pg_locks AS table_lock
                        ON table_lock.pid = shadow_lock.pid
                        AND table_lock.locktype = 'relation'
                        AND table_lock.database = shadow_lock.database
                        AND table_lock.relation = table_relation.oid
                        AND table_lock.mode = 'ShareUpdateExclusiveLock'
                        AND table_lock.granted
                    WHERE canonical_index.indrelid = table_relation.oid
                        AND canonical_index.indisvalid
                        AND canonical_index.indisready
                        AND canonical_index.indislive
                        AND canonical_index_relation.relname !~ $2
                        AND pg_catalog.starts_with(
                            canonical_index_relation.relname,
                            pg_catalog.regexp_replace(
                                index_relation.relname,
                                $2,
                                ''
                            )
                        )
                    ORDER BY canonical_index_relation.oid
                    LIMIT 1
                )
            END AS active_concurrent_reindex_pid
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
    .bind(CONCURRENT_REINDEX_SHADOW_INDEX_PATTERN)
    .fetch_all(&mut *connection)
    .await
}

async fn require_index_contract(
    connection: &mut PgConnection,
    version: u16,
    contract: IndexContract,
) -> Result<(), SchemaConformanceError> {
    let actual = catalog_index_shape(connection, contract.name).await?;
    validate_index_contract(version, contract, actual.as_ref())
}

fn validate_index_contract(
    version: u16,
    contract: IndexContract,
    actual: Option<&CatalogIndexShape>,
) -> Result<(), SchemaConformanceError> {
    let Some(actual) = actual else {
        return Err(index_contract_error(version, contract, "is missing"));
    };
    let expected_key_expressions = contract
        .keys
        .iter()
        .map(|key| key.expression.to_owned())
        .collect::<Vec<_>>();
    let expected_key_orderings = contract
        .keys
        .iter()
        .map(|key| key.ordering.catalog_label().to_owned())
        .collect::<Vec<_>>();
    let expected_key_opclasses = contract
        .keys
        .iter()
        .map(|key| key.opclass.to_owned())
        .collect::<Vec<_>>();
    let expected_included_expressions = contract
        .included_expressions
        .iter()
        .map(|expression| (*expression).to_owned())
        .collect::<Vec<_>>();

    if actual.table_name != contract.table {
        return Err(index_contract_error(
            version,
            contract,
            format!(
                "belongs to table {:?}; expected {:?}",
                actual.table_name, contract.table
            ),
        ));
    }
    if actual.access_method != "btree" {
        return Err(index_contract_error(
            version,
            contract,
            format!(
                "uses access method {:?}; expected \"btree\"",
                actual.access_method
            ),
        ));
    }
    if actual.is_unique != contract.unique {
        return Err(index_contract_error(
            version,
            contract,
            format!(
                "has unique={}; expected unique={}",
                actual.is_unique, contract.unique
            ),
        ));
    }
    if actual.key_expressions != expected_key_expressions {
        return Err(index_contract_error(
            version,
            contract,
            format!(
                "has key expressions {:?}; expected {expected_key_expressions:?}",
                actual.key_expressions
            ),
        ));
    }
    if actual.key_orderings != expected_key_orderings {
        return Err(index_contract_error(
            version,
            contract,
            format!(
                "has key ordering {:?}; expected {expected_key_orderings:?}",
                actual.key_orderings
            ),
        ));
    }
    if actual.key_opclasses != expected_key_opclasses {
        return Err(index_contract_error(
            version,
            contract,
            format!(
                "has key operator classes {:?}; expected {expected_key_opclasses:?}",
                actual.key_opclasses
            ),
        ));
    }
    if actual.included_expressions != expected_included_expressions {
        return Err(index_contract_error(
            version,
            contract,
            format!(
                "has included expressions {:?}; expected {expected_included_expressions:?}",
                actual.included_expressions
            ),
        ));
    }
    if actual.predicate.as_deref() != contract.predicate {
        return Err(index_contract_error(
            version,
            contract,
            format!(
                "has predicate {:?}; expected {:?}",
                actual.predicate, contract.predicate
            ),
        ));
    }
    if !actual.is_valid || !actual.is_ready || !actual.is_live {
        return Err(index_contract_error(
            version,
            contract,
            format!(
                "is not planner/write ready (valid={}, ready={}, live={})",
                actual.is_valid, actual.is_ready, actual.is_live
            ),
        ));
    }
    Ok(())
}

fn index_contract_error(
    version: u16,
    contract: IndexContract,
    detail: impl std::fmt::Display,
) -> SchemaConformanceError {
    contract_error(
        version,
        format!("{} index {} {detail}", contract.purpose, contract.name),
    )
}

#[derive(Clone, Debug, sqlx::FromRow)]
struct CatalogIndexShape {
    table_name: String,
    access_method: String,
    key_expressions: Vec<String>,
    key_orderings: Vec<String>,
    key_opclasses: Vec<String>,
    included_expressions: Vec<String>,
    predicate: Option<String>,
    is_unique: bool,
    is_valid: bool,
    is_ready: bool,
    is_live: bool,
}

async fn catalog_index_shape(
    connection: &mut PgConnection,
    index_name: &str,
) -> Result<Option<CatalogIndexShape>, sqlx::Error> {
    sqlx::query_as::<_, CatalogIndexShape>(
        r#"
        SELECT
        table_relation.relname AS table_name,
        access_method.amname AS access_method,
        ARRAY(
            SELECT pg_catalog.pg_get_indexdef(
                catalog_index.indexrelid,
                key_position.position,
                true
            )
            FROM generate_series(
                1,
                catalog_index.indnkeyatts
            ) AS key_position(position)
            ORDER BY key_position.position
        ) AS key_expressions,
        ARRAY(
            SELECT concat(
                CASE WHEN pg_catalog.pg_index_column_has_property(
                    catalog_index.indexrelid,
                    key_position.position,
                    'desc'
                ) THEN 'DESC' ELSE 'ASC' END,
                CASE WHEN pg_catalog.pg_index_column_has_property(
                    catalog_index.indexrelid,
                    key_position.position,
                    'nulls_first'
                ) THEN ' NULLS FIRST' ELSE ' NULLS LAST' END
            )
            FROM generate_series(
                1,
                catalog_index.indnkeyatts
            ) AS key_position(position)
            ORDER BY key_position.position
        ) AS key_orderings,
        ARRAY(
            SELECT concat(operator_class_namespace.nspname, '.', operator_class.opcname)
            FROM unnest(catalog_index.indclass::oid[]) WITH ORDINALITY
                AS key_operator_class(operator_class_oid, position)
            INNER JOIN pg_catalog.pg_opclass AS operator_class
                ON operator_class.oid = key_operator_class.operator_class_oid
            INNER JOIN pg_catalog.pg_namespace AS operator_class_namespace
                ON operator_class_namespace.oid = operator_class.opcnamespace
            WHERE key_operator_class.position <= catalog_index.indnkeyatts
            ORDER BY key_operator_class.position
        ) AS key_opclasses,
        ARRAY(
            SELECT pg_catalog.pg_get_indexdef(
                catalog_index.indexrelid,
                included_position.position,
                true
            )
            FROM generate_series(
                catalog_index.indnkeyatts::integer + 1,
                catalog_index.indnatts::integer
            ) AS included_position(position)
            ORDER BY included_position.position
        ) AS included_expressions,
        pg_catalog.pg_get_expr(
            catalog_index.indpred,
            catalog_index.indrelid,
            true
        ) AS predicate,
        catalog_index.indisunique AS is_unique,
        catalog_index.indisvalid AS is_valid,
        catalog_index.indisready AS is_ready,
        catalog_index.indislive AS is_live
        FROM pg_catalog.pg_class AS index_relation
        INNER JOIN pg_catalog.pg_index AS catalog_index
            ON catalog_index.indexrelid = index_relation.oid
        INNER JOIN pg_catalog.pg_class AS table_relation
            ON table_relation.oid = catalog_index.indrelid
        INNER JOIN pg_catalog.pg_namespace AS index_namespace
            ON index_namespace.oid = index_relation.relnamespace
        INNER JOIN pg_catalog.pg_namespace AS table_namespace
            ON table_namespace.oid = table_relation.relnamespace
        INNER JOIN pg_catalog.pg_am AS access_method
            ON access_method.oid = index_relation.relam
        WHERE index_namespace.nspname = 'public'
            AND table_namespace.nspname = 'public'
            AND index_relation.relkind = 'i'
            AND index_relation.relname = $1
        "#,
    )
    .bind(index_name)
    .fetch_optional(&mut *connection)
    .await
}

async fn require_catalog_fingerprint(
    connection: &mut PgConnection,
    version: u16,
    expected: u64,
    billing_indexes: &[BillingIndexCatalogEntry],
) -> Result<(), SchemaConformanceError> {
    let actual = canonical_catalog_fingerprint(connection, billing_indexes).await?;
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
    billing_indexes: &[BillingIndexCatalogEntry],
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
    let indexes = billing_indexes
        .iter()
        .filter(|index| index.active_concurrent_reindex_pid.is_none())
        .map(|index| index.definition.clone())
        .collect::<Vec<_>>();
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
    let billing_indexes = load_billing_index_catalog(&mut connection).await?;
    canonical_catalog_fingerprint(&mut connection, &billing_indexes).await
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
