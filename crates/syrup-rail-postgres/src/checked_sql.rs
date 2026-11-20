use sqlx::error::BoxDynError;
use sqlx::postgres::PgArguments;
use sqlx::{Execute, Postgres, SqlStr};

/// Retains the SQL and encoded bindings from a SQLx compile-checked query so
/// existing row codecs can continue to validate durable values at runtime.
pub(crate) fn query_parts(
    mut checked: impl Execute<'static, Postgres>,
) -> (SqlStr, Result<PgArguments, BoxDynError>) {
    let arguments = checked
        .take_arguments()
        .map(|arguments| arguments.expect("SQLx query macros produce prepared statements"));
    (checked.sql(), arguments)
}

macro_rules! checked_query {
    ($sql:literal $(+ $fragment:literal)* $(, $arg:expr)* $(,)?) => {{
        let (sql, arguments) =
            $crate::checked_sql::query_parts(sqlx::query!($sql $(+ $fragment)*, $($arg),*));
        sqlx_core::query::query_with_result::<sqlx::Postgres, _>(sql, arguments)
    }};
}

macro_rules! checked_query_as {
    ($sql:literal $(+ $fragment:literal)* $(, $arg:expr)* $(,)?) => {
        checked_query_as!(_, $sql $(+ $fragment)* $(, $arg)*)
    };
    ($out:ty, $sql:literal $(+ $fragment:literal)* $(, $arg:expr)* $(,)?) => {{
        let (sql, arguments) =
            $crate::checked_sql::query_parts(sqlx::query!($sql $(+ $fragment)*, $($arg),*));
        sqlx_core::query_as::query_as_with_result::<sqlx::Postgres, $out, _>(sql, arguments)
    }};
}

macro_rules! checked_query_scalar {
    ($sql:literal $(+ $fragment:literal)* $(, $arg:expr)* $(,)?) => {
        checked_query_scalar!(_, $sql $(+ $fragment)* $(, $arg)*)
    };
    ($out:ty, $sql:literal $(+ $fragment:literal)* $(, $arg:expr)* $(,)?) => {{
        let (sql, arguments) =
            $crate::checked_sql::query_parts(sqlx::query_scalar!($sql $(+ $fragment)*, $($arg),*));
        sqlx_core::query_scalar::query_scalar_with_result::<sqlx::Postgres, $out, _>(sql, arguments)
    }};
}

#[cfg(test)]
macro_rules! concat_sql {
    ($sql:literal $(+ $fragment:literal)*) => {
        concat!($sql $(, $fragment)*)
    };
}
