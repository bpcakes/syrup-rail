# Fix lifecycle alert claim concurrency on the 0.2 release line

## Progress
- [ ] Make the alert cadence claim atomic under concurrent callers.
- [ ] Add a deterministic concurrency regression test.
- [ ] Prepare and validate the coordinated 0.2.2 patch release.

## Surprises & Discoveries
- The 0.2.1 claim computes due state in a materialized CTE before updating, so PostgreSQL row rechecks cannot reject a waiter after the winner advances the cadence.

## Decision Log
- Put concurrency ownership in syrup-rail-postgres and remove the host advisory lease; do not expose a transaction callback or require two pool connections.
- Backport from the maintained release/0.2 line in an isolated worktree; do not modify the shared main worktree.

## Outcomes & Retrospective
Pending.

## Context and orientation
crates/syrup-rail-postgres/src/lifecycle_quarantine.rs owns the canonical quarantine alert cadence claim. CreditKit 0.2.1 currently wraps it in a host advisory transaction because concurrent claims can both observe the old due state.

## Plan of work
Move the cadence predicate directly onto the UPDATE so PostgreSQL EvalPlanQual rechecks the current row after lock waits. Add a concurrent claim test proving one winner without an outer lease. Commit the behavior fix, prepare all four coordinated crates as 0.2.2, run the release and repository gates, then publish through the documented release workflow if available.

## Validation and acceptance
Run the focused syrup-rail-postgres test, then every required release and Jig gate. Acceptance requires one winner under concurrency, no schema artifact changes, and clean 0.2.2 packages.

## Idempotence and recovery
The SQL operation remains idempotent and no schema migration changes. Work only in this isolated worktree.

## Interfaces and dependencies
No public signature changes. All four crates retain coordinated versions for the patch release.