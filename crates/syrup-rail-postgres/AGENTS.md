# Syrup Rail PostgreSQL Crate Guide

## Purpose

Own the canonical provider-neutral PostgreSQL schema contract, SQLx queries,
and transaction orchestration.

## Key entrypoints

- `schema/v1/install.sql` — executable schema-v1 candidate and authoritative
  fresh-install DDL once frozen.
- `src/schema_contract.rs` — read-only catalog conformance plus fresh-install
  behavior fixtures behind `schema-contract-test-support`.
- `src/gateway_accounts.rs` — transaction-local gateway account registration
  and exact configuration activation.
- `src/entitlement.rs` — exact scope/subscriber/plan entitlement projection.
- `src/deletion.rs` — transaction-local canonical account-deletion blockers.

## Edit here for X

- Change canonical tables, constraints, functions, triggers, or views in
  `schema/v1/install.sql` before any host materializes version 1.
- Change host conformance or schema behavior tests in
  `src/schema_contract.rs` and update the catalog fingerprint intentionally.
- Change reusable gateway account/configuration metadata transitions in
  `src/gateway_accounts.rs`; keep host credentials outside this crate.
- Change reusable subscription access projection in `src/entitlement.rs`;
  keep host authentication and gateway availability outside the query.
- Change canonical deletion admission in `src/deletion.rs`; keep host order and
  fulfillment blockers in the host transaction.

## Invariants

- No runtime migrator in production service construction.
- Committed SQLx metadata lives in `crates/syrup-rail-postgres/.sqlx`.
- Provider wire strings belong in `syrup-rail-nmi`, not here.
- `assert_v1_conforms` is read-only; mutation and locking behavior belongs in
  package fixtures and host-seeded integration tests.
- Host objects attached to canonical relations use explicit host prefixes;
  `billing_*` constraint and index names are reserved for canonical objects.
- Host code composes transaction-local operations on its existing connection;
  shared operations never persist or receive plaintext provider credentials.

## Common commands

- `cargo test -p syrup-rail-postgres`
