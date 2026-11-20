use sqlx::PgPool;
use uuid::Uuid;

use crate::test_support::fixture_query_scalar;

pub(super) async fn table_columns(pool: &PgPool, table: &str) -> Result<Vec<String>, sqlx::Error> {
    checked_query_scalar!(
        r#"
            SELECT column_name::text
            FROM information_schema.columns
            WHERE table_schema = 'public' AND table_name = $1
            "#,
        table,
    )
    .fetch_all(pool)
    .await
}

/// Serializes only the listed columns of one fixed billing row.
pub(super) async fn projection(
    pool: &PgPool,
    (table, id): (&str, Uuid),
    columns: &[&str],
) -> Result<String, sqlx::Error> {
    let query = format!(
        r#"
            SELECT COALESCE(jsonb_object_agg(fields.key, fields.value), '{{}}'::jsonb)::text
            FROM public.{} AS rows, jsonb_each(to_jsonb(rows)) AS fields
            WHERE rows.id = $1 AND fields.key = ANY($2)
            "#,
        fixed_table(table)
    );
    fixture_query_scalar(sqlx::AssertSqlSafe(query.as_str()))
        .bind(id)
        .bind(columns)
        .fetch_one(pool)
        .await
}

/// Counts the listed columns of one fixed billing row that are not NULL.
pub(super) async fn nonnull_count(
    pool: &PgPool,
    (table, id): (&str, Uuid),
    columns: &[&str],
) -> Result<i64, sqlx::Error> {
    let query = format!(
        r#"
            SELECT count(*)
            FROM public.{} AS rows, jsonb_each(to_jsonb(rows)) AS fields
            WHERE rows.id = $1 AND fields.key = ANY($2) AND fields.value <> 'null'::jsonb
            "#,
        fixed_table(table)
    );
    fixture_query_scalar(sqlx::AssertSqlSafe(query.as_str()))
        .bind(id)
        .bind(columns)
        .fetch_one(pool)
        .await
}

fn fixed_table(table: &str) -> &'static str {
    match table {
        "billing_payment_attempts" => "billing_payment_attempts",
        "billing_payment_methods" => "billing_payment_methods",
        _ => unreachable!("test projection table is fixed"),
    }
}

pub(super) async fn row_snapshot(
    pool: &PgPool,
    table: &str,
    id: Uuid,
) -> Result<String, sqlx::Error> {
    let query = match table {
        "billing_payment_attempts" => {
            "SELECT to_jsonb(rows)::text FROM billing_payment_attempts AS rows WHERE id = $1"
        }
        "billing_payment_methods" => {
            "SELECT to_jsonb(rows)::text FROM billing_payment_methods AS rows WHERE id = $1"
        }
        _ => unreachable!("test snapshot table is fixed"),
    };
    fixture_query_scalar(query).bind(id).fetch_one(pool).await
}
