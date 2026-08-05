//! Version-1 schema contract fixtures for conformance tests.
//!
//! `install.sql` is intentionally absent until the Milestone 0 schema gate
//! delivers the exact column, constraint, and index specification.

use sqlx::PgPool;

/// Placeholder until `schema/v1/install.sql` is authored from the approved spec.
pub const V1_INSTALL_SQL: &str = "";

/// Asserts that a database already migrated to the version-1 contract conforms.
pub async fn assert_v1_conforms(_pool: &PgPool) -> Result<(), sqlx::Error> {
    if V1_INSTALL_SQL.is_empty() {
        panic!(
            "version-1 install.sql is not yet encoded; complete the Milestone 0 schema gate first"
        );
    }
    Ok(())
}
