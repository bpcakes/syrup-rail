# Flatten unreleased schemas into v5

This living ExecPlan follows `.agent/PLANS.md`. It combines the unreleased tuple-validation and approval-evidence schema changes so a host on the last released schema v4 can install one v5 upgrade. No deployed database is modified by this work.

## Progress

- [x] 2026-09-06: Checked consumer manifests, lockfiles, overrides and linked worktrees under Documents: 13 checkouts, none above crate release 0.5.2 or pinned to unreleased Git code. Deployed databases were not inspected.
- [x] Read schema contracts, upgrade guards, guides and security baseline.
- [x] Combined SQL artifacts, runtime contract, tests and current documentation; v5 fresh install declares the final columns/functions directly.
- [x] Eight focused tests passed for direct v4 upgrade, rollback, retained evidence, defaults and fresh-install equivalence; current fingerprint remains 0x0a997a70f2ff0659.
- [x] Full repository gates and final diff review completed; all required receipts are fresh and passing.

## Surprises & Discoveries

The Rust formatting gate enumerated deleted tracked files and failed on the removed v6 test module. It now skips files absent from the working tree while retaining checks of existing sources. The first five tuple/conformance tests passed; final install cleanup and evidence tests are being rechecked.

Current documentation calls v5 shipped, but release v0.5.2 contains schema v4. Both v5 and v6 are unreleased. The existing v6 upgrade deliberately rejects terminal host attempts whose previously safe targets would become stuck; a recovery policy remains a release blocker.

## Decision Log

The user explicitly authorized flattening after the consumer audit. Edit unreleased v5 and remove v6; preserve shipped v1–v4 byte-for-byte. Use one v4-to-v5 transaction with the terminal-host guard under the existing evidence-table lock order before DDL, followed by tuple constraint validation and classification changes. Keep both read-only blocker audits and combine preflight counts into one result. Preserve financial evidence and all runtime behavior. Rename the current production assertion to v5 and remove unreleased v6 APIs. Do not invent compatibility for databases on an unreleased intermediate schema.

## Outcomes & Retrospective

Implementation is complete. Eight focused tests, the full workspace test gate, Rust 1.88 Clippy, formatting, default/all-feature public API documentation and doctests, and repository policy checks passed. SQLx passed without metadata changes. All required work-gate receipts match the final working-tree fingerprint. Release-blocker policy remains in force; flattening does not solve recovery of terminal legacy attempts.

## Context and Orientation

Work in `/home/aa/Documents/syrup-rail`. `crates/syrup-rail-postgres/schema/v5` currently tightens the external-reversal resolution tuple constraint; `schema/v6` adds explicit approval classification on attempts, charges and attestations. `schema/current.rs` selects the fresh install used by tests and SQLx. `src/schema_contract.rs` implements read-only startup validation and a catalog fingerprint (a deterministic drift checksum, not a security mechanism). Migration tests live in `src/schema_contract/tests`, with shared database helpers in `src/test_support.rs`. Host applications own migrations and authorization. Follow `docs/security/threat-model.md`; no new security boundary is introduced.

## Plan of Work

First copy the complete current install into v5 and combine upgrades under the v6 lock/guard before changing schema. Retain v5 tuple validation. Rename the review-attempt audit to use v4 as its baseline; combine the preflight queries through aggregate subqueries into one row containing both sets of counts. Replace active schema-v6 references in code/docs, leaving historical execution records and unrelated IPv6/GitHub action versions unchanged. Keep schema v5 as current and v1–v4 immutable in guides. Adapt v6 classification tests to seed immutable v4 and execute the direct upgrade; retain v5 tuple-matrix and rollback tests. Remove obsolete intermediate upgrade helpers and duplicate conformance tests. Corrupt-schema tests must explicitly corrupt disposable fixtures without weakening the supported upgrade.

Second validate that fresh v5 and upgraded v4 have the same canonical fingerprint and defaults; incompatible tuples and each unsupported terminal population must abort and preserve v4. Verify retained raw fields and tuple identities, empty-attempt classification, immutable charge evidence, replay and host admission. All create, reconcile, retry, manual failure, reversal, terminal release and scrub behavior continues through the same current SQL functions and Rust codecs; no business logic is changed.

Finally run repository gates and update this document with outcomes and receipts.

## Concrete Steps

From the repository root, run focused PostgreSQL schema tests with `SQLX_OFFLINE=true SQLX_OFFLINE_DIR="$PWD/crates/syrup-rail-postgres/.sqlx" cargo test -p syrup-rail-postgres --all-features --locked schema_contract::tests -- --test-threads=1`. Format modified Rust files explicitly as included fragments are checked separately. Use `scripts/jig check fmt`, Rust 1.88 clippy via `RUSTUP_TOOLCHAIN=1.88.0 scripts/jig check clippy`, and `scripts/check-public-api.sh`. Run `scripts/jig work check --plan-id plan_01M1VZEEM9GFCG1HFKCZGTM1F0` for the contract, full test and SQLx gates. Inspect `work evidence` and `work gates`, then finish the plan only after success. Use the repository launcher consistently so receipt fingerprints agree.

## Validation and Acceptance

A fresh current install and direct v4 upgrade must pass `assert_runtime_schema_v5_compatible`, including identical catalogs. V4 must fail current startup validation. Both old tuple blockers and legacy terminal host attempts must reject the entire upgrade and leave original schema/admission intact. Preserved charged/attested rows become structured without tuple updates; genuinely empty attempts become absent; other retained attempts remain unclassified. Default and all-feature public docs compile. Full workspace tests, SQLx metadata, formatting, clippy and contract checks pass. `git diff -- crates/syrup-rail-postgres/schema/v1 crates/syrup-rail-postgres/schema/v2 crates/syrup-rail-postgres/schema/v3 crates/syrup-rail-postgres/schema/v4` is empty.

## Idempotence and Recovery

Only disposable harness databases are used for validation. SQL upgrades are apply-once host-owned transactions, not repeatable install scripts. A failure rolls back to v4; after commit hosts roll forward with v5 code and cannot restart v4 writers. Stop all billing, reconciliation, operator and scrub writers before host cutover. Preserve the terminal-host blocker and audited remediation requirements. Do not change real consumer repositories or databases. Retain the pre-existing reconciliation stash. If a local test fails, fix code and rerun the affected gate; never edit receipts or relax assertions merely to pass.

## Interfaces and Dependencies

Keep PostgreSQL 18 and existing SQLx dependencies. Export production `assert_runtime_schema_v5_compatible(&PgPool) -> Result<(), SchemaConformanceError>`. Feature-gated artifacts are V5_INSTALL_SQL, V4_TO_V5_UPGRADE_SQL, V4_TO_V5_PREFLIGHT_SQL and both v4 blocker audits. Older assertions remain feature-gated. No changes to other billing public APIs are intended.

Revision 2026-09-06: Consolidated the fresh-install DDL as well as migration selection, preserved physical column order and fingerprint, and avoided a duplicate retained-attestation scan in the combined preflight.

Validation update: full workspace test receipt `receipt_01M1W17DRWXCEZJ52EQBX8421V` exited 0; Clippy receipt `receipt_01M1VZT4NA5SB6QDY11MCJP23R` and formatting receipt `receipt_01M1VZVKXN2VGVZ79EE68NSQXE` passed. Schema immutability matches the first stable release tags, and formatting-discovery and immutability shell regression suites pass. Rechecked primary Documents consumers during validation: none exceed 0.5.2 or reference schema v5/v6.

Final validation: full workspace tests passed in 1555.1 seconds and SQLx in 73.1 seconds. Batch receipt `receipt_01M1W19P9DA38SR9T7WYJW0NDZ` ties contract, tests and SQLx to the final worktree. SQLx receipt: `receipt_01M1W19N63ASX7K4GWVDESDT38`. `scripts/jig work gates` reports all three required gates passed and fresh. Reviewed the final diff: no changes to shipped v1–v4, Cargo.lock or committed SQLx metadata; active runtime/docs references use v5. Changes remain local and uncommitted; no consumer database or repository was modified.

Final outcome: one current v5 install, one guarded direct v4-to-v5 transaction, a combined preflight and two minimized audits. Fresh install uses final declarations without redundant intermediate DDL. Tests retain the complete tuple matrix, evidence preservation, terminal-host refusal and rollback coverage. The original terminal-host recovery policy remains a release blocker.
