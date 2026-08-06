# Syrup Rail PostgreSQL Crate Guide

## Purpose

Own the canonical provider-neutral PostgreSQL schema contract, SQLx queries,
and transaction orchestration.

## Key entrypoints

- `schema/v1/install.sql` — executable schema-v1 candidate and authoritative
  fresh-install DDL once frozen.
- `src/schema_contract.rs` — read-only catalog conformance plus fresh-install
  behavior fixtures behind `schema-contract-test-support`.

## Edit here for X

- Change canonical tables, constraints, functions, triggers, or views in
  `schema/v1/install.sql` before any host materializes version 1.
- Change host conformance or schema behavior tests in
  `src/schema_contract.rs` and update the catalog fingerprint intentionally.

## Invariants

- No runtime migrator in production service construction.
- Committed SQLx metadata lives in `crates/syrup-rail-postgres/.sqlx`.
- Provider wire strings belong in `syrup-rail-nmi`, not here.
- `assert_v1_conforms` is read-only; mutation and locking behavior belongs in
  package fixtures and host-seeded integration tests.
- Host objects attached to canonical relations use explicit host prefixes;
  `billing_*` constraint and index names are reserved for canonical objects.

## Common commands

- `cargo test -p syrup-rail-postgres`
