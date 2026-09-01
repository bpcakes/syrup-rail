# Make gateway account-mode authority structural

This ExecPlan is a living document and must be maintained in accordance with `.agent/PLANS.md`. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must remain current while implementation proceeds.

## Purpose / Big Picture

Syrup Rail must use the real NMI APIs in both environments while preventing the same mutable NMI merchant account from being used under the wrong deployment policy. Staging requires NMI's account-wide TEST mode and production requires live mode. After this work, every supported low-level provider-submission API will require a non-forgeable capability before it can invoke NMI. The high-level service performs an early readiness check before final admission, and consuming the capability performs a mandatory second mode check at the real provider-submission boundary.

The schema-v4 requirement will ship at the Cargo-incompatible `0.4.0` boundary instead of silently replacing a `0.3.x` runtime. The v3-to-v4 migration will use separate prepare, validate, and finalize artifacts so the long check-constraint scan uses PostgreSQL's weaker validation lock, and the temporary `live` default will be removed before v4 becomes canonical. Direct regression tests will demonstrate that matching Test and Live modes both reach the provider while every mismatch produces zero mutations.

## Progress

- [x] (2026-08-26T14:15Z) Read the repository, PostgreSQL crate, threat-model, and ExecPlan instructions.
- [x] (2026-08-26T14:15Z) Research NMI account TEST mode, Cargo pre-1.0 compatibility, and PostgreSQL constraint-validation locking from authoritative documentation.
- [x] (2026-08-26T14:15Z) Start Jig plan `plan_01M0Z6T645DNQXK1Q7692Q31JY` and record the structural decisions below.
- [x] (2026-08-26T16:40Z) Implement a mode-verified gateway capability and require it at all five public submission functions.
- [x] (2026-08-26T16:40Z) Remove duplicate cooldown reads and separate account-mode replay mismatch from ordinary gateway-configuration drift.
- [x] (2026-08-26T16:40Z) Move release metadata to 0.4.0, hide historical schema assertions from production exports, and stage the v3-to-v4 migration.
- [x] (2026-08-26T16:40Z) Remove the permanent schema default and make every v4 attempt writer, including fixtures, provide the required mode explicitly.
- [x] (2026-08-26T16:40Z) Add focused direct-boundary, schema-stage, replay-classification, and service tests.
- [x] (2026-08-26T17:05Z) Run format, Clippy, tests, SQLx, contract, evidence, gates, and inspect the final diff.
- [x] (2026-08-26) Research NMI capacity and testing alternatives after review questioned the added Query API traffic.
- [x] (2026-08-26) Refactor every high-level mutation to mint exactly one mode capability while the attempt is prepared and carry it across final admission.
- [x] (2026-08-26) Re-run the complete repository gates for the one-query design; formatting, Clippy, SQLx, contract, the 200-test PostgreSQL suite, and the serialized repository test gate pass.
- [x] (2026-08-26) Research the comprehensive-review questions and confirm that account-mode serialization is documented but not enforced locally and cannot cover Merchant Portal toggles.
- [x] (2026-08-26) Replace the one-query design with early readiness plus mandatory submission-boundary revalidation, and centralize mode-aware replay classification.
- [x] (2026-08-26) Run focused regressions and complete repository gates for the final two-check design; format, Clippy, all tests, SQLx, and contract checks pass.

## Surprises & Discoveries

- Observation: NMI's account TEST mode is not a mock endpoint or a local no-op. It toggles the entire active account; callers still submit to the Payment Gateway, while NMI simulates processor behavior and separates test transactions and vault records from live records.
  Evidence: NMI's official Test Mode documentation says the entire active gateway account is toggled, all submitted transactions are simulated, and test transactions and vault IDs do not appear in live mode.

- Observation: the high-level service already implements the intended matching-mode behavior, but the five public `submit_admitted_*` functions accept a raw `ResolvedGateway` and can invoke it without any account-mode observation.
  Evidence: each function calls `gateway.sale` or `gateway.store_payment_method`; none calls `gateway.account_mode`. They are explicitly re-exported from `crates/syrup-rail-postgres/src/lib.rs` as supported composition points.

- Observation: Cargo considers `0.3.1` compatible with `0.3.0`, even though schema v4 is mandatory for the new runtime.
  Evidence: the Cargo reference defines `0.3.0` as `>=0.3.0, <0.4.0`, so a host can receive 0.3.1 through an ordinary compatible update while retaining its schema-v3 startup call.

- Observation: PostgreSQL's fast constant default avoids rewriting historical rows, but adding a validated check constraint scans the table under `ACCESS EXCLUSIVE`. A separately committed `NOT VALID` constraint followed by `VALIDATE CONSTRAINT` moves the scan to `SHARE UPDATE EXCLUSIVE`.
  Evidence: PostgreSQL 18 documents both lock levels and states that validation must occur after the `NOT VALID` addition is committed to avoid blocking concurrent updates during the scan.

- Observation: removing the database default exposed 47 raw test writers that had silently depended on live authority; after making them explicit, the canonical no-default contract became a practical omission detector rather than documentation alone.
  Evidence: the first full PostgreSQL test run failed every affected insert with a `required_gateway_account_mode` not-null violation; the repaired suite and explicit omission regression now pass.

- Observation: the requested independent Claude/Codex comprehensive review confirmed the capability chain and staged catalog design, then found inconsistent terminal cross-mode replay classification and silent live defaults in lower-level Rust APIs.
  Evidence: terminal enrollment and host-charge reservation logic now returns canonical terminal results across mode changes, low-level reservation functions and `PaymentAttemptIdentity::new` require an explicit mode, and a direct capability mismatch test proves zero provider mutations.

- Observation: NMI documents a system-wide rate limit shared by Payment API and Query API traffic but publishes no numeric per-account or per-IP allowance. The repository has no provider-capacity evidence for multiplying every mutation into two or three account-mode queries.
  Evidence: NMI's official rate-limit page describes HTTP 429 for combined Payment and Query API traffic and directs merchants to support when fewer concurrent connections are insufficient; it gives no threshold.

- Observation: NMI supports `test_mode=enabled` for an individual test transaction on a live account, and separately recommends sandbox or dedicated Test Merchant Accounts. The transaction override does not establish that a production mutation will be live and is not documented as a uniform replacement for every Customer Vault mutation used by this library.
  Evidence: NMI's official Testing Methods page scopes the flag to one test transaction and recommends sandbox accounts for integration testing.

- Observation: the repository does not own an account-mode toggle, lease, or cross-process serialization primitive; its serialization requirement exists only in documentation, while NMI administrators can toggle the account in the Merchant Portal.
  Evidence: local source search found only the Query API status call and host guidance. NMI's Test Mode documentation states that the switch affects the entire active account and can be changed by portal users.

## Decision Log

- Decision: preserve actual NMI API mutations in account TEST mode; verification is an authorization guard, not an alternate transport.
  Rationale: this is the downstream staging requirement and matches NMI's documented account behavior.
  Date/Author: 2026-08-26, Codex.

- Decision: add a public, opaque `ModeVerifiedGateway` capability in `syrup-rail-postgres`, minted only by an async account-mode verifier, and consume it in every public `submit_admitted_*` function.
  Rationale: checking only in `SubscriptionBillingService` is a leaky abstraction because supported lower-level composition can bypass it. A required opaque argument converts an easy-to-forget convention into a compile-time API requirement. Each submit function must also compare the capability's required mode and gateway identity to the durable admission before provider I/O.
  Date/Author: 2026-08-26, Codex.

- Decision: retain the early readiness observation and require the capability's mutation delegation to re-query mode immediately before provider submission.
  Rationale: the account mode is mutable external state, so an earlier observation cannot be a durable proof. Boundary revalidation structurally prevents supported low-level callers from omitting the final safety check. It narrows rather than eliminates the provider-side race; separate accounts remain the hard isolation boundary.
  Date/Author: 2026-08-26, Codex after comprehensive review and follow-up research.

- Decision: centralize the combination of durable attempt state and requested account mode in one replay classifier.
  Rationale: independent workflow branches had drifted: recovery and payment-method replacement ignored mode for prepared replays, and initial enrollment evaluated mode before stale repair. One state-machine decision makes terminal replay, local repair, resumability, and mode conflict consistent.
  Date/Author: 2026-08-26, Codex after comprehensive review.

- Decision: recommend separate NMI test and production merchant accounts as the operational long-term boundary; do not model the transaction-scoped test flag as a universal replacement for account-mode authority.
  Rationale: account isolation removes the shared mutable staging/production switch. The transaction flag can safely force a test transaction but cannot prove that a production transaction is live and is not documented for every mutation shape used here.
  Date/Author: 2026-08-26, Codex after NMI capacity research.

- Decision: release the mandatory schema/runtime change as 0.4.0 and remove the v3 runtime assertion from ordinary root exports while preserving historical conformance helpers behind test support.
  Rationale: this makes Cargo resolution communicate the incompatible runtime boundary and prevents an obsolete production startup guard from approving an unusable schema.
  Date/Author: 2026-08-26, Codex.

- Decision: split v3-to-v4 into prepare, validate, and finalize artifacts. Prepare adds the fast-default column and a `NOT VALID` closed-value check; validate scans under the weaker lock; finalize drops the temporary default immediately before the v4 application cutover.
  Rationale: persisted database state can straddle deploys and the payment-attempt ledger grows indefinitely. Separate transactions follow PostgreSQL's supported low-lock pattern while keeping v3 writers temporarily compatible through the explicit `live` default.
  Date/Author: 2026-08-26, Codex.

- Decision: canonical v4 has no default for `required_gateway_account_mode`.
  Rationale: every new writer must explicitly state trusted deployment authority. A permanent default would silently convert future omissions into live authority and enlarge the financial bug surface.
  Date/Author: 2026-08-26, Codex.

- Decision: introduce a distinct enrollment reservation rejection for required-mode replay mismatch instead of reusing `GatewayConfigurationChanged`.
  Rationale: one enum value currently represents both durable gateway identity drift and deployment-mode mismatch, which caused the service to reclassify existing public behavior. Distinct facts preserve existing error mapping and make tests precise.
  Date/Author: 2026-08-26, Codex.

- Decision: preserve terminal idempotent replay across deployment-mode changes and reject only nonterminal attempts that could still reach the provider.
  Rationale: terminal results are immutable financial facts and require no new provider authority. Treating them as configuration conflicts contradicted the replay-first public contract and created divergent behavior among payment flows.
  Date/Author: 2026-08-26, Codex after comprehensive review.

- Decision: remove silent live defaults from low-level reservation APIs and direct payment-attempt identity construction.
  Rationale: dropping the SQL default while retaining Rust defaults merely moved the omission hazard. The 0.4 breaking boundary is the correct point to require trusted deployment policy explicitly at every advanced composition boundary.
  Date/Author: 2026-08-26, Codex after comprehensive review.

## Outcomes & Retrospective

All five real provider mutation paths consume one opaque, non-cloneable capability that matches the durable attempt's explicit mode and gateway identity. The high-level service performs an early provider account-mode query, and capability consumption performs a mandatory final query before mutation. Matching Test and Live policies execute the scripted provider mutation; either early or final mismatches execute zero mutations. Terminal results remain replayable across deployment-mode changes, stale local attempts are repaired first, and prepared work fails closed consistently across attempt kinds.

Schema v4 now has separately committed prepare, validate, and finalize artifacts, no canonical default, an explicit omission regression, and fingerprint-identical fresh and upgraded catalogs. The public runtime boundary and workspace version are 0.4.0, and advanced low-level reservation APIs no longer silently select live mode.

The requested Claude-plus-Codex comprehensive review and follow-up root-cause pass completed. Its replay omissions were resolved through the shared replay policy, and its mode-verification race now has mandatory submission-boundary defense in depth plus explicit separate-account guidance. The migration-only availability window is documented as an operational cutover constraint rather than accepted as another runtime schema. Formatting, Clippy, the full test suite, SQLx, and contract checks pass for the final design.

## Context and Orientation

`crates/syrup-rail/src/gateway.rs` defines provider-neutral gateway modes and the `ResolvedGateway` object that delegates account lookup and mutations to an adapter. `crates/syrup-rail-nmi/src/adapter.rs` sends those calls through the real NMI client. `crates/syrup-rail-postgres/src/subscription_billing_service/*.rs` owns the recommended high-level workflows. Final committed admission and provider mutation live in `crates/syrup-rail-postgres/src/enrollment_application/{initial,recovery,renewal,payment_method_replacement}.rs` and `crates/syrup-rail-postgres/src/host_charge_application.rs`.

A mode-verified gateway capability means an opaque Rust value whose fields callers cannot construct. The only constructor performs `ResolvedGateway::account_mode`, compares the observed mode with the trusted required mode, and returns either the capability or a typed readiness failure. Submission functions consume the capability instead of accepting a raw gateway. They compare its stored requirement and canonical gateway identity with the durable admitted attempt, then re-query mode immediately before calling the real `sale` or `store_payment_method` method.

`crates/syrup-rail-postgres/schema/v4/install.sql` is the unshipped current fresh schema. The existing `upgrade_from_v3.sql` currently adds a permanent default and validates a check constraint under one exclusive-lock transaction. Because v4 has not shipped, its artifacts may be corrected. `crates/syrup-rail-postgres/src/schema_contract.rs` embeds the artifacts for tests and computes a canonical catalog fingerprint; fresh v4 and fully finalized v3 upgrades must remain identical.

## Plan of Work

First add the opaque verification type and error in a small provider-submission boundary module owned by `syrup-rail-postgres`. The verifier accepts a raw resolved gateway and trusted required mode, queries the real provider, and either returns a non-cloneable capability or preserves the existing typed distinction between mode mismatch, provider rate limiting, and other gateway failures. Re-export only the capability, verifier, and safe errors required for low-level composition. Give the capability crate-private accessors for identity and actual mutation delegation.

Change all five `submit_admitted_*` signatures to consume this capability. Replace their raw gateway identity comparisons and calls with capability access. Require exact equality among the durable attempt's required mode, the capability's required mode, and the canonical gateway identity. The service mints the capability once after durable reservation, preserves retryable prepared-state behavior on transient lookup failure, carries it through final admission and the last local cooldown check, then consumes it for submission. Delete duplicate account-mode and cooldown checks.

In initial enrollment, add a distinct required-mode replay rejection in the core domain enum and map only that rejection to the top-level service `GatewayConfigurationChanged` error. Leave the pre-existing gateway identity drift rejection mapped through `ReservationRejected`, preserving prior behavior. Update exhaustive matches and tests.

Change all workspace package versions and release documentation from 0.3.1 to 0.4.0. Remove `assert_runtime_schema_v3_compatible` from the ordinary root facade and compile it only for tests or the explicit schema-contract support feature. Keep `assert_v3_conforms` available there for historical artifact tests.

Replace the single v3 upgrade contract with three immutable unshipped v4 artifacts. The prepare artifact adds `required_gateway_account_mode text NOT NULL DEFAULT 'live'` and the named check constraint `NOT VALID`. The validate artifact validates that constraint in its own transaction. The finalize artifact drops the column default. Update embedded constants, docs, crate guidance, examples, and schema tests. Change fresh v4 to `text NOT NULL` without a default. Update every raw v4 test insert to bind an explicit mode so accidental omissions fail. Recompute the catalog fingerprint only if the final canonical catalog changes; a removed default will change it intentionally, while fresh and finalized upgrade paths must match.

Add direct tests that obtain a proof for matching Live and Test modes and show that the real submission abstraction invokes the scripted provider exactly once. Add mismatch tests for all five public submission functions showing that no capability is produced and no provider mutation is possible. Keep service tests for post-admission mode flips and add assertions that the successful Test flow still calls the provider. Add staged schema tests for temporary default behavior, constraint validation, final default removal, and catalog equality.

## Concrete Steps

Work from `/home/aa/Documents/syrup-rail`.

Inspect focused diffs and run focused compilation frequently:

    cargo test -p syrup-rail-postgres gateway_mode
    cargo test -p syrup-rail-postgres schema_contract::tests::v4

After implementation, run the repository contract:

    scripts/jig work check
    scripts/jig check fmt
    scripts/jig check clippy
    scripts/jig check test
    scripts/jig check sqlx
    scripts/jig check contract
    scripts/jig work evidence
    scripts/jig work gates

Use `scripts/jig work finish` only after the diff and evidence show every acceptance condition is satisfied.

## Validation and Acceptance

The direct low-level API must no longer compile when passed a raw `ResolvedGateway`; it must require `ModeVerifiedGateway`. A matching Test proof followed by submission must increment the scripted gateway's actual mutation counter once. A matching Live proof must do the same. Live-required/Test-observed and Test-required/Live-observed verification must return typed mismatches and leave mutation counters at zero.

Every high-level enrollment, renewal, recovery, payment-method replacement, and host-charge path must perform an early account-mode query and a second query at the provider-submission boundary, preserve prepared-state retry semantics, and retain zero-mutation mismatch tests. The capability must cross final admission without becoming cloneable or forgeable. A scripted mode flip between the two queries must resolve the admitted attempt without invoking the provider mutation.

Applying prepare to schema v3 must give historical and newly omitted rows `live` while the check reports not validated. Applying validate in a separately committed transaction must mark the constraint valid. Applying finalize must remove the default so a new insert that omits the mode fails. The fully finalized catalog fingerprint must equal fresh v4, and both must pass `assert_runtime_schema_v4_compatible`.

An ordinary build must not expose `assert_runtime_schema_v3_compatible`; schema-contract tests and the support feature must still be able to validate historical v3. Cargo metadata and all package manifests must report 0.4.0 consistently.

## Idempotence and Recovery

All Rust and documentation edits are ordinary source changes and can be reapplied or reverted file by file. Schema v4 is unshipped, so edit only v4 and never modify immutable v1, v2, or v3 bytes. The staged artifacts are forward-only: a failed prepare transaction rolls back to v3; after prepare commits, retry validate; after validation commits, stop v3 writers and retry finalize. Do not run finalize until the 0.4.0 application is ready, because dropping the compatibility default intentionally makes omitted mode writes fail.

If catalog fingerprint tests fail, inspect fresh and finalized catalogs rather than guessing a constant. Update the fingerprint only after proving both paths are structurally identical. Preserve all unrelated staged user changes.

## Artifacts and Notes

Authoritative research conclusions:

    NMI account TEST mode: real gateway calls, account-wide simulated processing,
    separated test/live transaction and vault records.

    Cargo 0.3 requirement: >=0.3.0, <0.4.0.

    PostgreSQL ADD CHECK: ACCESS EXCLUSIVE plus table scan.
    PostgreSQL VALIDATE CONSTRAINT after committed NOT VALID: SHARE UPDATE EXCLUSIVE.

## Interfaces and Dependencies

The final public boundary in `syrup-rail-postgres` must include an opaque lifetime-bound capability similar to:

    pub struct ModeVerifiedGateway<'gateway> { /* private */ }

    pub async fn verify_gateway_account_mode(
        gateway: &ResolvedGateway,
        required: GatewayAccountMode,
    ) -> Result<ModeVerifiedGateway<'_>, GatewayAccountModeVerificationError>;

The exact error name may adapt to existing vocabulary, but it must distinguish a typed `GatewayAccountModeMismatch` from the existing `GatewayError` without exposing provider values. `ModeVerifiedGateway` must not implement `Clone` or expose its raw gateway publicly. Each `submit_admitted_*` function consumes one capability and confirms it matches the admitted attempt before using crate-private mutation delegation.

The schema test-support surface must expose three v3-to-v4 artifact constants corresponding to prepare, validate, and finalize. Historical runtime assertions remain available only under `cfg(test)` or `schema-contract-test-support`; ordinary applications use only `assert_runtime_schema_v4_compatible`.

Plan revision note: created after the merged review and authoritative research so implementation addresses the shared abstraction and compatibility causes rather than patching individual call sites.
