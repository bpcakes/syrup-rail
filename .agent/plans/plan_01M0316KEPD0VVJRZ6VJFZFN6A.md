# Close payment recovery review gaps

Repair the four follow-up findings from the `v0.2.0...HEAD` comprehensive
review in isolated commits, then run all required repository checks. The
security baseline is `docs/security/threat-model.md`: attempts that never set
`submitted_at` did not cross the provider boundary and must be retired locally,
while host-owned target state and canonical attempt state must change in one
database transaction.

## Progress

- [x] (2026-08-15) Started a dedicated Jig work item and reviewed the affected
  host-charge, replay-preflight, entitlement, deletion, and reconciliation
  boundaries.
- [x] (2026-08-15) Added bounded, host-aware retirement for stale
  unsubmitted host charges and focused coverage for atomic target release,
  account scoping, and fresh-attempt preservation.
- [x] (2026-08-15) Resolved stale payment-method replacement replays under the
  subscription aggregate lock before host admission or gateway resolution;
  focused foreground coverage proves all three downstream callbacks remain at
  zero calls.
- [x] (2026-08-15) Made entitlement and deletion reads ignore attempts that
  are already locally abandoned by each operation's canonical age policy;
  added fresh-versus-stale entitlement coverage, deletion coverage, and
  refreshed the one affected SQLx query artifact.
- [x] (2026-08-15) Documented the local-before-exact reconciliation phase
  order and the patch-upgrade integration requirement for existing hosts.
- [ ] Run focused tests, repository checks, gates, and final diff review.

## Surprises & Discoveries

- Host charges already expose the required atomic host callback:
  `HostChargeTargetStore::apply_transition(PaymentFailed)`. The existing
  non-approved application path establishes the required lock order: host
  target first, canonical attempt second.
- Payment-method replacement already expires stale attempts during reservation
  and final admission; only its initial idempotency preflight omitted the same
  lock/expire/reload sequence used by recovery.
- The new subscription cleanup phase is public and bounded, but the repository
  has no scheduler implementation. Upgrade guidance must therefore state that
  hosts add it to their existing per-account reconciliation loop.

## Decision Log

- Each behavioral or integration slice is committed separately.
- Preserve retryable host-charge `Unavailable` behavior for the normal retry
  window. Add a later bounded cleanup phase instead of immediately
  terminalizing the attempt, so same-key retry remains available while
  abandoned work gains a deterministic recovery path.
- A host-charge cleanup transition must call the host target store and update
  the canonical attempt in the same transaction. If the candidate becomes
  submitted or terminal concurrently, roll back the host transition and leave
  the current attempt untouched.
- Read-side decisions mirror the stale-local predicate directly. They cannot
  assume a bounded asynchronous cleanup phase has already drained its backlog.

## Outcomes & Retrospective

- Pending.

## Context and orientation

`crates/syrup-rail-postgres/src/subscription_billing_service/host_charge.rs`
preserves a prepared host charge when gateway readiness is temporarily
unavailable. `src/host_charges.rs` owns the host extension and ledger admission
rules; `src/host_charge_application.rs` demonstrates atomic payment-failure
transitions. Reconciliation phases live in `src/reconciliation.rs` and are
exported explicitly from `src/lib.rs`.

Prepared subscriber mutations are preflighted in `src/attempts`. Recovery's
preflight is the reference implementation for locking the subscription
aggregate, expiring a stale matching attempt, reloading it under lock, and
returning the resulting replay before any host callback.

`src/entitlement.rs` and `src/deletion.rs` are read-side projections. Their
blocking predicates must agree with the age and `submitted_at IS NULL`
semantics used by renewal dispatch and local reconciliation.

## Plan of work

First, add an account-scoped host-charge reconciliation phase. It selects a
bounded deterministic candidate page, invokes the host's `PaymentFailed`
transition, revalidates the attempt as stale and unsubmitted, and commits both
changes together. Regression coverage proves the target becomes releasable,
while fresh and sibling-account attempts remain untouched.

Second, make payment-method replacement preflight follow recovery's
candidate-check, aggregate-lock, locked-reload, stale-expiry sequence. Extend
the foreground crash-window test to prove an expired replay reaches neither
host admission nor gateway resolution.

Third, update entitlement and deletion blocker predicates so stale local rows
do not continue to request provider confirmation or block account deletion.
Add focused database tests for fresh-versus-stale behavior. Refresh committed
SQLx metadata if the checked query macro changes.

Fourth, add patch-release upgrade instructions and the canonical local-before-
exact reconciliation phase order to the public PostgreSQL documentation.

## Concrete steps

1. Implement and export host-charge cleanup, run its focused reconciliation
   test, inspect the staged diff, and commit only that slice plus the living
   plan/state updates.
2. Patch replacement preflight and its foreground regression, run the focused
   foreground tests, and commit that slice.
3. Patch read projections and focused tests, refresh SQLx metadata if required,
   run entitlement/deletion tests, and commit that slice.
4. Patch the changelog and public integration documentation, run formatting and
   documentation checks, and commit that slice.
5. Run `scripts/jig work check`, `scripts/jig check fmt`,
   `scripts/jig check clippy`, `scripts/jig check test`,
   `scripts/jig check sqlx`, `scripts/jig check contract`, and the configured
   Jig gates. Review `git status`, the complete commit range, and every changed
   file before closing the work item.

## Validation and acceptance

- A stale prepared host charge is failed locally and its host target receives
  `PaymentFailed` atomically; a fresh or different-account attempt is not
  changed and no gateway query occurs.
- A stale matching payment-method replacement retry returns its failed durable
  attempt without calling host admission, gateway resolution, or provider
  storage.
- A fresh recovery remains `ConfirmRecoveryPayment`, while the same local
  attempt beyond its stale threshold becomes `RecoverPayment` even before the
  cleanup worker runs. Equivalent stale local evidence does not block deletion.
- Upgrade documentation tells existing hosts exactly which bounded phase to add
  before exact provider queries.
- All repository-required checks and gates pass at the final commit.

## Idempotence and recovery

Focused tests use isolated PostgreSQL fixtures and are safe to rerun. Cleanup
queries transition only unresolved rows with `submitted_at IS NULL` and
revalidate under locks, so repeated passes are no-ops. Do not alter schema v1
or overwrite schema v2 artifacts. If SQLx metadata is regenerated, retain only
entries produced by the repository's checked prepare command. The user's
untracked `.agent/0.2.1-bug-findings.md` remains untouched.

## Interfaces and dependencies

No dependency or schema change is required. The host-charge cleanup function
will use the existing `HostChargeTargetStore`,
`HostChargeTargetTransitionKind::PaymentFailed`, and
`HostChargeApplicationError` interfaces. Other changes remain private SQL and
documentation updates unless an explicit root export is required for the new
reconciliation phase.
