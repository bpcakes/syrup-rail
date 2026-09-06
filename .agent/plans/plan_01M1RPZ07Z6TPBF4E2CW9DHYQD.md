# Consolidate validated duplicate mechanics

Implement DU-001 through DU-005 from the dup-unifier report. Investigate DU-006's zero-value lifecycle validation before deciding whether it is safe to consolidate. Preserve DU-007 through DU-010 as separate public types and capabilities. Follow docs/security/threat-model.md: no provider mutation retries, no new security boundary, and no changes to immutable payment evidence or shipped schemas.

## Progress

- [x] Inspect current worktree, report, crate guides, and threat model.
- [x] DU-001: centralize ordered initial-attempt row locks and verify transaction/scope behavior.
- [x] DU-002: centralize transient SQLSTATE classification while preserving contextual policies.
- [x] DU-003: centralize bounded audit-reason validation behind existing public types.
- [x] DU-004: centralize transaction-local two-timeout execution, retaining operation policy.
- [x] DU-005: centralize processor-charge role decoding and preserve error context.
- [x] DU-006: characterize zero/positive lifecycle amount semantics and record disposition.
- [x] Run focused regression tests and all required workspace, SQLx, formatting, Clippy, and public-API checks.
- [x] Review the final delta, record evidence, close this plan, and audit the active goal.

## Surprises & Discoveries

The worktree already contains staged review fixes and Jig records. Keep these intact and do not stage, commit, push, or reset them. Previous tests exposed Docker per-container startup flakes; use the supported POSTGRES_TEST_ADMIN_URL fixture backend with a disposable, locally cached PostgreSQL 18 image.

## Decision Log

Use narrow private helpers, not public mega-types or new generic traits. Keep public errors, formatting, serialization, nominal type identity, capability consumption, transaction ownership, SQL lock order, timeout values and retry limits unchanged. Preserve the provider-free-only PoolTimedOut allowance and the host-charge cleanup's narrower lock-only classification. No schema modification is planned.

## Outcomes & Retrospective

DU-001 through DU-005 implemented with focused regression coverage. Contract, workspace tests (including PostgreSQL 18 integration tests), SQLx metadata, formatting, Clippy, and default/all-feature public documentation and doctests passed. Native and Opus reviews completed; no actionable defect remains after triage and the follow-up fixes.

## Context and orientation

- PostgreSQL attempt locks are duplicated in attempts/shared.rs, discounts/persistence.rs and grants.rs.
- Retry predicates are duplicated in subscription_billing_service.rs, enrollment_application.rs and processor_charges/storage.rs.
- Core audit-reason constructors are in operator_review.rs, gateway/lifecycle.rs and subscription/grant.rs.
- Two-timeout SQL appears in attempts/shared.rs, discounts/persistence.rs, enrollment_application.rs and lifecycle_reconciliation.rs.
- Processor-charge role parsing appears in processor_charge_persistence.rs and processor_charges/storage.rs.
- DU-006 compares core attempt.rs lifecycle validation with lifecycle_reconciliation.rs and schema/v5/install.sql.

## Plan of work

Work on one validated cluster at a time. Strengthen meaningful regression coverage around shared behavior and intentional differences, extract the smallest private implementation, migrate its callers, and run focused tests. Investigate DU-006 with source evidence and a state/amount truth table; do not change persisted contracts merely to make predicates identical.

## Validation and acceptance

Use existing offline Rust dependencies and a disposable PostgreSQL 18 server. Each recommended helper must have one canonical implementation and no leftover duplicated invariant. Public types and shipped schemas must remain unchanged. Run scripts/jig work check for contract, workspace tests and SQLx; scripts/jig check fmt; scripts/jig check clippy; scripts/check-public-api.sh. Review source after checks and use work evidence/gates/finish to record completion. A failed test is investigated before broadening changes.

## Idempotence and recovery

All changes remain local and unstaged. Do not restore or overwrite staged files. Stop only the disposable database created for this task. Keep .agent/state JSONL append-only. If a cluster has unresolved semantic differences, document them and preserve the boundary rather than forcing a merge.

## DU-006 investigation and disposition

Keep these validators separate for this consolidation. The source establishes
that they are not interchangeable, but does not establish a safe new contract
for zero-value financial lifecycle reports. This is not a validated duplicate to
merge. No persisted-state or public-constructor policy changes are made here.

For capture=0, the core attempt validator accepts Settled(None) and
Chargeback(None), rejects either with a positive refund, and rejects Refunded.
Incoming reconciliation rejects all Settled, Refunded, and Chargeback states at
zero. Schema v5 rejects settled/refunded zero rows but accepts chargeback with
zero refund. Stored reconciliation rejects that latter schema-permitted row.
For positive capture, the three representations agree after translating an
absent cumulative refund to the stored zero sentinel: settled requires refund
less than capture, refunded requires equality, chargeback permits up to capture.
Unknown, pending settlement, and voided carry no cumulative refund.

Evidence: core attempt.rs::PaymentAttempt::new admits zero only for payment-method
updates; attempt.rs::lifecycle_amount_is_valid validates optional evidence.
Postgres lifecycle_reconciliation.rs::lifecycle_transition validates the stored
aggregate before incoming evidence and again after merging cumulative refunds;
its candidate query includes approved payment-method updates. Schema v5's
billing_payment_attempts lifecycle CHECK uses a nonoptional zero refund sentinel.
The NMI adapter lifecycle.rs::lifecycle_from_report can interpret condition labels
without a positive capture. These are real representation/admission differences,
not grounds to loosen incoming financial evidence checks or tighten historical
readers as a behavior-preserving refactor. A future policy change must explicitly
define zero-value verification lifecycle handling and account for existing
schema-permitted chargeback rows before changing constructors or persistence.

DU-007 through DU-010 remain separate as recommended: their nominal types and
capabilities encode domain or ownership boundaries. No public types were merged.

## Verification evidence

- Focused row-lock, retry-policy, public reason, local-timeout, and role-decoder
  regressions passed. The timeout test checks both commit and rollback on the
  same connection; the lock test checks exact scope and release on rollback.
- `scripts/jig work check --plan-id plan_01M1RPZ07Z6TPBF4E2CW9DHYQD` passed
  contract, workspace test, and SQLx gates with PostgreSQL 18 via the supported
  shared-server harness. Batch receipt: receipt_01M1RQQD8YZ2WHR143ASEVXVZP.
- `scripts/jig check fmt` passed (receipt_01M1RQNZ4R6H5P3QPR9N8MCEKJ).
- `scripts/jig check clippy` passed (receipt_01M1RQP91D3DX9ECPVXPXRSCKN).
- `scripts/check-public-api.sh` passed for default and all features, including
  warning-denied documentation and doctests. `git diff --check` passed.
- Native review verified every extraction against its prior caller behavior:
  lock ordering and scope, transaction reuse, retry error extraction and pool
  exclusions, Unicode character bounds, error precedence and formatting, bound
  timeout values and transaction locality, and exact durable role labels.
  Targeted searches found one canonical implementation of each extracted rule.

## Comprehensive review loop

First Opus pass completed successfully and confirmed behavioral equivalence of
all five extractions. Its actionable retry-policy test gap is fixed: each of the
four workflow predicates now tests its own accepted and rejected database codes,
including rejection of connection failure 08006; nested attempt errors are tested
where supported. A shared test-only database-error fixture supplies codes without
making the tests inherit the production acceptance set.

Also retained the transaction-typed initial-row-lock entrypoint, with one
connection-level SQL implementation for the existing host-connection discount
boundary. Removed the claim-only lock alias and repeated approval-reservation
conversions, and cleaned up changelog grouping and wrapping.

Two comments do not warrant a behavioral change: the existing processor-charge
persistence error text is intentionally preserved by this compatibility refactor;
and the summary reducer's possible future misuse is speculative, since every
production application passes through record_application and applies the correct
newly-staged count. Native review confirmed both paths. The second Opus pass and
repeated gates are running against the revised source.

## Final review and completion audit

Second Opus pass completed and explicitly classified its remaining findings as
quality issues rather than defects. Native review confirmed no actionable defect.
The suggested further PMR evidence-loop consolidation is outside the validated
five clusters and does not identify a changed behavior; preserving its separate
workflow remains safe. The typed row-lock forwarding function is intentional:
it preserves the transaction requirement for existing callers while sharing SQL
with the already connection-based discount API. The suggested zero-value test
for terminal_approved_progression concerns a state unreachable through its only
caller: ApprovedParkingReservation admits only positive charged flows. It is not
a regression gap in the narrowed PMR boundary. These comments do not require
another production change. The release-fixture verification gap was closed by
running `bash scripts/tests/check-release.sh` successfully.

Final verification after the follow-up changes:
- All four retry-policy tests passed against workflow wrappers, not just the
  shared SQLSTATE helper.
- Contract, workspace tests, and SQLx passed again; fresh batch receipt
  receipt_01M1RR9DDWK2EBRX09S3T88V8B.
- Formatting passed: receipt_01M1RR7FXKD1YARQGY2GGMH135.
- Clippy passed: receipt_01M1RR8EDJHF4T1Q5F1PA64BCW.
- Default/all-feature public-API documentation and doctests passed again.
- Release wrapper argument tests and final diff whitespace validation passed.
- All five actionable clusters have one shared implementation and migrated
  callers; public identities, errors, retry authorization, transaction ownership,
  lock ordering, timeout policy values, and shipped schemas are preserved.
- DU-006's source-backed boundary investigation is recorded above; it was not a
  validated consolidation or confirmed billing defect. DU-007–010 remain separate.
- Changes remain uncommitted and unstaged over the user's pre-existing staged work.
