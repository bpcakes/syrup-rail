//! Self-provisioning SQLx metadata gate for `syrup-rail-postgres`.
//!
//! This tool intentionally does not depend on the query-owning package: it
//! must be able to provision PostgreSQL before new query macros have metadata.

#![forbid(unsafe_code)]

use std::{
    env, io,
    path::{Path, PathBuf},
    process::{Command, ExitStatus},
};

use postgres_test_harness::{HarnessConfig, PostgresHarness};
use sqlx::postgres::PgPoolOptions;

#[path = "../../../crates/syrup-rail-postgres/schema/current.rs"]
mod current_schema;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mode = parse_mode()?;
    let crate_root = postgres_crate_root()?;

    let harness =
        PostgresHarness::start(HarnessConfig::new("syrup_sqlx_gate")?.with_connection_budget(2)?)
            .await?;
    let lease = harness.empty_database().await?;
    let database_url = lease.database_url().to_owned();

    let gate_result = run_gate(&crate_root, &database_url, mode).await;
    let lease_cleanup = lease.cleanup().await;
    let harness_shutdown = harness.shutdown().await;

    let mut failures = Vec::new();
    match gate_result {
        Ok(status) if status.success() => {}
        Ok(status) => failures.push(format!("cargo sqlx prepare exited with {status}")),
        Err(error) => failures.push(error),
    }
    if let Err(error) = lease_cleanup {
        failures.push(format!("failed to clean disposable database: {error}"));
    }
    if let Err(error) = harness_shutdown {
        failures.push(format!("failed to stop owned PostgreSQL server: {error}"));
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(io::Error::other(failures.join("; ")).into())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PrepareMode {
    Check,
    Prepare,
}

fn parse_mode() -> Result<PrepareMode, String> {
    match env::args().nth(1).as_deref() {
        Some("--check") | None => Ok(PrepareMode::Check),
        Some("--prepare") => Ok(PrepareMode::Prepare),
        Some(other) => Err(format!("unsupported mode argument: {other}")),
    }
}

fn postgres_crate_root() -> Result<PathBuf, String> {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(|workspace_root| workspace_root.join("crates/syrup-rail-postgres"))
        .ok_or_else(|| "tools/sqlx-gate must live two levels below the workspace root".into())
}

async fn run_gate(
    crate_root: &Path,
    database_url: &str,
    mode: PrepareMode,
) -> Result<ExitStatus, String> {
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(database_url)
        .await
        .map_err(|error| format!("failed to connect to disposable database: {error}"))?;

    let install_result = sqlx::raw_sql(current_schema::INSTALL_SQL)
        .execute(&pool)
        .await
        .map(|_| ())
        .map_err(|error| format!("failed to apply the current schema install artifact: {error}"));
    pool.close().await;
    install_result?;

    run_sqlx_prepare(crate_root, database_url, mode)
}

fn run_sqlx_prepare(
    crate_root: &Path,
    database_url: &str,
    mode: PrepareMode,
) -> Result<ExitStatus, String> {
    let mut command = Command::new("cargo");
    command
        .arg("sqlx")
        .arg("prepare")
        .current_dir(crate_root)
        .env_remove("DATABASE_URL")
        .env_remove("SQLX_OFFLINE")
        .env_remove("SQLX_OFFLINE_DIR")
        .env("DATABASE_URL", database_url)
        .env("SQLX_OFFLINE", "false")
        .env("SQLX_OFFLINE_DIR", ".sqlx");

    if mode == PrepareMode::Check {
        command.arg("--check");
    }

    command
        .arg("--")
        .args(["--all-targets", "--all-features"])
        .status()
        .map_err(|error| format!("failed to run cargo sqlx prepare: {error}"))
}
