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
/// The version-5 fresh-install artifact.
pub const V5_INSTALL_SQL: &str = include_str!("../schema/v5/install.sql");
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
#[cfg(any(test, feature = "schema-contract-test-support"))]
/// The read-only version-4-to-version-5 upgrade preflight.
pub const V4_TO_V5_PREFLIGHT_SQL: &str = include_str!("../schema/v5/preflight_from_v4.sql");
#[cfg(any(test, feature = "schema-contract-test-support"))]
/// The read-only audit of incompatible v4 external-reversal attestations.
pub const V4_TO_V5_INCOMPATIBLE_ATTESTATION_AUDIT_SQL: &str =
    include_str!("../schema/v5/audit_incompatible_attestations_from_v4.sql");
#[cfg(any(test, feature = "schema-contract-test-support"))]
/// The forward-only version-4-to-version-5 upgrade artifact.
pub const V4_TO_V5_UPGRADE_SQL: &str = include_str!("../schema/v5/upgrade_from_v4.sql");

#[cfg(any(test, feature = "schema-contract-test-support"))]
/// Complete current install artifact, for host migration packaging and tests.
pub const V6_INSTALL_SQL: &str = include_str!("../schema/v6/install.sql");
#[cfg(any(test, feature = "schema-contract-test-support"))]
/// Read-only v5-to-v6 preflight for sizing retained evidence classification.
pub const V5_TO_V6_PREFLIGHT_SQL: &str = include_str!("../schema/v6/preflight_from_v5.sql");
#[cfg(any(test, feature = "schema-contract-test-support"))]
/// Read-only audit of review-required v5 attempts that remain unclassified.
pub const V5_TO_V6_UNCLASSIFIED_REVIEW_AUDIT_SQL: &str =
    include_str!("../schema/v6/audit_unclassified_review_attempts_from_v5.sql");
#[cfg(any(test, feature = "schema-contract-test-support"))]
/// Forward-only v5-to-v6 upgrade; host applications own migration execution.
pub const V5_TO_V6_UPGRADE_SQL: &str = include_str!("../schema/v6/upgrade_from_v5.sql");

// Non-cryptographic drift fingerprint over the canonical PostgreSQL catalog.
// Host objects use host-prefixed names and are deliberately excluded.
#[cfg(any(test, feature = "schema-contract-test-support"))]
const V1_CATALOG_FINGERPRINT: u64 = 0xc949_7313_2b48_83d9;
#[cfg(any(test, feature = "schema-contract-test-support"))]
const V2_CATALOG_FINGERPRINT: u64 = 0x373b_9c1c_8b27_5be0;
#[cfg(any(test, feature = "schema-contract-test-support"))]
const V3_CATALOG_FINGERPRINT: u64 = 0x475d_91d1_6525_a966;
#[cfg(any(test, feature = "schema-contract-test-support"))]
const V4_CATALOG_FINGERPRINT: u64 = 0x0931_8e66_2d53_c5b6;
const V5_CATALOG_FINGERPRINT: u64 = 0xa565_eddd_a93b_3368;
const V6_CATALOG_FINGERPRINT: u64 = 0x37fe_b100_42e9_893a;
const CONCURRENT_REINDEX_SHADOW_INDEX_PATTERN: &str = r"_cc(new|old)[0-9]*$";
const REINDEX_TRANSITION_DETAIL: &str = "concurrent reindex state changed during schema validation";
pub(crate) const INCOMPATIBLE_EXTERNAL_REVERSAL_DETAIL: &str =
    "external reversal attestations contain an incompatible resolution tuple";
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
const V5_CURRENT_SUBSCRIPTION_COLUMNS: &[&str] = V4_CURRENT_SUBSCRIPTION_COLUMNS;

/// Why a host database does not satisfy a canonical schema contract.
///
/// Runtime compatibility assertions report their schema version in the
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

/// Asserts that a host database still on schema v3 is compatible while the host
/// stages its forward-only cutovers.
///
/// This retained v3 assertion supports only migration tooling built with the
/// `schema-contract-test-support` feature.
/// Call it after the host has applied its immutable schema-v3 install or
/// forward-only upgrade migration through its normal migration deployment.
/// This function does not install, upgrade, audit, or otherwise mutate the
/// database. It runs the same full canonical v3 catalog conformance and
/// fingerprint check used by the schema-contract tests, then verifies that
/// every live external-reversal attestation can be represented by the typed
/// runtime model. Both checks share one `REPEATABLE READ READ ONLY` PostgreSQL
/// transaction. PostgreSQL major version 18 is required; other majors are
/// rejected before catalog comparison.
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

/// Asserts that a historical host database matches the canonical schema-v4
/// contract in tests or migration tooling.
///
/// Call this after the host has applied its immutable Syrup Rail install or
/// forward-only upgrade migration through its normal migration deployment.
/// This function is read-only and requires PostgreSQL major version 18.
#[cfg(any(test, feature = "schema-contract-test-support"))]
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

/// Asserts the complete canonical schema-v6 catalog before accepting billing work.
/// The host must first apply its immutable install or v5-to-v6 upgrade. Read-only;
/// requires PostgreSQL 18 and never installs or migrates a database.
pub async fn assert_runtime_schema_v6_compatible(
    pool: &PgPool,
) -> Result<(), SchemaConformanceError> {
    assert_schema_conforms_in_read_only_snapshot(
        pool,
        6,
        V5_CURRENT_SUBSCRIPTION_COLUMNS,
        V6_CATALOG_FINGERPRINT,
    )
    .await
}

/// Checks the historical schema-v5 catalog while preparing a v6 cutover.
///
/// This is read-only and requires PostgreSQL 18. Success validates the old side
/// of the migration only; current billing queries require schema v6 and
/// [`assert_runtime_schema_v6_compatible`].
pub async fn assert_runtime_schema_v5_compatible(
    pool: &PgPool,
) -> Result<(), SchemaConformanceError> {
    assert_schema_conforms_in_read_only_snapshot(
        pool,
        5,
        V5_CURRENT_SUBSCRIPTION_COLUMNS,
        V5_CATALOG_FINGERPRINT,
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
/// Asserts that an already-migrated host database contains the canonical v5
/// objects without exposing a production runtime migrator.
pub async fn assert_v5_conforms(pool: &PgPool) -> Result<(), SchemaConformanceError> {
    assert_runtime_schema_v5_compatible(pool).await
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
    // Shipped v3 and v4 cannot express the typed tuple matrix in their
    // constraints, so their compatibility APIs retain the live-row preflight.
    // V5 validates the invariant during cutover and fingerprints the
    // replacement constraint.
    if matches!(version, 3 | 4) {
        require_compatible_external_reversal_attestations(connection, version).await?;
    }
    require_unchanged_active_reindex_shadows(connection, version, &billing_indexes).await
}

async fn require_compatible_external_reversal_attestations(
    connection: &mut PgConnection,
    version: u16,
) -> Result<(), SchemaConformanceError> {
    let incompatible_tuple_exists = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM billing_external_reversal_attestations
            WHERE NOT (
                (
                    prior_resolution_code = 'subscription_initial_current_grant_conflict'
                    AND (
                        (
                            reversal_kind = 'refund'
                            AND final_resolution_code =
                                'subscription_initial_externally_refunded'
                        )
                        OR (
                            reversal_kind = 'void'
                            AND final_resolution_code =
                                'subscription_initial_externally_voided'
                        )
                    )
                )
                OR (
                    prior_resolution_code = 'processor_charge_external_reversal_required'
                    AND (
                        (
                            reversal_kind = 'refund'
                            AND final_resolution_code IN (
                                'subscription_initial_externally_refunded',
                                'processor_charge_externally_refunded'
                            )
                        )
                        OR (
                            reversal_kind = 'void'
                            AND final_resolution_code IN (
                                'subscription_initial_externally_voided',
                                'processor_charge_externally_voided'
                            )
                        )
                    )
                )
            )
        )
        "#,
    )
    .fetch_one(&mut *connection)
    .await?;
    if incompatible_tuple_exists {
        Err(contract_error(
            version,
            INCOMPATIBLE_EXTERNAL_REVERSAL_DETAIL,
        ))
    } else {
        Ok(())
    }
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

include!("schema_contract/catalog_requirements.rs");
include!("schema_contract/catalog_fingerprint.rs");
