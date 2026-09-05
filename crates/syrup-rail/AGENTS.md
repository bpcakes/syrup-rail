# Syrup Rail Core Crate Guide

## Purpose

Own validated domain types, pure lifecycle policy, gateway contracts, and
command/outcome types for application-independent subscription billing.

## Key entrypoints

- `src/lib.rs` is the public facade.
- `src/{identity,money,subscription,terms,discount,enrollment,renewal,host_charge,attempt,event,resolution,admission}.rs`
  own validated billing values and closed lifecycle facts, including paid
  trial and recurring terms, relative dunning and terminal nonpayment policy,
  card-safe grant commands, lossless grant audit records, canonical discount
  claim and exact clear commands, immutable enrollment expectations, secret-free initial and
  host-charge reservations, preflight and submission outcomes, durable payment
  results, attempt fingerprints and state snapshots, exact-plan cancellation
  outcomes, typed entitlement guards, subscriber billing-data scrub
  commands/results, stable renewal-dispatch scan cursor/page values, and the
  closed host admission boundary for end-user billing mutations.
- `src/attempt/{fingerprint,snapshots}.rs` own canonical request equality and
  optimistic/durable snapshots; `src/attempt.rs` remains the attempt lifecycle
  and target facade.
- `src/subscription/{grant,discount,access}.rs` own their separate entitlement
  sources and policies; `src/subscription.rs` retains subscription lifecycle
  and cancellation facts while re-exporting the established public paths.
- `src/{gateway,gateway_value,resolver}.rs` own the five-method provider port,
  typed evidence, sensitive values, provider-I/O-free host resolver contract,
  and diagnostic boundary.
- `src/audit_reason.rs` owns private normalization, Unicode length bounds, and
  card-data rejection shared by the distinct public audit-reason types. Their
  errors, accessors, and formatting remain with their domain owners.
- `src/{card_data,policy}.rs` own the provider-neutral PAN scanner and pure
  payment/calendar policy.
- `src/billing_portal.rs` owns the provider-neutral customer billing portal,
  masked-card display, and bounded exact-plan payment-history read types.

## Edit here for X

- Change reusable subscription phases, statuses, terms, dunning disposition,
  entitlement facts, or lifecycle events in their owning core modules and
  keep the public exports explicit in `src/lib.rs`.
- Change provider-independent period calculations in `src/policy.rs`; keep
  wall-clock, persistence, scheduling, and gateway behavior outside core.
- Change gateway-facing traits or evidence values in
  `src/{gateway,gateway_value,resolver}.rs`; do not add provider wire strings.
- Change fingerprint grammar in `src/attempt/fingerprint.rs`, payment/contact
  snapshots in `src/attempt/snapshots.rs`, and attempt lifecycle/targets in
  `src/attempt.rs`; adapt every reservation/reconciliation constructor that
  consumes changed authority.
- Change grant, discount, or access/deletion facts in the matching
  `src/subscription/{grant,discount,access}.rs` owner rather than growing the
  subscription lifecycle facade.
- Change reusable customer billing-read values in `src/billing_portal.rs`.
  Keep fields private, payment-method formatting value-free, and provider
  identifiers, contacts, and diagnostics outside these types. A present
  payment-method display must contain at least one normalized renderable field;
  provider/persistence conversion alone owns the `None` result for absence.
- Change provider-neutral renewal dispatch facts in `src/renewal.rs`. Keep
  cursor fields private and preserve the database-observed scan timestamp with
  its strict scheduling continuation key; queue/outbox ownership stays with
  the host. Cursors are trusted host persistence facts from prior pages, never
  end-user input, and do not create a cross-page database snapshot.

## Invariants

- No SQLx, Axum, Runledger, or application-specific crate dependencies.
- No provider request, response, or lifecycle protocol vocabulary in this
  crate. `PaymentCardBrand` may recognize common card-scheme presentation
  aliases; each provider adapter must conformance-test its documented labels.
- Subscription offers explicitly choose immediate recurring or a positive paid
  trial; accepted cadence, recurring economics, dunning, and access terms are
  immutable authority for replay and reconciliation.
- Initial activation derives its provider charge, opened period, persisted
  phase and next recurring charge, and consumed discount count from
  `SubscriptionActivationProjection`; do not add context-free pricing helpers
  that collapse those temporal facts.
- Only submitted determinate automatic-renewal failures consume dunning.
- `SubscriptionPaymentFailed.outcome.access()` is the canonical post-failure
  product access projection. Consumers must not reconstruct it from the
  disposition projection or a current offer; it reflects snapshotted policy
  and causal failure history.
  Recovery and infrastructure/provider pacing remain distinct, and `Unpaid`
  is terminal collection history with no payment-state authority.
- Provider identifiers, tokens, contacts, and diagnostics have value-free
  ordinary formatting; expose their values only at adapter/persistence edges.
- Preserve exact provider card-brand evidence where reconciliation requires
  it, but derive `PaymentCardBrand` before customer display or host-event
  projection. Unknown provider text must become `Other`, not durable host
  payload.
- Keep `src/lib.rs` exports explicit. The event/outbox boundary enables the
  missing-rustdoc warning so new event fields or variants cannot enter the
  supported facade without consumer guidance; `scripts/check-public-api.sh`
  elevates that warning to an error and enforces the facade and all-feature
  workspace documentation gates.

## Common commands

- `cargo test -p syrup-rail`
