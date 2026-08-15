# Close comprehensive-review findings

Resolve host-charge cleanup starvation without weakening atomic release,
repair v0.2.0 unsubmitted initial attempts parked in review, and make the
post-reservation readiness result contract explicit. Add focused regressions,
commit each behavioral slice separately, and finish with required repository
gates.

## Progress

- [x] Reviewed the comprehensive findings against the repository guides,
  public API, prior implementation decisions, and threat model.
- [x] Make unreleasable host targets observable candidate-local skips while
  preserving atomic target/attempt rollback.
- [x] Expire legacy unsubmitted initial attempts parked as `review_required`
  through foreground and scheduled local cleanup.
- [x] Document and test durable post-reservation readiness errors and
  same-idempotency-key result recovery.
- [x] Run focused tests, required checks, and final gates; record evidence.

## Surprises & Discoveries

- The host-charge cleanup API was added after `v0.2.0`, so its return type can
  become a typed summary before release without breaking a shipped consumer.
- Version 0.2.0 could move a never-submitted initial attempt to
  `review_required`; current exact-query selection excludes it, but the local
  enrollment expiry path still accepts only `pending`.
- The readiness `Ok`-to-`Err` change is deliberate: the prior plan and focused
  test require exact `GatewayReadiness` categories after reservation. The
  missing contract is how hosts recover the already-durable payment result.

## Decision Log

- Decision: Treat `StaleTarget` and `Unchanged` as candidate-local skips,
  rollback that candidate, continue the bounded page, and report failed and
  skipped counts. Host callback errors still abort because their scope and
  transaction outcome are not semantically classified.
- Decision: Expire stale initial attempts in `pending` or `review_required`
  only when `submitted_at IS NULL`. This authoritative boundary proves no
  provider mutation began and does not touch genuine processor-review rows.
- Decision: Retain typed post-reservation readiness errors. Document that a
  determinate failure may have terminalized the attempt and prove that replaying
  the same idempotency key returns the canonical result without downstream I/O.

## Outcomes & Retrospective

The work landed as three independently reviewable commits:

- `d31e42f` makes host cleanup continue past `StaleTarget` and `Unchanged`
  candidates while reporting typed failed/skipped counts. Its regression covers
  both skip reasons followed by a valid later candidate.
- `daf79f0` repairs stale 0.2.0-style unsubmitted initial reviews in foreground
  replay and scheduled cleanup. Regressions cover exact typed expiry, same-key
  repair, and a subsequent reservation for the same subscriber and plan.
- `ec1b883` defines the durable readiness-error contract and proves same-key
  replay recovers the canonical terminal result without repeating admission,
  resolution, readiness, or provider work.

Focused PostgreSQL tests passed after each slice. The final Jig evidence is
fresh for the current worktree: contract receipt
`receipt_01M037YKEZA41ZZ9SN0D7W9TXV`, workspace-test receipt
`receipt_01M0382WZ2SE7HED9444799E0Y`, and SQLx receipt
`receipt_01M0384DDGS3QEKEG5FT7NYH81`. Formatting receipt
`receipt_01M0386EE604BK2PPV42VRDHDT` and Clippy receipt
`receipt_01M0387RXC58P3VHPWH9Y6PRAK` also passed. No schema artifact or
migration changed.

## Context and orientation

The changes are owned by `crates/syrup-rail-postgres`. Host-target cleanup is
in `src/host_charge_reconciliation.rs`; initial-attempt expiry and replay are
in `src/attempts/{shared,initial,initial/support}.rs` and
`src/reconciliation.rs`; high-level readiness orchestration is in
`src/subscription_billing_service/{subscriber,host_charge}.rs` with its public
error contract in `src/subscription_billing_service.rs`.

## Plan of work

Implement and commit the host cleanup summary and starvation regression first.
Then broaden the authoritative local initial-expiry predicates and cover both a
0.2.0-style scheduled cleanup row and same-key foreground replay. Finally,
clarify the readiness contract in rustdoc and migration documentation and
extend the configuration-readiness regression through terminal replay.

## Validation and acceptance

Run focused PostgreSQL tests after each slice. Finish with `scripts/jig check
fmt`, `scripts/jig check clippy`, `scripts/jig check test`, `scripts/jig check
sqlx`, `scripts/jig check contract`, and `scripts/jig work gates`. Success means
all checks pass, every semantic slice is committed separately, and the only
remaining untracked file is the user's `.agent/0.2.1-bug-findings.md`.

## Idempotence and recovery

All cleanup paths remain retry-safe. A skipped host target is untouched, a
concurrently changed attempt rolls back its paired target transition, and an
already-expired initial attempt is ignored by later passes. No schema artifact
or migration is changed.
