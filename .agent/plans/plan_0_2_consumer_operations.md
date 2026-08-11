# Complete the version 0.2 consumer and operations surface

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. Maintain it in accordance with `.agent/PLANS.md` from the repository root.

## Purpose / Big Picture

After this work, a host can use the high-level PostgreSQL billing service for every subscriber-owned billing mutation instead of rebuilding cancellation and discount admission around low-level functions. A host can also render a billing portal from typed, redacted Rust read models, drain every renewal that was due in one stable scan even when more than one hundred subscriptions are ready, verify schema-v2 compatibility at process startup without running a migrator, and classify service failures without exhaustively coupling itself to every internal variant.

The behavior is observable through public API tests and PostgreSQL integration scenarios. Cancellation must append its typed event in the same host-prepared transaction as the subscription mutation. Billing reads must not expose payment tokens, provider payment-method references, transaction identifiers, billing contacts, or raw gateway diagnostics. Renewal pagination must return more than one hundred unchanged due subscriptions exactly once across one scan while every later `renew` call retains its existing current-state revalidation. The runtime schema check must accept both a fresh v2 catalog and a valid v1-to-v2 upgraded catalog, reject v1, permit host-prefixed extensions, and never execute DDL. Error classifications must be closed values with documented conservative retry semantics.

## Progress

- [x] (2026-08-11) Read `AGENTS.md`, `agent-map.md`, both affected crate guides, `.agent/PLANS.md`, and `docs/security/threat-model.md`; inspected the current service, cancellation, discount, entitlement, renewal, schema-conformance, and error APIs.
- [x] (2026-08-11) Commit the already-verified consumer API cutover that was present in the worktree before this plan (`25f4823fc90101a2adcc4f118cb61f83c8d1c44b`).
- [x] (2026-08-11) Add and commit the high-level cancellation and discount subscriber facade, including atomic host-event cancellation, typed exact clear commands, admission coverage, and gateway-free discount paths.
- [x] (2026-08-11) Add and commit typed billing-portal and payment-history reads, preserving the canonical entitlement projection inside one read-only repeatable-read snapshot and keeping customer-facing payment facts redacted.
- [x] (2026-08-11) Add and commit stable renewal-dispatch pagination.
- [x] (2026-08-11) Add and commit the production runtime schema-v2 compatibility check.
- [x] (2026-08-11) Add and commit structured service-error dispositions and non-exhaustive operational-error hardening.
- [ ] Run all repository gates, audit every requirement in this plan against current evidence, and record the final outcome.

## Surprises & Discoveries

- Observation: `EndUserMutationOperation` already declares `SubscriptionCancel` and `SubscriptionDiscountClear`, but no production path consumes either value.
  Evidence: `crates/syrup-rail/src/admission.rs` declares the variants while searches under `crates/` find no use outside that declaration.

- Observation: cancellation has the correct exact-plan locking and event semantics, but is exposed only as `cancel_subscription_in_transaction`; hosts must currently reproduce service admission, host-recipient locking, event append, and commit behavior.
  Evidence: `crates/syrup-rail-postgres/src/cancellation.rs` and `crates/syrup-rail-postgres/src/subscription_billing_service.rs`.

- Observation: `due_renewals` has a deliberate fixed limit of one hundred and no continuation cursor. Repeated scans therefore revisit the leading mutable set until workers reserve it, and a caller cannot deliberately drain one observed due set.
  Evidence: the `ORDER BY ... LIMIT 100` query in `crates/syrup-rail-postgres/src/renewal.rs`.

- Observation: exact schema-v2 conformance already exists and is read-only, but the complete `schema_contract` module is compiled publicly only for tests or the `schema-contract-test-support` feature. Production hosts therefore lack an ordinary startup API unless they opt into a test-named feature that also embeds migration artifacts.
  Evidence: `crates/syrup-rail-postgres/src/lib.rs`, `crates/syrup-rail-postgres/src/schema_contract.rs`, and `crates/syrup-rail-postgres/Cargo.toml`.

- Observation: schema v2 supplies host-readable current-subscription, payment-fact, and active-discount views, while the Rust API exposes a typed entitlement only. There is no typed, redacted customer payment-method snapshot or cursor-paginated customer payment history.
  Evidence: `crates/syrup-rail-postgres/schema/v2/install.sql` and `crates/syrup-rail-postgres/src/entitlement.rs`.

- Observation: `billing_payment_facts` is safe for ordinary host reporting but omits billing-period and submitted-at facts needed by a customer payment history.
  Evidence: `crates/syrup-rail-postgres/schema/v2/install.sql` defines the view without those columns, while `billing_payment_attempts` carries them beside protected fields.

- Observation: the coordinator deliberately exposes a `PgConnection` from its host-prepared transaction, while the preexisting public cancellation primitive accepted only `Transaction<Postgres>`.
  Evidence: `crates/syrup-rail-postgres/src/transactions.rs` and the former signature of `cancel_subscription_in_transaction` in `crates/syrup-rail-postgres/src/cancellation.rs`.
  Resolution: retain the public transaction-local primitive as a wrapper and use a crate-visible connection-local implementation from the high-level facade, so no second transaction can break cancellation/event atomicity.

- Observation: a valid schema-v2 catalog cannot contain a gateway account without
  its provider cooldown row because `billing_gateway_accounts.provider_key` has
  a foreign key to `billing_gateway_provider_rate_limits`.
  Evidence: `crates/syrup-rail-postgres/schema/v2/install.sql` and the renewal
  pagination fixture attempt, which PostgreSQL rejected with the named foreign
  key.
  Resolution: retain the defensive `MissingProviderCooldown` query/error for
  catalog drift, but test all realizable candidate gates through valid
  schema-backed rows rather than disabling a canonical constraint.

- Observation: one parallel full PostgreSQL library-suite invocation exhausted
  container startup capacity for seven unrelated tests, while the other 125
  tests (including every renewal pagination test) passed.
  Evidence: the captured `cargo test -p syrup-rail-postgres --lib --locked`
  exit was 101 with only `ContainerStart { WaitContainer(StartupTimeout) }`
  failures; a sequential status-wrapped retry of the exact seven tests passed
  all seven in roughly three seconds each.
  Resolution: record the parallel harness-capacity caveat with the validation
  evidence rather than treating it as a product failure or changing test
  behavior.

- Observation: the existing catalog checks issued every query through the pool,
  so concurrent DDL could theoretically make one assertion observe more than
  one catalog state even though every individual query was read-only.
  Evidence: the former `assert_schema_conforms` helper accepted `&PgPool` and
  each catalog query called `fetch_*` on that pool.
  Resolution: the production assertion and feature-gated v1/v2 test wrappers
  now begin one PostgreSQL `REPEATABLE READ READ ONLY` transaction and delegate
  every existing canonical check and fingerprint query to its connection.

## Decision Log

- Decision: Keep this work inside the existing version-2 schema and public 0.2 API cutover. Do not add free trials, arbitrary pricing phases, proration, plan changes, provider-managed recurring plans, email, or queue transports.
  Rationale: Those capabilities change economic authority and durable attempt/event semantics. The requested additions close consumer and operations gaps in the already implemented paid-trial and dunning model.
  Date/Author: 2026-08-11 / Codex.

- Decision: Add stable renewal scan pagination rather than a canonical dispatch lease.
  Rationale: Queue transport and encoding remain host-owned. A canonical lease without an atomic host-outbox write creates a lost-dispatch interval; a new arbitrary SQL callback would violate the transaction-boundary guide. A database-observed scan timestamp plus a strict `(next_payment_attempt_at, subscription_id)` continuation key lets a host drain the observed due set, while the existing reservation and service preflights remain the authority against stale or duplicate work.
  Date/Author: 2026-08-11 / Codex.

- Decision: Bind the first page's observed PostgreSQL timestamp into every
  time-dependent renewal eligibility gate, not only the due predicate.
  Rationale: Re-evaluating provider/account cooldowns, stale unsubmitted
  update age, infrastructure pacing, or provider-rate retry windows against a
  later continuation clock would add candidates partway through a scan. The
  cursor freezes the eligibility-time cut while each continuation still reads
  current mutable rows and retains their structural gates. It is not a
  cross-page MVCC snapshot: concurrent candidates inserted, retimed, or
  unblocked behind the key wait for a new scan.
  Date/Author: 2026-08-11 / Codex.

- Decision: Treat `RenewalDispatchPageCursor` as a trusted host persistence
  value, even though its public constructor permits reconstructing a prior
  cursor after process restart.
  Rationale: The cursor contains only bound time/identity values and cannot
  inject SQL, while host code already owns renewal dispatch authorization and
  queue/outbox state. Accepting it from an end user would let that caller steer
  host work selection, so hosts must reconstruct it only from a prior trusted
  page rather than expose it as a user-controlled token.
  Date/Author: 2026-08-11 / Codex.

- Decision: Customer read models expose only provider-neutral lifecycle, money, cadence, masked card display, and timestamps. They do not expose provider payment-method references, transaction IDs, response text, billing contacts, or raw diagnostics.
  Rationale: Those values are unnecessary for an ordinary billing portal and are protected data under `docs/security/threat-model.md`.
  Date/Author: 2026-08-11 / Codex.

- Decision: The masked payment-method display is card-only: brand, last four, and expiration fields. It deliberately excludes `payment_type`, which is arbitrary provider text rather than a normalized cross-provider customer contract.
  Rationale: A card-only display provides the ordinary portal need without widening the public surface to a provider-originated free-form value. The selected card fields are validated and ordinary formatting reports only their presence.
  Date/Author: 2026-08-11 / Codex.

- Decision: Classify service errors conservatively. Only explicitly temporary conditions are retryable; storage/application and invalid-state failures remain internal rather than encouraging an unbounded retry loop. An indeterminate provider mutation is already represented by a durable payment result and reconciliation, not by a retryable service error.
  Rationale: The gateway contract guarantees at-most-once mutation and forbids blind retry of indeterminate outcomes.
  Date/Author: 2026-08-11 / Codex.

- Decision: Each milestone is committed as an independently green slice by a `gpt-5.6-terra` subagent running at max reasoning. The root agent integrates, updates this living plan, and performs the final full-suite audit.
  Rationale: This is the user's explicit delivery requirement and keeps failures attributable to one public capability.
  Date/Author: 2026-08-11 / Codex.

- Decision: Only cancellation opens the host coordinator transaction. Discount claim and clear use an ordinary local transaction after admission because they do not emit a host billing event.
  Rationale: The coordinator's required host-recipient lock and transactional outbox projection protect event-producing cancellation. Discount operations already own all canonical/offer locks needed for their durable state, and introducing an empty host transaction would add host coupling without an atomic host side effect.
  Date/Author: 2026-08-11 / Codex.

- Decision: Expose only `assert_runtime_schema_v2_compatible` and
  `SchemaConformanceError` from a default production build. Keep the
  install/preflight/audit/upgrade SQL constants and v1/v2 fixture wrappers in
  the public `schema_contract` module only under tests or the existing
  `schema-contract-test-support` feature.
  Rationale: A host needs a fail-closed startup assertion, not a crate-owned
  migration runner. Reusing the exact canonical machinery prevents version
  drift between runtime and test checks, while a repeatable-read, read-only
  snapshot gives one coherent catalog view and PostgreSQL-enforced no-write
  semantics.
  Date/Author: 2026-08-11 / Codex.

- Decision: Classify configuration/authority snapshots as conflicts, explicit
  temporary admission/provider conditions as safely resubmittable with the
  same idempotency command, and malformed gateway request/response paths as
  internal contract faults. Return an exact delay only for admission denial.
  Rationale: A changed configuration or billing snapshot needs reload/rebuild
  or idempotency reconciliation rather than an unchanged retry. Gateway or
  account cooldowns are temporary but do not carry a trustworthy exact delay,
  while malformed provider boundaries do not establish that another submission
  is safe. The service maps every outer and relevant nested current variant
  explicitly so new cases require a policy choice.
  Date/Author: 2026-08-11 / Codex.

## Outcomes & Retrospective

Milestone 1 is complete: the public service now admits and executes exact cancellation, discount claim, and discount clear commands. Cancellation keeps canonical mutation, typed event append, and commit on the coordinator's single host-prepared transaction; semantic replays/blockers emit no event, while mutation or append errors roll back. Discount mutations preserve existing typed outcomes without gateway resolution or provider I/O. Core identity/admission tests and PostgreSQL integration scenarios cover allowed and denied admission, atomic append/mutation rollback, replay, blockers, and discount paths. The final retrospective will add the complete cross-milestone commit list and repository-wide validation evidence.

Milestone 2 is complete: public core types now model an authorized exact billing portal query, canonical entitlement snapshot, optional masked-card display, checked payment-history page size, strict cursor, and safe payment-history facts. PostgreSQL retains the existing entitlement SQL as the authority and evaluates it with the card projection in one `REPEATABLE READ READ ONLY` snapshot. The history reader uses a narrow, explicit select list and strict descending `(created_at, id)` pagination with one extra row; it never constructs `PaymentAttempt` or selects protected provider/contact/diagnostic columns. Core and PostgreSQL tests cover value-free formatting, bounds, empty/active/trial/dunning/canceled/grant/scrubbed snapshots, saved/applied discounts, exact identity isolation, timestamp ties, zero-value method updates, host-charge exclusion, and redaction.

Milestone 3 is complete: `RenewalDispatchPageCursor` carries a
PostgreSQL-observed scan time and the last strict ascending scheduling key, and
`RenewalDispatchPage` exposes a bounded dispatch slice plus an optional next
cursor. `due_renewals_page` fetches one hundred and one eligible rows, returns
at most one hundred, and freezes every time-dependent due, cooldown,
stale-update, infrastructure, and provider-rate gate at the first page's
timestamp. It rechecks current structural eligibility on every continuation;
it does not claim, lease, or enqueue work. `due_renewals` now delegates to the
new API's first page and preserves its fixed-order, fixed-limit compatibility
contract. Core and PostgreSQL tests cover 205 tied rows over three pages,
strict UUID tie-breaking with no gaps/no duplicates for unchanged candidates,
later-due exclusion, current gate rechecks, every realizable legacy gate,
frozen clock windows, and the legacy wrapper. The cursor freezes time
eligibility rather than a cross-page MVCC snapshot, so concurrent mutable
candidates behind the key become work for a fresh scan.

Milestone 4 is complete: ordinary production builds now expose
`assert_runtime_schema_v2_compatible(&PgPool)` and `SchemaConformanceError`.
The assertion runs the existing full v2 relation, view, function, trigger,
column, constraint, index, legacy-vocabulary, and catalog-fingerprint checks
inside one `REPEATABLE READ READ ONLY` PostgreSQL transaction. It performs no
DDL or migration/preflight/audit work, accepts valid host-prefixed extensions,
and reports unchanged v1 as a version-2 contract failure. The checked-in
install and upgrade SQL constants remain available only to crate tests or the
explicit `schema-contract-test-support` feature, so default binaries do not
embed or expose a migrator. Fresh-v2, checked-in v1-upgrade, host-extension,
unchanged-v1, and canonical-drift coverage exercise the public runtime API;
the default-feature host example compiles a recommended startup helper.

Milestone 5 is complete: `SubscriptionBillingServiceError` and its public,
copyable `SubscriptionBillingServiceErrorDisposition` are non-exhaustive.
Hosts can use `disposition`, `is_retryable`, `is_conflict`, and `retry_after`
without exposing nested diagnostics or matching the full error. Retryability
means only that resubmitting the same idempotent command is safe; it does not
promise success. Admission denial retains its exact delay, while account and
gateway cooldowns deliberately report no fabricated delay. Exhaustive service,
cancellation, discount, resolution, readiness, and definitely-not-submitted
mapping coverage makes every current category an intentional policy decision;
the host integration helper uses a wildcard for future dispositions.

## Context and Orientation

The workspace contains four publishable Rust crates. `crates/syrup-rail` owns provider-neutral validated values, commands, outcomes, and policies. `crates/syrup-rail-postgres` owns canonical PostgreSQL queries and orchestration on host-supplied transactions. Hosts own authentication and authorization, plan catalog rows, provider credentials, queue transport, outbox encoding, and presentation.

`SubscriptionBillingService` in `crates/syrup-rail-postgres/src/subscription_billing_service.rs` is the high-level facade. Workflow-specific methods live under `crates/syrup-rail-postgres/src/subscription_billing_service/`. Its `EndUserMutationAdmission` dependency is an abuse-control boundary invoked only after a host has authenticated and authorized the subject. Its `BillingTransactionCoordinator` opens a host-prepared PostgreSQL transaction, locks the live or retained event recipient before shared billing rows, and appends typed `BillingEvent` values to the host outbox on the same connection.

Cancellation policy lives in `crates/syrup-rail-postgres/src/cancellation.rs`. `cancel_subscription_in_transaction` locks the exact subscription aggregate, respects in-flight renewal and payment-method-update fences, produces `CancelSubscriptionOutcome`, and returns a `BillingEvent::SubscriptionCanceled` only when state actually changes. Discount operations live in `crates/syrup-rail-postgres/src/discounts.rs`; the service already owns the `SubscriptionOfferStore` needed to lock current pricing for a claim.

Entitlement policy lives in `crates/syrup-rail/src/subscription/access.rs`, and its PostgreSQL projection lives in `crates/syrup-rail-postgres/src/entitlement.rs`. A billing-portal snapshot is a read-only customer-facing projection; it is not authorization. The host must authorize scope, subscriber, and plan before querying it. A payment-history cursor is a validated continuation fact from a previous page, not arbitrary SQL supplied by a caller.

`due_renewals` in `crates/syrup-rail-postgres/src/renewal.rs` selects active or past-due subscriptions whose `next_payment_attempt_at` is due, whose gateway cooldowns and operational pacing permit work, and which have no blocking attempt. A renewal scan timestamp is the PostgreSQL time captured for the first page. Every continuation page binds that same time to the due, provider/account cooldown, stale-update, infrastructure, and provider-rate windows, then selects rows strictly after its prior `(next_payment_attempt_at, subscription_id)` key. This avoids offset/timestamp-tie gaps and repeats for unchanged candidates, but freezes time eligibility without promising a cross-page MVCC snapshot: inserted, retimed, or newly unblocked rows behind the key wait for a fresh scan. A cursor is trusted host state reconstructed only from a prior page, not an end-user token, and this does not weaken the later `SubscriptionBillingService::renew` preflight.

Full catalog conformance is implemented in `crates/syrup-rail-postgres/src/schema_contract.rs`. It compares canonical relations, views, functions, triggers, constraints, indexes, and a catalog fingerprint while permitting explicitly host-prefixed objects. The runtime addition must reuse that exact read-only logic without installing or exposing a production migrator by default.

## Plan of Work

### Milestone 1: Complete subscriber-owned service mutations

Add a core `ClearSubscriptionDiscount` command containing exact billing scope, subscriber, and plan identity. Add `SubscriptionDiscountClaim` to `EndUserMutationOperation` and use the existing cancel and clear variants rather than leaving them declarative only. In a new owning service module, add `SubscriptionBillingService::cancel`, `claim_discount`, and `clear_discount`.

`cancel` first invokes subscriber mutation admission. It then begins a host-prepared transaction for the exact `BillingEventSubject`, calls the cancellation implementation on that same connection after the host subject lock, appends the returned event only for a new `Canceled` outcome, and commits. Semantic outcomes such as `AlreadyCanceled`, `NotFound`, and in-flight blockers commit without an event. Any mutation or append failure rolls back by consuming the host transaction. No provider I/O occurs.

Discount claim and clear invoke their exact admission operation before database work. Claim reuses the service's `SubscriptionOfferStore`; clear uses the exact command identity. Add explicit service error variants for cancellation and discount failures rather than erasing their sources. Test allowed and denied admission, exact operation values, cancellation event atomicity, replay without another event, append failure rollback, and discount claim/clear admission without provider I/O.

### Milestone 2: Add typed customer billing reads

Add provider-neutral core query/result types in an owning module under `crates/syrup-rail/src/`. Define `SubscriptionBillingPortalQuery`, `SubscriptionPaymentMethodDisplay`, `SubscriptionBillingPortalSnapshot`, `SubscriptionPaymentHistoryCursor`, a checked `SubscriptionPaymentHistoryPageLimit` bounded from one through one hundred, `SubscriptionPaymentHistoryItem`, and `SubscriptionPaymentHistoryPage`. Keep fields private and provide exact accessors. Ordinary `Debug` output for payment-method display must reveal only field presence, never card values.

In a new `crates/syrup-rail-postgres/src/billing_portal.rs`, add `subscription_billing_portal` and `subscription_payment_history_page`. The snapshot must be read from one PostgreSQL snapshot and combine the same entitlement semantics as `entitlement` with only the active subscription's stored masked payment-method display. Missing or grant-only entitlements have no method. Payment history is exact scope/subscriber/plan subscription attempt history ordered by `(created_at DESC, id DESC)`, uses a strict cursor predicate, requests one extra row to determine continuation, and returns at most the checked limit. It excludes host charges and never selects provider references, transaction IDs, contact fields, response text, or diagnostics. Test empty, active, paid-trial, dunning, canceled-paid-through, grant, scrubbed display, multiple pages with timestamp ties, wrong scope/plan, and redacted formatting.

### Milestone 3: Make renewal scans drainable

Add `RenewalDispatchPageCursor` and `RenewalDispatchPage` to the core renewal API. The cursor carries the database-observed scan timestamp and the last returned due-order key, with private fields and accessors. Add `due_renewals_page(pool, cursor)` in `crates/syrup-rail-postgres/src/renewal.rs`. The first page captures one PostgreSQL timestamp; continuation pages retain it. The query applies `next_payment_attempt_at <= observed_at`, then a strict continuation predicate over `(next_payment_attempt_at, subscription_id)`, and retains every existing status, attempt, payment-method, infrastructure, account, and provider gate. Fetch one hundred and one rows, return at most one hundred, and emit a next cursor only when another row exists. Preserve `due_renewals` as a first-page compatibility wrapper.

Tests must seed at least 205 eligible subscriptions and prove three pages return all 205 exactly once in unchanged state. Also prove a row becoming due after the first page's observed timestamp is excluded from that scan, an old cursor cannot bypass current eligibility gates, and the legacy wrapper still returns at most one hundred in the same order.

### Milestone 4: Expose runtime schema compatibility without a migrator

Refactor schema conformance so `assert_runtime_schema_v2_compatible(&PgPool)` and its error type are available in ordinary production builds. Keep the exact install, preflight, audit, and upgrade SQL byte constants behind tests or `schema-contract-test-support` so ordinary binaries do not embed or expose a runtime migrator. Reuse the existing v2 catalog requirements and fingerprint; do not replace them with a shallow column probe.

Tests must prove the runtime check accepts a fresh v2 database, accepts a v1 database after the checked-in upgrade, permits host-prefixed extensions, rejects an unchanged v1 catalog with a version-2 diagnostic, and rejects canonical drift. Add a compile-tested example or public API test that calls it without enabling the test-support feature.

### Milestone 5: Add structured service-error dispositions

Mark `SubscriptionBillingServiceError` non-exhaustive and add a non-exhaustive `SubscriptionBillingServiceErrorDisposition` with conservative categories for conflict, rejected request/state, temporarily unavailable, misconfigured capability, and internal failure. Provide `disposition()`, `is_retryable()`, `is_conflict()`, and `retry_after()` helpers. `retry_after()` returns a value only where the service has an exact duration, currently admission denial. Cooldown without an exact duration remains temporarily unavailable with no fabricated delay.

Map every service variant explicitly, including cancellation and discount errors added in Milestone 1 and the inner variants of gateway resolution, readiness, and definitely-not-submitted errors. Storage, application, and invalid durable state are internal; idempotency conflict is conflict; semantic reservation/submission blockers are rejected; unavailable/rate-limited/timeouts are temporary; missing or invalid gateway/host capability is misconfigured. Add exhaustive internal mapping tests, consumer-style helper tests, and rustdoc describing that retryability means it is safe to resubmit the same idempotent command, not that success is guaranteed.

Update `README.md`, `CHANGELOG.md`, crate guides, and compile-tested examples as each public capability lands. Do not describe host authentication, queueing, migrations, or presentation as crate-owned.

## Concrete Steps

Work from `/home/aa/Documents/syrup-rail`. Before each slice, require a clean worktree except for this living plan, inspect the preceding commit, and run focused tests. Commit only after the slice is green. Useful focused commands are:

    cargo fmt --all -- --check
    cargo test -p syrup-rail --locked
    cargo test -p syrup-rail-postgres --lib --locked
    cargo check -p syrup-rail-postgres --all-targets --locked
    cargo clippy -p syrup-rail -p syrup-rail-postgres --all-targets --locked -- -D warnings

Schema-conformance or query-shape changes also require:

    scripts/jig check sqlx --no-receipt
    scripts/jig check contract --no-receipt

At the end, run the complete repository contract:

    cargo fmt --all -- --check
    cargo check --workspace --all-targets --locked
    cargo clippy --workspace --all-targets --locked -- -D warnings
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked
    scripts/jig check contract --no-receipt
    scripts/jig check sqlx --no-receipt
    scripts/jig check test-locked --no-receipt
    scripts/jig check test --no-receipt

Expected success is zero exit status from every command, no SQLx metadata drift unless a reviewed query macro change genuinely requires refreshed metadata, no mutation to schema v1, and a clean `git diff --check`.

## Validation and Acceptance

The subscriber facade is accepted when a service cancellation records the subscription change and exactly one `SubscriptionCanceled` host event in the same transaction, replay records no second event, append failure leaves both host event and cancellation state uncommitted, and denied admission performs no database or provider work. Discount claim and clear must expose their existing semantic outcomes while invoking the distinct admission operations.

The billing read model is accepted when a consumer can render current terms/access, a masked payment method, saved or applied discount state, and more than one page of payment history using only public typed APIs. Tests and formatting inspection must prove protected provider/contact values never enter these types or their `Debug` output.

Renewal pagination is accepted when one observed scan returns 205 unchanged due subscriptions in deterministic order over three pages with no duplicates, while a newly due row waits for the next scan and stale jobs are still rejected or replayed by existing service authority.

Runtime schema compatibility is accepted when an ordinary dependency build can call the read-only function against v2, v1 fails closed, and no install or upgrade SQL is reachable without the explicit test-support feature.

Error hardening is accepted when downstream code can branch on stable disposition helpers without matching every error variant and every current variant has a deliberate, tested classification.

Completion additionally requires each milestone to have its own commit and the final full repository gate set to pass from the combined history.

## Idempotence and Recovery

All implementation and validation steps are safe to rerun. PostgreSQL integration tests use ephemeral databases. Do not edit `crates/syrup-rail-postgres/schema/v1/**`. This plan deliberately avoids schema-v2 DDL changes; if implementation unexpectedly requires durable state, stop that slice, record the discovery, and redesign both fresh-install and forward-upgrade artifacts before editing either.

Agents share one worktree and Git index. Stage explicit paths only, never `git add -A`, and do not commit another agent's files. Run feature agents serially unless their work is isolated in a separate Git worktree. If a commit fails validation, fix forward in the same slice before committing rather than resetting unrelated work. `.agent/state/*.jsonl` is append-only; use `--no-receipt` during intermediate gates and never rewrite or discard an existing receipt.

## Artifacts and Notes

The plan begins while an already-verified consumer API cutover is dirty in the worktree. That prerequisite renames the broad high-level error to `SubscriptionBillingServiceError`, documents the direct 0.1-to-0.2 migration, and adds a compile-tested host integration example. It must be committed separately before Milestone 1 so later error hardening builds on the intentional public name.

## Interfaces and Dependencies

Use existing workspace dependencies only: Rust 2024, Tokio, SQLx 0.8, Chrono, UUID, `async-trait`, and `thiserror`. Do not add a web framework, serialization contract, queue library, or provider dependency.

The final public surface must include equivalent capabilities to:

    impl SubscriptionBillingService {
        pub async fn cancel(
            &self,
            command: CancelSubscription,
        ) -> Result<CancelSubscriptionOutcome, SubscriptionBillingServiceError>;

        pub async fn claim_discount(
            &self,
            command: SubscriptionDiscountClaim,
        ) -> Result<SubscriptionDiscountClaimOutcome, SubscriptionBillingServiceError>;

        pub async fn clear_discount(
            &self,
            command: ClearSubscriptionDiscount,
        ) -> Result<SubscriptionDiscountClearOutcome, SubscriptionBillingServiceError>;
    }

    pub async fn subscription_billing_portal(
        pool: &PgPool,
        query: &SubscriptionBillingPortalQuery,
    ) -> Result<SubscriptionBillingPortalSnapshot, SubscriptionBillingPortalQueryError>;

    pub async fn subscription_payment_history_page(
        pool: &PgPool,
        query: &SubscriptionBillingPortalQuery,
        cursor: Option<&SubscriptionPaymentHistoryCursor>,
        limit: SubscriptionPaymentHistoryPageLimit,
    ) -> Result<SubscriptionPaymentHistoryPage, SubscriptionBillingPortalQueryError>;

    pub async fn due_renewals_page(
        pool: &PgPool,
        cursor: Option<&RenewalDispatchPageCursor>,
    ) -> Result<RenewalDispatchPage, RenewalStoreError>;

    pub async fn assert_runtime_schema_v2_compatible(
        pool: &PgPool,
    ) -> Result<(), SchemaConformanceError>;

Exact type names may be adjusted only when compiler-guided implementation reveals an established repository naming convention that is clearer. Record any adjustment in the Decision Log and preserve every behavior described above.

Revision note (2026-08-11): Initial plan written after auditing the version-0.2 worktree and translating the user's five requested feature/hardening slices into independently testable public contracts.
