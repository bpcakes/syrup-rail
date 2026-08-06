# Syrup Rail Core Crate Guide

## Purpose

Own validated domain types, pure lifecycle policy, gateway contracts, and
command/outcome types for application-independent subscription billing.

## Key entrypoints

- `src/lib.rs` is the public facade.
- `src/{identity,money,subscription,event,resolution}.rs` own validated billing
  values and closed lifecycle facts.
- `src/{gateway,gateway_value}.rs` own the five-method provider port, typed
  evidence, sensitive values, and diagnostic boundary.
- `src/{card_data,policy}.rs` own the provider-neutral PAN scanner and pure
  payment/calendar policy.

## Invariants

- No SQLx, Axum, Runledger, or application-specific crate dependencies.
- No provider wire vocabulary in this crate.
- Provider identifiers, tokens, contacts, and diagnostics have value-free
  ordinary formatting; expose their values only at adapter/persistence edges.

## Common commands

- `cargo test -p syrup-rail`
