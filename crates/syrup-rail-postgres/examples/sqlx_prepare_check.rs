//! Self-provisioning SQLx metadata gate for `syrup-rail-postgres`.
//!
//! Leases PostgreSQL 18 through `postgres-test-harness`, applies the version-1
//! install artifact, and runs `cargo sqlx prepare` from this crate without
//! `--workspace` so metadata stays package-local.

#![forbid(unsafe_code)]

use std::{
    env, io,
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
    let database_url = lease.database_url().to_owned();

    let gate_result = run_gate(&crate_root, &database_url, install_sql, mode).await;
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

fn load_install_sql(crate_root: &Path) -> Result<&'static str, String> {
    let install_path = crate_root.join("schema/v1/install.sql");
    match std::fs::read_to_string(&install_path) {
        Ok(on_disk) if on_disk == V1_INSTALL_SQL => Ok(V1_INSTALL_SQL),
        Ok(_) => Err(format!(
            "{} differs from schema_contract::V1_INSTALL_SQL",
            install_path.display()
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound && V1_INSTALL_SQL.is_empty() => {
            Ok(V1_INSTALL_SQL)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Err(format!(
            "{} is missing but schema_contract::V1_INSTALL_SQL is populated",
            install_path.display()
        )),
        Err(error) => Err(format!(
            "failed to read {}: {error}",
            install_path.display()
        )),
    }
}

async fn run_gate(
    crate_root: &Path,
    database_url: &str,
    install_sql: &str,
    mode: PrepareMode,
) -> Result<ExitStatus, String> {
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(database_url)
        .await
        .map_err(|error| format!("failed to connect to disposable database: {error}"))?;

    let install_result = if install_sql.trim().is_empty() {
        Ok(())
    } else {
        sqlx::raw_sql(install_sql)
            .execute(&pool)
            .await
            .map(|_| ())
            .map_err(|error| format!("failed to apply schema/v1/install.sql: {error}"))
    };
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
