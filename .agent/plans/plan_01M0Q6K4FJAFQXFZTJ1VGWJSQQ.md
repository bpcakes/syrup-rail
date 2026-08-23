# Implement the five accepted simplification-audit beads

This plan implements R-10, R-08, R-09, R-16, and R-25 as a coordinated,
compatibility-sensitive refactor. The observable result is that invalid states
become unrepresentable at the core and raw-client boundaries, gateway identity
travels as one value, persisted fingerprints retain their historical bytes,
and all approved-evidence parking uses one policy-complete state machine.

## Progress

- [x] 2026-08-23: Read `AGENTS.md`, `agent-map.md`, crate guides,
  `.agent/PLANS.md`, and `docs/security/threat-model.md`.
- [x] 2026-08-23: Claim all five beads and start Jig work session
  `session_01M0Q6K4EXV10YNYTCEYY07Z45`.
- [x] 2026-08-23: Implement R-10's shared `GatewayAccountIdentity`, compatible
  resolver entry point, and whole-value PostgreSQL checks.
- [x] 2026-08-23: Implement R-08's canonical request factory and explicitly
  named persisted-parts rehydration path; migrate live factories and storage.
- [x] 2026-08-23: Implement R-09's closed payment-failure outcome and preserve
  the host-owned V1 wire through disposition/access projections.
- [x] 2026-08-23: Implement R-16's five-variant raw sale intent, fixed USD wire,
  exhaustive validation/encoding, and legacy 12-shape conversion.
- [x] 2026-08-23: Implement R-25's centralized approved-evidence parking state
  machine with exhaustive lock, terminal-progression, and fallback policies.
- [x] 2026-08-23: Add exact fingerprint vectors, opaque persisted-byte and
  expected-state divergence tests, four failure projections/V1 replay, the
  five NMI routes, parking policy coverage, and forced retry exhaustion.
- [x] 2026-08-23: Pass focused tests, stable/MSRV packaged-client tests, format,
  clippy, contract, SQLx, and the full repository test gate; inspect the diff.
- [x] 2026-08-23: Record evidence, finish Jig work successfully, close exactly
  the five claimed Beads, and flush their JSONL export.

## Surprises & Discoveries

- The worktree contained substantial pre-existing changes in the same billing
  area. Those changes are preserved; this plan does not reset or overwrite them.
- R-10 can remain source-compatible by retaining `GatewayAccountRegistration`
  as an alias and the old resolver method as the required trait method while
  adding a default whole-identity entry point.
- Renewal's approved-evidence retry path intentionally has no lock-free fallback.
  This is a security-relevant fail-closed policy, not duplicate-code drift.
- Payment-method replacement intentionally locks only the attempt while initial,
  recovery, and renewal parking lock the subscription aggregate first.
- The NMI raw client has exactly five valid legacy sale-mode combinations out of
  twelve. Keeping legacy types only for explicit conversion preserves migration
  assistance without admitting the clump in `SaleRequest`.

## Decision Log

- 2026-08-23: Sequence R-10 before R-08 because both touch attempt factories and
  PostgreSQL identity checks.
- 2026-08-23: Keep R-10's old public names and resolver signature as compatibility
  surfaces; make all production paths use and compare the new whole identity.
- 2026-08-23: Keep `PaymentAttemptRequest::new` as a documented persisted-parts
  compatibility wrapper. Only `canonical` may derive bytes for live requests;
  `from_persisted_parts` names the byte-preservation obligation.
- 2026-08-23: Make R-09's event shape a direct closed-enum cutover while retaining
  V1 outbox JSON exactly through projection methods. No database schema changes.
- 2026-08-23: Make `SaleRequest` accept `SaleIntent` directly and serialize USD
  internally. Preserve old mode enums solely for `SaleIntent::from_legacy_parts`.
- 2026-08-23: Put R-25 mechanics in `enrollment_application.rs`. Operation-local
  messages and entry decisions stay in child modules; the closed reservation
  enum owns lock scope, terminal progression, and optional fallback.
- 2026-08-23: Follow the existing threat model. No new secret, cryptographic
  control, authentication boundary, or schema is introduced.

## Outcomes & Retrospective

All five representations are implemented without a database migration. Jig
work check and gates passed for contract, tests, and SQLx; format and clippy also
passed. Both stable and Rust 1.88 packaged NMI client suites passed with 164 unit
tests, three public-API tests, and the compile-fail doctest. The PostgreSQL lib
suite passed through 222 tests before one disposable-container startup failure;
the failed schema-contract test passed immediately in isolation, and the Jig
test gate then passed the full repository suite. Compatibility wrappers and the
host V1 payload remain in place as planned.

## Context and orientation

Core public billing types live in `crates/syrup-rail/src`. PostgreSQL attempt,
renewal-failure, and approved-outcome application live in
`crates/syrup-rail-postgres/src`. Raw NMI request validation and wire encoders
live in `crates/syrup-rail-nmi-client/src`; the domain adapter is in
`crates/syrup-rail-nmi/src/adapter.rs`. Host V1 outbox projection is an example
owned by the PostgreSQL crate. Payment fingerprints and V1 outbox JSON are
compatibility-sensitive persisted/wire representations and must not change.

## Plan of work

First carry gateway account scope, account, provider, and configuration as one
identity value through resolution while retaining compatibility methods. Next
split new canonical payment requests from opaque persisted rehydration. Replace
independent failure disposition/access fields and raw sale mode fields with
closed enums. Finally, replace four copied approved-evidence parking flows with
one algorithm whose intentional differences are exhaustive enum policies.

## Concrete steps

1. Update core identity/resolver and PostgreSQL consumers; verify exact equality.
2. Add canonical and persisted payment-request constructors; migrate factories.
3. Add the closed failure outcome; update producer, semantic keys, and V1 mapper.
4. Add `SaleIntent`; update validation, Classic/v5 encoding, dispatch, and adapter.
5. Move approved parking/retry/reload mechanics to the parent module and delete
   operation-local copies.
6. Add characterization and regression tests for every acceptance matrix.
7. Run formatting, focused tests, public API/MSRV checks, and Jig gates.

## Validation and acceptance

R-10 requires whole-value and per-component mismatches plus resolver and three
service-path contracts. R-08 requires exact vectors for all attempt shapes,
opaque legacy bytes, expected-versus-related method divergence, replay, and
schema upgrade coverage. R-09 requires four valid outcomes, timestamp semantics,
semantic keys, and identical V1 JSON/replay. R-16 requires all twelve legacy
shapes, all five routes/wires, bounds/no-network failures, redaction, public API,
and MSRV. R-25 requires the four-operation failure matrix, retry exhaustion,
lock scope, terminal progression, durable evidence, and result shapes.

Run focused `cargo test` commands while iterating, then `scripts/jig check fmt`,
`scripts/jig check clippy`, `scripts/jig check contract`, and
`scripts/jig check test`. Run the NMI standalone stable/MSRV script required by
its crate guide. Successful completion means all commands exit zero and the
final diff contains no accidental schema or V1 wire changes.

## Idempotence and recovery

All source edits are repeatable and no migration is added. Tests use existing
database harness isolation. If a gate fails, fix only the implicated files and
rerun that focused gate before the full suite. Do not reset the dirty worktree;
unrelated existing modifications belong to the user. Beads are closed only
after all validation succeeds, followed by `br sync --flush-only`.

## Interfaces and dependencies

Public compatibility surfaces retained during this change are
`GatewayAccountRegistration`, `GatewayResolver::resolve`, and
`PaymentAttemptRequest::new`. New primary interfaces are
`GatewayAccountIdentity`, `GatewayResolver::resolve_identity`,
`PaymentAttemptRequest::canonical`, `PaymentAttemptRequest::from_persisted_parts`,
`SubscriptionPaymentFailureOutcome`, and `SaleIntent`. The PostgreSQL schema and
SQLx query metadata are unchanged.
