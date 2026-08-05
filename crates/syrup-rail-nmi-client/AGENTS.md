# Syrup Rail NMI Client Crate Guide

## Purpose

Own the reusable NMI payment-provider client: bounded HTTP transport, Classic and
v5 payment requests, Customer Vault operations, stored-credential fields,
transaction queries, and report parsing.

## Key entrypoints

- `ClientFactory` in `src/client.rs` owns the shared credential-free HTTP pool and
  report-operation admission budget.
- `Client` in `src/client.rs` is an account-bound concrete NMI client.
- `Endpoint` and `Credentials` in `src/configuration.rs` validate provider
  connection configuration.
- Public request, response, and error types live with their implementations in
  `src/requests.rs`, `src/responses.rs`, and `src/errors.rs`; `src/lib.rs` is the
  stable re-export facade.
- Private wire and lossless-JSON modules own protocol encoding and parsing.

## Edit here for X

- NMI request/response behavior and transport bounds: edit this crate.
- Application persistence, idempotency, host allowlisting, subscriptions, and
  reconciliation policy: edit the host billing adapter, not this crate.
- Browser tokenization and Collect.js behavior: edit the owning application.

## Invariants

- Never retry a mutation internally.
- Preserve the distinction between known non-submission and an indeterminate
  mutation outcome.
- Never expose credentials, provider bodies, identifiers, or free-form provider
  text through `Debug`, `Display`, or an error message.
- Report anomalous payment evidence through payload-free diagnostics; leave
  logging and metrics policy to the caller.
- Keep response, report, action, and field counts bounded.
- Reject excess report work before submission instead of queueing it locally.
- Keep idle-connection retention independent from report admission policy.
- Keep endpoint joining on the configured origin.
- Do not add plan or subscription-schedule APIs.
- Do not depend on application crates, SQLx, Runledger, or UUID.

## Common commands

- `cargo test -p syrup-rail-nmi-client`
- `cargo tree -p syrup-rail-nmi-client`
- `bash crates/syrup-rail-nmi-client/check-standalone.sh` (requires the `stable`
  toolchain for packaging)
- `RUSTUP_TOOLCHAIN=1.88 bash crates/syrup-rail-nmi-client/check-standalone.sh`
  (requires both `stable` and `1.88`; compiles and tests the packaged crate at
  the MSRV)

The standalone commands preserve dependency artifacts in compiler-keyed
subdirectories of `.cache/syrup-rail-nmi-client-standalone/` while rebuilding this
package's own artifacts. Keeping the tree outside Cargo's normal `target/`
directory lets CI register only the compiler-target subtree as a second
rust-cache workspace target, where obsolete dependency artifacts are pruned;
the re-extracted source is not cached. The packaged source uses a stable path so
Cargo's package-only clean removes its prior units without accumulating a new
path-keyed copy on every run. Both source and target paths are compiler-keyed,
so the documented default-toolchain and MSRV commands can run independently;
alternating toolchains do not invalidate another compiler's dependency
artifacts.
