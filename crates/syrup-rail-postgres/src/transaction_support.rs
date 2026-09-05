use sqlx::PgConnection;

/// Recognizes transient database failures after the caller establishes a safe
/// retry boundary. This does not classify pool errors or authorize provider I/O.
pub(crate) fn is_transient_sqlstate(code: &str) -> bool {
    matches!(code, "40001" | "40P01" | "55P03" | "57014")
}

/// Sets both bounds inside the caller's transaction; policy values stay with the workflow.
pub(crate) async fn set_local_timeouts(
    connection: &mut PgConnection,
    lock_timeout: &str,
    statement_timeout: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "SELECT set_config('lock_timeout', $1, true), set_config('statement_timeout', $2, true)",
    )
    .bind(lock_timeout)
    .bind(statement_timeout)
    .execute(connection)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestDatabase;
    use sqlx::Acquire;

    #[tokio::test]
    async fn timeout_settings_are_local_to_commit_and_rollback()
    -> Result<(), Box<dyn std::error::Error>> {
        let database = TestDatabase::start("sr_timeouts").await?;
        let result = async {
            let mut connection = database.pool.acquire().await?;
            let query =
                "SELECT current_setting('lock_timeout'), current_setting('statement_timeout')";
            let before: (String, String) =
                sqlx::query_as(query).fetch_one(&mut *connection).await?;
            for commit in [false, true] {
                let mut transaction = connection.begin().await?;
                set_local_timeouts(&mut transaction, "123ms", "456ms").await?;
                let during: (String, String) =
                    sqlx::query_as(query).fetch_one(&mut *transaction).await?;
                assert_eq!(during, ("123ms".to_owned(), "456ms".to_owned()));
                if commit {
                    transaction.commit().await?;
                } else {
                    transaction.rollback().await?;
                }
                let after: (String, String) =
                    sqlx::query_as(query).fetch_one(&mut *connection).await?;
                assert_eq!(after, before);
            }
            Ok::<_, Box<dyn std::error::Error>>(())
        }
        .await;
        let cleanup = database.cleanup().await;
        result?;
        cleanup
    }
}
