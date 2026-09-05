# Contain provider approval evidence

This living ExecPlan follows `.agent/PLANS.md` and the security baseline in `docs/security/threat-model.md`. It addresses AP-RUST-001 and Bead `syrup-rail-zhe`.

## Purpose

Provider response spellings must not change financial-review behavior after the NMI adapter has interpreted them. In particular, numeric approval code `0100` must retain the same review protection as `100`, including after database reload. Authoritative payment status remains separate from conservative approval signals in an uncertain observation.

## Progress

- [x] Inspected core predicates, NMI classification, ledger writes, operator guards, and schema release history (2026-09-05).
- [x] Add typed approval evidence and adapt NMI observations.
- [x] Persist classification with complete schema-v6 install and forward-only v5 upgrade; preserve older artifacts.
- [x] Update reconciliation, replay, manual review, documentation, and focused regressions.
- [x] Run formatting, Clippy, public API, SQLx, contract, and workspace tests; inspect final diff and finish work receipts.

## Surprises & Discoveries

The NMI parser numerically classifies response codes but preserves their original spelling. Core predicates duplicate this vocabulary with literal matching. Schema v5 appears in the dated 0.6.0 changelog; absent local release tags are insufficient evidence that it is unshipped.

## Decision Log

Use schema v6 rather than mutate v5. Add a provider-neutral classification to processor evidence: unclassified legacy evidence, absent approval signals, text-only hints, and structured approval signals. Raw fields remain untouched. Unclassified historical evidence must remain conservative; migration must never infer absence from unknown provider vocabulary. Unclassified observations must not create immutable pending charges from uninterpreted text: doing so can occupy a transaction identity before its genuine approval arrives. Preserve such observations on the attempt and fail closed for manual review. Exact raw observation replay must remain compatible when an older writer lacked classification. Preserve legacy public string helpers only as deprecated compatibility APIs, with no production financial-policy callers.

## Outcomes & Retrospective

Implementation and validation are complete. Core/NMI tests, targeted reconciliation tests, new application/manual-review regressions, schema-v6 fresh/upgrade tests, formatting, SQLx, Clippy, and the public API gate passed. The full workspace test command passed; a repeated parallel work-gate run encountered two Docker PortNotExposed startup errors before assertions. The final serialized work check passed all required gates with RUST_TEST_THREADS=1 (batch receipt receipt_01M1RY1K2XXSVT55JNBS2P3FVX). Work evidence and gates both report fresh, passing results with no unresolved gates. Shipped schema v1-v5 artifacts remain unchanged.

## Context and Orientation

`crates/syrup-rail/src/gateway.rs` defines processor evidence and authoritative outcomes. `src/operator_review.rs` currently parses raw evidence to guard manual failure. `crates/syrup-rail-nmi/src/adapter.rs` translates the raw NMI client's result into core facts. PostgreSQL owns attempt transitions in `src/attempts/transitions.rs`, row decoding in `src/attempts/persistence.rs`, immutable charge storage in `src/processor_charges{,/storage}.rs`, and attestation decoding in `src/processor_charge_persistence.rs`. `src/schema_contract.rs` fingerprints the complete canonical catalog. `schema/current.rs` selects the install used by tests and the SQLx gate.

## Plan of Work

First introduce the typed classification with conservative compatibility construction and explicit adapter construction. Remove protocol parsing from core financial decisions. NMI alone classifies its raw structured response fields and approval text; classification never promotes an unknown payment outcome to approved.

Next carry classification through every evidence reconstruction and persist it atomically with attempt/charge/attestation evidence. Preserve historical unknown classification and immutable replay semantics. Add schema v6 by copying the complete v5 install and applying the same additive changes as `schema/v6/upgrade_from_v5.sql`. Add a v6 runtime assertion, current-install selection, catalog fingerprint, fresh-versus-upgrade comparison, and classification constraint tests. Old version assertions and fixtures continue to exercise their exact historical schemas.

Finally test numeric spelling equivalence, absent versus uncertain evidence, identity quarantine, foreground unknown observations, database reload, exact replay, and manual-review refusal. Update the public API guide, changelog, schema cutover guide, and relevant crate guides with the new contract and migration requirements.

## Concrete Steps

Run from `/Users/aa/Documents/syrup-rail`. The active work plan is `plan_01M1RVVQ0GGTQ4YR4DW21D5BZ0`; use it for `work check`, `work evidence`, `work gates`, and `work finish`. Use `br` for issue mutation and flush its export afterwards. Do not commit or push application changes without a user request.

## Validation and Acceptance

Run focused core/NMI tests while implementing, then PostgreSQL regression tests using the existing local PostgreSQL 18 harness. Run `scripts/jig check fmt`, `scripts/jig check clippy`, `scripts/check-public-api.sh`, `scripts/jig check sqlx`, `scripts/jig check contract`, and finish backend validation with `scripts/jig check test`. All must pass. The regression must show that unknown observations containing either `100` or `0100` retain equal pending-charge and manual-review protection after reload. An explicit absent classification still allows the intended operator exit; unclassified historical approval-bearing evidence cannot silently become absence.

## Idempotence and Recovery

Only disposable test databases are modified during implementation. Hosts stop billing writers, apply the v5-to-v6 upgrade in one transaction, then start v6-aware code. Failed upgrades roll back to v5; after commit roll forward. Never rewrite original financial evidence or shipped schema artifacts. No provider requests or credential access are needed.

## Interfaces and Dependencies

Keep all production dependencies unchanged unless provider-local text classification requires moving an existing regex use to the NMI adapter. The core classification is a closed value with stable storage labels. The NMI adapter owns provider vocabulary; PostgreSQL only encodes and decodes classification and applies typed review policy. Host authorization and independent operator investigation remain required.

Revision: initial implementation plan, 2026-09-05.

Revision: implementation complete, with conservative legacy handling and schema-v6 fingerprint 0x8cf659faaa084a77; final verification pending, 2026-09-05.

Validation note (2026-09-05): container startup contention affected the parallel gate repetition. No application assertion failed in that run (295 PostgreSQL tests passed, two failed during ContainerStart). Serialized gate validation passed, including the workspace tests and SQLx verification.
