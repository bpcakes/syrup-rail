# Make gateway-mode authority durable and release dependency hardening

This ExecPlan is a living document. Maintain it in accordance with `.agent/PLANS.md`, including the `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` sections as work proceeds.

## Purpose / Big Picture

Syrup Rail 0.3.1 introduces an opt-in test gateway-account mode for a downstream deployment that uses one mutable NMI merchant account: staging requires the account's Test Mode to be enabled, and production requires that same account's Test Mode to be disabled. The initial implementation checks mode before provider mutations but does not persist which mode authorized an attempt. A prepared attempt can therefore outlive one service instance and later resume under another service configured for a different mode, and an approved test attempt is indistinguishable from a live attempt in the canonical ledger.

After this work, every attempt will durably snapshot the required gateway account mode, prepared replays will fail closed when the current service requirement differs, and canonical data will distinguish live-required from test-required authority. Existing schema-v3 rows will upgrade as live-required because versions through 0.3.0 rejected test accounts. The provider mode will also be checked at the narrowest practical pre-mutation boundary. NMI exposes account mode and sale as separate requests, so the code will document that it cannot make an external account toggle atomic with a charge.

The release also updates `h2` from 0.4.15 to 0.4.16. A lockfile update alone does not set a minimum for downstream library users, so the workspace dependency and changelog will make that floor explicit.

## Progress

- [x] (2026-08-26 13:34Z) Read repository, crate, schema, security, and ExecPlan guidance and inspect the working-tree review findings.
- [x] (2026-08-26 13:34Z) Research provider semantics, release tags, schema shipment, Cargo version resolution, and current test coverage.
- [x] (2026-08-26 13:40Z) Confirm the downstream deployment uses one mutable NMI account across staging and production, with environment-wide required mode.
- [x] (2026-08-26 13:58Z) Add the durable core identity field and mode parsing without breaking existing constructors that default to live.
- [x] (2026-08-26 14:05Z) Add schema v4 install and v3-to-v4 forward-only upgrade artifacts; preserve shipped v1, v2, and v3 bytes.
- [x] (2026-08-26 14:18Z) Update PostgreSQL attempt insertion, loading, replay, admission, runtime schema assertions, SQLx metadata, and public schema documentation.
- [x] (2026-08-26 14:12Z) Bind every foreground workflow to the service requirement, reject prepared cross-mode resumes, and narrow the final mode-check-to-mutation window.
- [x] (2026-08-26 14:21Z) Add database-backed positive and negative tests for enrollment, renewal, recovery, replacement, and host-charge mode boundaries.
- [x] (2026-08-26 14:06Z) Set the published `h2` minimum to 0.4.16 and document the hardening accurately.
- [x] (2026-08-26 14:31Z) Run Jig format, Clippy, tests, SQLx, contract, evidence, and gates; inspect the final diff. Required gates passed in receipt `receipt_01M0Z5T9VZMB3CKHASYRXMAG60`; format passed in `receipt_01M0Z5WABFAG4SF17D31XX4YN9`; Clippy passed in `receipt_01M0Z5X84M5G1HYQZJWE09HK0G`.

## Surprises & Discoveries

- Observation: Schema v3 is already shipped in tag `v0.3.0`, which points at current `HEAD`; it cannot be edited for this fix.
  Evidence: `git tag --list` includes `v0.3.0`, and `git log --decorate` places that tag at `457c4f4`.
- Observation: The downstream deployment intentionally shares one NMI merchant account across staging and production and toggles its account-wide Test Mode. The account and gateway configuration identifiers therefore do not identify the safety mode over time.
  Evidence: User-supplied downstream deployment contract on 2026-08-26.
- Observation: NMI documents account Test Mode as mutable merchant-account state and warns that it must be disabled before production. It exposes mode status and transaction submission as separate API calls.
  Evidence: NMI's official Testing Methods, Enabling Test Mode, and Classic Query API documentation.
- Observation: A local mode check cannot be atomic with a later NMI mutation. Persisted data must therefore be named as the service's required mode, not claimed as infallible processor-observed evidence.
- Observation: Cargo's `h2 = "0.4"` requirement permits versions from 0.4.0 through the next incompatible release even when this repository's lockfile selects 0.4.16.
  Evidence: `Cargo.toml` retains `h2 = "0.4"`; `Cargo.lock` alone selects 0.4.16.
- Observation: Fresh v4 and upgraded v3 initially produced different catalog fingerprints because PostgreSQL appends upgrade-added columns while the first fresh artifact placed the mode beside gateway identity.
  Evidence: The v4 equivalence test reported fingerprints `0x63f21008d16b5ef1` and `0x00082d855bc8d9b7`; moving the fresh-install column to the upgrade-equivalent ordinal made both paths conform to `0x00082d855bc8d9b7`.

## Decision Log

- Decision: Persist `required_gateway_account_mode` on each payment attempt rather than a mutable database-wide flag.
  Rationale: A per-attempt snapshot is immutable provenance and remains truthful when the same external NMI account is toggled over time. A database-wide flag would lose history after a toggle and would not protect attempts resumed by another service instance.
  Date/Author: 2026-08-26 / Codex
- Decision: Treat required mode as part of `PaymentAttemptIdentity` while preserving `PaymentAttemptIdentity::new` as a live-default compatibility constructor.
  Rationale: Gateway account ID, configuration ID, and required mode jointly identify the authorized provider submission boundary. Shared identity equality makes replay checks fail closed instead of relying on five unrelated workflow comparisons.
  Date/Author: 2026-08-26 / Codex
- Decision: Add schema v4 with a non-null mode column defaulted to `live`; do not edit schema v3.
  Rationale: Version 0.3.0 shipped schema v3. Historical attempts were created while the service accepted live accounts only, so the v3-to-v4 backfill is deterministic.
  Date/Author: 2026-08-26 / Codex
- Decision: Keep mode as trusted service/deployment policy rather than per-command or end-user input.
  Rationale: The downstream contract is environment-wide and NMI warns against production Test Mode. Persisting the requirement solves audit and retry correctness without widening who can choose mode.
  Date/Author: 2026-08-26 / Codex
- Decision: Recheck mode immediately before provider submission while explicitly documenting the irreducible NMI toggle race.
  Rationale: This minimizes the time-of-check/time-of-use window. Because the provider exposes separate requests, only separate merchant accounts can remove the race completely.
  Date/Author: 2026-08-26 / Codex
- Decision: Set `h2 = "0.4.16"` rather than relying on `Cargo.lock`.
  Rationale: This remains compatible with h2 0.4 while ensuring published library consumers cannot resolve an older 0.4 release.
  Date/Author: 2026-08-26 / Codex

## Outcomes & Retrospective

Implemented a durable required-mode snapshot in payment-attempt identity and
PostgreSQL schema v4. All five foreground mutation workflows now bind new
attempts to the trusted service mode, reject prepared cross-mode resumes, and
query account mode after final admission before invoking the provider. Fresh
v4 and v3-to-v4 upgrades share a canonical fingerprint, and historical rows
are deterministically live-required. Existing constructors and lower-level
reservation functions retain live-default source compatibility.

Positive and negative database-backed tests cover live and test requirements,
post-admission mode changes, prepared cross-mode replay, recovery, renewal,
payment-method replacement, host charge, and migration histories. The
workspace now declares `h2 = "0.4.16"`; the lockfile and changelog agree.
Format, Clippy, Cargo metadata, SQLx, contract, schema immutability, core tests,
and repository test gates pass.

One provider-level limitation remains by design: NMI's account-mode query and
payment mutation are separate requests. The application minimizes but cannot
eliminate that race. A host sharing one merchant account must stop billing
traffic in both environments while toggling mode; separate accounts or an
NMI-supported atomic/per-transaction mechanism would be required to remove the
external race.

## Context and Orientation

`crates/syrup-rail/src/attempt.rs` owns `PaymentAttemptIdentity`, `PaymentAttempt`, request, and state. A payment attempt is the durable idempotency and financial-authority record shared by subscription and host-charge workflows. `GatewayAccountMode` in `crates/syrup-rail/src/gateway.rs` has `Live` and `Test` variants.

`crates/syrup-rail-postgres/src/subscription_billing_service.rs` owns the service and its trusted `required_gateway_account_mode`. Workflow modules reserve token-free attempts, query provider readiness, commit final admission, perform one mutation, and apply outcomes. Attempt inserts live in `crates/syrup-rail-postgres/src/attempts/*.rs` and `crates/syrup-rail-postgres/src/host_charges.rs`; the fixed-column loader is `crates/syrup-rail-postgres/src/attempts/persistence.rs`.

Schema artifacts are complete versioned distributions. `crates/syrup-rail-postgres/schema/v3/**` shipped with 0.3.0 and is immutable. Schema v4 needs a complete `install.sql`, a transactional `upgrade_from_v3.sql`, a README cutover guide, exported test-support constants, catalog fingerprint coverage, and runtime compatibility updates.

The security baseline is `docs/security/threat-model.md`. This change protects the host-controlled policy boundary; it does not claim that a separate provider query proves the provider's later mutation atomically.

## Plan of Work

Extend `GatewayAccountMode` with canonical lowercase storage conversion and extend `PaymentAttemptIdentity` with a required mode field. Keep the existing constructor live-default and add an explicit non-live construction/builder path used by service-owned reservations. Equality must include mode.

Create schema v4 from the complete v3 install artifact and add `required_gateway_account_mode text NOT NULL DEFAULT 'live'` plus a closed `live`/`test` check on `billing_payment_attempts`. Add a v3-to-v4 upgrade. Update schema contract column lists, fingerprint, runtime assertion names, test support, fresh-install/upgrade tests, crate entrypoints, README files, and release notes. Leave `schema/v1`, `schema/v2`, and `schema/v3` byte-for-byte unchanged.

Extend the fixed loader and every production attempt insert to round-trip mode. New reservations obtain the service requirement. Attempts reconstructed for reconciliation retain it. Prepared replay and admission compare persisted identity with current service mode before another mutation. Terminal replay remains readable regardless of current mode because it does not mutate the provider.

Narrow the final mode check so each workflow verifies account state immediately before invoking its one-shot provider mutation. Reuse a shared typed helper/capability where practical so future mutation paths cannot bypass the check by omission. Add integration tests for all five workflows, including cross-mode prepared replay and zero sale calls on mismatch.

Finally, change the workspace `h2` dependency to 0.4.16 and add a factual changelog item without claiming a RustSec advisory that the current advisory database does not report.

## Concrete Steps

Work from `/home/aa/Documents/syrup-rail`. Use `apply_patch` for hand-authored edits. A mechanical copy of v3 install SQL may seed v4, after which semantic changes must be reviewable. Never edit shipped schema files.

After each milestone run focused tests. Regenerate/check SQLx metadata only through `scripts/jig check sqlx`. At final validation run:

    scripts/jig work check
    scripts/jig check fmt
    scripts/jig check clippy
    scripts/jig check test
    scripts/jig check sqlx
    scripts/jig check contract
    scripts/jig work evidence
    scripts/jig work gates
    scripts/jig work finish

## Validation and Acceptance

An upgraded schema-v3 database must gain a non-null `required_gateway_account_mode` column whose historical rows read `live`. Fresh v4 must reject values outside `live` and `test`; fresh and upgraded catalogs must share one fingerprint.

Every new attempt must persist the trusted service requirement. Test-required plus observed Test may submit once. Test-required plus observed Live must submit zero times. A prepared attempt whose persisted mode differs from the current service must submit zero times and return a typed conflict/configuration result rather than silently changing authority. Mode must be queried at the final pre-mutation boundary, and docs must state that concurrent environment use while toggling one NMI account is unsupported because NMI offers no atomic check-and-charge operation.

The published manifest must show an h2 minimum of 0.4.16, and `cargo metadata --locked --format-version 1` must succeed.

## Idempotence and Recovery

The v3-to-v4 upgrade is forward-only and transactional. Hosts stop schema-v3 billing writers, apply it once, and roll forward. Failure before commit leaves v3 intact. After commit, do not restart a v3 binary.

Development databases are ephemeral. `.agent/state` files are append-only. The working tree already contains the user's 0.3.1 changes; do not reset, discard, or commit unrelated work.

## Artifacts and Notes

Research established:

    v0.3.0 is the latest tag and contains schema v3.
    Cargo.toml: h2 = "0.4"
    Cargo.lock: h2 0.4.16
    NMI account mode and sale are separate network operations.

The current RustSec database reports no h2 advisory for 0.4.15, while 0.4.16's upstream changelog includes resource and connection-handling fixes. Release notes should describe minimum-version hardening factually.

## Interfaces and Dependencies

`GatewayAccountMode` must own stable database conversion. `PaymentAttemptIdentity` must expose `required_gateway_account_mode()` and an explicit non-live construction path while preserving `new(...)` as live-default.

`billing_payment_attempts.required_gateway_account_mode` must be `text NOT NULL DEFAULT 'live'` with a canonical closed check. Production inserts bind the explicit identity value even though the default supports historical fixtures.

The service remains the trusted policy owner. End-user commands do not accept mode. Reconciliation and operator-review paths read persisted mode but never resubmit based only on it.

Plan revision note (2026-08-26): Replaced the initial one-line work note after research and downstream clarification showed schema v3 is shipped, the same NMI account is toggled across environments, and the correct durable value is required-mode authority rather than a claimed atomic provider observation.
