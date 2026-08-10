# Syrup Rail NMI Adapter Crate Guide

## Purpose

Adapt the raw NMI client to Syrup Rail gateway and lifecycle-evidence contracts.

## Key entrypoints

- `src/adapter.rs` maps the raw client to the five-method Syrup Rail gateway.
- `src/lifecycle.rs` alone interprets NMI action/condition vocabulary into
  canonical evidence or quarantine.
- `src/reference.rs` owns the two-letter host namespace and exact mutation
  reference format.
- `src/lib.rs` exports the adapter and retains the raw client re-export for
  hosts that construct short-lived clients from their own credential stores.

## Edit here for X

- Change one-shot sale, stored-method, exact-query, or report-page adaptation
  in `src/adapter.rs` while preserving the retry-free raw-client boundary.
- Change provider lifecycle vocabulary mapping only in `src/lifecycle.rs`;
  unknown evidence remains quarantined rather than guessed.
- Change generated mutation references in `src/reference.rs` and preserve the
  exact durable-attempt correlation required for reconciliation.

## Invariants

- Depends on `syrup-rail` and `syrup-rail-nmi-client` only among Syrup Rail
  packages.
- No SQLx or application crate dependencies.
- Never move NMI action parsing into `syrup-rail` or `syrup-rail-postgres`.

## Common commands

- `cargo test -p syrup-rail-nmi`
