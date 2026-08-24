# Implement the requested audit tasks sequentially

This plan verifies and implements accepted simplification-audit issues in order. The user skipped R-41 after verification, so it remains open and unchanged. R-35 is a private Rust error-boundary refactor in `crates/syrup-rail-postgres`. R-40 requires changes in the upstream Jig checkout at `/Users/aa/Documents/jig-sh` followed by regeneration of Syrup Rail's managed harness. Each implemented issue is validated and closed before the next is claimed.

## Progress

- [x] Read repository guidance, the security baseline, Beads descriptions, and current worktree state.
- [x] Reproduce R-41 structurally in the rendered installer and claim the bead.
- [x] Return R-41 to open without code changes after the user explicitly skipped it.
- [x] Verify, claim, implement, and validate R-35 without changing public errors.
- [x] Close R-35 and sync Beads.
- [x] Verify and claim R-40; implement and validate both migration layouts in upstream Jig.
- [x] Regenerate the downstream config, contract, launchers, and guidance for `versioned_artifacts` without changing schema artifacts.
- [x] Run downstream contract, recursive immutability, full test, SQLx, formatting, and Clippy gates.
- [x] Resolve the review portability blocker by abandoning the unpublished R-40 pin and regenerating from reachable Jig master `f2b38c9`.
- [x] Return R-40 to open because reachable Jig master does not yet implement the migration-layout contract.
- [x] Re-run contract, formatting, Clippy, SQLx, and full tests and prove a clean Cargo install from the reachable master pin.

## Surprises & Discoveries

- The Syrup Rail worktree already contains completed R-02 changes and append-only Jig/Beads receipts. These are user-owned and must be preserved.
- `scripts/install-jig.sh` is listed in `.agent/jig-managed-paths.json`; direct downstream-only edits would be overwritten. The source of truth is the sibling Jig checkout's Jinja template.
- The current lock directory is empty, reclaimed only when its mtime exceeds 300 seconds, and removed unconditionally by the original process's EXIT trap. This proves both live-owner theft and ABA release are present.
- The current upstream Jig master already has a much newer guard/owner-record protocol, but Syrup Rail's pinned release is on a divergent older history. No upstream or downstream R-41 code was changed before the user skipped it.
- R-35's existing malformed-attestation test called the codec directly. Routing it through `attest_external_reversal` now proves the private error conversion at the public operator workflow boundary.
- A full downstream harness render from the current upstream Jig head also removed unrelated repository-owned CI and security guidance accumulated since the old pin. Those unrelated replacements were not retained; the R-40 config/contract/launcher/guidance changes were kept, and the existing CI was extended only with the new recursive migration-immutability check.
- Upstream Jig's complete standard library suite passed once (1,577 passed, 2 ignored) and its full work gate passed once (2,206 tests plus vault groups). Later reruns exposed an unrelated Nextest-only worker cleanup failure; the same exact test passes under `cargo test`. Clippy is independently blocked by a pre-existing `collapsible_if` lint in `crates/jig/build.rs:300`.
- The upstream implementation was committed only on local branch `codex/syrup-rail-r40` at `2f1a744`. Review proved that GitHub rejected the pinned revision with `upload-pack: not our ref`. At the user's direction, no branch was pushed; Syrup Rail was regenerated from reachable Jig master `f2b38c9`, and R-40 was returned to open.

## Decision Log

- Execute one bead at a time. The user's explicit skip supersedes implementing R-41; do not claim R-40 until R-35 is closed.
- Follow `docs/security/threat-model.md` for R-35 because processor-charge and operator-attestation rows are protected financial evidence. No new security mechanism or trust boundary is planned.
- Preserve R-40's smallest credible downstream scope by retaining repository-owned CI/security customizations while accepting the generated contract-v4 launchers required to distinguish the newly pinned runtime from older `0.2.0` binaries.
- Do not publish the local R-40 branch. Use the installed Jig binary from reachable master `f2b38c9`, regenerate the downstream harness from that exact remote revision, and leave R-40 open until the feature exists upstream on a reachable commit.

## Outcomes & Retrospective

R-41 was verified, unclaimed, and left unchanged after the user skipped it. R-35 now owns a private `ProcessorChargePersistenceError { Sql, InvalidState }`, maps it exhaustively at both consumers, and no longer admits operator host/workflow errors into charge storage. Focused charge/operator suites, formatting, Clippy, contract, SQLx, and repository tests pass; no schema, SQLx metadata, or public error type changed.

R-40's local upstream experiment modeled `flat_migrations | versioned_artifacts`, but it was never published and is not part of the final downstream render. Syrup Rail now pins reachable Jig master `f2b38c9`; the generated contract again exposes `jig.migration_add`, while repository guidance continues to forbid that command for the versioned schema tree. R-40 remains open for a future implementation on a reachable upstream revision.

Fresh downstream `jig.contract_check`, `jig.test`, `jig.sqlx_check`, `jig.fmt_check`, and `jig.clippy` receipts pass against reachable Jig master `f2b38c9`; the work evidence and required gates are fresh. A clean `cargo install --git ... --rev f2b38c9... --locked` also succeeds and reports `jig 0.2.0`.

## Context and orientation

R-41's generated installer is `scripts/install-jig.sh`; its upstream source is `templates/project/scripts/install-jig.sh.jinja` in `/Users/aa/Documents/jig-sh`. The installer serializes `cargo install` calls through `<install-root>.lock`. Its current age-only stale check can steal a live lock, and the old process can then delete a successor's lock.

R-35 centers on `crates/syrup-rail-postgres/src/processor_charge_persistence.rs`, consumed by `operator_review.rs` and `processor_charges/storage.rs`. The neutral persistence codec currently returns the operator-review workflow's broader error type. The target is a private `ProcessorChargePersistenceError { Sql, InvalidState }` converted independently at each consumer boundary while preserving public error variants, messages, redaction, and transient SQL handling.

R-40 centers on `.jig.toml`, `.agent/jig-contract.json`, generated guidance, and upstream Jig config/contract/CLI validation. Syrup Rail uses complete versioned schema artifacts, but the current generated contract exposes the flat `migration-add` capability. The target is a closed migration-layout setting with a backward-compatible flat default and explicit `versioned_artifacts` mode that suppresses and rejects migration-add while retaining recursive immutability checks.

## Plan of work

R-41 was intentionally skipped after verification.

For R-35, read the PostgreSQL crate guide and inspect all codec consumers and tests. Introduce the private two-variant error at the neutral module, add exhaustive conversions at operator-review and charge-store boundaries, delete reverse/catch-all mappings, and add focused malformed-row plus SQL classification regression coverage. Run focused PostgreSQL tests, formatting, Clippy, SQLx, and the repository backend test gate.

Third, inspect Jig's configuration schema, generated contract, migration CLI/MCP handlers, managed guidance, and immutability tests. Add the closed layout setting with a flat default. Gate contract exposure and runtime admission on flat mode, keep recursive immutability protection for both modes, add upstream fixtures for both layouts, then update the Syrup Rail pin/config and regenerate managed files. Prove the contract lacks migration-add, direct CLI/MCP calls reject it, immutable v1/v2 artifacts remain protected, and no schema artifact changed.

## Concrete steps

Run upstream tests from `/Users/aa/Documents/jig-sh` and downstream commands from `/Users/aa/Documents/syrup-rail`. Use `scripts/jig work check`, `scripts/jig work evidence`, and `scripts/jig work gates` to record repository validation. After each bead, use `br close <id> --reason <evidence>` and `br sync --flush-only`.

## Validation and acceptance

R-41 is complete only when a fake-cargo harness proves a live owner remains exclusive beyond the former stale threshold, a killed/dead owner can be reclaimed, an old owner's exit cannot remove a replacement generation, and default/runtime/MCP profile-root reuse is unchanged. Upstream template tests and Syrup Rail's generated contract check must pass.

R-35 is complete only when malformed charge and attestation rows traverse both consumers with unchanged public invalid-state behavior, transient SQL remains distinguishable/retryable where currently supported, and replay/drift/reversal tests plus exact public error/redaction behavior pass.

R-40 is complete only when upstream flat and versioned fixtures prove contract presence/absence, CLI and MCP rejection in versioned mode, recursive immutability in both layouts, and the regenerated Syrup Rail contract no longer advertises migration-add. No schema or SQLx metadata change is expected.

Final completion also requires `scripts/jig check fmt`, `scripts/jig check clippy`, `scripts/jig check test`, `scripts/jig check sqlx`, and `scripts/jig check contract`, with the diff reviewed for generated drift and unrelated user changes preserved.

## Idempotence and recovery

Tests and generation commands must be rerunnable. Lock cleanup operates only on directories whose acquisition identifier matches the caller's observed generation. If upstream generation fails, retain upstream source changes and rerun after fixing the generator; do not hand-edit generated downstream files as the source of truth. Do not overwrite versioned schema artifacts or rewrite `.agent/state/*.jsonl` history.

## Interfaces and dependencies

R-41 and R-40 depend on the local upstream Jig checkout and coordinated downstream regeneration. R-35 has no external dependency and must not change public Rust error types. R-40 must default older/unspecified configurations to flat migrations so existing adopters retain their contract, while Syrup Rail explicitly opts into versioned artifacts.
