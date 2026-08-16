# Harden split-phase submission identity

Centralize recovery and payment-method replacement submission identity at the
domain reservation boundary; use one entitlement observation timestamp;
restore host-charge reconciliation ownership guidance; add regression coverage
and run the full repository gates.

## Progress

- [x] Read the repository, crate, security, and abstraction-boundary guidance.
- [x] Confirm the three review findings and classify their root causes.
- [x] Implement and test domain-owned submission matching.
- [x] Use the entitlement query's materialized observation timestamp everywhere.
- [x] Add the missing reconciliation ownership-map entry.
- [x] Rerun the full repository gates on the settled final commit and record
  evidence.

## Surprises & Discoveries

- Recovery and payment-method replacement already store complete immutable
  requests and derive `Eq`, but their submit paths bypass that canonical value
  with sibling hand-written field lists.
- The entitlement query already has a materialized `clock` CTE; only the two
  newly added stale predicates bypass it.
- The first full structured check passed contract, workspace tests, and SQLx,
  but its aggregate fingerprint changed because the documentation slice was
  committed while it ran. That aggregate receipt was discarded and the gates
  were rerun on the settled final commit.

## Decision Log

- Reconstruct retry submissions through each reservation's canonical builder
  and compare the whole reservation. Candidate attempt IDs and one-shot tokens
  stay intentionally retryable; every durable field remains equality-bound.
- Keep the timestamp and guide repairs in independent commits because they are
  localized omissions rather than part of the invariant-boundary redesign.

## Outcomes & Retrospective

Three independently reviewable commits now own submission identity in the
domain reservation, use one entitlement observation, and restore the crate
ownership map:

- `5301d45` — domain-owned whole-reservation submission matching plus direct
  no-provider-I/O regressions.
- `e6bfa0c` — one materialized entitlement observation timestamp.
- `a912308` — host-charge reconciliation ownership guidance.

Focused core, application, and entitlement regressions pass. Settled-commit
Jig work check `receipt_01M0579S340FHSS2A29QS1YGJ0` records fresh passing
contract, full workspace test, and SQLx gates. CI's exact serial locked test
command passes in `receipt_01M057WNZTAPQXDWPFS85TE6W7`; formatting, clippy
with warnings denied, public API docs/doctests, and the changed-file LOC policy
also pass.

## Context and orientation

The core reservation types live in `crates/syrup-rail/src/recovery.rs` and
`crates/syrup-rail/src/payment_method_update.rs`. PostgreSQL consumes them in
`crates/syrup-rail-postgres/src/enrollment_application/`. Entitlement
projection is implemented in `crates/syrup-rail-postgres/src/entitlement.rs`.

## Plan of work

1. Add reservation-owned submission matchers that reuse canonical construction
   and full equality, then replace the two PostgreSQL partial checks.
2. Cover allowed retry-only changes and rejected contact/idempotency changes,
   including direct submit-path assertions that provider I/O stays at zero.
3. Replace both extra `clock_timestamp()` calls with the materialized
   `clock.observed_at` value.
4. Add `host_charge_reconciliation.rs` to PostgreSQL ownership guidance.
5. Commit each slice separately and run the repository's full gates.

## Concrete steps

Use `apply_patch` for edits. Run focused Cargo tests for the core and PostgreSQL
application/entitlement areas after their slices, then run `scripts/jig work
check`, `scripts/jig work evidence`, `scripts/jig work gates`, and `scripts/jig
work finish` around the repository-required checks.

## Validation and acceptance

Acceptance requires regression tests showing mismatched durable submission
identity is rejected before gateway I/O, all relevant focused tests passing,
and the full backend test suite plus configured CI-equivalent gates passing.

## Idempotence and recovery

All source edits are ordinary Git patches and each logical slice is committed
before the next begins. Test databases are disposable fixtures. Existing
append-only `.agent/state/*.jsonl` history is preserved.

## Interfaces and dependencies

No schema, external service, or dependency change is required. The repair adds
an additive domain operation to the two existing reservation types and keeps
the public submit-function signatures unchanged.
