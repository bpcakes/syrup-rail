# Fix lifecycle alert claim concurrency on the 0.2 release line

## Progress
- [x] Make the alert cadence claim atomic under concurrent callers.
- [x] Add a deterministic concurrency regression test.
- [x] Prepare and validate the coordinated 0.2.2 patch release.

## Surprises & Discoveries
- The 0.2.1 claim computes due state in a materialized CTE before updating, so PostgreSQL row rechecks cannot reject a waiter after the winner advances the cadence.
- Moving the due predicate onto individual quarantine rows would avoid duplicate winners but change account-level cadence: recently alerted rows could trigger another aggregate alert before one full interval elapsed. A transaction-scoped, nonblocking account claim preserves existing semantics.
- Dependent package dry-runs cannot resolve the unpublished exact 0.2.2 core requirement. The documented dependency-order publish and visibility wait is required after the core dry-run succeeds.

## Decision Log
- Put concurrency ownership in syrup-rail-postgres and remove the host advisory lease; do not expose a transaction callback or require two pool connections.
- Backport from the maintained release/0.2 line in an isolated worktree; do not modify the shared main worktree.

## Outcomes & Retrospective
The canonical PostgreSQL operation now acquires a nonblocking, transaction-scoped account claim on its existing connection. Contenders return no work immediately, while the winner preserves the existing aggregate cadence update. The deterministic blocker regression passes, and the coordinated 0.2.2 release passed release metadata, schema immutability, public API, advisories, formatting, Clippy, contract, full locked tests, and SQLx gates without changing a shipped schema artifact or public signature.

## Context and orientation
crates/syrup-rail-postgres/src/lifecycle_quarantine.rs owns the canonical quarantine alert cadence claim. CreditKit 0.2.1 currently wraps it in a host advisory transaction because concurrent claims can both observe the old due state.

## Plan of work
Acquire the account-scoped alert claim inside the existing canonical transaction so the check and aggregate cadence update have one owner without another pool connection. Add a concurrency test proving a contender returns immediately. Commit the behavior fix, prepare all four coordinated crates as 0.2.2, run the release and repository gates, then publish in dependency order through the documented workflow.

## Validation and acceptance
Run the focused syrup-rail-postgres test, then every required release and Jig gate. Acceptance requires one winner under concurrency, no schema artifact changes, and clean 0.2.2 packages.

## Idempotence and recovery
The SQL operation remains idempotent and no schema migration changes. Work only in this isolated worktree.

## Interfaces and dependencies
No public signature changes. All four crates retain coordinated versions for the patch release.
