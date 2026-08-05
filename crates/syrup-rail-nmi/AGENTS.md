# Syrup Rail NMI Adapter Crate Guide

## Purpose

Adapt the raw NMI client to Syrup Rail gateway and lifecycle-evidence contracts.

## Key entrypoints

- `src/lib.rs` re-exports `syrup-rail-nmi-client` as `nmi_client` until the
  adapter implementation lands in Milestone 2.

## Invariants

- Depends on `syrup-rail` and `syrup-rail-nmi-client` only among Syrup Rail
  packages.
- No SQLx or application crate dependencies.

## Common commands

- `cargo test -p syrup-rail-nmi`
