# Harden payment lifecycle boundaries

This ExecPlan repairs the findings from the comprehensive review of
`v0.2.0...HEAD` at their owning boundaries. The outcome is observable in five
ways: durable cooldowns take precedence over gateway resolution, cancellation
does not fail merely because an abandoned local charge row is contended,
subscription payment results reject structurally unrelated values, replay
identity includes the durable billing-contact projection, and the release line
honestly communicates its breaking compatibility boundary.

The security baseline is `docs/security/threat-model.md`. These changes do not
create a new authentication, credential, or authorization boundary. They
preserve the existing at-most-once provider mutation rule and keep database
transactions out of provider I/O.

## Progress

- [x] Review the merged Claude/Codex findings and reproduce each source-level
  failure path.
- [x] Classify the root causes and choose the owning abstraction for each fix.
- [x] Split durable host-charge cooldown handling from resolved-gateway
  readiness and add resolver-short-circuit regressions.
- [x] Make shared stale subscription-charge cleanup skip contended rows and
  prove cancellation returns its semantic blocker under row contention.
- [x] Strengthen subscription payment-result construction around subscription
  kind, target association, and approved evidence.
- [x] Represent locally expirable statuses as one global provider-boundary
  policy while retaining per-kind expiry ages.
- [x] Treat the durable billing-contact snapshot as immutable replay identity
  for every token-bearing foreground payment command.
- [x] Move the unreleased compatibility line to 0.3.0 and update migration
  guidance and internal dependency metadata.
- [x] Commit each behavioral slice independently, run focused checks after each
  slice, then run all repository-required gates and record the final evidence.

## Surprises & Discoveries

- Host-charge readiness combines a database-only cooldown decision with a
  gateway `account_mode` query. Requiring `ResolvedGateway` for both decisions
  forces resolution before the durable gate can run.
- Every caller of the shared stale subscription-charge cleanup is safe when a
  contended stale row remains a blocker. `FOR UPDATE SKIP LOCKED` can therefore
  be the helper's universal contention behavior instead of a cancellation-only
  mode.
- The locally expirable status set is derived solely from replay phase and is
  identical for every attempt kind. Only the age threshold varies by kind, so
  the current per-kind status accessor communicates a false dimension.
- A payment token is deliberately memory-only and replaceable before first
  submission. Billing contact is different: its normalized projection is part
  of the immutable durable `PaymentAttemptRequest`, so accepting a different
  retry value makes the provider request disagree with the ledger.
- The result type is constructed across a crate boundary, so Rust visibility
  cannot make its constructors private to `syrup-rail-postgres`. The core
  constructors must validate every structural invariant available in the
  supplied values; database atomicity remains the PostgreSQL application's
  responsibility.
- Raw processor evidence is deliberately allowed to omit a transaction ID even
  for an authoritative approved response. A typed `ApprovedProcessorEvidence`
  refinement therefore has to come from `GatewayPaymentOutcome::Approved`, not
  from a heuristic over incomplete evidence fields; exact reconciliation can
  fill the provider identity later.
- Host charge had the same billing-contact replay gap as the three subscription
  commands. Applying the invariant at both command preflight and reservation
  comparison prevents concurrent contenders and direct lower-level callers
  from bypassing it.
- The first full-suite run exposed an older concurrency fixture whose supposedly
  matching host-charge retry changed contact and then waited for provider sale
  admission. The new conflict correctly prevented that signal, so the fixture
  was corrected to refresh only the token and retain the original contact; its
  changed-contact case remains covered separately as a conflict regression.

## Decision Log

- Decision: Model host-charge cooldown and resolved-gateway readiness as two
  ordered operations. Rationale: cooldown needs only durable account state;
  provider readiness needs a resolved gateway. The signature should make the
  prerequisite distinction visible rather than relying on call order inside a
  combined helper.
- Decision: Add `FOR UPDATE SKIP LOCKED` to the shared foreground stale-charge
  cleanup. Rationale: leaving a contended abandoned row untouched is safe
  because every caller immediately performs a blocking-attempt check; this
  preserves a semantic rejection and avoids transient lock errors.
- Decision: Make expirable status vocabulary an associated global policy and
  keep only `stale_after_seconds` per kind. Rationale: provider submission
  phase, not attempt kind, determines whether local expiry is legal.
- Decision: Validate that payment results contain a subscription attempt,
  approved evidence where required, and an applied subscription whose ID and
  plan match the attempt target. Rationale: these facts are available in core
  and should not remain caller obligations.
- Decision: Compare the normalized durable contact snapshot during command
  replay. A refreshed one-shot token and candidate attempt ID remain allowed;
  changed contact returns the existing idempotency conflict.
- Decision: Release the accumulated public API and operational cutover as
  0.3.0 rather than weakening the new invariant through compatibility escape
  hatches. Rationale: restoring the old unchecked constructors would preserve
  source compatibility by reintroducing invalid states.

## Outcomes & Retrospective

The fixes now live at the boundaries that own each invariant: durable cooldown
admission precedes resolution; local cleanup owns nonblocking lock behavior;
core result constructors own representable state coherence; provider-boundary
phase owns expirable status vocabulary; replay matching owns immutable contact;
and release metadata owns the compatibility claim. No schema artifact changed.

Each production slice was committed separately, followed by one focused test
fixture correction discovered by the full suite. Formatting, warning-free
Clippy, the complete workspace test suite, SQLx validation, and the repository
contract check all pass on the 0.3.0 tree. Final plan-linked receipts are
recorded by the Jig work gates before this plan is closed.

## Context and orientation

Core result construction lives in `crates/syrup-rail/src/enrollment.rs`, with
provider evidence in `crates/syrup-rail/src/gateway.rs`. Foreground orchestration
lives under `crates/syrup-rail-postgres/src/subscription_billing_service`.
Replay identity and local cleanup live under
`crates/syrup-rail-postgres/src/attempts`, while cancellation composes the
cleanup in `src/cancellation.rs`. Release metadata lives in the workspace
manifest, package manifests, lockfile, changelog, and public guides.

## Plan of work

Implement each decision as a separate green commit. Add focused unit or
PostgreSQL integration coverage at the same time as its production change.
After all slices, update this living plan, run `scripts/jig work check`, the
repository formatting, Clippy, test, SQLx, and contract gates, and finish the
Jig work session only after the final diff is clean.

## Concrete steps

1. Split host-charge cooldown handling and add account/provider cooldown tests
   whose resolver records zero calls.
2. Change shared subscription-charge cleanup to select stale rows with
   `FOR UPDATE SKIP LOCKED`; add a cancellation test that locks a stale attempt
   on a second connection and observes `BlockedByRenewal`.
3. Add evidence and target-association validation to the core payment-result
   constructors; migrate test fixtures and reuse the core evidence predicate in
   PostgreSQL application code.
4. Remove kind from the expirable-status accessor, update every SQL binder, and
   retain the exhaustive state matrix.
5. Add normalized billing-contact equality to initial, recovery, and
   payment-method-replacement command replay matching; cover changed-contact
   prepared retries without downstream calls.
6. Bump workspace and internal dependency versions to 0.3.0, refresh the
   lockfile, and update release/migration wording.
7. Run focused tests after each step, commit it, then run the complete required
   gates and record receipts.

## Validation and acceptance

- `cargo test -p syrup-rail`
- Focused `syrup-rail-postgres` tests for readiness, cancellation, and prepared
  replay identity.
- `scripts/jig check fmt`
- `scripts/jig check clippy`
- `scripts/jig check test`
- `scripts/jig check sqlx`
- `scripts/jig check contract`
- `scripts/jig work check --plan-id plan_01M04XWY09FNCN49ZXBPJAFFBV`
- `scripts/jig work evidence --plan-id plan_01M04XWY09FNCN49ZXBPJAFFBV`
- `scripts/jig work gates --plan-id plan_01M04XWY09FNCN49ZXBPJAFFBV`

Success means all commands pass, each slice is committed separately, the only
untracked pre-existing file remains `.agent/0.2.1-bug-findings.md`, and no
provider call or durable state transition can bypass the repaired boundaries.

## Idempotence and recovery

All code edits are ordinary forward changes and can be rerun through their
focused tests. PostgreSQL tests use disposable databases. If a gate fails,
repair only the owning slice and rerun that focused check before repeating the
full gate; do not rewrite schema v1 or reset unrelated worktree state.

## Interfaces and dependencies

No new external dependency or schema migration is required. Public API changes
remain confined to the already-breaking 0.3.0 line. Existing provider, host
target, transaction coordinator, and SQLx boundaries remain in place.
