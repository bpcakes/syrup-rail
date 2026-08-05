# Syrup Rail Core Crate Guide

## Purpose

Own validated domain types, pure lifecycle policy, gateway contracts, and
command/outcome types for application-independent subscription billing.

## Key entrypoints

- `src/lib.rs` is the public facade; domain modules land here as extraction
  proceeds.

## Invariants

- No SQLx, Axum, Runledger, or application-specific crate dependencies.
- No provider wire vocabulary in this crate.

## Common commands

- `cargo test -p syrup-rail`
