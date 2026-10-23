// Observer-role SQL is built only from the generated fixture role name.
use super::*;

pub(super) async fn drop_schema_observer_role(
    pool: &PgPool,
    role: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(AssertSqlSafe(format!("DROP OWNED BY {role}")))
        .execute(pool)
        .await?;
    sqlx::query(AssertSqlSafe(format!("DROP ROLE {role}")))
        .execute(pool)
        .await?;
    Ok(())
}

pub(super) async fn require_hidden_reindex_to_fail_closed(
    pool: &PgPool,
    observer_role: &str,
    expected_shadow: &str,
) -> Result<(), Box<dyn Error>> {
    let mut transaction = pool
        .begin_with("BEGIN TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .await?;
    sqlx::query(AssertSqlSafe(format!("SET LOCAL ROLE {observer_role}")))
        .execute(&mut *transaction)
        .await?;
    let result = assert_schema_conforms(
        &mut transaction,
        2,
        V2_CURRENT_SUBSCRIPTION_COLUMNS,
        V2_CATALOG_FINGERPRINT,
    )
    .await;
    transaction.rollback().await?;

    match result {
        Err(SchemaConformanceAttemptError::RetryableReindexTransition {
            fallback: crate::SchemaConformanceError::Contract { detail, .. },
        }) if detail.contains("not planner/write ready") && detail.contains(expected_shadow) => {
            Ok(())
        }
        Err(error) => Err(io::Error::other(format!(
            "expected hidden reindex progress to fail closed, got {error:?}"
        ))
        .into()),
        Ok(()) => Err(io::Error::other(
            "schema validation accepted a reindex whose progress details were hidden",
        )
        .into()),
    }
}
