# Changelog

All notable changes to the Syrup Rail crates are documented in this file.

## [Unreleased]

The eventual release for these breaking API and schema changes is **0.2.0**.
Version manifests, dependency requirements, publication, and tagging remain a
separate release task.

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
  `DunningSchedule::from_delays` construction.
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
  only a value-redacted masked-card display; history uses a checked cursor page
  size and excludes provider references, transaction IDs, contacts, responses,
  and raw diagnostics.

### Changed

- Separate the economic `next_renewal_at` period anchor from the mutable
  `next_payment_attempt_at` scheduler clock. Only submitted determinate
  automatic-renewal failures consume dunning; operational and provider pacing
  remain independent.
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

### Migration

- Schema-v1 hosts must prebuild the 0.2 application, stop all 0.1 billing
  writers, run `schema/v2/preflight_from_v1.sql`, apply
  `schema/v2/upgrade_from_v1.sql` transactionally, and roll forward with 0.2.
  Schema v1 remains byte-immutable and 0.1 writers must not restart after the
  v2 migration commits.
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
  use billing/payment terminology; variant-specific behavior and error sources
  are unchanged.
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
  `SubscriptionPaymentFailureDisposition`, and the new `SubscriptionEnded`
  event. `Subscription` now exposes phase, recurring-period, failure-policy,
  and scheduler facts. `Entitlement::PastDue` no longer inherently means that
  product access is suspended: use `Entitlement::permits_product_access()` for
  the canonical subscription decision. Inspect `PastDueAccess` separately only
  when the host needs to present the dunning reason. With
  `ContinueUntilDunningExhausted` plus `RemainPastDue`, `DunningExhausted` is
  the access-revocation signal and no `SubscriptionEnded` event follows it.

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

[Unreleased]: https://github.com/bpcakes/syrup-rail/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/bpcakes/syrup-rail/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/bpcakes/syrup-rail/tree/v0.1.0
