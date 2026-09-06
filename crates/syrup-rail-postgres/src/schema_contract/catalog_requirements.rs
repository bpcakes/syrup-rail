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
