use std::error::Error;

use super::super::REQUIRED_FUNCTIONS;
use super::*;

/// The catalog fingerprint hashes each canonical function's `pg_proc` row,
/// which records argument and return types as type OIDs. Built-in type OIDs
/// are the same in every PostgreSQL 18 database. A type created at initdb or
/// later gets its OID per cluster or database, so one in a canonical
/// signature would give hosts different fingerprints.
#[tokio::test]
async fn canonical_function_signatures_use_only_built_in_types() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("sr_fp_types").await?;
    let result = async {
        let functions = REQUIRED_FUNCTIONS
            .iter()
            .map(|name| (*name).to_owned())
            .collect::<Vec<_>>();
        // PostgreSQL's source fixes every OID below 10000. Higher ones are
        // assigned when the server is built, at initdb, or afterwards.
        let signatures: Vec<(String, bool)> = checked_query_as!(
            r#"
            SELECT
                routine.oid::regprocedure::text AS signature,
                EXISTS (
                    SELECT 1
                    FROM unnest(
                        ARRAY[routine.prorettype, routine.provariadic]
                            || routine.proargtypes::oid[]
                            || COALESCE(routine.proallargtypes, '{}')
                            || COALESCE(routine.protrftypes, '{}')
                    ) AS signature_type (type_oid)
                    WHERE signature_type.type_oid >= 10000
                )
            FROM pg_catalog.pg_proc AS routine
            INNER JOIN pg_catalog.pg_namespace AS namespace
                ON namespace.oid = routine.pronamespace
            WHERE namespace.nspname = 'public'
                AND routine.proname = ANY($1)
            ORDER BY 1
            "#,
            &functions,
        )
        .fetch_all(&database.pool)
        .await?;
        assert!(
            signatures.len() >= REQUIRED_FUNCTIONS.len(),
            "every canonical function must be inspected"
        );
        let unstable = signatures
            .iter()
            .filter(|(_, uses_unstable_type)| *uses_unstable_type)
            .map(|(signature, _)| signature.as_str())
            .collect::<Vec<_>>();
        assert!(
            unstable.is_empty(),
            "canonical functions must use only source-assigned built-in types: {unstable:?}"
        );
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}
