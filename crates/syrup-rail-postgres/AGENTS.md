# Syrup Rail PostgreSQL Crate Guide

## Purpose

Own the canonical provider-neutral PostgreSQL schema contract, SQLx queries,
and transaction orchestration.

## Key entrypoints

- `src/lib.rs` — the public PostgreSQL operation facade and crate-private
  module ownership map.
- `schema/v2/install.sql` — current authoritative fresh-install DDL.
- `schema/v2/preflight_from_v1.sql`,
  `schema/v2/audit_retry_reclassification_from_v1.sql`, and
  `schema/v2/upgrade_from_v1.sql` — checked-in read-only preflight,
  informational retry-reclassification audit, and forward-only v1 cutover
  artifact.
- `schema/v1/**` — immutable shipped version-1 distribution artifacts.
- `src/schema_contract.rs` — production read-only v2 runtime compatibility
  assertion plus canonical catalog conformance. Version-specific, upgrade, and
  shared fixture tests live under `src/schema_contract/tests/`; checked-in
  install/upgrade SQL constants remain behind tests or the explicit
  `schema-contract-test-support` feature.
  `src/schema_contract/tests/upgrade/retry_reclassification.rs` owns the
  executable contract for the read-only cutover audit.
- `src/gateway_accounts.rs` — transaction-local gateway account registration
  and exact configuration activation.
- `src/attempts.rs` — stable payment-attempt persistence facade and shared
  error contract. `src/attempts/{initial,renewal,recovery,payment_method_replacement}.rs`
  own the four reservation and final-admission workflows;
  `src/attempts/{shared,persistence}.rs` own common lock/replay primitives and
  the fixed-column row codec/loaders. Initial-enrollment query and replay
  support lives in `src/attempts/initial/support.rs`.
- `src/enrollment_application.rs` — stable facade plus shared outcome,
  application, locking, and persistence support for subscription payment
  workflows.
- `src/enrollment_application/{initial,recovery,renewal,payment_method_replacement}.rs`
  — workflow-owned provider submission, foreground/reconciled outcome
  application, and compensation paths. Initial and payment-method replacement
  approval mutations live in their nested `approval.rs` modules.
- `src/renewal_failure.rs` — the single qualifying automatic-failure history
  predicate and atomic retry, exhausted, or terminal-unpaid projection.
- `src/renewal.rs` — deterministic due-renewal selection and stable
  cursor-paginated dispatch pages. The scan timestamp is observed by
  PostgreSQL; host queue/outbox persistence and eventual renewal submission
  remain outside this crate.
- `src/paid_trial_dunning_tests.rs` — cross-module acceptance scenarios for
  paid-trial enrollment, dunning, recovery, reconciliation, entitlement, and
  cancellation.
- `src/subscription_billing_service.rs` — stable service type, shared closed
  readiness facts, and facade. Its `subscription_billing_service/{enrollment,
  recovery,renewal,payment_method_replacement,host_charge,reconciliation,
  subscriber,subscriber_mutation}.rs` modules own the corresponding
  orchestration and shared subscriber-admission boundary. The
  `subscriber_mutation.rs` owner runs cancellation through the host-prepared
  event transaction and keeps discount claim/clear gateway-free.
- `src/host_charge_application.rs` — host-charge final admission, one-shot
  provider submission, atomic host target/attempt/charge/event application,
  exact reconciliation, and approved-failure compensation.
- `src/transactions.rs` — host-prepared billing transaction and typed event
  projection capability; the host recipient authorization lock comes first.
- `src/entitlement.rs` — exact scope/subscriber/plan entitlement projection and
  caller-transaction protected-write guard.
- `src/billing_portal.rs` — read-only, provider-neutral customer billing
  portal and exact-plan payment-history projections. It reuses entitlement
  semantics inside one repeatable-read snapshot and selects only masked card
  display and safe attempt facts.
- `src/grants.rs` — caller-transaction grant admission, creation, and
  revocation.
- `src/discounts.rs` — exact-plan durable discount operations and the host
  offer-store port; `src/discounts/persistence.rs` owns shared locks, row
  reconstruction, and quote persistence support.
- `src/cancellation.rs` — caller-transaction exact-plan cancellation,
  attempt fences, stale update cleanup, and canonical event production.
- `src/deletion.rs` — transaction-local canonical account-deletion blockers
  and mutable billing-data scrubbing.
- `src/reconciliation.rs` — deterministic registered-account reconciliation
  candidate selection and bounded local account phases;
  `src/reconciliation/classification.rs` owns pending-charge locking and state
  classification.
- `src/lifecycle_reconciliation.rs` and `src/lifecycle_quarantine.rs` — report
  lifecycle application, pending evidence, quarantine alert cadence, and
  operator incident review/resolution.
- `src/operator_review.rs` — stable privileged-review facade and shared error
  contract. `src/operator_review/{pages,manual_failure,external_reversal}.rs`
  own pagination and the two operator workflows; `src/processor_charge_persistence.rs`
  is their neutral shared processor-charge and immutable-attestation codec.
- `src/processor_charges.rs` — canonical charge observation and transition
  facade; `src/processor_charges/storage.rs` owns exact replay,
  transactionless identification, progression derivation, and bounded
  compensating persistence support.
- `src/host_charges.rs` — host target extension port, canonical attempt
  reservation/final admission, and typed caller-transaction access to the
  host-charge `Reserve`, `Submit`, and `Release` ledger modes.

## Edit here for X

- Change canonical tables, constraints, functions, triggers, or views in the
  current versioned schema artifact and supply a forward-only upgrade for any
  materialized version. Never edit `schema/v1/**`.
- Change host conformance or schema behavior tests in the matching
  `src/schema_contract/tests/{v1,v2,upgrade}` module, keep shared setup in the
  fixture modules, and update the catalog fingerprint intentionally.
- Change reusable gateway account/configuration metadata transitions in
  `src/gateway_accounts.rs`; keep host credentials outside this crate.
- Change canonical attempt row parsing/loaders in `src/attempts/persistence.rs`,
  common lock/replay primitives in `src/attempts/shared.rs`, and one operation's
  reservation/final admission in its owning `src/attempts/*.rs` workflow.
  Keep the `src/attempts.rs` facade stable and keep payment tokens and provider
  credentials outside the transaction and durable model.
- Change one subscription payment workflow's provider submission,
  attempt/charge resolution, subscription or payment-method mutation,
  discount progression, or approved-failure parking in its owning
  `src/enrollment_application/{initial,recovery,renewal,payment_method_replacement}.rs`
  module. Put initial and payment-method-replacement approval mutations in the
  corresponding nested `approval.rs`; keep only genuinely shared outcome,
  locking, codec, and transaction support in the root facade. Reconciliation
  must rebuild authority from the exact durable attempt and must not submit
  another provider mutation. Keep the complete application write set on the
  host-prepared transaction.
- Change one foreground workflow in its matching
  `src/subscription_billing_service/*.rs` owner. Put shared subscriber
  admission, resolver identity, cooldown, and readiness behavior in
  `subscriber.rs`; keep the root as the stable service/fact facade and do not
  introduce another subscription payment path in a host adapter.
- Change high-level cancellation or subscriber discount orchestration in
  `src/subscription_billing_service/subscriber_mutation.rs`. Admission must
  precede database work; a changed cancellation, its host event, and commit
  share the coordinator transaction, while discount claim/clear do not resolve
  a gateway or perform provider I/O.
- Change the host transaction/event boundary in `src/transactions.rs`; do not
  add arbitrary SQL callbacks or a production no-op event implementation.
- Change reusable subscription access projection or protected-write admission
  in `src/entitlement.rs`; keep host authentication and gateway availability
  outside the query/guard.
- Change customer billing portal or history SQL in `src/billing_portal.rs`.
  Preserve the exact entitlement projection and one-snapshot transaction;
  never use broad payment-attempt loaders or select provider references,
  transaction IDs, contacts, response text, or diagnostics for this surface.
- Change due-renewal pagination in `src/renewal.rs`. Preserve every current
  eligibility gate, bind the first page's database-observed timestamp into all
  time-dependent gates on every continuation, retain strict ascending
  `(next_payment_attempt_at, subscription_id)` keyset order, and keep
  `due_renewals` as the fixed-limit first-page compatibility wrapper. Do not
  introduce a canonical lease or queue writer; host outbox/queue transactions
  remain host-owned. This freezes eligibility time rather than holding a
  cross-page MVCC snapshot, so concurrent candidates behind a cursor can wait
  for a fresh scan.
- Change grant mutation policy in `src/grants.rs`; keep host user existence,
  actor authorization, and actor presentation in the host transaction.
- Change reusable discount operations or the offer-store port in
  `src/discounts.rs`, and shared row/lock support in
  `src/discounts/persistence.rs`; keep host acquisition metadata in the host
  transaction and host plan pricing behind `SubscriptionOfferStore`.
- Change exact-plan cancellation in `src/cancellation.rs`; the host must lock
  the live or retained event recipient first, append the returned event in the
  same transaction, and leave stored payment methods unchanged.
- Change canonical deletion admission or billing attempt/payment-method scrub
  policy in `src/deletion.rs`; keep host identity, order, fulfillment, and
  retained-subject work in the host transaction.
- Change registered-account reconciliation selection, local stale-attempt
  phases, or exact-query claiming/negative observation transitions in
  `src/reconciliation.rs`, and pending-charge classification in
  `src/reconciliation/classification.rs`; preserve persisted
  plan-key identity and aggregate locks, keep the fixed per-account envelopes,
  and keep host configuration filtering and queue encoding outside the shared
  operations.
- Change report lifecycle persistence or operator quarantine review in
  `src/lifecycle_reconciliation.rs` and `src/lifecycle_quarantine.rs`; keep host
  alert transport, authorization, cursor encoding, and admin presentation out.
- Change processor-charge external-reversal attestation in
  `src/operator_review/external_reversal.rs`, manual failure in
  `src/operator_review/manual_failure.rs`, pagination in
  `src/operator_review/pages.rs`, and shared charge/attestation reconstruction
  in `src/processor_charge_persistence.rs`. Preserve exact plan-bearing
  revalidation, immutable evidence, replay/conflict semantics, and
  same-transaction host release.
- Change processor-charge observation/transition entrypoints in
  `src/processor_charges.rs` and exact replay, progression, or compensating
  storage support in `src/processor_charges/storage.rs`; keep one canonical
  writer, preserve immutable evidence and transaction ownership, and retry only
  explicitly transient database failures outside a caller-owned transaction.
- Change host-charge shared-ledger safety in `src/host_charges.rs`; the host
  target must already be locked on the supplied connection, and ordinary
  unsafe or same-key contender outcomes remain typed rather than errors.
- Change host-charge outcome application or exact reconciliation in
  `src/host_charge_application.rs`; target transition, attempt resolution,
  processor evidence, and event append must share the host transaction.

## Invariants

- No runtime migrator in production service construction.
- `assert_runtime_schema_v2_compatible` must reuse the complete canonical v2
  catalog/fingerprint check in one read-only snapshot; hosts apply versioned
  install and forward-only upgrade artifacts through their own migrations.
- Committed SQLx metadata lives in `crates/syrup-rail-postgres/.sqlx`.
- Provider wire strings belong in `syrup-rail-nmi`, not here.
- The feature-gated `assert_v1_conforms` and `assert_v2_conforms` wrappers are
  also read-only; mutation and locking behavior belongs in package fixtures
  and host-seeded integration tests.
- Host objects attached to canonical relations use explicit host prefixes;
  `billing_*` constraint and index names are reserved for canonical objects.
- Host code composes transaction-local operations on its existing connection;
  shared operations never persist or receive plaintext provider credentials.
- Offer-dependent discount operations lock the exact host plan row through the
  caller's connection; implementations must not open a second connection.
- Enrollment offer locks receive one stable reservation context at both
  reservation and submission-admission stages. Host attempt-history
  eligibility queries exclude the supplied in-flight attempt ID so a prepared
  enrollment cannot disqualify itself.
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
- Host charges use the same replay, cooldown, readiness, committed-admission,
  one-shot submission, and durable evidence rules. Host target policy stays
  behind `HostChargeTargetStore`, whose operations use only the supplied
  connection and lock the target before entering the canonical ledger.
  Preflight snapshots without changing host business state; reservation may
  perform the host's retryable-target transition before the attempt insert.
- Initial approval begins with the host recipient lock, then the shared
  payment-method domain and exact plan aggregate. Method, subscription,
  discount/claim, attempt, processor charge, and `SubscriptionStarted` event
  commit together. Pending confirmation requires a durable review attempt or
  immutable processor-charge observation.
- Recovery derives its due period, amount, subscription identity, and expected
  payment state from the locked canonical subscription. Approval replaces the
  method, advances the period and discount, applies the immutable charge, and
  appends `SubscriptionRenewed` in the same host-prepared transaction.
- New recovery reservations require `past_due`. Final admission and application
  separately honor the exact `active` or `past_due` state snapshotted by an
  already-durable attempt, so preserved v1 authority can complete without
  reopening recovery for active subscriptions.
- `next_renewal_at` remains the economic period anchor;
  `next_payment_attempt_at` alone controls automatic dispatch. Foreground,
  exact reconciliation, and operator review apply qualifying automatic
  customer failures through `renewal_failure.rs` and append every returned
  event before commit.
- A qualifying automatic-renewal failure is terminal attempt history. Late
  approved evidence is retained through processor-charge reversal review
  without rewriting that terminal attempt. Runtime v2 transitions require
  qualifying automatic-renewal history. The v1 cutover additionally accepts a
  submitted, operator-reviewed recovery manually transitioned to `failed` with
  an exact `active` optimistic snapshot as legacy `past_due` provenance: its
  earliest resolution timestamp owns the suspension boundary, it consumes no
  dunning step, and the scheduler resumes at the economic period anchor. Rows
  with neither causal form remain invalid; readers never synthesize payment
  evidence or access timestamps to repair them. Load both forms through the
  causal-history boundary in `renewal_failure.rs`; it owns admission of the
  first v2 automatic result and access timing for cancellation and terminal
  events.
- Discount-code list and disable operations administer durable records without
  consulting the current offer. Only active create/update, validation, and
  claim paths lock the host offer and construct current pricing.
- `unpaid` is terminal financial history: it grants no entitlement, permits no
  renewal or recovery, and cannot keep a payment method enabled as collection
  authority. It is not rewritten to canceled during deletion cleanup.
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
