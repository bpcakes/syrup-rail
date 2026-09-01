# Fix NMI terminal outcome provenance for 0.5.1

Preserve authoritative terminal NMI decisions when a Classic response contains
an empty transaction identifier, retain provider-neutral anomaly provenance
across the NMI adapter, add regression coverage, prepare the coordinated 0.5.1
package release, and verify the full repository gates.

## Progress

- [x] Research the NMI response-code contract and reproduce the lossy adapter
  boundary from the 0.5.0 source.
- [x] Correct required-identifier parsing without weakening approved-outcome or
  conflicting-identifier checks.
- [x] Carry payload-free anomaly diagnostics across the provider-neutral
  gateway boundary.
- [x] Prepare coordinated 0.5.1 release metadata.
- [x] Run focused tests, full repository gates, and release validation.
- [x] Push the isolated branch and open a pull request into `main`.

## Surprises & Discoveries

- Local `main` in the source checkout was stale at 0.3.0. The Herdr worktree is
  based on refreshed `origin/main` at commit `7542ce7`, whose workspace version
  is 0.5.0.
- The raw NMI client already classifies NMI's documented terminal response
  codes as `Failed`. A blank required transaction identifier is subsequently
  combined with malformed/conflicting identifiers and downgrades that status to
  `Unknown`.
- Public mutation methods already enforce transaction identity only for an
  `Approved` outcome. This is the appropriate authority for missing-identity
  handling; the wire parser should reserve its stronger anomaly for conflicting
  or malformed non-empty identifiers.

## Decision Log

- Treat an identifier field containing only blank or null occurrences as
  absent. Preserve `InvalidOrConflicting` for a blank/null occurrence mixed
  with a non-empty identifier, malformed non-empty values, and divergent
  identifiers. This keeps approvals fail-closed through
  `require_approved_identities` while preserving determinate terminal outcomes.
- Add provider-neutral, payload-free diagnostic variants and map every raw NMI
  payment diagnostic. Do not expose NMI wire strings or provider payloads in
  the core crate.
- Keep the behavioral parser correction and diagnostic-boundary expansion as
  separate commits, followed by a release-preparation commit.
- Keep the provider-neutral diagnostic enum payload-free. The adapter's
  non-exhaustive fallback maps future provider diagnostics to
  `UnmappedProviderDiagnostic`, so adding a raw diagnostic cannot silently
  erase all provenance again.

## Outcomes & Retrospective

- Commit `5f1b38d` makes foreground mutation parsing distinguish absent
  transaction identity from contradictory identity evidence. All documented
  terminal NMI error codes now remain determinate with blank identity, while
  approved outcomes and mixed blank/non-empty aliases still fail closed.
- Commit `bca2bc7` expands the provider-neutral anomaly vocabulary and maps
  every current raw NMI diagnostic, including a future-proof non-exhaustive
  fallback.
- Commit `24ab71b` prepares the coordinated 0.5.1 workspace release metadata.
- Focused client, core, and adapter suites pass. The full locked serialized
  workspace suite and the schema, advisory, public API, check, contract,
  format, Clippy, and SQLx gates pass. The ordinary plan test lane failed under
  container saturation: the unconstrained run had 107 `StartupTimeout`
  failures, and its serialized retry passed 224 of 225 before one container
  startup timeout. That exact remaining scenario passed on a serialized retry.
  This is recorded as infrastructure evidence rather than relabeled as a green
  plan receipt; the configured release-grade locked suite is green.
- The product commits were pushed from the Herdr-managed worktree and opened
  as <https://github.com/bpcakes/syrup-rail/pull/4>. Package publication and
  release dispatch remain explicitly out of scope.

## Context and orientation

`crates/syrup-rail-nmi-client/src/client/response/common.rs` owns bounded scalar
collection and payment-decision parsing. Public operations in
`crates/syrup-rail-nmi-client/src/client.rs` apply approved-identity
requirements after parsing. `crates/syrup-rail/src/gateway.rs` owns the
provider-neutral diagnostic contract, and
`crates/syrup-rail-nmi/src/adapter.rs` maps NMI client outcomes to it.

## Plan of work

1. Teach the scalar collector to distinguish identifier absence from a
   contradictory absent/present combination. Add Classic, JSON, and public API
   regression coverage, including all documented terminal NMI response codes.
2. Expand `GatewayPaymentDiagnostic` with provider-neutral evidence-anomaly
   categories and map all `PaymentOutcomeDiagnostic` variants. Prove the
   adapter preserves diagnostics and provider decisions end to end.
3. Update the coordinated workspace version, internal requirements, lockfile,
   package READMEs, public API version, and changelog for 0.5.1.
4. Run focused package tests during implementation, then the repository's full
   Jig gates and `scripts/check-release.sh 0.5.1 --allow-dirty`.

## Validation and acceptance

- NMI terminal codes 300, 400, 410, 411, 440, 441, 460, and 461 remain
  `Failed` when the response supplies an empty transaction identifier.
- Approved responses with an empty transaction identifier remain `Unknown`
  with a missing-identity diagnostic.
- Empty plus non-empty aliases remain `Unknown` with an invalid/conflicting
  identity diagnostic.
- Every raw client anomaly diagnostic has a payload-free provider-neutral
  mapping.
- Focused package tests and all configured repository gates pass.

## Idempotence and recovery

All source edits are isolated in the Herdr-managed worktree. Tests may be
rerun. Commits stage explicit paths, and the original checkout's unrelated
changes remain untouched. No package publication or release workflow dispatch
is part of this plan.

## Interfaces and dependencies

The public `GatewayPaymentDiagnostic` enum gains additive variants under its
existing `#[non_exhaustive]` contract. No database schema, persisted wire
format, provider request, or retry behavior changes.
