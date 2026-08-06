//! PostgreSQL schema contract and transaction orchestration for Syrup Rail.
//!
//! Production service construction must not expose or invoke a migrator; host
//! applications materialize versioned install artifacts as immutable migrations.

#![forbid(unsafe_code)]

#[cfg(any(test, feature = "schema-contract-test-support"))]
pub mod schema_contract;
