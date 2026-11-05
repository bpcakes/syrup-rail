# Explicit payment state machines

Implement the four safety refactors identified in the audit, and publish reviewable PRs. Baseline: merged PR #10, commit 5279a26. Financial evidence, retry certainty, collection authority, and access must retain their current meaning under replay and concurrent requests. Follow docs/security/threat-model.md; host authorization and SQL locks remain authoritative.

## Progress

- [x] Revalidated merged baseline and transition entrypoints.
- [x] SM4 implementation and local verification: explicit lifecycle evidence transition matrix; delivered as PR #11 (https://github.com/bpcakes/syrup-rail/pull/11).
- [x] SM1 implementation and local verification: named processor-charge transitions and shared classification; delivered as PR #12 (https://github.com/bpcakes/syrup-rail/pull/12).
- [ ] SM2: validated attempt phases and evidence-bound non-submission restoration.
- [ ] SM3: explicit subscription collection states with separate access projection.
- [ ] Verify every PR and publish final links with dependencies and test results.

## Surprises & Discoveries

Existing code already has consuming submission capabilities and typed attempt transitions. Remaining ambiguity lies in composite state and broadly parameterized persistence, not a complete absence of enums. Public advanced charge transition APIs require compatibility handling. Lifecycle numeric ranks encode an evidence precedence policy, including out-of-order reports, rather than merely chronological events. Existing Rust file-size policy prevents growing already oversized owners; extract cohesive decision modules.

## Decision Log

- Preserve durable schema labels and data; no schema migration is planned. Keep SQL compare-and-set and lock ordering, because Rust values cannot establish freshness across processes.
- Deliver SM4 first because it is independent and bounded; then SM1, SM2, and SM3. Stack subsequent PRs only where shared ownership requires it.
- Retain public compatibility adapters where signatures already ship. Route adapters through validation; avoid retaining a second unguarded state machine.
- Do not enable the deferred broad Clippy size/complexity limits as part of this work. New decision owners should be small enough to review.

## Outcomes & Retrospective

SM4: focused lifecycle tests passed (5), formatting and Clippy passed, and
`scripts/jig --json check test` passed all 763 tests with zero ignored/filtered
tests. The production lifecycle owner shrank by 117 lines; the transition
owner and its tests are separate small files. Schema and public API unchanged.
PR #11 published with all 11 CI checks green.

SM1: named observation and transition operations now own charge progression.
Application requires matching approved attempt evidence; terminal reversal
requires immutable attestation. Expired-period parking cannot be cleared by
compatibility transitions. Unresolved host conflicts retain their legitimate
reconciliation path. Two new database scenarios and two policy tests pass;
full workspace verification passed 767 tests (none ignored/filtered), plus
fmt, Clippy, SQLx and public API documentation checks. No schema changes.
SM1 is published as PR #12, stacked on #11; all 11 CI checks pass.

SM2 implementation: explicit attempt phases now drive admission and replay.
Non-submission proof binds the exact attempt identity and admission timestamp;
SQL fences both retry restoration and post-failure timestamp clearing. Local
failures and submitted outcomes are distinct commands shared with host charges.
Core phase coverage and the database stale-proof regression pass. Full tests
passed 769 with no ignored/filtered tests; the initial crate run had one Docker
port-exposure startup failure, and the full rerun with four test threads passed.
Final rerun after host admission cleanup is in progress. Fmt, Clippy, SQLx and
public API documentation passed. SM3 remains unimplemented.

## Work and acceptance

### SM4 — lifecycle evidence (Depends on: none; tracker syrup-rail-1le)

Replace numeric lifecycle ranks in crates/syrup-rail-postgres/src/lifecycle_reconciliation.rs with an exhaustive source/incoming decision table in a cohesive transition module. Preserve monotonic refund totals, timestamp handling, stale evidence disposition, and quarantine outcomes. Verify every state pair plus equal/older/missing timestamps and conflicting refund economics, and retain database replay/host-target tests. Done when adding a lifecycle variant requires an explicit policy decision and the full tests pass. Revert code only if needed; no durable changes.

### SM1 — processor-charge application (Depends on: none; tracker syrup-rail-jjw)

Own source states, destination, reason, and evidence prerequisites in named commands for observation/application/reversal/attestation. Update processor_charges, reconciliation/classification, enrollment_application, host_charge_application, and operator_review. Share decision logic where foreground and reconciliation classify the same facts, retaining distinct event semantics. Test late approval, transactionless evidence, additional charges, terminal reversal replay, ownership conflicts, and application races. Done when production callers cannot independently select arbitrary transition dimensions and compatibility entrypoints validate equivalent rules. Preserve persisted labels and atomic write sets.

### SM2 — attempt retry authority (Depends on: SM1; tracker syrup-rail-njz)

Introduce validated attempt phases derived from durable status and submission evidence. Replace independently selectable outcome boundary/status fields with legal transition commands. Bind proof of non-submission to the exact attempt before clearing submission authority. Integrate replay, admission, resolution, reconciliation, and qualifying dunning checks. Retain existing non-cloneable committed-admission capabilities. Verify uncertain outcomes never restore retry authority, definite non-submission can restore only the matching pending attempt, terminal races fail closed, and dunning excludes unsubmitted failures.

### SM3 — collection and access (Depends on: SM2; tracker syrup-rail-3j0)

Decode subscription status and scheduling into explicit collection states (scheduled, retry scheduled, exhausted, canceled, unpaid), attaching required facts. Integrate renewal failure, recovery, cancellation, period retirement, and persistence consumers. Derive access separately from snapshotted policy and causal history, preserving legacy recovery provenance. Preserve existing public construction paths through validated adapters where feasible. Test retry exhaustion, terminal unpaid, cancellation paid-through access, recovery/expiry races, and invalid state combinations.

## Verification and delivery

For each PR run focused behavior tests, scripts/jig check fmt, scripts/jig check clippy, and finish backend verification with scripts/jig check test. Run scripts/jig check sqlx when SQLx queries change; scripts/check-public-api.sh when exports or public signatures change. Review the complete branch diff and CI before reporting delivery. Record commands/results and PR URLs here as work progresses. Keep each PR based on master or its explicitly documented prerequisite; do not merge without user instruction.

## Recovery

Work on dedicated branches; never rewrite shipped schemas. If a check fails, fix within the owning PR and rerun relevant checks. Continue other independent tasks while CI runs. Existing test databases are harness-owned; do not alter host databases or credentials.
