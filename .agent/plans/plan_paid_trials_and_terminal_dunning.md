# Add optional paid trials and terminal subscription dunning

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept current as work proceeds. Maintain this document in accordance with `.agent/PLANS.md` from the repository root.

## Purpose / Big Picture

After this work, a host application can use Syrup Rail to sell an optional paid introductory period and then collect a differently priced recurring subscription without building a second billing engine. The motivating host, IdentityPro, will be able to charge USD 1.00 for seven days, attempt the normal one-calendar-month charge at the end of those seven days, retry determinate payment failures according to a host-supplied dunning schedule, and end the subscription in a durable `unpaid` state after the configured retries are exhausted.

The behavior remains reusable. A host that does not offer a trial selects an immediate recurring start. A host that does not want automatic terminalization can retain the current exhausted `past_due` behavior. Syrup Rail records provider-neutral lifecycle events; it must not import or call IdentityPro's Array integration. IdentityPro will consume the terminal nonpayment event in its own transactional outbox and asynchronously remove the corresponding Array enrollment.

The result is observable through core policy tests, PostgreSQL schema conformance and upgrade tests, and end-to-end package tests that demonstrate the exact paid-trial-to-monthly transition, retry timing, successful recovery, terminal nonpayment, cancellation during dunning, stale/reconciled outcomes, and the unchanged no-trial path.

## Progress

- [x] (2026-08-09 09:00Z) Read `AGENTS.md`, `agent-map.md`, both affected crate guides, `.agent/PLANS.md`, and `docs/security/threat-model.md`.
- [x] (2026-08-09 09:00Z) Traced the current offer, enrollment, attempt fingerprint, subscription, renewal, recovery, entitlement, cancellation, operator-review, event, and schema-v1 paths.
- [x] (2026-08-09 09:00Z) Resolved the domain and persistence decisions recorded below and wrote this initial ExecPlan.
- [x] (2026-08-09 09:48Z) Revalidated the plan against discount, SQLx-gate, schema parity, provider-preflight, cancellation-replay, migration-tooling, and deployment-cutover paths; incorporated the concrete blockers recorded below.
- [x] (2026-08-09 10:14Z) Revalidated infrastructure pacing, automatic-failure history ownership, terminal-row cancellation replay, and payment-method reference predicates; corrected the remaining cross-path gaps recorded below.
- [x] (2026-08-09 10:40Z) Revalidated discount-duration semantics, v2 writer/fixture cutover order, migration anomaly discovery, and the public past-due entitlement shape; incorporated the newly found execution blockers below.
- [x] (2026-08-09 11:15Z) Added immutable schema-v2 fresh-install, read-only v1 preflight, and transactional v1-to-v2 upgrade artifacts; proved catalog parity, legacy data backfill, anomaly rollback, constraint shapes, and v1 byte immutability.
- [x] (2026-08-09 11:25Z) Implemented and tested provider-neutral terms, generic periods, paid-trial enrollment expectations, status/phase, dunning dispositions, access classification, terminal events, and discount cadence rules in `syrup-rail`.
- [x] (2026-08-09 12:39Z) Persisted complete versioned enrollment terms; implemented full-price and discounted paid trials, durable reconstruction, foreground/reconciled approval, replay, initial-decline safety, immediate-recurring compatibility, and zero-applied trial discounts in `syrup-rail-postgres`.
- [x] (2026-08-09 12:39Z) Replaced customer-failure pacing with snapshotted dunning and one atomic automatic-failure projection shared by foreground, exact reconciliation, and operator review; proved recovery, replay/stale re-entry, terminal unpaid, late approval parking, and pre-provider scheduler fences.
- [x] (2026-08-09 12:59Z) Updated entitlement, cancellation, deletion, grants, views, payment-method cleanup, and remaining subscription consumers for paid-trial, scheduled-past-due, canceled, and unpaid states; the complete PostgreSQL package passes 96 tests.
- [x] (2026-08-09 12:59Z) Refreshed SQLx against schema v2 (no metadata drift), documented the public terms and maintenance cutover, updated guides/workflow paths/changelog, passed every repository gate, proved v1 byte immutability, and completed the final diff/security/obsolete-reference audit.
- [x] (2026-08-09 14:10Z) Corrected the cutover authority seam found by comprehensive review: fresh v2 recovery remains `past_due`-only, while prepared, submitted-pending, unknown, and review-required v1 attempts with exact `active` snapshots can complete after upgrade. Added the compiled public example, removed the unused retry aggregate, covered non-default paid-trial operator review, and reran every required gate.
- [x] (2026-08-09 14:40Z) Made the intentional v1-to-v2 retry reclassification operationally explicit with a read-only candidate audit and a regression proving that renewal-only dunning resumes after recovery failures cease counting. Removed the partial monthly compatibility shim, added cadence-specific discount errors, documented the breaking 0.1-to-0.2 API replacements, and reran the full required gate set.
- [x] (2026-08-09) Corrected the host-boundary model found by follow-up review: enrollment offer locks now receive one typed reservation identity and explicit stage, activation pricing is exposed only through the temporal projection, repository/schema ownership guidance matches the v2 workflow and split modules, and integration coverage exercises host eligibility, non-monthly cadence, and concurrent failure replay.
- [x] (2026-08-10) Closed the final comprehensive-review findings at their owning boundaries: accepted operator-reviewed/manual-failed v1 recovery-only `past_due` state through cutover without consuming dunning, rejected similar recovery rows without that provenance, preserved the suspension timestamp through subsequent v2 transitions, documented the host entitlement decision, corrected the 0.1 API migration text, completed responsibility-based module splits, and passed every required repository gate with 117 PostgreSQL tests.
- [x] (2026-08-10) Corrected the v1 retry-reclassification audit at its source boundary: made it a checked-in executable read-only artifact, included both `active` and `past_due` histories plus zero automatic failures, and added exact inclusion and exclusion fixtures for the cutover cases.
- [x] (2026-08-10) Documented exhausted `RemainPastDue` as an explicit host access-revocation boundary without a `SubscriptionEnded` event, aligned the release documentation around all four immutable schema-v2 SQL artifacts, extracted the audit regression into its owning module to satisfy the staged LOC policy, and reran every required gate before commit.

## Surprises & Discoveries

- Observation: `SubscriptionOffer` currently carries only `plan_key` and one `base_charge`, so an initial price and a recurring price cannot both survive admission and reconciliation.
  Evidence: `crates/syrup-rail/src/money.rs` defines the complete type at `SubscriptionOffer`.

- Observation: initial approval always creates a monthly period. For a non-discounted enrollment, the amount copied to `billing_subscriptions.amount_cents` is the initial attempt amount. A USD 1.00 workaround would therefore create a one-month USD 1.00 subscription rather than a seven-day trial followed by the normal charge.
  Evidence: `apply_approved_on_connection`, `insert_subscription`, and `recurring_amount_after_initial` in `crates/syrup-rail-postgres/src/enrollment_application.rs`.

- Observation: schema v1 makes `next_renewal_at` equal `current_period_end_at`, while the scheduler also treats it as the time at which a retry is due. The `SubscriptionPaymentFailed` event returns that already-due boundary even though candidate selection actually waits a hard-coded 24 hours after the last terminal attempt.
  Evidence: `billing_subscriptions_period_check` in `crates/syrup-rail-postgres/schema/v1/install.sql`, `due_renewals` in `crates/syrup-rail-postgres/src/renewal.rs`, and `resolve_renewal_non_approved_outcome` in `crates/syrup-rail-postgres/src/enrollment_application.rs`.

- Observation: after five counted terminal attempts, `due_renewals` simply stops returning the subscription. Nothing changes `past_due` to a terminal state, and there is no terminal lifecycle event.
  Evidence: `MAX_RENEWAL_TERMINAL_ATTEMPTS_PER_PERIOD` and `RenewalAttemptState::blocks_automatic_retry` in `crates/syrup-rail/src/renewal.rs`, plus the terminal-count predicate in `crates/syrup-rail-postgres/src/renewal.rs`.

- Observation: customer cancellation is currently rejected for `past_due`, which would prevent a subscriber from voluntarily stopping scheduled retries.
  Evidence: `cancel_subscription_in_transaction` in `crates/syrup-rail-postgres/src/cancellation.rs` returns `BlockedByPastDue`.

- Observation: schema v1 has already been published and is explicitly immutable. The repository is currently at workspace version 0.1.1 and tag `v0.1.1`; this work changes both public Rust APIs and durable state.
  Evidence: `crates/syrup-rail-postgres/schema/v1/README.md`, root `Cargo.toml`, and the repository tags.

- Observation: `scripts/jig work status` reported the old repository-establishment plan `plan_01KZ9V96JRAB7MWTZHA13ABKZ4` as open when this ExecPlan was authored. Do not overwrite, repurpose, or close that plan without first checking its actual ownership and completion state.
  Evidence: `scripts/jig work status --json` on 2026-08-09.

- Observation: a paid-trial discount cannot currently be represented as zero recurring periods applied. The v1 constraint requires `periods_applied >= 1`, the core `AppliedSubscriptionDiscount` constructor rejects a limited discount whose remaining count still equals its total, and successful-charge advancement skips indefinite discounts entirely.
  Evidence: `billing_subscription_discounts_duration_periods_check` in `crates/syrup-rail-postgres/schema/v1/install.sql`, `AppliedSubscriptionDiscount::new` in `crates/syrup-rail/src/subscription.rs`, and `advance_subscription_discount_after_successful_charge` in `crates/syrup-rail-postgres/src/enrollment_application.rs`.

- Observation: the existing finite discount type is explicitly `LimitedMonths`, but its durable progression increments once per successful recurring charge. Allowing a limited-month discount on `FixedDays(n)` or `CalendarMonths(n > 1)` would make “three months” mean three arbitrary billing periods rather than three calendar months.
  Evidence: `SubscriptionDiscountDuration::LimitedMonths` and `LimitedDiscountMonths` in `crates/syrup-rail/src/subscription.rs`, plus `advance_subscription_discount_after_successful_charge` in `crates/syrup-rail-postgres/src/enrollment_application.rs`.

- Observation: renewal reservation is not the first provider boundary. `SubscriptionBillingService::renew` resolves the gateway and calls `account_mode()` before `reserve_subscription_renewal_in_transaction`, so changing only reservation admission would still let an early queued retry perform provider I/O.
  Evidence: `renew` and `renewal_gateway_account` in `crates/syrup-rail-postgres/src/subscription_billing_service.rs`.

- Observation: changing `TestDatabase::start` to install v2 without another edit would silently move every retained v1 schema-contract test onto v2. Fresh/upgrade catalog parity is also sensitive to column order because the fingerprint includes `ordinal_position`.
  Evidence: all v1 tests in `crates/syrup-rail-postgres/src/schema_contract.rs` call `TestDatabase::start`, `src/test_support.rs` currently installs `V1_INSTALL_SQL`, and `require_catalog_fingerprint` hashes columns in ordinal order.

- Observation: making v2 the default test schema also invalidates existing production writers and raw fixtures before their later behavioral milestones run, while the breaking core constructors and enum variants require every downstream Rust module to compile even when a test-name filter selects only one module. In particular, active cancellation leaves the new scheduler timestamp populated, renewal and recovery success move the economic period without moving that timestamp, and Milestone 3 explicitly exercises those paths before Milestones 4 and 5.
  Evidence: `cancel_active_subscription` in `crates/syrup-rail-postgres/src/cancellation.rs`, the successful renewal/recovery updates in `crates/syrup-rail-postgres/src/enrollment_application.rs`, direct `billing_subscriptions` inserts across PostgreSQL package tests, and the exhaustive `SubscriptionStatus` matches and `Subscription::new` calls throughout the crate.

- Observation: past-due cancellation needs a separate replay path. Once an already-ended past-due row becomes canceled, `billing_current_subscriptions` no longer selects it, so a repeated cancellation would return `NotFound` instead of the existing idempotent `AlreadyCanceled` outcome.
  Evidence: `current_subscription_id` in `crates/syrup-rail-postgres/src/cancellation.rs`, the v1 definition of `billing_current_subscriptions`, and the existing cancellation replay test.

- Observation: the Jig migration check requires an explicit comparison ref and checks every change recursively below the single configured migration directory.
  Evidence: `scripts/jig check migration-immutability --help` and the pinned Jig 0.2.0 implementation at commit `ecd216698e3ba46faf47ab74926efd5e683c5df2`.

- Observation: SQLx preparation is pinned independently of `TestDatabase`: `tools/sqlx-gate` embeds, reads, and installs `schema/v1/install.sql`. Leaving it unchanged would check v2 production queries against the v1 catalog.
  Evidence: `V1_INSTALL_SQL`, `load_install_sql`, and `run_gate` in `tools/sqlx-gate/src/main.rs`.

- Observation: the current 24-hour `last_terminal_at` gate is not only customer dunning. It also prevents an immediately repeated automatic renewal after live-readiness, malformed-request, rejected-request, configuration, or unavailable-before-submission failures, none of which necessarily creates an account/provider cooldown. Removing `last_terminal_at` without a replacement infrastructure timestamp would hot-loop those failures until the infrastructure cap.
  Evidence: `RenewalAttemptState::blocks_automatic_retry` and `provider_rate_limit_retry_after_seconds` in `crates/syrup-rail/src/renewal.rs`; the `last_terminal_at` filters in `crates/syrup-rail-postgres/src/renewal.rs`; and `renewal_readiness_open` plus `resolve_renewal_readiness_failure` in `crates/syrup-rail-postgres/src/subscription_billing_service.rs`.

- Observation: the same qualifying automatic-renewal failure history drives runtime dunning, suspended-access timestamps, and past-due cancellation. Implementing separate SQL predicates for those paths would let one path count recovery or pre-submission failures that another excludes.
  Evidence: the duplicated terminal-attempt filters in `due_renewals` and `renewal_attempt_state` in `crates/syrup-rail-postgres/src/renewal.rs`, plus the new first/last-failure requirements in this plan's terminalization and cancellation paths.

- Observation: an `AlreadyCanceled` fallback that searches only canceled rows can return an older canceled subscription after a newer subscription has ended as unpaid, because terminal rows are historical and excluded from the current-subscription index/view.
  Evidence: `billing_subscriptions_current_owner_plan_idx` and `billing_current_subscriptions` in `crates/syrup-rail-postgres/schema/v1/install.sql`, and `current_subscription_id` in `crates/syrup-rail-postgres/src/cancellation.rs`.

- Observation: the revised upgrade deliberately rejects a v1 `past_due` row without qualifying causal payment history, but discovering that anomaly only after writers stop turns a predictable data-quality issue into an avoidable maintenance-window failure.
  Evidence: v1 permits direct `past_due` state without a causal foreign key or check, while this plan's v2 backfill and runtime history loader require a submitted automatic renewal decline/failure for the same subscription and period anchor.

- Observation: PostgreSQL `CHECK` expressions that compare a nullable timestamp can evaluate to unknown and therefore pass; likewise, `array_position` raises an error on a multidimensional array before a surrounding Boolean expression can reliably reject its shape.
  Evidence: the first v2 malformed-shape fixtures admitted an active null `next_payment_attempt_at` and raised an unnamed multidimensional-array evaluation error. The final constraints use an explicit non-null predicate and shape-first `CASE` expressions, and the named constraint tests now pass.

- Observation: The deterministic deletion test fixture could not transition its v2 active subscription to canceled until it also cleared `next_payment_attempt_at`; the production scrub operation does not rewrite subscription status and was already correct.
  Evidence: the original full PostgreSQL test run failed `billing_subscriptions_scheduler_state_check` only in the fixture UPDATE. Clearing the scheduler timestamp made the full 92-test package pass.

- Observation: SQLx preparation against the final v2 schema completed without changing any checked-in query metadata.
  Evidence: `scripts/check-sqlx.sh --prepare` exited zero and `git status --short crates/syrup-rail-postgres/.sqlx` remained empty; `scripts/jig check sqlx` then passed with receipt `receipt_01KZK8D4MVQ6RMQX76WRQTE3P8`.

- Observation: Jig 0.2.0 requires an explicit scope for `rust-file-loc`, even though the initial plan listed the command without one, and the guide gate validates every existing crate guide rather than only changed guides.
  Evidence: the unscoped command printed `Exactly one of --staged, --changed-against, or --all is required`; `--changed-against v0.1.1` passed. Structured guide output identified missing `Edit here for X` sections in the core and NMI guides plus the PostgreSQL `src/lib.rs` entrypoint; the corrected guides pass.

- Observation: A changed-against LOC check cannot see brand-new untracked Rust files, so it initially passed while the new focused scenario module exceeded the absolute per-file limit.
  Evidence: the manual final-file audit found `paid_trial_dunning_tests.rs` at 1,514 lines. It now owns only shared fixtures at 500 lines and delegates seven 154-to-369-line scenario modules; formatting, clippy, focused tests, and the full workspace test gate all pass after the split.

- Observation: A comprehensive review hypothesized that a late approved renewal and its external reversal could erase an already-consumed dunning failure. The executable transition disproved that premise: the consumed `declined` attempt is terminal, late approved evidence is parked on a separate processor-charge path, and external-reversal attestation does not rewrite the terminal attempt's status or null resolution code.
  Evidence: `paid_trial_dunning_tests::reclassification::late_approval_and_reversal_preserve_terminal_failure_history_and_cancellation` drives the original decline, late approval, operator attestation, next automatic decline, and cancellation. The next decline consumes schedule entry two and suspended-access cancellation still uses the original failure timestamp.

- Observation: The shipped v1 operator workflow could create a valid `past_due` subscription by manually failing an operator-reviewed recovery whose optimistic subscription snapshot was `active`; requiring automatic-renewal history alone therefore confused v2 dunning authority with v1 status provenance.
  Evidence: the v1 manual-failure transition updated both renewal and recovery attempts, then moved only an `active` subscription to `past_due`. `schema_v1_upgrade_backfills_legacy_lifecycle_and_attempt_terms` now executes that exact state transition, passes preflight and upgrade, resumes at the economic anchor, and preserves the recovery resolution time through cancellation.

- Observation: Backfilling that legacy row was insufficient by itself: the first v2 automatic-failure projection still required an `active` before-state, and a later automatic failure caused cancellation and terminal events to prefer the newer automatic timestamp over the earlier v1 suspension.
  Evidence: the upgraded integration fixture now applies the first automatic failure while the subscription is already `past_due`, then cancels it and requires the original recovery timestamp. A pure terminal projection independently requires that same legacy timestamp for `SuspendImmediately`.

- Observation: The prior workflow splits moved names but left twelve changed Rust files above the enforced hard or absolute LOC limits, so the documented successful gate no longer described the current working tree.
  Evidence: the review-time staged gate failed on attempt, cancellation, discount, entitlement, operator-review, processor-charge, reconciliation, service, and core-domain files. A temporary-index simulation containing the complete corrected tree now reports `errors: []` and `ok: true` without policy exceptions.

## Decision Log

- Decision: Model exactly two commercial phases: one optional paid introductory phase followed by a recurring phase. Do not introduce an arbitrary multi-phase pricing language.
  Rationale: This directly supports the requested behavior while keeping validation, fingerprints, persistence, reconciliation, and period advancement understandable. More phases can be added later from concrete requirements.
  Date/Author: 2026-08-09 / Codex.

- Decision: Call the introductory API `PaidTrialTerms` and continue requiring a positive `ChargeAmount`.
  Rationale: A free trial requires a provider capability for storing a payment method without a sale. Treating that as a zero-value sale would make unsupported safety claims and conflate distinct provider flows.
  Date/Author: 2026-08-09 / Codex.

- Decision: Store calendar/fixed period rules in the accepted offer and subscription rather than leaving monthly recurrence hard-coded in PostgreSQL.
  Rationale: The seven-day first period and one-calendar-month recurring period must both be reproducible after a process restart or provider reconciliation. The same API also becomes useful to weekly, multi-month, or annual hosts without adding host conditionals.
  Date/Author: 2026-08-09 / Codex.

- Decision: Keep `next_renewal_at` as the economic start of the next recurring period and add `next_payment_attempt_at` as the mutable scheduler clock.
  Rationale: The period anchor must not drift when payment is retried. Separating the two timestamps lets a recovered charge still buy the originally scheduled monthly period while dunning is paced from actual determinate failures.
  Date/Author: 2026-08-09 / Codex.

- Decision: Interpret each dunning schedule entry as a delay after the preceding determinate automatic renewal failure. A schedule with N entries permits N automatic retries after the first due-date attempt.
  Rationale: Relative delays are deterministic, do not require catch-up charging through several missed absolute slots, and are easy for hosts to explain. For example, delays of one day and three days mean attempts at the due boundary, one day after the first failure, and three days after the second failure.
  Date/Author: 2026-08-09 / Codex.

- Decision: Split customer, infrastructure, and provider pacing explicitly. Customer failures move the snapshotted `next_payment_attempt_at`; non-cooldown infrastructure failures retain the existing 24-hour query-derived pace and finite cap; provider/account throttles retain their existing cooldown and fast/slow pacing rules.
  Rationale: The current global terminal clock serves all three concerns. Removing only its customer meaning without retaining the infrastructure timestamp would cause tight automatic retry loops, while continuing to use it for customer failures would defeat the host-supplied dunning schedule.
  Date/Author: 2026-08-09 / Codex.

- Decision: Only a submitted, determinate, automatic `subscription_renewal` decline/failure advances dunning. Recovery attempts, pre-submission infrastructure failures, provider throttles, unknown outcomes, and review-required outcomes do not consume dunning steps.
  Rationale: `unpaid` must mean repeated customer-payment failure, not Syrup Rail or provider unavailability. Excluding user-triggered recovery failures also prevents a subscriber from exhausting automatic dunning by repeatedly submitting a bad card.
  Date/Author: 2026-08-09 / Codex.

- Decision: Add `SubscriptionStatus::Unpaid` as a terminal state distinct from voluntary `Canceled`.
  Rationale: Both states stop collection, but preserving why a subscription ended is necessary for support, analytics, entitlement synchronization, and safe future policy. `Unpaid` is excluded from the current-subscription uniqueness index and cannot be recovered in place.
  Date/Author: 2026-08-09 / Codex.

- Decision: Make exhaustion and access behavior part of the snapshotted offer through `DunningExhaustion::{RemainPastDue, MarkUnpaid}` and `PastDueAccessPolicy::{SuspendImmediately, ContinueUntilDunningExhausted}`.
  Rationale: IdentityPro needs terminal nonpayment and, based on the requirement to cancel Array at the terminal step, should retain access during scheduled dunning. Other projects must be able to preserve the existing suspended/indefinite-past-due behavior.
  Date/Author: 2026-08-09 / Codex.

- Decision: A paid trial does not consume a subscription-discount period; subscription discounts apply to recurring periods only.
  Rationale: A paid trial is already an introductory commercial phase. Counting it as a discounted month would silently shorten a saved multi-month recurring discount. Immediate-recurring enrollment continues counting its initial recurring period exactly as it does today.
  Date/Author: 2026-08-09 / Codex.

- Decision: Preserve `LimitedMonths` as a literal calendar-month contract. Permit a finite limited-month discount only when recurring cadence is exactly `CalendarMonths(1)`; indefinite discounts and full-price offers may use any supported recurring period rule.
  Rationale: the existing snapshot, schema vocabulary, and host-facing validation all promise months, while progression counts successful charges. Rejecting an incompatible cadence prevents a weekly or multi-month charge from silently redefining an already-public financial term.
  Date/Author: 2026-08-09 / Codex.

- Decision: Separate durable discount-code administration from current-offer quoting. List, create, update, and disable return `SubscriptionDiscountCodeRecord`; only active create/update and explicit validation/claim paths lock the current offer and evaluate quote eligibility.
  Rationale: a historical code must remain listable and disableable after the host changes an offer's cadence. Making administrative CRUD construct `SubscriptionDiscountCodeQuote` coupled record maintenance to a projection that can legitimately become invalid.
  Date/Author: 2026-08-09 / Codex.

- Decision: Persist all accepted lifecycle terms on the durable initial attempt and copy the runtime terms to the subscription on approval. Include the complete terms in the v2 request fingerprint.
  Rationale: reconciliation reconstructs authority from the durable attempt without consulting a mutable host offer. Storing only the USD 1.00 request amount would lose the normal recurring price, cadence, and terminal policy.
  Date/Author: 2026-08-09 / Codex.

- Decision: Preserve the historical v1 initial-attempt fingerprint grammar and add a terms-version marker for upgraded attempts rather than rewriting request fingerprints.
  Rationale: fingerprints are immutable evidence of the accepted request and drive idempotent replay. Existing unresolved attempts must remain reconstructable through the upgrade.
  Date/Author: 2026-08-09 / Codex.

- Decision: Add a subscriber-aware enrollment-offer hook with a default implementation, while retaining the existing catalog-level offer hook for discount administration.
  Rationale: trial eligibility is host authorization/business policy and must not be hard-coded by Syrup Rail. A host that limits trials can lock its eligibility row on the supplied connection and return either paid-trial or immediate-recurring terms without changing discount quote APIs. The hook receives one typed context built from the durable reservation at both lifecycle stages; attempt-history queries exclude its attempt ID so the first call's insert cannot disqualify the second call.
  Date/Author: 2026-08-09 / Codex.

- Decision: Expose initial-subscription pricing through `SubscriptionActivationProjection` rather than a context-free “effective recurring at start” accessor.
  Rationale: enrollment authorization, the period opened by approval, and the charge persisted for the next recurring collection are distinct temporal facts. Naming all of them on one projection prevents one-period immediate discounts and paid trials from being interpreted at the wrong boundary.
  Date/Author: 2026-08-09 / Codex.

- Decision: Emit provider-neutral payment-failure and subscription-ended events atomically; never add Array vocabulary or calls to any Syrup Rail crate.
  Rationale: hosts need a durable terminal fact, while external fulfillment remains host-owned under `docs/security/threat-model.md`. IdentityPro can translate `SubscriptionEnded { reason: NonPayment }` into an Array synchronization job.
  Date/Author: 2026-08-09 / Codex.

- Decision: Introduce schema v2 with both a complete fresh-install artifact and a forward v1-to-v2 artifact. Do not edit any byte of `schema/v1`.
  Rationale: at least one host has materialized v1. IdentityPro can install v2 directly, while existing hosts need a tested forward transition.
  Date/Author: 2026-08-09 / Codex.

- Decision: Ship a read-only v1-to-v2 preflight query beside the v2 install and upgrade artifacts, and keep the upgrade's own validation authoritative.
  Rationale: hosts should be able to discover causal-history anomalies before entering maintenance, but a preflight run cannot replace in-transaction validation because state can change before writers are stopped.
  Date/Author: 2026-08-09 / Codex.

- Decision: Treat the single v1-to-v2 artifact and breaking 0.2 code as a maintenance cutover, not a rolling-compatible deployment. Prebuild and verify 0.2, stop all 0.1 billing writers, apply the upgrade transaction, and only then start 0.2 writers; unresolved durable attempts remain and are reconciled by the upgraded code. Before migration commit, failure rolls back to v1; after commit, recovery is roll-forward with 0.2 and 0.1 must not restart. If a host requires mixed-version or zero-downtime deployment, revise this plan to add an expand/dual-write/contract rollout before implementation.
  Rationale: v2 requires terms columns that 0.1 writers do not supply, while 0.2 queries require columns absent from v1. The repository has no established mixed-version schema path, so claiming rolling compatibility would be unsafe.
  Date/Author: 2026-08-09 / Codex.

- Decision: Adopt the renewal-only dunning classifier during the v1-to-v2 backfill even when a subscription had reached v1's combined renewal/recovery ceiling. Document and audit the resulting collection reactivation rather than preserving the old quiescence.
  Rationale: Recovery is subscriber-initiated and must not consume or delay automatic dunning under the new policy. Carrying the old effective count forward would encode two incompatible classifiers in otherwise identical v2 subscriptions.
  Date/Author: 2026-08-09 / Codex.

- Decision: Remove `next_monthly_billing_period` in the breaking 0.2 API instead of retaining a partial compatibility wrapper.
  Rationale: The generic period API and error type are already intentionally breaking, and keeping only the old function name would imply source compatibility that its changed return type did not provide.
  Date/Author: 2026-08-09 / Codex.

- Decision: Do not publish crates, push tags, modify host repositories, or perform the release-version bump in this ExecPlan. Record the change under `CHANGELOG.md` and note that the separately authorized release must be 0.2.0.
  Rationale: publishing and consumer rollout are external state changes requiring separate authorization, and this repository's release preflight expects workspace versions, internal dependency requirements, lockfile, and a dated changelog section to move together. The Rust and schema changes are breaking relative to 0.1.1, so a future 0.1.x release would be misleading.
  Date/Author: 2026-08-09 / Codex.

- Decision: Accept a submitted v1 recovery as legacy `past_due` provenance only when its retained review timestamp, terminal `failed` state, absent resolution code, and exact `active` optimistic snapshot identify the operator manual-failure transition. It supplies the immediate-suspension timestamp but counts as zero v2 automatic failures, so `next_payment_attempt_at` resumes at the unchanged economic period anchor. Continue rejecting rows with neither automatic-renewal nor this legacy recovery provenance.
  Rationale: status provenance and v2 dunning consumption are different facts. This mapping cuts over every state the v1 application could legitimately create, preserves financial evidence and access timing, avoids fabricating a renewal failure, and keeps the v2 rule that recovery never consumes or delays automatic dunning.
  Date/Author: 2026-08-10 / Codex.

- Decision: Load automatic dunning history and legacy recovery provenance through one `PastDueCausalHistory` boundary. Only automatic renewals determine failure count and pacing; legacy provenance admits the already-past-due before-state and participates only in the earliest suspension boundary.
  Rationale: migration validation, runtime transition admission, cancellation, and terminal events otherwise encode different subsets of the same causal fact and drift as soon as a v2 failure follows the legacy recovery.
  Date/Author: 2026-08-10 / Codex.

- Decision: Enforce file-size policy through responsibility-based modules, not thresholds or allowlists. Workflow facades retain stable public paths while fingerprints/snapshots, grants/discounts/access, SQL persistence/classification, and billing-service workflows live in their owning child modules; large test suites mirror those boundaries.
  Rationale: the LOC failures were a symptom of incomplete ownership extraction. Completing the boundaries reduces navigation and change coupling while preserving behavior and makes the repository gate describe the actual maintainability contract.
  Date/Author: 2026-08-10 / Codex.

## Outcomes & Retrospective

The implementation now demonstrates the requested reusable lifecycle end to end. A USD 1.00 seven-day paid trial persists complete accepted terms, transitions to an anchored USD 29.00 calendar-month renewal, follows relative one-day/three-day dunning, and becomes terminal unpaid exactly once after the final determinate automatic failure. The same application path works through foreground and exact reconciliation, while failed recovery, unknown outcomes, operational failures, throttling, cancellation, replay, stale jobs, and late approvals retain their distinct safety semantics. Immediate recurring enrollment remains compatible, and recurring discounts begin at zero applied periods during a trial; finite discounts advance once when the first recurring period is recovered or renewed, while indefinite discounts retain their unbounded duration.

The durable deliverables are `schema/v2/install.sql`, `schema/v2/preflight_from_v1.sql`, `schema/v2/audit_retry_reclassification_from_v1.sql`, and `schema/v2/upgrade_from_v1.sql`; schema v1 is byte-identical to v0.1.1. The public core API, PostgreSQL orchestration, SQLx gate, workflow filters, crate guides, README, schema-v2 cutover guide, and `[Unreleased]` changelog now describe v2, including the intentional possibility that automatic collection resumes when v1 exhaustion depended on recovery failures. The final boundary audit additionally replaced the under-specified enrollment-offer callback arguments with a stable reservation context and removed the ambiguous recurring-price shortcut in favor of the activation projection. SQLx preparation produced no metadata diff. Every required repository gate passed at the recorded checkpoint; the later boundary corrections are verified by the evidence recorded below.

The cutover now also accepts the operator-reviewed recovery-only `past_due` state that v1 could legitimately produce. The retained review timestamp and terminal manual-failure shape prevent ordinary recovery declines from being mistaken for status provenance. That legacy recovery remains durable suspension provenance while consuming no v2 dunning step; the first automatic renewal resumes at the existing period anchor and can apply against the already-past-due state. One causal-history boundary preserves the earlier suspension time through later v2 failures, cancellation, and terminalization. Host entitlement documentation and a compiled example make `PastDueAccess`—rather than the `PastDue` variant alone—the required product-access decision. Responsibility-based Rust modules bring every changed file under the enforced LOC ceiling without changing public paths.

Intentionally deferred work is unchanged from the plan: no crate version or dependency-requirement bump, publication, tag, or IdentityPro/Array integration was performed. Those belong to the separately authorized 0.2.0 release and host rollout. Jig evidence was not attached to the unrelated open repository-establishment plan `plan_01KZ9V96JRAB7MWTZHA13ABKZ4`; this checked-in ExecPlan and append-only gate receipts are the implementation record.

## Context and Orientation

Syrup Rail is a Rust workspace containing four publishable crates. `crates/syrup-rail` owns provider-neutral values, lifecycle policy, commands, outcomes, and events. `crates/syrup-rail-postgres` owns canonical tables and transaction orchestration on host-supplied PostgreSQL connections. `crates/syrup-rail-nmi` and `crates/syrup-rail-nmi-client` adapt one-shot gateway operations; NMI does not own the subscription schedule. The host owns authentication, plan catalog rows, job scheduling, credentials, outbound email, and product fulfillment.

Read `AGENTS.md`, `agent-map.md`, `crates/syrup-rail/AGENTS.md`, `crates/syrup-rail-postgres/AGENTS.md`, `.agent/PLANS.md`, and `docs/security/threat-model.md` before implementation. The feature changes financial state and provider-outcome handling. Preserve the threat model's at-most-once mutation rule: a provider mutation is admitted and committed before one-shot I/O, an indeterminate result is reconciled by durable attempt identity, and an approved observation that no longer matches application state is retained and parked for compensation rather than silently discarded.

The following terms have precise meanings in this plan:

- A **paid trial** is a positive initial charge that buys one introductory access period. It is not a discount code and is not a zero-dollar payment-method setup.
- The **current period** is the exact interval already paid for. During a trial it is the seven-day interval. After a successful recurring charge it is the monthly interval.
- `next_renewal_at` is the economic boundary at which the next recurring period starts. All attempts for the same unpaid period retain this anchor.
- `next_payment_attempt_at` is the time at which the automatic dispatcher may attempt collection. It initially equals `next_renewal_at` and moves during dunning.
- **Dunning** is the host-configured sequence of automatic retries after determinate customer-payment failures.
- A **terminal automatic failure** in existing code is a `declined` or `failed` payment attempt after exclusions. This plan narrows the customer dunning count to submitted automatic renewals; operational failures keep their separate bounded retry policy.
- **Infrastructure retry** means recovery from readiness failure, configuration turnover, gateway unavailability, or throttling. It must never imply nonpayment.
- **Unpaid** is the new terminal subscription status after a `MarkUnpaid` policy exhausts all dunning retries. It schedules no more provider mutations and grants no entitlement.
- **Reconciliation** is the path that applies a later provider observation to a durable attempt after a foreground outcome was unknown or storage was interrupted. It must use the same lifecycle transition as the foreground path.

The core model is currently spread across these files:

- `crates/syrup-rail/src/money.rs` defines `ChargeAmount`, exact `BillingPeriod`, and the current one-price `SubscriptionOffer`.
- `crates/syrup-rail/src/policy.rs` contains the hard-coded one-calendar-month period function.
- `crates/syrup-rail/src/discount.rs` and `enrollment.rs` define recurring discount quotes, `SubscriptionEnrollmentExpectedCharge`, `EnrollSubscription`, and the durable enrollment reservation reconstruction API.
- `crates/syrup-rail/src/attempt.rs` defines v1 request fingerprints, `PaymentAttemptTarget`, and optimistic subscription snapshots.
- `crates/syrup-rail/src/renewal.rs` defines renewal dispatch/reservation types and hard-coded customer/infrastructure retry limits.
- `crates/syrup-rail/src/resolution.rs` defines the exact resolution-code sets used for renewal retry accounting, infrastructure caps, and pacing.
- `crates/syrup-rail/src/identity.rs`, `subscription.rs`, and `event.rs` define subscription statuses, entitlement/cancellation values, and the event vocabulary.

The PostgreSQL implementation is currently spread across these files:

- `crates/syrup-rail-postgres/schema/v1/install.sql` is the immutable v1 schema. It allows only `active`, `past_due`, and `canceled`; it forces `next_renewal_at = current_period_end_at`.
- `crates/syrup-rail-postgres/src/attempts.rs` reserves and reconstructs initial, renewal, recovery, and payment-method attempts.
- `crates/syrup-rail-postgres/src/enrollment_application.rs` creates subscriptions, advances recurring periods and discounts, resolves foreground and reconciled outcomes, and appends events through a host-prepared transaction.
- `crates/syrup-rail-postgres/src/renewal.rs` selects due work and counts attempts.
- `crates/syrup-rail-postgres/src/operator_review.rs` has a second path that marks a manually failed renewal/recovery `past_due`; it must share the new transition.
- `crates/syrup-rail-postgres/src/subscription_billing_service.rs` performs the automatic renewal preflight, including gateway resolution and readiness I/O before reservation.
- `crates/syrup-rail-postgres/src/entitlement.rs` currently blocks all `past_due` protected writes.
- `crates/syrup-rail-postgres/src/cancellation.rs` currently blocks cancellation from `past_due`.
- `crates/syrup-rail-postgres/src/schema_contract.rs` embeds and fingerprints v1, while `src/test_support.rs` installs v1 for package tests.
- `tools/sqlx-gate/src/main.rs` independently embeds and installs v1 before checking or preparing committed SQLx metadata.

The current transaction/event interface in `crates/syrup-rail-postgres/src/transactions.rs` supports repeated calls to `BillingTransaction::append_event` on the same host transaction. Use that capability to append both a final payment-failure event and a terminal subscription-ended event before committing. Do not weaken the requirement that status, attempt resolution, processor evidence, discount state, and all corresponding events commit atomically.

## Scope and Required Behavior

The implementation must support the following host-neutral behavior.

An offer contains a recurring charge and recurring period rule, an optional paid trial, and a renewal-failure policy. All charges in one offer use the same currency. Subscription discount codes are quoted against the recurring base charge only; changing the trial amount must not change a discount quote. A paid-trial enrollment submits exactly one initial one-shot sale for the trial amount. Initial approval creates an active subscription whose current period is calculated from the paid-trial rule, whose stored `amount_cents` is the next effective recurring amount rather than the trial amount, and whose `next_renewal_at` and `next_payment_attempt_at` are the trial end.

At the trial boundary, the automatic renewal charges the effective recurring amount for a period calculated from the recurring rule. A one-calendar-month rule must retain the current clamping behavior from the preceding boundary. Successful approval changes the current-period phase to recurring, advances the current period and any recurring discount, resets the subscription to `active`, and sets both next timestamps to the new period end.

On a submitted determinate automatic renewal failure, resolve the attempt and subscription under the existing subject and subscription-aggregate locks. Count only qualifying automatic failures for this period. If the corresponding schedule delay exists, set `past_due`, compute `next_payment_attempt_at = resolved_at + delay`, and emit a payment-failure event whose retry timestamp is that exact value. If there is no remaining delay, leave `past_due` with no next payment time for `RemainPastDue`, or set terminal `unpaid`, set `unpaid_at`, clear `next_payment_attempt_at`, and emit both failure and subscription-ended events for `MarkUnpaid`. The terminal event's `access_ends_at` is the final failure time for `ContinueUntilDunningExhausted` and the first qualifying failure's durable `resolved_at` for `SuspendImmediately`.

A dunning schedule may be empty. Empty plus `MarkUnpaid` terminalizes on the first determinate automatic failure. Empty plus `RemainPastDue` leaves the subscription past due with no automatic retry. Bound the schedule to at most 16 entries and require every delay to be a positive whole number of seconds. Use checked timestamp arithmetic and treat overflow as invalid durable state; do not saturate to a misleading date.

Unknown and review-required attempts remain blocking and cannot advance dunning. Pre-submission infrastructure outcomes retain the existing finite cap and 24-hour pace unless an existing account/provider cooldown supplies the stronger gate; provider throttles retain their existing cooldown plus fast/slow pacing. These operational clocks do not move `next_payment_attempt_at` and never consume customer dunning steps. Reaching an infrastructure retry cap leaves the subscription recoverable and does not emit nonpayment termination. A successful recovery during dunning returns the subscription to active and advances the originally anchored recurring period. A failed user recovery does not move the automatic dunning schedule.

For `ContinueUntilDunningExhausted`, protected-write entitlement remains allowed while `past_due` has a scheduled `next_payment_attempt_at`. It is suspended once a `RemainPastDue` schedule is exhausted, and it ends when `MarkUnpaid` terminalizes. For `SuspendImmediately`, existing protected-write behavior remains: the first `past_due` transition denies access. IdentityPro should configure `ContinueUntilDunningExhausted`; other hosts choose explicitly.

An active paid trial can be canceled with access through its paid trial end, just like current paid-through cancellation. A past-due subscription can be canceled after in-flight renewal/recovery and payment-method-update fences are clear. Both active and past-due cancellation clear `next_payment_attempt_at`; past-due cancellation records one database timestamp as `canceled_at`. Its event uses that timestamp as `access_ends_at` only while continuing dunning access is still scheduled. If access was already suspended, report the first qualifying failure for `SuspendImmediately` or the last qualifying failure that exhausted `RemainPastDue` under `ContinueUntilDunningExhausted`. Repeating either cancellation returns `AlreadyCanceled` without another event. Cancellation and final nonpayment are distinct ledger states and events.

This plan does not implement free trials, arbitrary pricing phases, proration, upgrades/downgrades, provider-managed recurring plans, host trial-eligibility rules, email, queue transports, Array API calls, or publication to crates.io. It supplies the durable APIs and events those host features need.

## Plan of Work

The dependency-safe execution order is important because Milestone 2 intentionally changes public Rust shapes and Cargo compiles all downstream modules even for filtered tests. Implement and validate the schema-only Milestone 1 while the current core API still compiles. Then implement Milestone 2 and the compile-wide compatibility slice at the start of Milestone 3 as one coordinated Rust cutover before running any further PostgreSQL package test. Continue with the remaining Milestone 3 behavior, then Milestones 4 through 6. Do not introduce temporary wildcard matches, v1-backed ordinary tests, or placeholder defaults merely to bridge this interval.

### Milestone 1: Introduce immutable schema v2 and prove upgrade parity

At the end of this milestone, a fresh host can install schema v2, a v1 host can upgrade in one forward transaction, and package tests can prove that both paths produce the same canonical v2 catalog and equivalent backfilled lifecycle state.

Do not edit `crates/syrup-rail-postgres/schema/v1/install.sql` or `crates/syrup-rail-postgres/schema/v1/README.md`. Create:

- `crates/syrup-rail-postgres/schema/v2/install.sql`, the complete fresh-install DDL.
- `crates/syrup-rail-postgres/schema/v2/preflight_from_v1.sql`, one read-only query that reports v1 past-due rows which cannot satisfy the causal-history backfill.
- `crates/syrup-rail-postgres/schema/v2/upgrade_from_v1.sql`, a forward-only delta suitable for copying byte-for-byte into a host migration.
- `crates/syrup-rail-postgres/schema/v2/README.md`, explaining fresh installs, upgrades, immutable copying, and the exported Rust constants.

The preflight result is an operator-facing blocker list, not a Boolean: return `subscription_id`, `billing_scope_id`, `subscriber_id`, `plan_key`, `next_renewal_at`, and `qualifying_failure_count`, ordered by scope, subscriber, plan, and subscription ID. It reports only `past_due` rows whose qualifying count is zero, using the exact attempt-kind, submitted-status, resolution-code, subscription, and period-anchor predicate used by the upgrade. An empty result means no known causal-history blocker; it is not permission to skip the upgrade's in-transaction validation.

In both v2 paths, extend `billing_subscriptions` with the runtime terms and scheduler columns listed under `Interfaces and Dependencies`. Add `unpaid` to the status constraint. Require `unpaid_at` only for unpaid and `canceled_at` only for canceled. Keep active and past-due rows in the unique current-subscription index; canceled and unpaid rows are terminal and excluded. Replace the due index with one beginning at `next_payment_attempt_at`, filtered to active/past-due rows where the value is non-null.

Preserve the invariant that `next_renewal_at = current_period_end_at` for active and past-due subscriptions. Add shape constraints with these meanings: active has a non-null next payment equal to the renewal boundary; past due may have a later next payment or null after `RemainPastDue` exhaustion; canceled and unpaid have no next payment; trial-history amount/kind/count are all present or all absent; paid-trial phase requires complete trial history; recurring phase permits either retained trial history or no trial, but its current period is recurring; period counts are between 1 and 65,535 to match `NonZeroU16`; retry arrays contain no null elements and every delay is between 1 and 4,294,967,295 to match `NonZeroU32`; a schedule is either the canonical empty array or one-dimensional with lower bound one and at most 16 elements; trial and recurring terms use the subscription currency.

Replace `billing_subscription_discounts_duration_periods_check` in v2 so an active discount attached during a paid trial may have `periods_applied = 0`. For limited discounts, active rows allow zero through `periods_total - 1` and completed rows still require `periods_applied = periods_total`; for indefinite discounts, active rows allow zero or one. Upgraded v1 rows remain at one. Add named constraint tests for valid zero-applied rows and invalid negative, over-total, or completed-at-zero shapes.

Extend `billing_payment_attempts` with a versioned, immutable initial-enrollment terms snapshot. For every `subscription_initial` row, require a terms version, start kind, recurring base amount, recurring period rule, dunning schedule, exhaustion action, and past-due access policy. Trial fields are all present only for `paid_trial` and all absent for `recurring_immediately`. Non-initial attempt kinds have all these initial-term columns null. Accept terms version 1 for upgraded legacy attempts and version 2 for new attempts.

The v1-to-v2 backfill must preserve existing subscriptions as immediate recurring, one-calendar-month subscriptions. Give them a four-entry 24-hour retry schedule, `RemainPastDue`, and `SuspendImmediately`, which preserves the nominal five automatic determinate renewal attempts and existing access behavior while adopting the new narrower submitted-automatic classification. Set active rows' next payment to their renewal boundary. For existing past-due rows, count only submitted automatic renewal customer failures under the new classification: if fewer than five exist, set next payment to the latest qualifying resolved time plus 24 hours, never earlier than the renewal boundary; if five or more exist, set it null. Treat a v1 past-due row with no qualifying causal renewal failure as invalid migration input rather than inventing a failure timestamp. Existing canceled rows get no next payment. No existing row becomes unpaid during migration.

Backfill every legacy initial attempt as terms version 1, immediate recurring, one calendar month, and the same legacy failure policy. Derive its recurring base amount from its saved discount snapshot's base amount when present, otherwise from the attempt amount. Do not rewrite `request_fingerprint`. A pending, unknown, or review-required v1 initial attempt must remain reconstructable and reconcilable after upgrade.

Update `schema_contract.rs` without deleting `V1_INSTALL_SQL` or `assert_v1_conforms`. Export `V2_INSTALL_SQL`, `V1_TO_V2_PREFLIGHT_SQL`, `V1_TO_V2_UPGRADE_SQL`, and `assert_v2_conforms`. Parameterize the shared private catalog collectors and conformance error version while keeping version-specific required-column lists and fingerprint constants explicit. Make `TestDatabase::start` install v2 for normal package tests, add `TestDatabase::start_v1` for every retained v1 schema-contract test, and add `TestDatabase::start_v1_then_upgrade` for upgrade tests; the upgrade helper must execute the complete delta in one explicit SQLx transaction before committing. Do not let existing v1 tests inherit the new default silently. Keep `preflight_from_v1.sql` a single read-only result query with no transaction control or mutations so tests and hosts can execute the checked-in bytes directly.

Treat the default-test-schema switch as a deliberate writer-shape barrier. `start_v1` is only for retained v1 contract/immutability coverage, never as an escape hatch for ordinary package tests. Before running a v2-backed module test, update every direct `billing_subscriptions` INSERT/UPDATE fixture in that module to provide a valid explicit phase, recurring rule, legacy failure policy, and scheduler shape. Prefer a small test-support constructor for the common immediate-recurring/monthly/legacy-policy row, while keeping exceptional status and malformed-shape fixtures explicit. Milestone 3 closes the production active-cancellation and successful-charge writer gaps before its broad enrollment test command; later milestones update their specialized failure, entitlement, and terminal fixtures before running those suites.

Because the catalog fingerprint hashes column `ordinal_position`, define v2 fresh-install columns in the exact order produced by the upgrade artifact: retain all v1 columns in their original order, then append v2 columns in the same `ALTER TABLE ... ADD COLUMN` order. For canonical read views, preserve the complete v1 column prefix and append new columns, using `CREATE OR REPLACE VIEW` where PostgreSQL permits it so host dependents are not destroyed; never use `DROP ... CASCADE`. Use identical constraint, index, view, function, and trigger names and definitions in both paths; the parity test must compare independently computed v2 fingerprints rather than merely call conformance twice against one constant.

Add tests proving all of the following: v1 bytes remain unchanged; every retained v1 contract test still runs through `start_v1`; fresh v2 conforms; v1 plus upgrade conforms; fresh and upgraded v2 have the same canonical catalog fingerprint; upgraded active, past-due, canceled, discounted, unresolved-initial, and terminal-attempt fixtures have valid expected values; the read-only preflight returns no rows for valid and mixed-history fixtures but identifies the exact anomalous subscription; a v1 past-due row with no qualifying causal renewal failure aborts and rolls back the upgrade with a named diagnostic; malformed term shapes, null/multidimensional/noncanonical arrays, out-of-range period/delay values, zero-applied discount violations, and invalid status/timestamp combinations are rejected by named constraints; host-prefixed extensions remain allowed. Include one mixed v1 past-due history fixture containing a submitted customer renewal failure, a pre-submission infrastructure failure, a provider throttle, and a recovery failure; prove the preflight and upgrade count only the customer renewal. Retain that fixture so Milestone 4 can compare the upgraded count/first/last facts with its runtime history loader. Update `billing_current_subscriptions` and other read views deliberately and include their new definitions in the v2 fingerprint.

Set `.jig.toml`'s `rust_migration_dir` to `crates/syrup-rail-postgres/schema`, which makes Jig's recursive Git pathspec protect both v1 and v2. Update `crates/syrup-rail-postgres/AGENTS.md`, generated repository guidance if required by the Jig update workflow, and the path filters in `.github/workflows/repo-policy.yml` and `.github/workflows/rust-tests.yml` so changes under `schema/v2/**` run the same gates as v1. Do not use `scripts/jig migration-add` for these versioned distribution artifacts.

Update `tools/sqlx-gate/src/main.rs` to embed, byte-check, and install `schema/v2/install.sql` as the current SQLx preparation catalog; rename v1-specific constants and diagnostics accordingly. The retained v1 conformance and byte-immutability tests, not the current SQLx gate, continue to own v1 validation.

Document the existing-host rollout in `schema/v2/README.md`: prebuild and verify 0.2; run the checked-in read-only `preflight_from_v1.sql` before entering maintenance and investigate every returned subscription without synthesizing payment evidence; stop all 0.1 billing writers; apply `upgrade_from_v1.sql` in one transaction; start 0.2; and then resume dispatch/reconciliation. Explain that the preflight is advisory and the upgrade repeats the same validation authoritatively after writers stop. A failure before commit rolls back the SQL transaction and permits 0.1 to resume; after commit, keep writers stopped until 0.2 is restored and roll forward rather than restarting 0.1. The migration must preserve unresolved attempts; it must not require draining or resubmitting them. This plan does not authorize a mixed 0.1/0.2 rolling deployment.

Run:

    cargo test -p syrup-rail-postgres schema_contract --features schema-contract-test-support
    git diff --exit-code v0.1.1 -- crates/syrup-rail-postgres/schema/v1/install.sql crates/syrup-rail-postgres/schema/v1/README.md
    scripts/jig check migration-immutability --changed-against v0.1.1
    scripts/jig check contract

Expect every command to exit zero. The migration test must start from the checked-in v1 artifact; installing v2 directly is not evidence that upgrades work.

### Milestone 2: Add provider-neutral subscription terms and pure lifecycle policy

At the end of this milestone, `syrup-rail` can represent the complete IdentityPro offer without database or NMI concepts, calculate both seven-day and calendar-month periods, and decide the next dunning disposition from a validated policy and attempt count.

Create `crates/syrup-rail/src/terms.rs` and re-export it from `src/lib.rs`. Move `SubscriptionOffer` out of `money.rs` into this module while keeping it publicly re-exported from the crate root. Define the exact public types described under `Interfaces and Dependencies`: period rules, recurring terms, paid trial terms, subscription start, dunning delay/schedule, exhaustion action, access policy, renewal-failure policy, and complete offer. Constructors must reject currency mismatches, zero period counts, zero delays, and schedules longer than 16. Ordinary debugging may show non-sensitive prices and policy values; no token, provider reference, or billing contact enters these types.

Replace `SubscriptionEnrollmentExpectedCharge` in `enrollment.rs` with `SubscriptionEnrollmentExpectedTerms`. It contains the accepted `SubscriptionOffer` plus an optional saved `SubscriptionDiscountSnapshot`. Full-price admission requires the entire accepted offer and absence of a saved claim to match the locked state. Discounted admission requires the saved snapshot to match and requires plan key, trial terms, recurring period rule, and failure policy to match the current locked offer; retain the saved discount snapshot as the economic authority even if the host's current recurring base price changed. Expose the enrollment authorization, opened period, next recurring charge, and consumed discount count through `SubscriptionActivationProjection`; retain `initial_charge()` only as an unambiguous convenience for the provider authorization. The initial charge is the paid-trial charge when present; otherwise it is the full or discounted recurring charge.

Update `SubscriptionDiscountCodeQuote::new` in `discount.rs` to quote from `offer.recurring().charge()`, and adapt the catalog-level validation callers in PostgreSQL accordingly. Add a test in which two offers have the same recurring terms but different trial prices and produce the same discount quote. Because the existing finite duration is explicitly `LimitedMonths` and advances once per successful recurring charge, make quoting and `SubscriptionEnrollmentExpectedTerms::discounted` reject a limited-month snapshot unless `offer.recurring().period()` is exactly `CalendarMonths(1)`; indefinite discounts remain valid for any supported recurring cadence. For discounted expected terms, the saved snapshot's base and discounted charges are the accepted recurring economic authority; `discounted` rejects a snapshot currency that differs from the offer but deliberately does not require its saved base price to equal the later catalog price. Current catalog price drift is ignored only after the plan, trial, cadence, failure policy, and saved discount identity match.

In `policy.rs`, add one checked `next_billing_period(start_at, rule)` function. `FixedDays(n)` adds exactly n times 24 hours in UTC. `CalendarMonths(n)` uses Chrono's checked calendar-month addition and repeats the existing clamping behavior from the previous boundary. Remove `next_monthly_billing_period` in the breaking 0.2 API and document `next_billing_period` with an explicit one-calendar-month rule as its replacement.

In `identity.rs`, add `SubscriptionStatus::Unpaid` and `SubscriptionPhase::{PaidTrial, Recurring}` with exhaustive `ALL`, `as_str`, and parser tests. In `subscription.rs`, extend `Subscription` with phase, recurring period rule, failure policy, and `Option<DateTime<Utc>>` next payment attempt. Preserve `next_renewal_at` as the period anchor. Replace `CancelSubscriptionOutcome::BlockedByPastDue` with outcomes that allow a successful past-due cancellation while preserving in-flight fences.

In `renewal.rs`, remove customer dunning from the global `MAX_RENEWAL_TERMINAL_ATTEMPTS_PER_PERIOD` and `RENEWAL_RETRY_AFTER_SECONDS` policy. Replace the overloaded 24-hour constant with separately named `RENEWAL_INFRASTRUCTURE_RETRY_AFTER_SECONDS` and `RENEWAL_PROVIDER_RATE_LIMIT_SLOW_RETRY_AFTER_SECONDS` constants while retaining the existing provider fast delay and threshold. Update the characterized policy sets and exact-membership tests in `resolution.rs`: remove sets that exist only for the old global customer count/pacing, preserve `RENEWAL_INFRASTRUCTURE_RETRY_CODES` for the cap, and define an exact infrastructure-pacing subset for live-readiness, malformed, rejected, configuration, and unavailable-before-submission outcomes. Provider-rate-limit and account-cooldown outcomes continue using their durable cooldown paths rather than the infrastructure timestamp. Add a pure `renewal_failure_disposition` function that receives the snapshotted policy, the count of qualifying automatic failures including the newly resolved one, and the failure timestamp. It returns `RetryScheduled`, `RemainPastDue`, or `MarkUnpaid`. The first failure uses schedule index zero; when `failure_count > retry_delays.len()`, the policy is exhausted.

In `event.rs`, replace the ambiguous scalar retry field with a closed failure disposition. Add `BillingEvent::SubscriptionEnded` and `BillingEventKey::SubscriptionEnded(SubscriptionId)`, with reason `SubscriptionEndReason::NonPayment`. Include the final attempt ID for correlation, `ended_at` for the terminal transition, and a separate `access_ends_at` derived from the snapshotted access policy. One subscription can produce the semantic terminal key only once. Keep voluntary `SubscriptionCanceled` separate.

Update `SubscriptionPaymentStateSnapshot` in `attempt.rs` so the optimistic snapshot rejects both `Canceled` and `Unpaid`, then adapt its core callers/tests in `recovery.rs`, `payment_method_update.rs`, and `resolver.rs`. Separately update the PostgreSQL raw status predicates audited in Milestone 5 so an unpaid subscription cannot yield renewal, recovery, or payment-method-update authority.

Allow `AppliedSubscriptionDiscount::new` in `subscription.rs` to represent a limited discount before any recurring period has been consumed (`periods_remaining == total`), while continuing to reject remaining counts greater than the total. Keep completed-row exclusion in the PostgreSQL status/view constraints, where it already belongs. Add round-trip tests for zero-applied paid-trial state and the existing immediate-recurring state.

Add pure tests with descriptive names covering period validation, seven-day calculation, multi-month clamping, currency mismatch, no-trial initial charge, paid-trial initial charge, discount application to recurring rather than trial, limited-month rejection for fixed-day and multi-month recurring cadences, indefinite-discount acceptance for those cadences, empty and multi-step schedules, overflow, the exact failure-count indexing rule, both exhaustion actions, and all new string round trips. Run from the repository root:

    cargo test -p syrup-rail

Expect exit status zero. Before the implementation, tests that construct a paid trial or `Unpaid` status should not compile; after this milestone they pass without any PostgreSQL feature.

### Milestone 3: Persist accepted terms and create paid-trial subscriptions

At the end of this milestone, the enrollment service can charge a paid trial, survive replay or reconciliation, and create a subscription that knows the later recurring price and policy.

In `crates/syrup-rail-postgres/src/discounts.rs`, retain `SubscriptionOfferStore::lock_current_offer` for catalog-level discount operations. Add `lock_enrollment_offer(connection, context)` with a default implementation that delegates to `lock_current_offer`. The typed context carries scope, subscriber, plan, stable attempt and idempotency identity, and the reservation or submission-admission stage. Document that an override may lock host eligibility state using only the supplied connection and return paid-trial or immediate-recurring terms. It must not acquire a second connection, and an attempt-history query must exclude the supplied attempt ID. Enrollment reservation and final submission admission use this subscriber-aware hook; discount administration continues using the catalog hook.

In `attempts.rs`, change reservation/admission from expected charge to expected terms. `enrollment_request_from_locked_terms` must calculate the actual initial amount, build a v2 fingerprint containing plan, start kind, trial charge/rule, recurring base charge/rule, failure policy, and discount snapshot, and populate the new `PaymentAttemptTarget::SubscriptionInitial` terms. For a discounted enrollment, persist `subscription_initial_recurring_base_amount_cents` from the saved snapshot's base charge, not a later catalog price; its discounted charge is the next effective recurring amount. `insert_initial_attempt`, `PAYMENT_ATTEMPT_SELECT`, `payment_attempt_from_row`, `target_from_row`, replay matching, and all fixture inserts must persist and reconstruct the complete snapshot.

Keep a v1 fingerprint verifier for upgraded terms-version-1 attempts. New commands always create terms version 2. Reconciliation through `SubscriptionEnrollmentReservation::from_attempt` must use only durable fields and must not call the current offer store. Matching submitted/terminal replays still resolve before host admission, matching prepared replays rebind to the durable attempt ID, and changed trial or dunning terms cause the existing typed `EnrollmentTermsChanged` rejection before provider I/O.

In `enrollment_application.rs`, replace the hard-coded monthly first period with `next_billing_period` over the accepted start rule. A paid trial uses the trial charge already submitted and stores phase `paid_trial`. Immediate recurring uses the recurring rule and stores phase `recurring`. In both cases, copy the recurring rule, optional trial history, failure policy, next renewal, and next payment to the subscription. Compute `billing_subscriptions.amount_cents` as the next effective recurring charge, never as the trial charge.

Adjust discount creation so paid-trial approval creates the applied subscription discount with `periods_applied = 0` and does not complete a one-month discount. Immediate-recurring approval keeps `periods_applied = 1`. The first successful recurring charge after a trial increments to one for both limited and indefinite discounts; later limited charges retain existing progression/completion behavior, while an indefinite discount remains active at one. Keep the subscription's eventual post-discount base amount sourced from the saved snapshot.

Extend `SubscriptionStarted` with `phase: SubscriptionPhase`. Its `charge` remains the amount actually collected and its `period` remains the exact period bought. Thus IdentityPro's event reports USD 1.00 and seven days while the returned `Subscription::recurring_charge()` reports the normal monthly amount.

Close the compile-wide core/v2 compatibility boundary before running this milestone's tests. Adapt every workspace construction of the changed offers, subscriptions, enrollment targets, events, and outcomes, and make every exhaustive match handle `Unpaid` with the final no-authority semantics specified in Milestone 5; do not add a wildcard merely to compile. In `cancellation.rs`, make the existing active-cancellation UPDATE clear `next_payment_attempt_at`; keep the new past-due cancellation behavior in Milestone 5. In the successful renewal and recovery UPDATEs in `enrollment_application.rs`, set `phase = 'recurring'` and move `next_payment_attempt_at` to the same new period end as `next_renewal_at`; Milestone 4 will replace their hard-coded period derivation with the stored rule and add dunning-specific outcome handling. Update every compiled module's direct fixtures to valid v2 terms rather than running them on v1. This compatibility slice is required because Cargo compiles the whole package and `cargo test -p syrup-rail-postgres enrollment` also executes the existing renewal and recovery tests under the `enrollment_application` module path. Prove the boundary first with:

    cargo check --workspace --all-targets --all-features

Add package tests in a new focused `crates/syrup-rail-postgres/src/paid_trial_dunning_tests.rs` module registered under `#[cfg(test)]` from `src/lib.rs`, rather than further enlarging `enrollment_application.rs`. Cover full-price and discounted paid trials, limited and indefinite zero-applied discounts, immediate recurring compatibility, offer/command mismatch, idempotent replay, durable reconstruction, foreground approval, reconciled approval, initial decline (no subscription), trial cancellation before renewal, currency mismatch, and discount-period accounting. Include a saved-discount replay where the catalog recurring price changes after the claim: matching non-price terms still use the saved base/discounted charges, while a changed trial, cadence, or failure policy rejects before provider I/O. Use the existing fake gateway and host transaction coordinator; do not add live NMI calls.

Run:

    cargo test -p syrup-rail-postgres paid_trial
    cargo test -p syrup-rail-postgres enrollment
    cargo test -p syrup-rail-nmi

Expect zero failures. The NMI adapter should require no production behavior change because it already receives one exact one-shot amount; its tests prove the richer terms do not leak into provider wire vocabulary.

### Milestone 4: Apply dunning and terminal nonpayment through every outcome path

At the end of this milestone, due work follows each subscription's snapshotted schedule, all determinate automatic outcome paths share one atomic transition, and exhausted IdentityPro-style policy produces exactly one terminal event and no later charge authority.

Create `crates/syrup-rail-postgres/src/renewal_failure.rs`, register it as a crate-private module in `src/lib.rs`, and make it the single internal owner of automatic customer-failure history and application. Add one crate-private history loader whose SQL definition of a qualifying failure is: `attempt_kind = 'subscription_renewal'`, `submitted_at IS NOT NULL`, status `declined` or `failed`, `resolution_code IS NULL`, and the same subscription and economic period anchor. Canonical `PaymentResolutionCode` values describe operational or stale-state resolution rather than a customer decline, so none consume dunning. Return the checked count plus first, previous, and latest failures ordered deterministically by `resolved_at, id`; reuse this loader for dunning application and Milestone 5 cancellation instead of duplicating the predicate. The v1 upgrade SQL necessarily spells out the same predicate, so reuse the mixed upgrade fixture from Milestone 1 to prove parity.

Expose the application function with the interface specified below. The caller must already hold the host recipient transaction and subscription aggregate lock. The helper reloads/revalidates the exact just-resolved durable attempt plus subscription and period, requires that attempt to be the latest qualifying failure, checked-converts the count to `u16`, and derives both the current and immediately previous pure dispositions from ordered history. Apply the first failure only from the original active due state. For later failures, the previous disposition must have been `RetryScheduled`, and the stored row must still be past due, nonterminal, and have a non-null payment time that was due no later than the current attempt's durable `submitted_at`; a previous exhausted disposition cannot admit another automatic attempt. This due-shape check deliberately does not require equality with the originally scheduled timestamp because deterministic tests advance the scheduler clock after first asserting that timestamp. Return `Noop` when status, `next_payment_attempt_at`, and `unpaid_at` already match the current disposition. A stale older attempt is also a no-op, while any other projection mismatch is invalid durable state rather than permission to regress or skip a dunning step. Return the ordered events to append only for a newly applied transition. For terminal nonpayment, derive event `access_ends_at` from the stored policy: final `resolved_at` for continuing dunning access, or the first qualifying failure time for immediate suspension. A missing required history timestamp or a count outside the typed envelope is invalid durable state. In particular, the operator-review caller must reload the attempt after its SQL update from `review_required` to `failed` rather than pass the stale in-memory value.

In `src/renewal.rs`, change `due_renewals` to select by `next_payment_attempt_at <= clock_timestamp()`, order the bounded batch by `next_payment_attempt_at ASC, id ASC`, and return `next_renewal_at` as `RenewalDispatch.period_start_at`. Join attempt history by that economic period anchor. Remove the global customer terminal count and last-customer-failure pacing predicates; those are represented by the stored next payment time. Retain blocking attempt checks, payment-method-update fences, gateway/account cooldowns, provider pacing, the finite automatic infrastructure-attempt cap, and a 24-hour predicate over the latest non-cooldown infrastructure failure. Replace the old terminal fields in `RenewalAttemptState` with `attempt_sequence_count`, `automatic_dunning_failure_count`, `automatic_infrastructure_attempt_count`, `last_automatic_infrastructure_failure_at`, `provider_rate_limited_attempt_count`, `last_provider_rate_limited_at`, and `has_blocking_attempt`. `blocks_automatic_retry` uses the new infrastructure timestamp/constant and the existing provider fast/slow rule, never the customer-failure timestamp. The attempt sequence may continue counting renewal and recovery attempts for unique provider order references, but dunning counts only submitted automatic renewals.

In `attempts.rs`, renewal and recovery reservation continue calculating the charge period from `next_renewal_at` using the stored recurring period rule. Payment-due admission checks `next_payment_attempt_at`, not merely the economic boundary. A queued `ChargeRenewal` whose period anchor is correct but whose retry time is not yet due is rejected as `PaymentNotDue`; a stale command after cancellation, recovery, or unpaid terminalization is rejected without provider I/O.

Apply the same due/status gate in `SubscriptionBillingService::renewal_gateway_account` before gateway resolution or `account_mode()`: require the command's `period_start_at` to equal the economic `next_renewal_at`, but require `next_payment_attempt_at <= clock_timestamp()` for dispatch. Preserve blocking-attempt, payment-method-update, infrastructure-cap and infrastructure-pacing, account/provider-cooldown, and provider-throttle checks. This preflight and reservation are intentionally repeated around provider readiness to close races; neither may substitute `next_payment_attempt_at` for the period anchor.

In `enrollment_application.rs`, route submitted foreground renewal declines/failures through the new helper after the attempt resolution is persisted. Change the internal event return from `Option<BillingEvent>` to an ordered collection where needed and append every event before commit. The final failure emits `SubscriptionPaymentFailed` first and `SubscriptionEnded { NonPayment }` second. Reconciled renewal outcomes already rebuild `SubscriptionRenewalReservation` and delegate to the foreground outcome function; preserve that single path and add tests proving it yields the same transition.

Building on the v2-compatible successful renewal/recovery projection introduced in Milestone 3, calculate the approved recurring period from the stored rule rather than the monthly wrapper and preserve the atomic assignments `status = active`, phase `recurring`, `next_renewal_at = current_period_end_at`, and `next_payment_attempt_at = current_period_end_at`. The UPDATEs must still revalidate expected payment method, initial transaction, prior status, amount, currency, and period anchor. Recovery is allowed only while `past_due`, never after `unpaid`. A failed recovery records its attempt but does not call the automatic-failure helper or move the scheduler clock.

In `operator_review.rs`, replace `mark_subscription_past_due_for_manual_failure`. A manually failed, submitted `subscription_renewal` uses the same helper and event batch. A recovery failure or an unsubmitted/infrastructure review result does not consume dunning. Preserve unresolved processor-charge fences and same-transaction host target changes.

Audit all alternate outcome paths found by searching for `status = 'past_due'`, `SubscriptionPaymentFailed`, `resolve_renewal_non_approved_outcome`, `apply_reconciled_subscription_renewal_gateway_outcome`, `fail_review_required_attempt`, `RENEWAL_RETRY_ACCOUNTING_EXCLUDED`, and `RENEWAL_RETRY_PACING_EXCLUDED`. Foreground, exact reconciliation, manual review, duplicate replay, and stale approved evidence must agree. Do not add a scheduler terminalizer job: the final determinate failure already has the locks and evidence needed to terminalize atomically.

Add tests for due selection before/at customer retry boundaries, schedules with zero/one/multiple retries, actual event retry timestamps, terminal access timestamps under both access policies, final `unpaid`, no more due work, one semantic terminal event under replay, unknown outcomes blocking, recovery success between retries, recovery failure not moving the schedule, cancellation racing a queued renewal, foreground versus reconciled parity, operator-review parity, and late approved evidence after unpaid. Separately prove that a live-readiness or unavailable-before-submission failure does not consume dunning, is not immediately due again, and becomes eligible exactly at the retained 24-hour infrastructure boundary; preserve the provider-throttle fast/slow boundary tests. Test direct re-entry of the failure helper with the latest attempt (no-op), an older attempt after a later failure (no-op without scheduler regression), and a deliberately inconsistent subscription projection (typed invalid state). Use fake resolver/gateway call counters to prove early, infrastructure-paced, canceled, recovered, and unpaid queued commands return before resolver, `account_mode()`, or sale I/O. In the late-approval case, retain/park processor charge evidence according to the existing external-reversal policy and never resurrect the unpaid subscription.

Run:

    cargo test -p syrup-rail-postgres renewal
    cargo test -p syrup-rail-postgres dunning
    cargo test -p syrup-rail-postgres operator_review
    cargo test -p syrup-rail-postgres reconciliation

Expect zero failures and no test that relies on wall-clock sleeps. Derive retry expectations from the attempt's persisted database `resolved_at`; after asserting the scheduled value, use an exact fixture update to make `next_payment_attempt_at` due before invoking the next attempt. Use a separately seeded calendar boundary for month-clamping assertions rather than assuming the database clock equals a literal date.

### Milestone 5: Make entitlement, cancellation, and dependent readers exhaustive

At the end of this milestone, the new status and access policy are reflected consistently outside payment application, and a subscriber can stop retries without compromising in-flight payment safety.

In `entitlement.rs`, load phase, period rule, failure policy, and next payment into `Subscription`. Add the exact public `PastDueAccess::{AllowedDuringDunning, Suspended}` fact described below and include it in `Entitlement::PastDue`, so presentation does not have to reinterpret configuration plus scheduler state. Put the pure classifier in core and reuse it in both the read query projection and `require_entitlement_for_update`: access is allowed only when the stored policy is `ContinueUntilDunningExhausted` and a next payment is still scheduled. `SuspendImmediately` and exhausted `RemainPastDue` classify as suspended and return the existing typed past-due denial from the protected-write guard. `unpaid` grants no entitlement and must not be selected as a current paid subscription. Keep active grants independent and preserve the invalid multiple-current-source checks.

In `cancellation.rs`, allow `PastDue` to follow the same renewal and payment-method-update fences as active cancellation. Both active and past-due UPDATE paths clear `next_payment_attempt_at`, as required by the v2 canceled-row shape. Active cancellation keeps access through `current_period_end_at`, including an active paid trial. Past-due cancellation returns the persisted database `canceled_at`. Use it as event `access_ends_at` only when policy is `ContinueUntilDunningExhausted` and a next payment was scheduled before the UPDATE; otherwise use the first or latest durable failure returned by `renewal_failure.rs`'s shared history loader: first for `SuspendImmediately`, latest for exhausted `RemainPastDue` under the continuing-access policy. The loader is already scoped to the subscription's current `next_renewal_at` anchor and exact customer-failure classification. A past-due row without the causal failure required by its state is invalid durable state. A concurrent admitted or unknown renewal/recovery still blocks cancellation. `Unpaid` is already terminal and is not treated as a current cancellable subscription.

Preserve the public idempotence contract after a past-due period has ended. Keep active/past-due/paid-through-canceled selection authoritative; only when no such current row exists may cancellation lock the latest exact subscription row of any status by scope, subscriber, and plan. Define latest by immutable lifecycle creation order, `created_at DESC, id DESC`, not mutable `updated_at`. Return `AlreadyCanceled` only when that newest row is canceled; return `NotFound` when it is unpaid. This prevents a historical canceled row from shadowing a newer active, past-due, or unpaid lifecycle. Test both a past-due cancel replay with no duplicate `SubscriptionCanceled` event and an older-canceled/newer-unpaid history that still returns `NotFound` after the older row's `updated_at` is made later than the unpaid row.

Audit and update every exhaustive status match and every raw status predicate in `grants.rs`, `deletion.rs`, `reconciliation.rs`, `processor_charges.rs`, `subscription_billing_service.rs`, `enrollment_application.rs`, payment-method update code, schema-contract fixtures, and test fixtures. In particular, change `disable_payment_method_if_unreferenced` in `enrollment_application.rs`: its current `subscriptions.status <> 'canceled'` predicate would let an unpaid terminal row keep an otherwise unreferenced method active forever. Only collection-capable `active` or `past_due` subscriptions prevent disabling; canceled and unpaid rows remain financial references but not usable collection authority. `Unpaid` is terminal financial history: it does not block a new current subscription, does not allow recovery or protected writes, remains subject to financial retention/scrubbing rules, and must never be rewritten to `canceled` by deletion cleanup. Deletion blockers should treat unpaid like other terminal financial history according to the existing charge/retention evidence, not as an active subscription.

Update `billing_current_subscriptions` to include active and past-due rows plus paid-through canceled rows exactly as before; exclude unpaid. Extend selected columns with the terms needed by canonical entitlement readers. Update any fact/read view that intentionally exposes status so `unpaid` passes through without host vocabulary.

Add tests for active trial access, paid-through trial cancellation, both past-due access policies, exhausted remain-past-due suspension, unpaid missing entitlement, cancellation during scheduled dunning, continuing-access cancellation timestamp equality, suspended-access first-failure reporting, exhausted-access last-failure reporting, idempotent past-due cancellation replay, older-canceled/newer-unpaid replay selection, cancellation blocked by an in-flight attempt, stale queued work after cancellation, grant conflicts, payment-method disabling when only canceled/unpaid references remain, and deletion/scrub behavior on unpaid history.

Run:

    cargo test -p syrup-rail-postgres entitlement
    cargo test -p syrup-rail-postgres cancellation
    cargo test -p syrup-rail-postgres deletion
    cargo test -p syrup-rail-postgres grants

Expect zero failures and exhaustive Rust matches with no wildcard added merely to silence the new status.

### Milestone 6: Finish compatibility, documentation, metadata, and repository gates

At the end of this milestone, a consumer can understand and adopt the new API and schema artifacts, all generated metadata matches v2, and the repository's complete contract is green.

Update `README.md` with a concise package-level example showing immediate recurring and optional paid-trial offers. Document schedule semantics, the distinction between dunning and infrastructure retry, terminal unpaid, and the host event boundary. Update `CHANGELOG.md` under `[Unreleased]`; state that the eventual compatible release line is 0.2.0, but leave `Cargo.toml`, internal dependency requirements, the lockfile version entries, publication, and tagging to the separate release task.

Update crate guides and schema READMEs so v2 is the current contract and v1 remains immutable historical input. Confirm `tools/sqlx-gate` now reports and installs v2. Update `.jig.toml` and generated workflow path filters carefully. If `scripts/jig update` would introduce unrelated template drift, do not accept it blindly; make the smallest documented generated update supported by the repository and review the diff.

Refresh SQLx metadata only after the final v2 schema and queries are stable:

    scripts/check-sqlx.sh --prepare
    scripts/jig check sqlx

Review every metadata change and ensure no development database URL or credential is committed. Then run this audit search and inspect every result rather than assuming compilation found dynamic SQL strings:

    rg -n "next_renewal_at|next_payment_attempt_at|past_due|canceled|unpaid|SubscriptionStatus::|SubscriptionOffer|SubscriptionEnrollmentExpectedCharge|BlockedByPastDue|next_monthly_billing_period|offer\.base_charge\(\)|MAX_RENEWAL_TERMINAL|RENEWAL_RETRY_AFTER_SECONDS|RENEWAL_RETRY_ACCOUNTING_EXCLUDED|RENEWAL_RETRY_PACING_EXCLUDED|last_terminal_at|terminal_attempt_count|subscriptions\.status <> 'canceled'" crates tools README.md CHANGELOG.md .github .jig.toml

The old enrollment type, past-due cancellation blocker, monthly-only period helper, offer-level `base_charge()` calls, customer-terminal constants, overloaded terminal-count/timestamp fields, obsolete customer accounting/pacing sets, and the canceled-only payment-method reference predicate should have no production references. Remaining `next_renewal_at` uses must mean the economic period anchor. Remaining raw status strings must have an explicit reason for including or excluding unpaid.

Run formatting and focused gates first, then the full required suite:

    scripts/jig check fmt
    scripts/jig check clippy
    scripts/jig check contract
    scripts/jig check agent-map
    scripts/jig check agent-guides
    scripts/jig check rust-file-loc --changed-against v0.1.1
    scripts/jig check migration-immutability --changed-against v0.1.1
    scripts/jig check sqlx
    scripts/jig check test

All commands must exit zero. `scripts/jig check test` runs the complete workspace with all features and is the final backend acceptance gate required by `AGENTS.md`. Review `git diff --check`, `git status --short`, and the complete diff. Confirm that `schema/v1/install.sql` is byte-identical to `v0.1.1`:

    git diff --exit-code v0.1.1 -- crates/syrup-rail-postgres/schema/v1/install.sql crates/syrup-rail-postgres/schema/v1/README.md

If structured Jig work is available, attach receipts with `scripts/jig work check`, `scripts/jig work evidence`, and `scripts/jig work gates` using the implementation plan ID. Do not repurpose the unrelated open repository-establishment plan noted above.

## Concrete Steps

All commands below run from `/home/aa/Documents/syrup-rail`.

First establish the baseline and inspect structured work ownership:

    cd /home/aa/Documents/syrup-rail
    git status --short --branch
    scripts/jig doctor
    scripts/jig work status
    cargo test -p syrup-rail

Expected baseline evidence is the recorded branch/status plus passing core tests; do not assume the worktree is clean. Preserve every unrelated user change and adapt the implementation around it. If the old open Jig plan remains, do not close it merely to make `work start` succeed; either coordinate its ownership or maintain progress directly in this checked-in ExecPlan until a separate implementation plan can be registered.

Implement Milestone 1 before the breaking core API or any production PostgreSQL query changes. Test the read-only v1 preflight, a fresh v2 installation, and a real v1-plus-upgrade installation. Capture concise evidence such as:

    test schema_contract::tests::schema_v1_upgrade_preflight_reports_only_blockers ... ok
    test schema_contract::tests::schema_v2_fresh_install_conforms ... ok
    test schema_contract::tests::schema_v1_upgrade_matches_fresh_v2 ... ok

Do not record an expected test count because the suite will grow during implementation; exit status zero and the named behavioral tests are the stable acceptance signal.

Next implement Milestone 2 in small core-crate slices. After adding terms and period policy, run core tests. After changing enrollment expectations/fingerprints, run them again. After statuses and events, run them again. The PostgreSQL package may be temporarily uncompilable after a breaking core slice; proceed directly into Milestone 3's compile-wide compatibility boundary, run `cargo check --workspace --all-targets --all-features`, and do not begin behavioral PostgreSQL work until it is green. Update `Progress`, `Surprises & Discoveries`, and `Decision Log` at every stopping point.

Complete Milestones 3 through 5 against v2 test databases. Prefer a new `crates/syrup-rail-postgres/src/paid_trial_dunning_tests.rs` included under `#[cfg(test)]` from `src/lib.rs` for cross-module scenarios. Keep narrow unit tests beside pure helpers. Avoid adding more large inline test blocks to `attempts.rs` or `enrollment_application.rs`, which are already several thousand lines.

Before final gates, run the complete behavioral scenario described below and save the named test output in `Artifacts and Notes`. Then refresh SQLx metadata, run all gates, and update this plan's outcome sections. Do not publish crates or change IdentityPro in this repository task.

## Validation and Acceptance

The primary acceptance proof is a PostgreSQL package test using database-derived timestamps and a fake provider. Configure an offer with a USD 1.00 paid trial for seven fixed days, a normal recurring charge such as USD 29.00 for one calendar month, retry delays of one day and three days, exhaustion `MarkUnpaid`, and access `ContinueUntilDunningExhausted`. Let `T0` be the initial attempt's persisted `submitted_at`; derive every assertion from durable row timestamps rather than assuming the test database clock equals a literal date.

Approve the USD 1.00 initial attempt. Observe an active subscription with phase `paid_trial`, current period ending `T0 + 7 days`, recurring charge USD 29.00, and both next timestamps at that trial boundary. Observe one `SubscriptionStarted` event reporting USD 1.00 and the exact seven-day period. Separately seed a pure/package boundary at `2026-08-08T00:00:00Z` to prove the one-calendar-month recurring rule ends at `2026-09-08T00:00:00Z` without depending on wall time.

After asserting the untouched enrollment result, drive the trial boundary without sleeping by coherently shifting the fixture's seven-day `current_period_start_at`, `current_period_end_at`, `next_renewal_at`, and active `next_payment_attempt_at` so the common end/anchor is a captured due database timestamp; do not update one anchor in isolation or violate the active-row constraint. Reserve the due renewal and observe a USD 29.00 request for the one-month recurring period anchored there. Resolve it as a submitted decline and let `F1` be its persisted `resolved_at`. Observe `past_due`, an unchanged economic anchor, `next_payment_attempt_at = F1 + 1 day`, allowed dunning access, and a payment-failure event reporting that exact timestamp.

After asserting the stored schedule, make the retry due with an exact fixture update rather than a sleep. Resolve it as a submitted decline, let `F2` be its persisted `resolved_at`, and observe `next_payment_attempt_at = F2 + 3 days`. Make the final retry due the same way, resolve it, and let `F3` be its persisted `resolved_at`. Observe terminal `unpaid`, `unpaid_at = F3`, no next payment, no due dispatch, no protected-write entitlement, one final payment-failure event, and exactly one `SubscriptionEnded { reason: NonPayment, ended_at: F3, access_ends_at: F3 }` event for the continuing-access policy. Replay the final provider outcome and rerun due selection; observe no new event and no resolver, readiness, or sale call.

Add a parallel scenario in which recovery succeeds after the first automatic failure. Observe that it charges USD 29.00 for the recurring period anchored at the unchanged trial boundary, returns the subscription to active recurring, restores both next timestamps to that period's end, advances a recurring discount exactly once if present, and makes every stale queued retry reject before provider I/O.

Prove alternate safety paths:

- A no-trial offer still charges the recurring amount initially and creates the recurring period immediately.
- A finite `LimitedMonths` discount is rejected for fixed-day or multi-month recurring cadence before a claim or payment attempt is created; an indefinite discount remains valid for those cadences.
- A declined USD 1.00 initial attempt creates no subscription and no started event.
- A pre-submission infrastructure failure and provider throttle do not consume a dunning step or cause unpaid; a non-cooldown infrastructure failure is suppressed before resolver I/O until its 24-hour operational boundary, while provider throttles retain their fast/slow cooldown behavior.
- An unknown attempt blocks another mutation until reconciliation.
- A failed user recovery leaves the automatic next payment unchanged.
- Voluntary cancellation during an active trial preserves access through trial end and prevents the full charge.
- Voluntary cancellation during past due clears future retries and ends access immediately after in-flight fences clear; replay returns `AlreadyCanceled` without another event, while a newer unpaid lifecycle is never shadowed by an older canceled row.
- A late approved observation after terminal unpaid is retained/parked for external reversal and cannot reactivate the subscription.
- The checked-in read-only preflight reports the anomalous v1 past-due fixture before maintenance, and the upgrade still rejects it authoritatively if present.
- Fresh schema v2 and v1 upgraded to v2 pass the same conformance fingerprint.
- Legacy upgraded subscriptions retain one-month/no-trial/legacy-past-due behavior.

The complete acceptance command is:

    scripts/jig check test

It must exit zero after the specific scenario tests above have been observed by name. Also require `scripts/jig check sqlx`, `scripts/jig check migration-immutability --changed-against v0.1.1`, `scripts/jig check clippy`, and `scripts/jig check contract` to exit zero.

## Idempotence and Recovery

Core Rust edits and tests are safe to rerun. Use `cargo fmt` only through the normal formatter and review its scope. Never use destructive Git commands to recover; preserve unrelated work and use focused patches.

Schema v1 is immutable. The v2 install, preflight, retry-reclassification audit, and upgrade SQL artifacts are edited before any host materializes them, but once this feature ships they become immutable as well. The preflight and audit are read-only and safe to rerun before maintenance; the preflight is advisory, while the audit is informational. The v1-to-v2 upgrade must repeat its causal-history validation in the same transaction as the DDL/backfill. If any constraint, backfill, causal-history validation, or view recreation fails before commit, the host transaction can roll back without a half-upgraded catalog and 0.1 can resume. A past-due row reported without either a qualifying automatic renewal failure or the operator-reviewed, manually failed active-snapshot recovery supported by v1 must be investigated and repaired under v1 before retrying; do not synthesize payment evidence or bypass the check. Do not use `IF EXISTS` or `IF NOT EXISTS` to hide unexpected canonical drift. Test retries by creating a new ephemeral database, not by repeatedly applying a one-shot migration to the same database. For a real upgrade, prebuild 0.2 and keep 0.1 billing writers stopped from before the migration begins until 0.2 is serving; after migration commit, recovery is roll-forward and 0.1 must not restart against v2.

Provider submission remains at most once. Never recover a failed application by submitting a second sale. Foreground storage/application failures retain attempt and processor evidence; reconciliation rebuilds authority from the exact durable attempt. If an approved observation arrives after cancellation, recovery, or unpaid terminalization, preserve the existing external-reversal/manual-review behavior.

The dunning transition itself is idempotent because it runs only when the exact attempt changes from unresolved to its terminal result under the aggregate lock. A replayed terminal attempt sees its already-resolved state and returns no lifecycle events. The subscription's period anchor plus attempt sequence keeps retry provider references unique.

If SQLx preparation fails, leave existing metadata intact, repair the schema/query mismatch, and rerun `scripts/check-sqlx.sh --prepare`. Do not hand-edit `.sqlx/*.json`. If generated Jig files show unrelated changes, revert only those generated changes with a focused patch after confirming they are unrelated; never reset the whole worktree.

## Artifacts and Notes

During implementation, record concise evidence here rather than pasting full logs. At minimum retain:

- The named pure-policy tests proving schedule indexing and period calculations.
- The named discount-cadence compatibility tests.
- The named read-only upgrade-preflight tests.
- The named schema fresh/upgrade parity tests.
- The named paid-trial and terminal-dunning scenario tests.
- The final `scripts/jig check test` and `scripts/jig check sqlx` exit summaries.
- The `git diff --exit-code v0.1.1 -- crates/syrup-rail-postgres/schema/v1/install.sql crates/syrup-rail-postgres/schema/v1/README.md` proof.

Initial investigation evidence:

    SubscriptionOffer fields: plan_key, base_charge
    Initial period policy: next_monthly_billing_period(submitted_or_created_at)
    Existing customer retry policy: five counted attempts, 24-hour pacing
    Existing exhausted state: past_due with no terminal transition
    Current schema contract: version 1, immutable after host materialization

Milestone 1 evidence (2026-08-09):

    cargo test -p syrup-rail-postgres schema_contract --features schema-contract-test-support
    16 passed; 0 failed
    schema_v1_upgrade_preflight_reports_only_blockers ... ok
    schema_v1_upgrade_anomaly_rolls_back_with_named_diagnostic ... ok
    schema_v1_upgrade_backfills_legacy_lifecycle_and_attempt_terms ... ok
    schema_v1_upgrade_matches_fresh_v2 ... ok
    schema_v2_fresh_install_conforms ... ok
    schema_v2_term_schedule_status_and_discount_shapes_are_constrained ... ok
    git diff --exit-code v0.1.1 -- schema/v1/install.sql schema/v1/README.md ... exit 0
    scripts/jig check migration-immutability --changed-against v0.1.1 ... exit 0
    scripts/jig check contract ... exit 0 (receipt_01KZK3R33AP8VAT0FMQQHA79NM)

Milestone 2 evidence (2026-08-09):

    cargo test -p syrup-rail
    81 passed; 0 failed
    policy::tests::fixed_day_policy_adds_exact_utc_days ... ok
    policy::tests::multi_month_policy_clamps_from_each_previous_boundary ... ok
    renewal::tests::dunning_failure_count_is_one_based_and_indexes_the_current_step ... ok
    renewal::tests::empty_schedule_and_mark_unpaid_exhaust_immediately ... ok
    enrollment::tests::initial_charge_uses_trial_but_discount_applies_to_recurring ... ok
    enrollment::tests::limited_month_discount_requires_monthly_cadence_but_indefinite_does_not ... ok
    discount::tests::discount_quote_uses_recurring_charge_not_trial_charge ... ok

Milestones 3 through 5 evidence (2026-08-09):

    cargo test -p syrup-rail-postgres paid_trial
    10 passed; 0 failed
    paid_trial_dunning_transitions_to_unpaid_once_with_exact_schedule_and_events ... ok
    paid_trial_recovery_collects_discounted_recurring_period_and_invalidates_queued_retry ... ok
    paid_trial_and_scheduled_dunning_cancellation_preserve_exact_access_and_stop_collection ... ok
    enrollment_compatibility_decline_reconciliation_replay_and_term_mismatch_are_durable ... ok
    indefinite_discounted_paid_trial_starts_with_zero_recurring_periods_applied ... ok
    access_policies_drive_terminal_and_cancellation_timestamps_without_reinterpreting_terms ... ok
    newer_unpaid_history_allows_grants_and_deletion_scrub_without_becoming_canceled ... ok
    infrastructure_failure_is_paced_for_twenty_four_hours_without_consuming_dunning ... ok
    v1_active_recovery_authority_survives_the_v2_cutover ... ok
    manual_failure_uses_the_paid_trial_subscription_dunning_policy ... ok

    cargo test -p syrup-rail-postgres --all-features
    98 passed; 0 failed

    Primary scenario observed USD 1.00 / seven-day paid trial, USD 29.00 monthly terms,
    F1 + one-day and F2 + three-day retries, terminal unpaid at F3, ordered failure/end
    events, replay/stale-helper no-ops, no due work, no entitlement, and late-approval
    external-reversal parking. Parallel recovery observed a failed recovery preserving the
    scheduler, unknown-outcome fencing, reconciled USD 23.20 discounted recovery at the
    unchanged trial boundary, zero-to-one discount progression, and stale-job rejection.

Milestone 6 final evidence (2026-08-09):

    scripts/check-sqlx.sh --prepare ... exit 0; no .sqlx diff
    scripts/jig check sqlx ... exit 0 (receipt_01KZK9J9DC236Z9557CK9XWKZ2)
    scripts/jig check fmt ... exit 0 (receipt_01KZK9GTD2BYM09QYTDGNQKZST)
    scripts/jig check clippy ... exit 0 (receipt_01KZK9H0BEVR8PH7C20JMNZ0VC)
    scripts/jig check contract ... exit 0 (receipt_01KZK9QTDGYJYYN8YPXH502BQN)
    scripts/jig check agent-map ... exit 0
    scripts/jig check agent-guides ... exit 0
    scripts/jig check rust-file-loc --changed-against v0.1.1 ... exit 0
    scripts/jig check migration-immutability --changed-against v0.1.1 ... exit 0
    scripts/jig check test ... exit 0 (receipt_01KZK9N8RWE1SPWSV5H55ZJE95)
    git diff --exit-code v0.1.1 -- schema/v1/install.sql schema/v1/README.md ... exit 0
    git diff --check ... exit 0
    focused scenario files after policy split: 500, 369, 338, 328, 289, 240, 187, 179, and 153 LOC

Post-review correction evidence (2026-08-09):

    cargo test -p syrup-rail-postgres paid_trial ... 10 passed; 0 failed
    scripts/jig check test ... exit 0 (receipt_01KZKDKD13V8472BGV1JDSC86S)
    scripts/jig check fmt ... exit 0 (receipt_01KZKDKKVEGEGP9MVJHB2BT44Z)
    scripts/jig check sqlx ... exit 0 (receipt_01KZKDN18P2M5EW0QXY3NDJKGF)
    scripts/jig check clippy ... exit 0 (receipt_01KZKDNY37064MQYFNAE9C3SXE)
    scripts/jig check contract ... exit 0 (receipt_01KZKDP1FD2RH4C2NQ4G4NDF0G)
    scripts/jig check rust-file-loc --changed-against v0.1.1 ... exit 0
    scripts/jig check agent-map ... exit 0
    scripts/jig check agent-guides ... exit 0
    scripts/jig check migration-immutability --changed-against v0.1.1 ... exit 0
    scripts/jig check rust-file-loc --staged --json ... errors: []; ok: true
    git diff --check ... exit 0

    The cutover matrix begins with four version-1 active recovery attempts in
    prepared, submitted-pending, unknown, and review-required states. After the
    transactional upgrade, every exact durable attempt admits or reconciles an
    approved result, advances its period, and emits one renewal event. A fresh
    recovery against the resulting active subscription remains rejected.

Comprehensive-review follow-up evidence (2026-08-09):

    late_approval_and_reversal_preserve_terminal_failure_history_and_cancellation ... ok
    historical_code_records_remain_listable_and_disableable_after_cadence_drift ... ok
    enrollment_application::tests ... 20 executed; 20 passed after one isolated container-start retry
    discounts::tests ... 5 passed; 0 failed
    cancellation filter ... 6 passed; 0 failed
    temporary-index rust-file-loc --changed-against v0.1.1 ... ok with all new files staged in the isolated index
    split enrollment files: 712, 645, 569, 509, 491, 396, 283, and 146 LOC
    scripts/jig check fmt ... exit 0 (receipt_01KZKVJBAFM65GMAECZ62VSKVX)
    scripts/jig check clippy ... exit 0 (receipt_01KZKVJVCJYS2DQBYA83E9WG9M)
    scripts/jig check contract ... exit 0 (receipt_01KZKVJVM9AHT8QAXW607PWWMD)
    scripts/jig check sqlx ... exit 0 (receipt_01KZKVM1SJX9XJWKYH40C8KXM5)
    scripts/jig check test ... exit 0 (receipt_01KZKVFTTS1GEG94NT6RSZEA9G)
    scripts/jig check test-locked ... exit 0 (receipt_01KZKW42TEBJ8YP0KNE3WZNKPW)

    A repeated parallel test run later saturated the disposable-container
    startup limit: 100 PostgreSQL tests passed and five reported
    WaitContainer(StartupTimeout) before executing. Each of those five passed
    immediately in isolation, and the complete serial locked workspace gate
    then passed. No assertion or application error occurred in that run.

Host-boundary correction evidence (2026-08-09):

    enrollment_offer_hook_excludes_one_in_flight_identity_across_both_stages ... ok
    fixed_day_recurring_cadence_persists_and_drives_the_next_renewal ... ok
    paid_trial_dunning_transitions_to_unpaid_once_with_exact_schedule_and_events ... ok
    cargo test -p syrup-rail ... 84 passed; 0 failed
    cargo clippy -p syrup-rail-postgres --all-targets -- -D warnings ... exit 0
    RUSTDOCFLAGS='-D warnings' cargo doc -p syrup-rail-postgres --no-deps ... exit 0
    scripts/jig check test --no-receipt ... exit 0
    scripts/jig check sqlx --no-receipt ... exit 0
    scripts/jig check fmt --no-receipt ... exit 0
    scripts/jig check contract --no-receipt ... exit 0
    scripts/jig check agent-guides ... exit 0
    scripts/jig check agent-map ... exit 0
    scripts/jig check rust-file-loc --changed-against v0.1.1 ... exit 0
    scripts/jig check migration-immutability --changed-against v0.1.1 ... exit 0
    git diff --exit-code v0.1.1 -- schema/v1/install.sql schema/v1/README.md ... exit 0
    git diff --check ... exit 0

    The custom host store queries real initial-attempt history. Its reservation
    call observes no current attempt; its final-admission call observes the one
    persisted in-flight attempt but excludes it by the stable context ID; and a
    later enrollment observes that first attempt as prior history and is
    rejected. The fixed-day scenario approves enrollment, persists the 14-day
    recurring rule, reserves the next 14-day period, and approves that renewal.
    The dunning scenario applies one determinate decline concurrently twice and
    proves identical replay results with one state transition and one failure
    event.

Final comprehensive-review correction evidence (2026-08-10):

    cargo test -p syrup-rail --all-targets --locked ... 84 passed; 0 failed
    RUST_TEST_THREADS=1 scripts/jig check test --no-receipt ... 117 PostgreSQL tests; exit 0
    schema_v1_upgrade_preflight_reports_only_blockers ... ok
    schema_v1_upgrade_backfills_legacy_lifecycle_and_attempt_terms ... ok
    scripts/jig check sqlx --no-receipt ... exit 0
    scripts/jig check fmt --no-receipt ... exit 0
    scripts/jig check clippy --no-receipt ... exit 0
    scripts/jig check contract --no-receipt ... exit 0
    scripts/jig check agent-guides ... exit 0
    scripts/jig check agent-map ... exit 0
    scripts/jig check migration-immutability --changed-against v0.1.1 ... exit 0
    temporary-index scripts/jig check rust-file-loc --staged --json ... errors: []; ok: true
    git diff --exit-code v0.1.1 -- schema/v1/install.sql schema/v1/README.md ... exit 0
    git diff --check and git diff --cached --check ... exit 0

    The v1 fixture inserts a submitted review-required recovery with an exact
    active optimistic snapshot, applies the shipped manual-failure updates, and
    proves that preflight does not report it. A submitted, declined recovery
    with the same active snapshot but no retained review marker remains a
    blocker. Upgrade preserves the legitimate recovery evidence, sets the
    scheduler to the economic anchor with zero automatic failures consumed,
    applies the first v2 automatic failure against the existing past-due state,
    and still reports the recovery resolution time through cancellation and a
    pure terminal projection. An unattributed raw past-due row remains a named
    transactional blocker.

Final review-documentation correction evidence (2026-08-10):

    cargo test -p syrup-rail --all-targets --locked ... 84 passed; 0 failed
    RUSTDOCFLAGS='-D warnings' cargo doc -p syrup-rail --no-deps --locked ... exit 0
    RUST_TEST_THREADS=1 scripts/jig check test --no-receipt ... exit 0
    scripts/jig check fmt --no-receipt ... exit 0
    scripts/jig check clippy --no-receipt ... exit 0
    scripts/jig check contract --no-receipt ... exit 0
    scripts/jig check sqlx --no-receipt ... exit 0
    scripts/jig check agent-guides ... exit 0
    scripts/jig check agent-map ... exit 0
    scripts/jig check migration-immutability --changed-against v0.1.1 ... exit 0
    git diff --check ... exit 0

## Interfaces and Dependencies

No new third-party dependency is required. Use existing `chrono`, `thiserror`, `sqlx`, and `uuid`. Keep provider-neutral terms in `syrup-rail`; keep all SQLx types and queries in `syrup-rail-postgres`; do not change NMI wire requests except for compiler-driven adaptation to renamed core APIs.

In `crates/syrup-rail/src/terms.rs`, the final public shape should be equivalent to the following. Fields remain private and constructors/accessors enforce invariants; exact error enum names may follow repository conventions, but do not weaken the modeled states.

    pub const MAX_DUNNING_RETRY_STEPS: usize = 16;

    pub enum SubscriptionPeriodRule {
        FixedDays(NonZeroU16),
        CalendarMonths(NonZeroU16),
    }

    pub struct RecurringSubscriptionTerms {
        charge: ChargeAmount,
        period: SubscriptionPeriodRule,
    }

    pub struct PaidTrialTerms {
        charge: ChargeAmount,
        period: SubscriptionPeriodRule,
    }

    pub enum SubscriptionStart {
        RecurringImmediately,
        PaidTrial(PaidTrialTerms),
    }

    pub struct DunningRetryDelay {
        seconds: NonZeroU32,
    }

    pub struct DunningSchedule {
        retry_delays: Vec<DunningRetryDelay>,
    }

    pub enum DunningExhaustion {
        RemainPastDue,
        MarkUnpaid,
    }

    pub enum PastDueAccessPolicy {
        SuspendImmediately,
        ContinueUntilDunningExhausted,
    }

    pub struct RenewalFailurePolicy {
        schedule: DunningSchedule,
        exhaustion: DunningExhaustion,
        past_due_access: PastDueAccessPolicy,
    }

    pub struct SubscriptionOffer {
        plan_key: PlanKey,
        recurring: RecurringSubscriptionTerms,
        start: SubscriptionStart,
        renewal_failure: RenewalFailurePolicy,
    }

`SubscriptionOffer::new` returns `Result` and requires trial and recurring currency equality. Supply ergonomic constructors for one fixed day/calendar month and delay durations without exposing invalid zero values. A host that does not want a trial explicitly uses `SubscriptionStart::RecurringImmediately`; do not hide a trial in a default.

In `crates/syrup-rail/src/enrollment.rs`, define:

    pub struct SubscriptionEnrollmentExpectedTerms {
        offer: SubscriptionOffer,
        discount_snapshot: Option<SubscriptionDiscountSnapshot>,
    }

    impl SubscriptionEnrollmentExpectedTerms {
        pub fn full_price(offer: SubscriptionOffer) -> Self;
        pub fn discounted(
            offer: SubscriptionOffer,
            snapshot: SubscriptionDiscountSnapshot,
        ) -> Result<Self, SubscriptionEnrollmentTermsError>;
        pub fn initial_charge(&self) -> ChargeAmount;
        pub fn activation_projection(&self) -> SubscriptionActivationProjection;
        pub fn matches_locked_terms(
            &self,
            current_offer: &SubscriptionOffer,
            saved_discount: Option<&SubscriptionDiscountSnapshot>,
        ) -> bool;
    }

Rename `EnrollSubscription::expected_charge` and `SubscriptionEnrollmentReservation::expected_charge` to `expected_terms`. For discounted terms, document that the saved snapshot's base and discounted charges override only the current offer's recurring price after every non-price term matches; snapshot and offer currency must agree, but saved and current base price need not. A `LimitedMonths` snapshot additionally requires an exact one-calendar-month recurring rule, while an indefinite snapshot does not. Those saved charges feed the v2 fingerprint, durable recurring base, initial recurring amount, and eventual post-discount amount. `PaymentAttemptTarget::SubscriptionInitial` carries the durable offer terms, terms version, discount snapshot, and optional application identities. Preserve value-free formatting for secret-bearing command/reservation types.

In `crates/syrup-rail/src/subscription.rs`, make the computed entitlement state explicit:

    pub enum PastDueAccess {
        AllowedDuringDunning,
        Suspended,
    }

    pub const fn classify_past_due_access(
        policy: PastDueAccessPolicy,
        has_scheduled_payment: bool,
    ) -> PastDueAccess;

    Entitlement::PastDue {
        subscription: Subscription,
        access: PastDueAccess,
        next_action: PastDueAction,
        applied_discount: Option<AppliedSubscriptionDiscount>,
    }

Use this one pure classifier for the field and PostgreSQL protected-write admission. Pass `next_payment_attempt_at.is_some()` as `has_scheduled_payment`: `ContinueUntilDunningExhausted` plus `true` maps to `AllowedDuringDunning`; every other past-due shape maps to `Suspended`. Do not expose the stored policy alone as if it were the current access result.

In `crates/syrup-rail/src/policy.rs`, define:

    pub fn next_billing_period(
        start_at: DateTime<Utc>,
        rule: SubscriptionPeriodRule,
    ) -> Result<BillingPeriod, BillingPeriodPolicyError>;

In `crates/syrup-rail/src/renewal.rs`, define this closed pure result:

    pub enum RenewalFailureDisposition {
        RetryScheduled { retry_at: DateTime<Utc> },
        RemainPastDue { exhausted_at: DateTime<Utc> },
        MarkUnpaid { ended_at: DateTime<Utc> },
    }

    pub fn renewal_failure_disposition(
        policy: &RenewalFailurePolicy,
        automatic_failure_count: u16,
        failed_at: DateTime<Utc>,
    ) -> Result<RenewalFailureDisposition, RenewalFailurePolicyError>;

The failure count is one-based and includes the current failure. Zero is invalid input. `RetryScheduled` uses `schedule.retry_delays[automatic_failure_count - 1]`; absence of that element means exhaustion.

Retain operational retry constants under names that cannot be mistaken for customer dunning:

    pub const RENEWAL_INFRASTRUCTURE_RETRY_AFTER_SECONDS: i64 = 24 * 60 * 60;
    pub const RENEWAL_PROVIDER_RATE_LIMIT_SLOW_RETRY_AFTER_SECONDS: i64 = 24 * 60 * 60;

The existing provider fast delay, fast-attempt threshold, and infrastructure-attempt cap remain. `RenewalAttemptState` carries `last_automatic_infrastructure_failure_at` and applies the first constant only to the exact infrastructure-pacing resolution-code subset; `provider_rate_limit_retry_after_seconds` returns the separately named slow constant after its existing threshold.

In `crates/syrup-rail/src/event.rs`, use a closed event disposition rather than nullable fields:

    pub enum SubscriptionPaymentFailureDisposition {
        RetryScheduled { retry_at: DateTime<Utc> },
        DunningExhausted { exhausted_at: DateTime<Utc> },
        SubscriptionEnded { ended_at: DateTime<Utc> },
    }

    pub enum SubscriptionEndReason {
        NonPayment,
    }

    BillingEvent::SubscriptionPaymentFailed {
        attempt_id: PaymentAttemptId,
        subscription_id: SubscriptionId,
        plan_key: PlanKey,
        disposition: SubscriptionPaymentFailureDisposition,
    }

    BillingEvent::SubscriptionEnded {
        attempt_id: PaymentAttemptId,
        subscription_id: SubscriptionId,
        plan_key: PlanKey,
        reason: SubscriptionEndReason,
        ended_at: DateTime<Utc>,
        access_ends_at: DateTime<Utc>,
    }

For `SubscriptionEnded`, `ended_at` is always the final failure's durable `resolved_at`; `access_ends_at` equals it under `ContinueUntilDunningExhausted` and equals the first qualifying automatic failure's durable `resolved_at` under `SuspendImmediately`. For a past-due `SubscriptionCanceled`, report cancellation time only if continuing access was still scheduled; otherwise report the first failure for immediate suspension or the last failure for exhausted `RemainPastDue`. An active cancellation continues to report the paid current-period end.

In `crates/syrup-rail-postgres/src/discounts.rs`, extend the existing trait without breaking catalog-level callers:

    async fn lock_enrollment_offer(
        &self,
        connection: &mut PgConnection,
        context: SubscriptionEnrollmentOfferContext<'_>,
    ) -> Result<Option<SubscriptionOffer>, sqlx::Error>;

Document that overrides must use only `connection` and must lock any subscriber-specific eligibility row before returning.

In new `crates/syrup-rail-postgres/src/renewal_failure.rs`, provide this internal application boundary:

    pub(crate) struct AutomaticRenewalFailureHistory {
        count: u16,
        first_attempt_id: Option<PaymentAttemptId>,
        first_resolved_at: Option<DateTime<Utc>>,
        previous_resolved_at: Option<DateTime<Utc>>,
        latest_attempt_id: Option<PaymentAttemptId>,
        latest_resolved_at: Option<DateTime<Utc>>,
    }

    pub(crate) async fn automatic_renewal_failure_history(
        connection: &mut PgConnection,
        subscription_id: SubscriptionId,
        period_start_at: DateTime<Utc>,
    ) -> Result<AutomaticRenewalFailureHistory, RenewalFailureStoreError>;

    pub(crate) enum RenewalFailureApplication {
        Applied {
            disposition: RenewalFailureDisposition,
            events: Vec<BillingEvent>,
        },
        Noop,
    }

    pub(crate) async fn apply_resolved_automatic_renewal_failure(
        connection: &mut PgConnection,
        attempt: &PaymentAttempt,
    ) -> Result<RenewalFailureApplication, RenewalFailureStoreError>;

The history type may use private accessors rather than public fields, but it must preserve exactly these facts and validate the all-none shape for zero history. It assumes the caller holds the subscription aggregate lock. The application function identifies an attempt expected to have just transitioned to a submitted `declined` or `failed` row with no operational resolution code; it must reload and revalidate the attempt kind, submitted timestamp, subscription owner, plan, period anchor, latest-history position, and preceding due-state shape before mutation. Return `Noop` on replay or stale older history rather than fabricating events.

Schema v2 adds these subscription columns, using these names so host migrations and consumers share one vocabulary:

    phase text NOT NULL
    recurring_period_kind text NOT NULL
    recurring_period_count integer NOT NULL
    trial_amount_cents integer
    trial_period_kind text
    trial_period_count integer
    dunning_retry_delays_seconds bigint[] NOT NULL
    dunning_exhaustion text NOT NULL
    past_due_access text NOT NULL
    next_payment_attempt_at timestamptz
    unpaid_at timestamptz

The current `amount_cents` remains the effective amount for the next recurring charge. The current `currency` applies to both trial and recurring terms. Constrain each period count to 1 through 65,535 and each delay element to 1 through 4,294,967,295. Require retry arrays to contain no null elements and to be either canonical empty arrays or one-dimensional, lower-bound-one arrays with cardinality at most 16 so SQL values round-trip into `Vec<DunningRetryDelay>` and the public nonzero integer types. Use canonical phase values `paid_trial` and `recurring`, start-kind values `paid_trial` and `recurring_immediately`, period values `fixed_days` and `calendar_months`, exhaustion values `remain_past_due` and `mark_unpaid`, and access values `suspend_immediately` and `continue_until_dunning_exhausted`.

Schema v2 adds the corresponding immutable initial-attempt columns:

    subscription_initial_terms_version smallint
    subscription_initial_start_kind text
    subscription_initial_trial_amount_cents integer
    subscription_initial_trial_period_kind text
    subscription_initial_trial_period_count integer
    subscription_initial_recurring_base_amount_cents integer
    subscription_initial_recurring_period_kind text
    subscription_initial_recurring_period_count integer
    subscription_initial_dunning_retry_delays_seconds bigint[]
    subscription_initial_dunning_exhaustion text
    subscription_initial_past_due_access text

New initial attempts use terms version 2. Upgrade sets existing initial attempts to version 1 and the legacy terms described above. Do not persist payment tokens, Array identifiers, eligibility decisions beyond the selected terms, provider credentials, or new host identity fields in canonical tables.

At completion, update this ExecPlan everywhere implementation discoveries changed the design, add final evidence under `Artifacts and Notes`, and replace the provisional retrospective. Append a dated revision note below for every material plan revision.

## Revision Notes

- 2026-08-09 / Codex: Created the initial self-contained plan after repository and lifecycle investigation. Chose optional paid-trial terms, relative snapshotted dunning, configurable past-due access, terminal unpaid, atomic generic events, and immutable schema-v2 artifacts so a separate agent can implement without the prior conversation.
- 2026-08-09 / Codex: Revalidated the plan against current core, PostgreSQL, schema-contract, SQLx-gate, service, cancellation, discount, release, and Jig paths. Added the zero-applied discount model, recurring-price authority, pre-provider retry gate, v1/v2 fixture and SQLx-catalog separation, ordinal catalog parity, policy-accurate access timestamps, past-due cancellation replay, roll-forward maintenance-cutover choreography, canonical bounded SQL arrays, deterministic database-time tests, release-version deferral, and executable migration-check commands.
- 2026-08-09 / Codex: Revalidated the revised plan against renewal readiness, operational retry pacing, attempt-history classification, cancellation selection, and payment-method cleanup. Preserved the existing non-cooldown infrastructure pace under explicit constants/state, centralized runtime customer-failure history with stale/replay projection guards, added migration/runtime classification parity and anomalous-v1 rollback coverage, prevented older canceled rows from shadowing newer unpaid history, and made unpaid terminal payment-method references non-authoritative for collection.
- 2026-08-09 / Codex: Revalidated the plan's public discount semantics and milestone ordering against the current enrollment, cancellation, renewal/recovery, entitlement, fixture, compiler, and schema boundaries. Preserved literal `LimitedMonths` behavior by rejecting incompatible recurring cadences, added a tested read-only v1 anomaly preflight, made schema validation precede the breaking Rust cutover, added a compile-wide v2 compatibility checkpoint ahead of broad enrollment tests, required ordinary fixtures to cross the v2 shape barrier, and specified the computed `PastDueAccess` API shared by entitlement presentation and protected-write admission.
- 2026-08-09 / Codex: Recorded completed implementation evidence, added focused cross-module paid-trial/dunning acceptance tests, corrected the Jig `rust-file-loc` invocation to use the v0.1.1 comparison scope, and documented the guide-gate and SQLx no-diff discoveries. The design and release boundary did not change.
- 2026-08-09 / Codex: Closed the implementation after all required gates and final audits passed. Split the new acceptance suite into bounded child modules after discovering that changed-against LOC policy does not inspect untracked files, and recorded the exact schema artifacts, final receipts, deferred release/host work, and completed outcome.
- 2026-08-09 / Codex: Expanded the focused acceptance suite to cover both access policies, infrastructure pacing, newer-unpaid consumer selection, deletion/grant behavior, and indefinite-discount zero progression. Refreshed the final evidence to eight focused scenarios, 96 PostgreSQL tests, and the latest successful repository-gate receipts.
- 2026-08-09 / Codex: Applied the comprehensive-review correction without redesigning the lifecycle: separated fresh `past_due` recovery eligibility from exact durable-attempt completion authority, documented the invariant, and proved all four unresolved v1 active-recovery states across the v2 cutover while fresh active recovery stays closed. Replaced the uncompiled README snippet with a Cargo example, removed the unused dunning aggregate, added non-default paid-trial operator-review coverage, and refreshed all gate evidence.
- 2026-08-09 / Codex: Resolved the follow-up comprehensive-review findings at their owning boundaries. Characterized and rejected the hypothesized dunning-history mutation after proving terminal attempts remain unchanged; separated discount administrative records from current-offer quotes; documented exact terminal cancellation outcomes; and split enrollment application workflow/tests below the enforced LOC limits without exceptions.
- 2026-08-09 / Codex: Addressed the deeper host-boundary pattern rather than patching individual symptoms. Replaced the scalar enrollment-offer callback with a stable typed reservation/stage context, made activation projection the sole temporal pricing API, aligned versioned-schema and split-module ownership guidance, and added the missing host-hook, cadence, and concurrency acceptance coverage.
- 2026-08-10 / Codex: Addressed the final comprehensive-review pattern at the durable and structural boundaries. Distinguished legacy v1 status provenance from v2 dunning consumption, preserved recovery suspension evidence across cutover/cancellation, made host access classification explicit and compiled, corrected the actual 0.1 API migration path, completed responsibility-based module/test extraction for every LOC violation, and refreshed all required verification evidence.
- 2026-08-10 / Codex: Closed the final documentation review findings by specifying the `DunningExhausted` host-access contract for `RemainPastDue`, inventorying the retry-reclassification audit as the fourth immutable schema-v2 artifact, moving its regression into a focused child module for the staged LOC contract, and recording the clean pre-commit gates.
