# Reconcile the local complexity and rollback changes onto 14b7583

This plan follows `.agent/PLANS.md` and `docs/security/threat-model.md`.

## Purpose

Preserve the uncommitted complexity lint/refactor and explicit host-charge rollback behavior while incorporating all fifteen upstream commits through 14b7583. The resulting branch must build and pass the full workspace tests, SQLx metadata check, formatting, Clippy, and contract checks. Leave the reconciled work uncommitted for user review.

## Progress

- [x] (2026-09-06) Fetch remote and preserve all local tracked/untracked changes in stash 2bc5fa0f627590bed99ec3a4cb11c8a53b6e16d9.
- [x] Fast-forward integration/0.6.0 from fe8cdf8 to 14b7583 and reapply the stash.
- [x] Resolve the release documentation conflict and port host-charge helpers into the upstream resolution fragment.
- [x] Compile and reconcile semantic/API conflicts; Rust 1.88 all-target/all-feature Clippy passed (receipt_01M1VG3Z7RK66QKAA8WH0J0RE2). Formatting passed (receipt_01M1VG3QNJMC2PH81GR7DGF9YZ).
- [x] Review the complete final code diff, verify all original local/upstream agent records survive, and pass public API/default/all-feature doctests plus release and rustfmt script regressions.
- [x] Full workspace tests passed in 940.6s (receipt_01M1VH0DB4NTG8301N5H6REAGH), SQLx passed in 55.6s (receipt_01M1VH23MSDXFV2AM0K8AR4V1A), and contract passed. Batch receipt: receipt_01M1VH24QY2Y9YJH6QQEK9BWY5.

## Surprises & Discoveries

The trial patch had two textual conflicts. Porting the host-charge resolution fragment exposed another overlapping change: upstream separates accumulated attempt evidence from the current charge observation. The persistence helper must accept ReconciledNonApprovedEvidence so it cannot use accumulated attempt risk to fabricate processor-charge evidence. The repository now sets --test-threads=1 for the full workspace gate.

## Decision Log

Preserve upstream schema v6 and typed approval-evidence semantics. Retain the new resolution.rs include fragment and port local structure into it. The release documentation must refer to main and retain the exact Clippy 1.88 guidance. Preserve every append-only state record and keep the recovery stash. No commit or push is needed to complete this reconciliation.

## Outcomes & Retrospective

Reconciled onto upstream 14b7583 with no unresolved conflicts. Full workspace/all-feature tests, SQLx, Rust 1.88 Clippy, formatting, contract, public API documentation/doctests, and release/rustfmt script regressions passed. The host-charge helpers preserve upstream per-observation charge evidence and the local explicit transaction finalization. All prior agent records were retained. The original dirty state remains in stash 2bc5fa0f627590bed99ec3a4cb11c8a53b6e16d9. No code was committed or pushed.

## Context and Orientation

The workspace is /home/aa/Documents/syrup-rail. The original local changes enable cognitive_complexity at threshold 20 in Cargo manifests and clippy.toml, split large functions and tests, and centralize host-charge transaction completion. Upstream moved the latter workflow's resolution functions from crates/syrup-rail-postgres/src/host_charge_application.rs into host_charge_application/resolution.rs. Other changed enrollment, lifecycle, schema-contract and operator modules merged textually but still require compilation and behavioral tests. Existing rollback regressions live in host_charge_application/tests/rollback.rs.

## Plan of Work

First reconcile against the upstream module layout. Keep finalization and locked approval classification in the parent; keep conflict, terminal, non-approved, unknown and compensation resolution in the included fragment. Preserve exact attempt identity and target-before-attempt lock order, durable cooldown before resolution, terminal replay rules, and per-observation charge evidence. Host target changes, attempts, charge transitions and events stay in one coordinator transaction, whose owner explicitly completes or rolls back. Provider submission and reservation behavior are unchanged. Existing foreground, observation, reconciliation, stale-target, conflict and rollback tests exercise these paths.

Then compile at Clippy 1.88 and simplify any newly exposed functions without lint suppression. Run all configured gates, review the final diff against upstream, record results, and finish the work plan.

## Concrete Steps

Run commands from the workspace root. The recovery steps already executed were git stash push --include-untracked, git merge --ff-only @{upstream}, and git stash apply. Resolve conflicts, use git add only to mark resolution, then git restore --staged . to leave normal uncommitted changes.

Run cargo fmt --all and rustfmt --edition 2024 for changed include fragments. Run RUSTUP_TOOLCHAIN=1.88.0 SQLX_OFFLINE=true SQLX_OFFLINE_DIR="$PWD/crates/syrup-rail-postgres/.sqlx" scripts/jig check clippy. Use scripts/jig work check --plan-id PLAN for configured contract, test and SQLx gates; this runs the same jig.test contract as scripts/jig check test. Also run scripts/jig check fmt and scripts/check-public-api.sh. Inspect git diff --check, the changed-file diff, and git status. Use scripts/jig work evidence and gates before finish.

## Validation and Acceptance

All configured gates must exit zero. The full test gate includes all workspace features and database-backed payment/reconciliation/schema tests, with one test thread per binary. Both explicit rollback regressions must pass alongside upstream charge observation tests; no schema or SQLx metadata drift is intended. A clean merge alone is insufficient.

## Idempotence and Recovery

The stash identified above retains the original dirty state and untracked files. Do not pop or drop it. HEAD is the fetched upstream commit; code changes remain outside the index. Retry failed verification only after understanding and addressing the cause. Never restore the old implementation wholesale over new evidence semantics or immutable schema artifacts.

## Interfaces and Dependencies

No dependency or public API additions are intended. BillingTransaction retains its existing interface with clarified rollback-on-drop documentation. The private persistence helper accepts ReconciledNonApprovedEvidence, persists attempt_evidence to the attempt, and records only charge_observation() in the processor-charge ledger. Existing SQLx, PostgreSQL test harness and Jig commands provide validation.

Revision note (2026-09-06): recorded completed reconciliation and actual gate receipts. The full test gate runs the repository-configured cargo test --workspace --all-features -- --test-threads=1 command.
