# Explicit payment state machines

Implement the four safety refactors identified in the audit, and publish reviewable PRs. Baseline: merged PR #10, commit 5279a26. Financial evidence, retry certainty, collection authority, and access must retain their current meaning under replay and concurrent requests. Follow docs/security/threat-model.md; host authorization and SQL locks remain authoritative.

## Progress

- [x] Revalidated merged baseline and transition entrypoints.
- [x] SM4: exhaustive lifecycle evidence transitions; PR #11.
- [x] SM1: named, guarded processor-charge transitions; PR #12.
- [x] SM2: explicit attempt phases and admission-bound non-submission proof; PR #13.
- [x] SM3: validated collection states with separate access projection; PR #14.
- [x] Full local verification and published PR links with dependencies below.

## Surprises & Discoveries

Existing code already has consuming submission capabilities and typed attempt transitions. Remaining ambiguity lies in composite state and broadly parameterized persistence, not a complete absence of enums. Public advanced charge transition APIs require compatibility handling. Lifecycle numeric ranks encode an evidence precedence policy, including out-of-order reports, rather than merely chronological events. Existing Rust file-size policy prevents growing already oversized owners; extract cohesive decision modules.

## Decision Log

- Preserve durable schema labels and data; no schema migration is planned. Keep SQL compare-and-set and lock ordering, because Rust values cannot establish freshness across processes.
- Deliver SM4 first because it is independent and bounded; then SM1, SM2, and SM3. Stack subsequent PRs only where shared ownership requires it.
- Retain public compatibility adapters where signatures already ship. Route adapters through validation; avoid retaining a second unguarded state machine.
- Do not enable the deferred broad Clippy size/complexity limits as part of this work. New decision owners should be small enough to review.

## Outcomes & Retrospective

All four refactors are published in this merge order:

| Refactor | PR | Base | Full local tests |
| --- | --- | --- | --- |
| SM4 lifecycle evidence | https://github.com/bpcakes/syrup-rail/pull/11 | master | 763 passed |
| SM1 processor charges | https://github.com/bpcakes/syrup-rail/pull/12 | PR #11 branch | 767 passed |
| SM2 attempt phases | https://github.com/bpcakes/syrup-rail/pull/13 | PR #12 branch | 769 passed |
| SM3 collection/access | https://github.com/bpcakes/syrup-rail/pull/14 | PR #13 branch | 773 passed |

Every full local run listed above had zero ignored/filtered tests. Formatting,
Clippy and the relevant SQLx/public API checks passed. PR #11–#13 had all 11 CI
checks green when this receipt was written; PR #14's final fixture correction
triggers a new CI run. Live check results remain on the PRs.

SM1 centralizes observation and application decisions, requires matching owning
approval for application and immutable attestation for terminal reversal, and
preserves permanent expiry parking. Legitimate unresolved host conflicts remain
reconcilable. SM2 makes prepared/admitted/uncertain phases explicit and fences
non-submission proof by identity and admission timestamp. SM3 validates schedule
combinations at persistence, application and access boundaries and attaches
required unpaid evidence to the terminal collection variant. Causal access
history and legacy recovery provenance remain separate. No schema or durable
label changes were made; compatibility constructors/adapters remain available.

Verification recovered from intermittent per-test Docker startup/connection
failures. One cross-worktree shared build directory also caused a doctest crate
version collision; independent build directories fixed that setup error. The
final SM2 and SM3 full suites used the harness-supported POSTGRES_TEST_ADMIN_URL
against a dedicated ephemeral PostgreSQL 18 server, with four test threads and
isolated per-test databases. Both passed cleanly. The collection query's added
anchor required updating the deliberately malformed temporary-table fixture so
its rollback test still reaches the intended client-side decode failure.

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
