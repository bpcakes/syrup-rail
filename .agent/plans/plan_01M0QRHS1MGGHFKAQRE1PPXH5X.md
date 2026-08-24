## Progress

- [x] Claimed the R-02 Beads task and inspected repository/crate guidance.
- [x] Define the validated core lifecycle/schedule representation and compatibility constructor boundary.
- [x] Migrate PostgreSQL hydration to reject invalid persisted combinations.
- [x] Add the core state matrix and persistence regression coverage.
- [x] Run focused suites and repository backend gates.
- [x] Record evidence, close R-02, and sync Beads.

## Surprises & Discoveries

- The worktree contains broad existing changes. Subscription grant edits overlap the same core files but do not touch R-02 fields; preserve them.
- The broad pre-existing edits were consolidated in commit `11e801d` before R-02 implementation resumed, leaving a clean baseline for the six source/documentation files in this task.

## Decision Log

- Preserve the infallible public Subscription::new API for compatibility; add a validated construction path for internal persistence first, matching the task safe slice.
- Represent `SubscriptionLifecycle` as an opaque validated value backed by a closed private enum. This keeps invalid past-due schedules unconstructable while preserving status-based public projections.

## Outcomes & Retrospective

- Added the public `SubscriptionLifecycle` validated construction path while preserving the infallible `Subscription::new` compatibility surface. `BillingPeriod::end_at` now owns the renewal projection, and the lifecycle matrix owns automatic-payment scheduling.
- Migrated the shared complete-row codec and entitlement projection to reject period/renewal mismatches and active, past-due, canceled, or unpaid schedule contradictions as their established invalid-state errors.
- Added exhaustive core and persistence-codec matrices. The core suite passed 113 tests; the PostgreSQL suite passed 226 library tests, five host-integration tests, and doctests. Format, Clippy, contract, SQLx, repository-wide tests, and the explicit public-API/documentation check passed.
- No schema artifact or SQLx metadata changed. R-02 was closed and the Beads JSONL export is current.

## Context and orientation

R-02 owns crates/syrup-rail/src/subscription.rs and PostgreSQL hydration in crates/syrup-rail-postgres/src/subscription_persistence.rs and entitlement.rs. The v3 schema constraint is the persisted state authority.

## Plan of work

Add a closed lifecycle value that couples status with valid scheduling, derive next renewal from BillingPeriod authority, and route PostgreSQL row reconstruction through a fallible validated constructor. Keep existing public projections and the legacy constructor behavior-compatible.

## Concrete steps

Edit the core subscription module and focused tests, update every PostgreSQL Subscription hydrator, add malformed-row rejection cases, format, run focused core/PostgreSQL suites, then run scripts/jig check test and relevant contract/public API gates.

## Validation and acceptance

Core lifecycle matrix passes; persisted contradictory status/schedule or period/renewal rows are rejected; entitlement, cancellation, renewal, dunning, migration, schema, and public API suites remain green.

## Idempotence and recovery

No schema or durable-data mutation is planned. Re-running tests and formatting is safe. Preserve all pre-existing worktree edits and isolate R-02 hunks during review.

## Interfaces and dependencies

Subscription::new remains available. New validated construction and errors are public only as needed by the PostgreSQL crate; no schema or SQLx metadata changes are expected.
