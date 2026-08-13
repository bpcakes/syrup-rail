# Changelog

All notable changes to the Syrup Rail crates are documented in this file.

## [Unreleased]

## [0.2.0] - 2026-08-13

### Added

- Add explicit immediate-recurring and positive paid-trial offer terms with
  fixed-day or calendar-month billing periods, separate introductory and
  recurring prices, and durable versioned enrollment snapshots.
- Add `Entitlement::permits_product_access()` as the canonical subscription
  entitlement decision while retaining host authentication and authorization
  as host responsibilities.
- Add the redacted, request-scoped `SubscriptionPaymentContext` shared by
  enrollment, recovery, and payment-method replacement commands.
- Add checked `DunningRetryDelay` constructors for whole hours, whole days, and
  exact `std::time::Duration` values, plus array-friendly
  `DunningSchedule::from_delays` construction. Both iterator-based schedule
  constructors consume at most the supported sixteen steps plus one overflow
  witness.
- Add per-subscription relative dunning schedules, configurable exhausted
  behavior and past-due access, terminal `unpaid` state, and provider-neutral
  nonpayment lifecycle events.
- Add PostgreSQL schema v2 fresh-install, read-only v1 preflight and retry-
  reclassification audit, and transactional forward-only v1-to-v2 upgrade
  artifacts with fresh/upgrade catalog-parity coverage.
- Add high-level subscriber cancellation and discount claim/clear operations.
  They run exact end-user mutation admission; cancellation commits its
  canonical mutation and typed outbox event through one host-prepared
  transaction, while discount operations preserve their typed semantic
  outcomes without gateway or provider I/O.
- Add provider-neutral customer billing portal and exact-plan payment-history
  reads. Portal snapshots preserve canonical entitlement semantics and expose
  only a value-redacted, nonempty masked-card display; normalized absence is
  represented only by `None`. History uses a checked cursor page size and
  excludes provider references, transaction IDs, contacts, responses, and raw
  diagnostics.
- Add stable `due_renewals_page` dispatch pagination. Its cursor carries the
  first PostgreSQL-observed scan timestamp and strict scheduling key so hosts
  can drain more than one hundred unchanged due subscriptions without
  offset/timestamp-tie gaps or repeats. It freezes eligibility time rather than
  taking a cross-page snapshot, so concurrent mutable candidates can wait for
  a fresh scan; it is not a queue lease, and `due_renewals` remains the
  compatible fixed-first-page helper.
- Add default-feature `assert_runtime_schema_v2_compatible` startup validation.
  It uses the exact schema-contract catalog and fingerprint checks in one
  repeatable-read, read-only transaction, requires PostgreSQL 18, accepts
  separately named host-prefixed objects, and fails closed for v1, added
  canonical columns, or other canonical drift without embedding or exposing a
  production migrator. `SUPPORTED_POSTGRES_MAJOR_VERSION` exposes the required
  major to host startup code.
- Add non-exhaustive `SubscriptionBillingServiceError` dispositions for
  conflict, rejected, temporarily unavailable, misconfigured, and internal
  failures. Hosts can use the conservative retry helpers without exposing
  gateway diagnostics; retryability permits only resubmitting the same
  idempotent command and does not guarantee success.

### Fixed

- Keep runtime schema-v2 validation available during PostgreSQL concurrent
  reindexing by excluding only invalid `_ccnew` and `_ccold` shadow indexes
  whose locks and visible PostgreSQL progress both identify a non-initializing
  `REINDEX CONCURRENTLY`. Recheck that live evidence before commit and retry a
  transition once from a fresh snapshot; stale, cross-role-hidden, or
  independently created suffix-bearing indexes remain fail-closed.
- Make the dependency-advisory wrapper run without an empty-array expansion
  when no RustSec exception is needed, including on Bash versions before 4.4.
- Narrow the internal offer comparison helper to ignore only the recurring
  amount; currency and paid-trial pricing remain accepted terms.
- Enforce complete PostgreSQL schema artifacts from their first stable release
  tag, so shipped versions remain byte-immutable while the current unreleased
  version can still be corrected before publication.

### Changed

- Separate the economic `next_renewal_at` period anchor from the mutable
  `next_payment_attempt_at` scheduler clock. Only submitted determinate
  automatic-renewal failures consume dunning; operational and provider pacing
  remain independent.
- Remove renewal dispatch's forced full-candidate CTE materialization and make
  its ordered outer limit eligible for an early-stopping index plan. PostgreSQL
  plan choice remains cost-based, so hosts must rehearse representative data.
  Align both renewal and exact-plan payment-history keysets with explicit
  schema-v2 indexes. The runtime contract validates each complete index shape,
  including its table, access method, uniqueness, key ordering and null
  behavior, operator classes, included columns, predicate, and planner/write
  readiness.
- Replace raw protected-write transactions with a pool-created top-level
  `EntitlementWriteTransaction` that cannot commit before admission and an
  `AdmittedEntitlementWriteTransaction` returned only on success. Completed
  denials and SQL failures await a full rollback; cancellation remains
  fail-closed through ownership. Successful guards restore the caller's
  timeout and retain their locks through the host write.
- Make renewal, recovery, reconciliation, operator review, entitlement,
  cancellation, grants, deletion, and payment-method cleanup exhaustive over
  paid trials, scheduled dunning, and terminal unpaid history.
- Require `past_due` for fresh recovery reservations while honoring the exact
  `active` or `past_due` authority snapshotted by unresolved v1 attempts across
  the v2 maintenance cutover.
- Treat finite `LimitedMonths` discounts as recurring-calendar-month terms;
  paid trials consume zero discount periods, and incompatible recurring
  cadences are rejected through a dedicated `LimitedDiscountCadence` error
  before an attempt is created.
- Give subscriber-specific enrollment offer locks a typed reservation context
  with lifecycle stage, stable attempt identity, and idempotency identity.
  Attempt-history eligibility must exclude the supplied in-flight attempt.
- Mark `SubscriptionBillingServiceError` non-exhaustive. Consumers must retain
  a wildcard when matching its disposition, use `retry_after()` only as an
  exact delay when it is present, and rebuild/reconcile rather than retry a
  conflict as-is.
- Reduce provider card-brand text to the closed `PaymentCardBrand` vocabulary
  before customer display or host event projection. Unknown nonempty values
  become `Other`; exact provider evidence remains available only at its
  explicit persistence and reconciliation boundary. Recognize NMI's documented
  `diners` label as `DinersClub` and conformance-test its card-scheme vocabulary.
- Keep arbitrary host callback errors out of ordinary formatting and the
  standard error-source chain. Hosts that intentionally need the original
  value consume the wrapper with `into_source()`.

### Developer experience

- Replace wildcard crate-facade exports with explicit reviewed export lists
  while preserving the pre-release core and PostgreSQL root API inventories.
  Add a layered public-API guide and enforce rustdoc on the closed
  billing-event, high-level service, and host transaction boundaries. The
  small-project CI gate deliberately avoids a custom compiler-diagnostic debt
  baseline: it checks readable facades, rejects wildcard exports, builds
  warning-free docs, and runs doctests.
- Add a compile-tested host-owned version-1 billing-event envelope example.
  It retains the admitted event subject, maps every closed event variant,
  separates host kind/version and semantic-key columns from a minimized
  payload, and keeps Rust domain types out of the wire-format contract. Its
  replay value compares every stable field while excluding first-write
  identifiers/timestamps. A concrete same-transaction helper compares the
  untouched split columns and structural JSONB payload before exact typed
  reconstruction, so unknown or normalized fields cannot become an accepted
  replay. V1 owns every nested enum label instead of inheriting future core
  display changes. Its `Debug` output exposes only schema version and event
  kind, never subject identifiers or payload values.
- Package a focused README and explicit proprietary notice with every crate.
  Release preflight now rejects a package that omits either file.
- Verify all workspace targets on the declared Rust 1.88 minimum in CI and the
  release workflow. Add a RustSec gate whose sole exception is the unreachable
  RSA implementation locked behind SQLx's unused MySQL feature; the gate fails
  if that implementation ever enters a workspace build graph.

### Migration

- Schema-v1 hosts must prebuild the 0.2 application, stop all 0.1 billing
  writers, run `schema/v2/preflight_from_v1.sql`, apply
  `schema/v2/upgrade_from_v1.sql` transactionally, and roll forward with 0.2.
  Schema v1 remains byte-immutable and 0.1 writers must not restart after the
  v2 migration commits.
- Budget that maintenance window for the upgrade's transactional replacement
  of the renewal-dispatch index and construction of the exact-plan
  payment-history index. PostgreSQL scans the full payment-attempt heap while
  building that partial index and stores only non-host-charge entries, so the
  build can dominate large upgrades; rehearse against representative data
  rather than changing the atomic artifact to `CREATE INDEX CONCURRENTLY`.
- The upgrade intentionally reclassifies dunning history. Version 1's global
  five-attempt ceiling combined automatic-renewal and subscriber-recovery
  failures; version 2 counts only submitted determinate automatic renewals.
  Some `active` or `past_due` subscriptions that had stopped dispatching under
  v1 can therefore resume automatic collection after cutover. Run
  `schema/v2/audit_retry_reclassification_from_v1.sql` before maintenance and
  review the returned accounts.

### API migration from 0.1

- Construct one `SubscriptionPaymentContext` from the authorized subscriber
  payment request, then pass it with operation-specific data to
  `EnrollSubscription::new`, `RecoverSubscriptionPayment::new`, or
  `ReplaceSubscriptionPaymentMethod::new`. The context keeps ordinary debug
  output free of idempotency-key, payment-token, and billing-contact values.
- Rename `SubscriptionEnrollmentServiceError` to
  `SubscriptionBillingServiceError`. The service error covers enrollment,
  recovery, renewal, payment-method replacement, reconciliation, and optional
  host-charge orchestration. Its generic storage and application messages now
  use billing/payment terminology while retaining typed error sources.
- Standard error-chain traversal no longer reaches arbitrary application
  errors supplied by host transaction, event, charge-target, or operator-review
  callbacks. Classify a `SubscriptionBillingServiceError` first; only in an
  explicitly protected diagnostic path, destructure its owned callback-error
  variant and consume that wrapper with `into_source()`. Generic error
  reporters should retain the intentionally terminated source chain.
- Provider-free cancellation and discount transactions now surface pool
  acquisition timeouts and PostgreSQL serialization, deadlock, lock-timeout,
  and statement-timeout failures as `StorageTemporarilyUnavailable`. Its
  disposition is retryable without an invented delay. Other storage failures,
  including ambiguous provider-facing paths, remain internal.
- Renewal dispatch selection now applies non-attempt eligibility first and
  probes the indexed attempt history only for each candidate's exact current
  billing period. Eligibility, ordering, page size, and cursor semantics are
  unchanged; unrelated historical attempts are no longer globally aggregated
  for every page.
- Replace `next_monthly_billing_period` and `MonthlyBillingPeriodError` with
  `next_billing_period(start_at, rule)` and `BillingPeriodPolicyError`. Pass
  `SubscriptionPeriodRule::calendar_months(1)` for the former monthly behavior.
- Construct `SubscriptionOffer` with `RecurringSubscriptionTerms`,
  `SubscriptionStart`, and `RenewalFailurePolicy`. Replace
  `SubscriptionEnrollmentExpectedCharge` and `expected_charge()` with
  `SubscriptionEnrollmentExpectedTerms` and `expected_terms()`.
- Replace overrides of `SubscriptionOfferStore::lock_enrollment_offer` with the
  new `(connection, SubscriptionEnrollmentOfferContext)` signature. Branch on
  `context.stage()` only for stage-specific locking, and exclude
  `context.attempt_id()` from historical eligibility queries.
- After replacing the version-0.1 enrollment charge expectation with
  `SubscriptionEnrollmentExpectedTerms`, use `activation_projection()` when
  applying accepted terms. Its `initial_charge()` authorizes enrollment and
  `recurring_charge_after_initial()` is stored for the next recurring
  collection; these differ for an immediate start with a one-period discount.
- Use `PaymentAttemptFingerprint::for_subscription_initial_v2` for new
  enrollment terms. The `_v1` constructor and matcher exist only to recognize
  durable version-1 attempts during migration and reconciliation.
- Remove handling for `CancelSubscriptionOutcome::BlockedByPastDue`.
  Cancellation during dunning now returns `Canceled` after the existing
  in-flight renewal and payment-method-update fences pass.
- Treat cancellation outcomes as lifecycle outcomes: the newest exact canceled
  subscription returns `AlreadyCanceled` even after paid-through access expires,
  while a newest terminal `unpaid` subscription returns `NotFound`. Here,
  `NotFound` means there is no cancelable lifecycle; retained financial history
  may still exist.
- Discount-code administration now returns durable
  `SubscriptionDiscountCodeRecord` values. Listing and disabling codes no longer
  require a current offer or attempt to construct a quote; use
  `validate_subscription_discount_code` for current-offer eligibility and
  pricing. Active create/update operations still validate the locked offer.
- Replace customer uses of `MAX_RENEWAL_TERMINAL_ATTEMPTS_PER_PERIOD` and
  `RENEWAL_RETRY_AFTER_SECONDS` with the offer's `RenewalFailurePolicy`. Use
  `RENEWAL_INFRASTRUCTURE_RETRY_AFTER_SECONDS` or
  `RENEWAL_PROVIDER_RATE_LIMIT_SLOW_RETRY_AFTER_SECONDS` only for their named
  operational pacing paths. Prefer checked `DunningRetryDelay::hours`,
  `DunningRetryDelay::days`, or `DunningRetryDelay::try_from(Duration)` values
  with `DunningSchedule::from_delays`; the second-based APIs remain available
  for persistence adapters.
- Update event consumers for `SubscriptionStarted.phase`, the closed
  `SubscriptionPaymentFailureDisposition`, the accompanying
  `SubscriptionPaymentFailureAccess`, and the new `SubscriptionEnded` event.
  The failure event's `access` field is the canonical post-failure access fact;
  do not infer access from its disposition or current offer. `Subscription`
  now exposes phase, recurring-period, failure-policy, and scheduler facts.
  `Entitlement::PastDue` no longer inherently means that product access is
  suspended: use `Entitlement::permits_product_access()` for the canonical
  subscription decision. Inspect `PastDueAccess` separately only when the host
  needs to present the dunning reason. With
  `ContinueUntilDunningExhausted` plus `RemainPastDue`, the final failure's
  `access: Ended { .. }` is the revocation signal and no `SubscriptionEnded`
  event follows it.
- Treat `SchemaConformanceError` as non-exhaustive and handle
  `UnsupportedPostgresVersion` during startup. PostgreSQL 18 is the sole
  supported major; `SUPPORTED_POSTGRES_MAJOR_VERSION` is the machine-readable
  requirement.

## [0.1.1] - 2026-08-09

### Fixed

- Normalize PostgreSQL timestamps to database precision so round trips do not
  produce false optimistic-state mismatches.

### Changed

- Centralize locked payment-attempt terms and subscriber-readiness policy.
- Consolidate payment outcome, approved-application, and fallback processor
  evidence persistence while preserving the existing public constructors.

### Maintenance

- Restore clean-runner Jig bootstrap and enforce schema-backed SQLx metadata in
  repository policy CI.
- Add a release preflight and a manual crates.io trusted-publishing workflow
  that uses short-lived OIDC credentials.

## [0.1.0] - 2026-08-08

- Initial crates.io release of `syrup-rail`, `syrup-rail-postgres`,
  `syrup-rail-nmi`, and `syrup-rail-nmi-client`.

[Unreleased]: https://github.com/bpcakes/syrup-rail/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/bpcakes/syrup-rail/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/bpcakes/syrup-rail/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/bpcakes/syrup-rail/tree/v0.1.0
