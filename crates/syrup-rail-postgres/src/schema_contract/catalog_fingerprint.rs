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
