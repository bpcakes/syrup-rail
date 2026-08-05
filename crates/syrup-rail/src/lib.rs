//! Application-independent subscription billing domain types and policy.
//!
//! PostgreSQL orchestration lives in `syrup-rail-postgres`; NMI adaptation lives
//! in `syrup-rail-nmi` and `syrup-rail-nmi-client`.

#![forbid(unsafe_code)]

/// Workspace package version exposed for consumer pinning diagnostics.
pub const PACKAGE_VERSION: &str = env!("CARGO_PKG_VERSION");
