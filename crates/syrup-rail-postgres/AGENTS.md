# Syrup Rail PostgreSQL Crate Guide

## Purpose

Own the canonical provider-neutral PostgreSQL schema contract, SQLx queries,
and transaction orchestration.

## Key entrypoints

- `schema/v1/install.sql` — authoritative fresh-install DDL (pending spec gate).
- `src/schema_contract.rs` — conformance fixtures behind
  `schema-contract-test-support`.

## Invariants

- No runtime migrator in production service construction.
- Committed SQLx metadata lives in `crates/syrup-rail-postgres/.sqlx`.
- Provider wire strings belong in `syrup-rail-nmi`, not here.

## Common commands

- `cargo test -p syrup-rail-postgres`
