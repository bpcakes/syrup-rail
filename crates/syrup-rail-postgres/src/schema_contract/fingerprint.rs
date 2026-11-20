use sqlx::PgConnection;

use super::{
    BillingIndexCatalogEntry, REQUIRED_FUNCTIONS, REQUIRED_TABLES, REQUIRED_TRIGGERS,
    REQUIRED_VIEWS, SchemaConformanceError, contract_error,
};
#[cfg(test)]
use super::{PgPool, load_billing_index_catalog};

pub(super) async fn require_catalog_fingerprint(
    connection: &mut PgConnection,
    version: u16,
    expected: &[u64],
    billing_indexes: &[BillingIndexCatalogEntry],
) -> Result<(), SchemaConformanceError> {
    let actual = canonical_catalog_fingerprint(connection, billing_indexes).await?;
    if expected.contains(&actual) {
        Ok(())
    } else {
        let expected = expected
            .iter()
            .map(|fingerprint| format!("{fingerprint:#018x}"))
            .collect::<Vec<_>>()
            .join(" or ");
        Err(contract_error(
            version,
            format!(
                "canonical catalog fingerprint differs: expected {expected}, found {actual:#018x}"
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
    // Read the MVCC-visible pg_proc tuple directly rather than calling
    // pg_get_functiondef(oid), which consults SysCache (latest committed
    // catalog) and can tear a REPEATABLE READ snapshot across an online
    // function replacement. Ownership and ACLs are host-owned and excluded.
    // The row records argument and return types as type OIDs, which are the
    // same in every database only for built-in types; a test keeps canonical
    // function signatures to those.
    let function_definitions = sqlx::query_scalar::<_, String>(
        r#"
        SELECT jsonb_build_array(
            namespace.nspname,
            language.lanname,
            pg_catalog.pg_get_expr(routine.proargdefaults, 0),
            to_jsonb(routine) - ARRAY[
                'oid', 'proowner', 'proacl', 'pronamespace', 'prolang', 'proargdefaults'
            ]
        )::text
        FROM pg_catalog.pg_proc AS routine
        INNER JOIN pg_catalog.pg_namespace AS namespace
            ON namespace.oid = routine.pronamespace
        INNER JOIN pg_catalog.pg_language AS language
            ON language.oid = routine.prolang
        WHERE namespace.nspname = 'public'
            AND routine.prokind = 'f'
            AND routine.proname = ANY($1)
        ORDER BY routine.proname, routine.proargtypes
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
pub(super) async fn canonical_catalog_fingerprint_for_pool(
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
