# Syrup Rail PostgreSQL Crate Guide

## Purpose

Own the canonical provider-neutral PostgreSQL schema contract, SQLx queries,
and transaction orchestration.

## Key entrypoints

- `schema/v1/install.sql` — executable schema-v1 candidate and authoritative
  fresh-install DDL once frozen.
- `src/schema_contract.rs` — read-only catalog conformance plus fresh-install
  behavior fixtures behind `schema-contract-test-support`.
- `src/gateway_accounts.rs` — transaction-local gateway account registration
  and exact configuration activation.
- `src/attempts.rs` — typed canonical payment-attempt loading, exact-owner
  idempotency row locking, and token-free enrollment/recovery reservation and
  final submission admission.
- `src/enrollment_application.rs` — committed one-shot initial and recovery
  sale authority, foreground and reconciliation entrypoints into one atomic
  application path, recurring-discount progression, permanent charge
  observation, terminal-race handling, and approved-failure compensation.
- `src/subscription_billing_service.rs` — complete foreground enrollment and
  recovery orchestration from replay-before-admission through fresh cooldown
  and the one-shot provider sale.
- `src/transactions.rs` — host-prepared billing transaction and typed event
  projection capability; the host recipient authorization lock comes first.
- `src/entitlement.rs` — exact scope/subscriber/plan entitlement projection and
  caller-transaction protected-write guard.
- `src/grants.rs` — caller-transaction grant admission, creation, and
  revocation.
- `src/discounts.rs` — exact-plan discount administration, offer-locked
  quoting, saved-claim mutation, and the host offer-store port.
- `src/cancellation.rs` — caller-transaction exact-plan cancellation,
  attempt fences, stale update cleanup, and canonical event production.
- `src/deletion.rs` — transaction-local canonical account-deletion blockers
  and mutable billing-data scrubbing.
- `src/reconciliation.rs` — complete deterministic registered-account
  reconciliation candidate selection and bounded local account phases.

## Edit here for X

- Change canonical tables, constraints, functions, triggers, or views in
  `schema/v1/install.sql` before any host materializes version 1.
- Change host conformance or schema behavior tests in
  `src/schema_contract.rs` and update the catalog fingerprint intentionally.
- Change reusable gateway account/configuration metadata transitions in
  `src/gateway_accounts.rs`; keep host credentials outside this crate.
- Change canonical attempt row parsing, idempotency locking, or enrollment and
  recovery reservation/final admission in `src/attempts.rs`; keep payment
  tokens and provider credentials outside the transaction and durable model.
- Change initial/recovery provider submission, attempt/charge resolution,
  payment-method and subscription mutation, discount progression, or
  approved-failure parking in `src/enrollment_application.rs`; reconciliation
  must rebuild authority from the exact durable attempt and must not submit
  another provider mutation. Keep the complete application write set on the
  host-prepared transaction.
- Change foreground replay, host admission, gateway resolution, cooldown and
  readiness ordering, or reservation-to-sale composition in
  `src/subscription_billing_service.rs`; do not introduce another subscription
  payment path in a host adapter.
- Change the host transaction/event boundary in `src/transactions.rs`; do not
  add arbitrary SQL callbacks or a production no-op event implementation.
- Change reusable subscription access projection or protected-write admission
  in `src/entitlement.rs`; keep host authentication and gateway availability
  outside the query/guard.
- Change grant mutation policy in `src/grants.rs`; keep host user existence,
  actor authorization, and actor presentation in the host transaction.
- Change reusable discount policy in `src/discounts.rs`; keep host acquisition
  metadata in the host transaction and host plan pricing behind
  `SubscriptionOfferStore`.
- Change exact-plan cancellation in `src/cancellation.rs`; the host must lock
  the live event recipient first, append the returned event in the same
  transaction, and leave stored payment methods unchanged.
- Change canonical deletion admission or billing attempt/payment-method scrub
  policy in `src/deletion.rs`; keep host identity, order, fulfillment, and
  retained-subject work in the host transaction.
- Change registered-account reconciliation selection, local stale-attempt
  phases, pending charge classification, or exact-query claiming/negative
  observation transitions in `src/reconciliation.rs`; preserve persisted
  plan-key identity and aggregate locks, keep the fixed per-account envelopes,
  and keep host configuration filtering and queue encoding outside the shared
  operations.

## Invariants

- No runtime migrator in production service construction.
- Committed SQLx metadata lives in `crates/syrup-rail-postgres/.sqlx`.
- Provider wire strings belong in `syrup-rail-nmi`, not here.
- `assert_v1_conforms` is read-only; mutation and locking behavior belongs in
  package fixtures and host-seeded integration tests.
- Host objects attached to canonical relations use explicit host prefixes;
  `billing_*` constraint and index names are reserved for canonical objects.
- Host code composes transaction-local operations on its existing connection;
  shared operations never persist or receive plaintext provider credentials.
- Offer-dependent discount operations lock the exact host plan row through the
  caller's connection; implementations must not open a second connection.
- Initial enrollment reserves a token-free attempt before provider I/O, then
  revalidates plan, claim, billing blockers, attempt fingerprint, and exact
  gateway configuration under locks immediately before submission admission.
- Final admission must commit before it yields the non-cloneable one-shot sale
  capability. No transaction or lock spans provider I/O, and an already
  admitted attempt never yields another sale capability.
- Matching submitted/terminal/stale replay and same-key conflict resolve before
  host admission. A matching prepared retry rebinds to the durable attempt ID;
  a newly generated candidate ID is never exposed to the provider.
- Initial enrollment checks the durable account/provider cooldown before
  reservation, after reservation before readiness, and after committed final
  admission. A post-reservation stop resolves the exact attempt, and provider
  throttle evidence extends its matching cooldown in that same transaction.
- Initial approval begins with the host recipient lock, then the shared
  payment-method domain and exact plan aggregate. Method, subscription,
  discount/claim, attempt, processor charge, and `SubscriptionStarted` event
  commit together. Pending confirmation requires a durable review attempt or
  immutable processor-charge observation.
- Recovery derives its due period, amount, subscription identity, and expected
  payment state from the locked canonical subscription. Approval replaces the
  method, advances the period and discount, applies the immutable charge, and
  appends `SubscriptionRenewed` in the same host-prepared transaction.
- Subscriber scrubbing enters every affected gateway-account payment-method
  domain in deterministic order before mutating attempts or methods and never
  changes immutable processor-charge observations.
- Exact-query claiming returns canonical `PaymentAttempt` values after one
  deterministic, account-scoped durable requery claim. Negative observations
  re-lock and revalidate the immutable shared request before changing status;
  provider I/O never occurs inside that transaction.
- Protected-write admission requires the caller's SQL transaction so accepted
  paid/grant locks remain held through the host mutation; it restores the
  caller's prior transaction-local `lock_timeout` after semantic results.

## Common commands

- `cargo test -p syrup-rail-postgres`
