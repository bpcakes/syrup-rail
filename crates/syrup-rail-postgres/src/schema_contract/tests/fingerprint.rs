use std::error::Error;

use super::super::REQUIRED_FUNCTIONS;
use super::*;

/// The catalog fingerprint hashes each canonical function's `pg_proc` row,
/// which records argument and return types as type OIDs. Built-in type OIDs
/// are the same in every PostgreSQL 18 database. A user-defined type gets its
/// OID per database, so one in a canonical signature would give every host a
/// different fingerprint.
#[tokio::test]
async fn canonical_function_signatures_use_only_built_in_types() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("sr_fp_types").await?;
    let result = async {
        let functions = REQUIRED_FUNCTIONS
            .iter()
            .map(|name| (*name).to_owned())
            .collect::<Vec<_>>();
        // 16384 is PostgreSQL's first OID for objects created after initdb.
        let signatures: Vec<(String, bool)> = sqlx::query_as(
            r#"
            SELECT
                routine.oid::regprocedure::text,
                EXISTS (
                    SELECT 1
                    FROM unnest(
                        ARRAY[routine.prorettype]
                            || routine.proargtypes::oid[]
                            || COALESCE(routine.proallargtypes, '{}')
                    ) AS signature_type (type_oid)
                    WHERE signature_type.type_oid >= 16384
                )
            FROM pg_catalog.pg_proc AS routine
            INNER JOIN pg_catalog.pg_namespace AS namespace
                ON namespace.oid = routine.pronamespace
            WHERE namespace.nspname = 'public'
                AND routine.proname = ANY($1)
            ORDER BY 1
            "#,
        )
        .bind(&functions)
        .fetch_all(&database.pool)
        .await?;
        assert!(
            signatures.len() >= REQUIRED_FUNCTIONS.len(),
            "every canonical function must be inspected"
        );
        let user_typed = signatures
            .iter()
            .filter(|(_, uses_user_type)| *uses_user_type)
            .map(|(signature, _)| signature.as_str())
            .collect::<Vec<_>>();
        assert!(
            user_typed.is_empty(),
            "canonical functions must use built-in types: {user_typed:?}"
        );
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}
