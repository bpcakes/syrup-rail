# Repair payment attempt crash recovery

This ExecPlan repairs three payment-lifecycle defects documented in
`.agent/0.2.1-bug-findings.md`. After this work, a retry with the same
idempotency key can finish a recovery or payment-method replacement that was
durably reserved but never submitted, purely local attempts cannot be mistaken
for provider-side transactions by reconciliation, and gateway-readiness
failures preserve their real error category. Each behavior change is delivered
as its own commit with focused regression tests.

The security baseline is `docs/security/threat-model.md`. The relevant
feature-specific invariant is mutation at-most-once: durable attempt identity,
idempotency state, and the provider mutation reference are fixed under a
PostgreSQL lock before provider I/O, while no database transaction spans that
I/O.

## Progress

- [x] (2026-08-15) Investigated and reproduced the three reported findings.
- [x] (2026-08-15) Read the repository, PostgreSQL-crate, planning, and payment
  threat-model guidance.
- [x] (2026-08-15) Implemented and focused-tested same-key continuation of
  prepared recovery and payment-method replacement attempts. The slice is
  ready to commit.
- [x] (2026-08-15) Implemented and focused-tested local renewal/recovery
  expiry, dispatch/cancellation self-healing, and provider exact-query phase
  eligibility. The slice is ready to commit.
- [ ] Implement and test workflow-specific gateway-readiness error mapping;
  commit the slice.
- [ ] Run the repository's required gates, record evidence, review the final
  diff, and finish this Jig work item.

## Surprises & Discoveries

- The domain crate already exposes
  `SubscriptionRecoveryReservation::from_attempt` and
  `SubscriptionPaymentMethodReplacement::from_attempt`. The PostgreSQL service
  can therefore continue an existing durable attempt without inventing a new
  public API.
- Enrollment and host-charge workflows already demonstrate the intended
  pattern: a matching pending attempt whose `submitted_at` is null is a
  continuation point, and its durable attempt ID—not the retry candidate's
  ID—must be used for the provider mutation reference.
- Provider exact-query selection currently includes every sufficiently old
  pending attempt by using `COALESCE(submitted_at, created_at)`. That is broader
  than the reported renewal/recovery symptom and can send initial-enrollment or
  host-charge attempts that never crossed the provider boundary to the gateway.
- `GatewayError::Unavailable` is already modeled separately from live-mode
  readiness failure, while `RateLimited` has its own special case. The defect is
  the catch-all mapping after reservation, not a missing domain error.
- A single result helper was the source of the misleading subscription payload:
  it loaded the current subscription for every recovery/replacement result,
  independent of attempt status. Restricting that load to approved attempts
  makes prepared and failed results follow the documented contract.
- Renewal dispatch has a generic-plan/index-shape regression test that checks
  PostgreSQL parameter positions literally. Adding the local-stale threshold
  therefore required shifting and updating both continuation keyset parameters
  while preserving the ordered due-index plan.

## Decision Log

- Decision: Handle a matching prepared attempt as continuation in the
  application service while retaining the storage preflight's `Replay` shape.
  Rationale: submitted and terminal attempts remain true replays, while the
  service has the gateway and admission context needed to reconstruct and
  safely submit a prepared reservation. This matches existing enrollment
  behavior and handles both the initial preflight and a race at reservation.
- Decision: Reconstruct from the durable row and validate the resolved gateway
  identity before provider I/O. Rationale: candidate retry IDs must never leak
  into mutation references, and configuration/account mismatches must fail
  closed.
- Decision: Exclude all attempts with `submitted_at IS NULL` from provider
  exact-query reconciliation, regardless of operation kind. Rationale: null
  `submitted_at` is the durable evidence that no provider boundary was crossed;
  operation-specific filtering would leave equivalent unsafe cases.
- Decision: Add bounded local expiry for unsubmitted renewal and recovery
  attempts and remediate already parked local rows without gateway I/O.
  Rationale: these attempts otherwise hold subscription lifecycle locks
  indefinitely. The concrete threshold and terminal transition will follow the
  existing lifecycle policy/constants and be captured in tests.
- Decision: Preserve each gateway error variant after reservation and persist
  the matching workflow failure code. Rationale: callers and durable evidence
  should distinguish provider unavailability, rate limiting, configuration,
  and readiness failures rather than report all non-rate-limit errors as a live
  mode problem.

## Outcomes & Retrospective

Work is in progress. On completion this section will list the three commit IDs,
the exact accepted behavior, final gate results, and any residual risk.

The first slice passes all nine focused foreground tests. Its crash-window tests
prove the high-level retry returns the original durable attempt ID and sends the
original durable gateway order ID for both a recovery sale and a stored-method
mutation.

The second slice passes the reconciliation, renewal-dispatch, cancellation, and
same-key recovery regressions. It also repairs legacy unsubmitted
`review_required` renewal/recovery rows locally and defensively refuses an
exact-query observation if the current row has no `submitted_at` boundary.

## Context and orientation

`crates/syrup-rail` owns validated domain types and lifecycle policy.
`crates/syrup-rail-postgres` owns durable orchestration, transaction boundaries,
reconciliation, and the high-level enrollment application service. The main
service implementation is
`crates/syrup-rail-postgres/src/enrollment_application.rs`; focused tests are in
`crates/syrup-rail-postgres/src/enrollment_application/tests/foreground.rs`.
Reservation and preflight SQL live in `src/enrollment.rs`; reconciliation lives
in `src/reconciliation.rs` and its tests. The current complete schema artifact
is `crates/syrup-rail-postgres/schema/v2`; `schema/v1` is immutable. No schema
change is expected for these fixes.

A "prepared" attempt is a durable payment-attempt row with status `pending` and
no `submitted_at` timestamp. A "submitted" attempt has crossed the provider
boundary and may legitimately need an exact provider lookup. "Exact query"
means looking up one provider transaction using the durable mutation reference.

## Plan of work

First, teach recovery and payment-method replacement service paths to distinguish
a prepared replay from a submitted or terminal replay. For a prepared row,
resolve admission and the configured gateway, reconstruct the reservation from
the stored attempt, validate gateway identity, and submit using the stored
attempt ID. Cover both workflows with crash-window tests that reserve in one
transaction and retry through the high-level service with a different candidate
ID. Also ensure a non-approved replay does not expose a subscription as if it
were applied.

Second, make reconciliation use local phase before age. Unsubmitted attempts
must never enter exact provider lookup. Renewal and recovery attempts that stay
prepared beyond the lifecycle threshold must transition locally so they stop
blocking dispatch, retry, and cancellation. Tests must prove no gateway query is
made, active work becomes eligible again, and already misclassified local rows
are repaired without fabricating provider evidence.

Third, replace catch-all post-reservation readiness mappings in initial,
renewal, recovery, replacement, and host-charge workflows with exhaustive
workflow-specific mappings. Tests will inject unavailable, misconfigured, and
other supported gateway errors and assert both the immediate service outcome
and persisted failure code.

After every slice, run its focused crate tests and inspect the staged diff before
committing. At the end, run all required Jig checks and gates.

## Concrete steps

From `/home/aa/Documents/syrup-rail`:

1. Edit the recovery/replacement branches and foreground tests with
   `apply_patch`. Run:

       cargo test -p syrup-rail-postgres enrollment_application::tests::foreground

   Success means the new prepared-attempt crash-window tests pass and existing
   foreground behavior remains green. Stage only the plan/state files and slice
   files, inspect `git diff --cached`, and create the first commit.

2. Edit reconciliation selection/transition logic and its tests. If SQLx macro
   metadata changes, refresh only the current committed metadata through the
   repository command. Run:

       cargo test -p syrup-rail-postgres reconciliation

   Success means unsubmitted rows receive no provider exact query and stale
   renewal/recovery rows no longer block lifecycle work. Commit this slice
   separately.

3. Edit readiness-error mapping and focused tests. Run:

       cargo test -p syrup-rail-postgres enrollment_application

   Success means injected gateway errors retain their distinct outcomes and
   durable evidence. Commit this slice separately.

4. Run:

       scripts/jig work check --plan-id plan_01M02TQKTVSPTACVAQ5TM0DGD8
       scripts/jig check fmt
       scripts/jig check clippy
       scripts/jig check test
       scripts/jig check sqlx
       scripts/jig check contract
       scripts/jig work evidence --plan-id plan_01M02TQKTVSPTACVAQ5TM0DGD8
       scripts/jig work gates --plan-id plan_01M02TQKTVSPTACVAQ5TM0DGD8

   Success is zero exit status for all required checks and gates. Review
   `git status --short`, `git log -3 --oneline`, and the full commit range before
   finishing the work item.

## Validation and acceptance

Acceptance is observable through regression tests:

- Given a prepared recovery or replacement attempt and a same-key retry whose
  candidate attempt ID differs, exactly one provider mutation occurs and its
  reference is derived from the original durable ID; the returned result also
  names that durable attempt.
- Given a stale unsubmitted renewal or recovery, reconciliation performs no
  provider query and removes its lifecycle blockage through a deterministic
  local transition. A previously parked unsubmitted row is handled locally too.
- Given each modeled readiness failure after durable reservation, the caller
  sees the correct workflow outcome and the durable attempt contains the
  matching failure code, never an unrelated live-readiness code.
- Repository formatting, linting, tests, SQLx validation, contract checks, and
  Jig gates all pass.

## Idempotence and recovery

Focused tests and checks are safe to rerun. Reservation tests use isolated test
databases supplied by the existing fixtures. If a test fails after a durable
row is created, the fixture cleanup handles it; do not delete shared database
state manually. The schema artifacts are complete versioned files: do not edit
`schema/v1` and do not invoke `scripts/jig migration-add` for this work.

Commits are intentionally incremental. If a later slice fails, leave earlier
commits intact and repair the active slice in the worktree. Do not reset or
overwrite `.agent/state/*.jsonl`; Jig state is append-only. The user's untracked
`.agent/0.2.1-bug-findings.md` remains outside implementation commits unless the
user explicitly asks to add it.

## Interfaces and dependencies

No new external dependency or public schema is planned. Existing domain
constructors `SubscriptionRecoveryReservation::from_attempt` and
`SubscriptionPaymentMethodReplacement::from_attempt` are the continuation
interfaces. Existing `GatewayError`, payment-attempt status/failure-code types,
and reconciliation policy constants remain authoritative; any needed helper is
kept private to the PostgreSQL crate unless tests prove a domain-level contract
is missing.
