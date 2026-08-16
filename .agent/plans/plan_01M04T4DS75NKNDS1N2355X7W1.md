# Enforce local-attempt and payment-result invariants

Centralize the provider-neutral classification of never-submitted payment attempts so cleanup and blocking consumers share one policy, then make payment-result construction encode whether subscription application occurred. Preserve existing public read methods while preventing invalid construction inside the workspace.

## Progress

- [x] Inventory every stale/local predicate and payment-result constructor.
- [x] Add the authoritative classification and state-matrix contract tests.
- [x] Migrate cleanup and blocking consumers without weakening their transaction boundaries.
- [x] Replace unconstrained result construction with applied, not-applied, and confirmation-pending constructors and migrate every caller.
- [x] Add regressions for stale review rows and parked replacement replay parity.
- [x] Run focused and repository-required gates and audit the final diff.

## Surprises & Discoveries

- The durable submitted-at boundary and replay disposition are sound; drift exists because consumers restate the same status/submission/age policy in SQL.
- SubscriptionEnrollmentPaymentResult documents an applied-only subscription but its general constructor accepts any Option.
- The cancellation path has distinct `SKIP LOCKED` cleanup semantics, so it shares the classifier and blocker while retaining its contention behavior.
- SQLx query macros encode PostgreSQL text arrays as `&[String]`; the shared policy therefore materializes its derived status vocabulary once as a lazy string slice.
- A plan-scoped repeat of the already-green full test gate encountered disposable-container startup timeouts while another repository's workspace tests were running under high machine load; no Syrup Rail assertion failed, and a bounded-concurrency full rerun produced the fresh passing receipt.

## Decision Log

- Use a narrow shared policy classification rather than merging payment workflows or introducing a broad union abstraction.
- Keep public result observation stable; constrain construction through semantically named constructors.
- Derive SQL status parameters from the existing replay disposition and keep attempt-kind-specific expiry timing in the same `LocalAttemptPolicy`.
- Treat an approved attempt without a loadable applied subscription as invalid durable state instead of recreating the old ambiguous approved-plus-None result.

## Outcomes & Retrospective

Implemented one `LocalAttemptPolicy` whose status vocabulary derives from the replay disposition and whose per-kind timeout is exhaustive over `PaymentAttemptKind`. Cleanup and stale-aware blocking consumers now bind that policy, with cancellation retaining its distinct `SKIP LOCKED` transaction behavior. The exhaustive state-matrix test covers every kind, status, submission state, and stale/fresh state.

Replaced the unconstrained optional-subscription payment result with private applied, not-applied, and confirmation-pending states plus checked constructors. Approved replay now fails closed when its applied subscription is missing, and the payment-method stale-application path no longer exposes an unrelated subscription. Regressions cover stale review cleanup in cancellation, renewal dispatch and the service preflight, plus parked approval/replay parity.

Final plan-scoped gates are fresh and green: full tests, SQLx metadata, and repository contract. Repository fmt and clippy checks also pass. The full test gate was rerun with `RUST_TEST_THREADS=8` after an earlier plan-scoped repeat encountered external disposable-container startup pressure; all tests passed under bounded concurrency.

## Context and orientation

Core result types live in crates/syrup-rail/src/enrollment.rs. PostgreSQL replay and stale-attempt policy lives under crates/syrup-rail-postgres/src/attempts, with consumers in renewal, cancellation, entitlement, deletion, and reconciliation. Follow docs/security/threat-model.md: submitted_at is the durable provider-boundary evidence.

## Plan of work

First define and test one state matrix for local attempt disposition. Expose only the narrow facts SQL consumers need, migrate every copied predicate and timeout to that authority, and add integration regressions. Then replace the unconstrained payment result constructor, migrate all construction sites, and prove parked results cannot expose applied subscription state or change shape on replay.

## Validation and acceptance

Run focused core and PostgreSQL tests, then scripts/jig check fmt, scripts/jig check clippy, scripts/jig check test, scripts/jig check sqlx, and scripts/jig check contract. Completion requires searches proving no obsolete predicate constants or unconstrained result constructor remain.
