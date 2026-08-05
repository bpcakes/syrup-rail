//! Self-provisioning SQLx metadata gate for `syrup-rail-postgres`.
//!
//! Leases PostgreSQL 18 through `postgres-test-harness`, applies the version-1
//! install artifact, and runs `cargo sqlx prepare` from this crate without
//! `--workspace` so metadata stays package-local.

#![forbid(unsafe_code)]

use std::{
    env,
    path::{Path, PathBuf},
    process::{Command, ExitStatus},
};

use postgres_test_harness::{HarnessConfig, PostgresHarness};
use sqlx::postgres::PgPoolOptions;
use syrup_rail_postgres::schema_contract::V1_INSTALL_SQL;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mode = parse_mode()?;
    let crate_root = crate_root()?;
    let install_sql = load_install_sql(&crate_root)?;

    let harness =
        PostgresHarness::start(HarnessConfig::new("syrup_sqlx_gate")?.with_connection_budget(2)?)
            .await?;
    let lease = harness.empty_database().await?;

    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(lease.database_url())
        .await?;
    if !install_sql.trim().is_empty() {
        sqlx::raw_sql(&install_sql).execute(&pool).await?;
    }
    pool.close().await;

    let status = run_sqlx_prepare(&crate_root, lease.database_url(), mode)?;
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }

    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PrepareMode {
    Check,
    Prepare,
}

fn parse_mode() -> Result<PrepareMode, String> {
    match env::args().nth(1).as_deref() {
        Some("--check") => Ok(PrepareMode::Check),
        Some("--prepare") => Ok(PrepareMode::Prepare),
        Some(other) => Err(format!("unsupported mode argument: {other}")),
        None => Ok(PrepareMode::Check),
    }
}

fn crate_root() -> Result<PathBuf, String> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    Ok(manifest_dir)
}

fn load_install_sql(crate_root: &Path) -> Result<String, String> {
    let install_path = crate_root.join("schema/v1/install.sql");
    if install_path.is_file() {
        std::fs::read_to_string(&install_path)
            .map_err(|error| format!("failed to read {}: {error}", install_path.display()))
    } else {
        Ok(V1_INSTALL_SQL.to_owned())
    }
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
