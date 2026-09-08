# Restore approved saved-card display for 0.5.3

The subsequent review fixes, expanded test matrix, and final review/commit
sequence are tracked in [the follow-up plan](plan_01M1Y0JSDC7MTZN0SDHXQXKT5J.md).
The validation counts below describe the initial implementation checkpoint.

This living ExecPlan follows `.agent/PLANS.md` and implements Beads
`syrup-rail-20u`. Hosts will be able to fill missing saved-card display from one
read-only exact provider transaction query after an approval, including old
approvals. Billing portal reads will then show the safe descriptor. The host
authorizes the scope and subscriber and schedules retries; the library never
resubmits a sale or reapplies approval to refresh display.

## Progress

- [x] (2026-09-07) Verified `integration/0.5.3` starts at v0.5.2 / `9f30107`;
  claimed the issue and read all affected crate guides and threat model.
- [x] (2026-09-07) Fixed Classic `cc_type` and Classic/XML `cc_exp`; synthetic client and adapter regressions pass.
- [x] (2026-09-07) Added the standalone and service refresh APIs and documented host integration.
- [x] (2026-09-07) Seven PostgreSQL scenarios pass, covering portal display, whole financial-row preservation, failures and retries, ownership, locks, cancellation, write rollback, replacement, and scrubbing.
- [x] (2026-09-07) Audited release boundary and passed all configured gates with the full serialized workspace suite; closed the work record and issue.

## Surprises & Discoveries

Classic and XML descriptors use scalar collectors that reject conflicting
occurrences; expiry currently bypasses parsing entirely. The existing exact
query checks requested transaction identity and the NMI adapter already maps
safe descriptors. Approval replay exits before method upsert, so it cannot
repair display. Approval and scrub currently use different advisory lock keys;
refresh must enter both domains before locking rows.

## Decision Log

Use the existing provider-neutral exact-query port, with NMI parsing remaining
in the client/adapter. Add a PostgreSQL operation and service convenience
entrypoint rather than changing the approval result or existing gateway trait.
The request identifies an authorized scope, subscriber, and approved attempt;
provider transaction, account, configuration, and method come from canonical
rows. One call issues at most one query with a fixed timeout, and no automatic
retry. Fetch before opening the write transaction. Recheck the same eligible
candidate under the approval and scrub domains and row locks before merging
only absent safe card fields. Existing conflicting fields reject the refresh.
Only the latest approved attempt for the method is eligible, so an older
transaction cannot refill a vault reference reused by a later approval.
Snapshots of method and subscription timestamps reject a replacement during
I/O, including switching away and back. Scrubbed references and disabled
methods are ineligible. Financial evidence remains immutable.

Work stays on the tag-derived branch; no schema file, release tag, deployment,
or host database is changed. See `docs/security/threat-model.md`: this uses
existing authorization and lock boundaries, adds no secret or security system,
and protects mutable display against stale provider responses. Host staging
retest and the four host backfill calls remain separate follow-up work.

## Outcomes & Retrospective

Implementation is complete. Focused client, adapter, and PostgreSQL tests pass.
Formatting, source-size, contract, schema immutability, SQLx, Clippy, public API
documentation/doctests and the full workspace suite have passed. The full suite
used `RUST_TEST_THREADS=1`; the unrestricted parallel run had PostgreSQL fixture
failures, so unbounded parallel execution is not claimed as verified. Synthetic regression fixtures are not captured staging sale
responses. Host staging backfill and publishing the future 0.5.3 release are
separate follow-up operations, not performed by this implementation task.

## Context and Orientation

`crates/syrup-rail-nmi-client/src/client/response/{form,xml}.rs` parse untrusted
provider responses; `client/text.rs` has safe last-four and expiry parsers.
`crates/syrup-rail-nmi/src/adapter.rs` maps these into core evidence without SQL.
`crates/syrup-rail-postgres/src/enrollment_application.rs` owns approval method
upserts. `deletion.rs` scrubs mutable method/attempt fields while retaining
charges. `billing_portal.rs` reads local methods. Schema v4 already contains
all required columns and is shipped and immutable.

## Plan of Work

First add the Classic alias to its existing scalar collector and parse expiry
only after resolving consistent scalar evidence in both formats. Keep payment
decision logic and full-PAN rejection unchanged. Regression tests exercise
aliases, duplicate and conflicting occurrences, invalid fields, and decisions.

Next create `crates/syrup-rail-postgres/src/payment_method_metadata.rs` for the
public request/outcome/error and refresh function, private candidate SQL,
timeout, identity checks, lock ordering, and non-erasing descriptor merge. Add
explicit facade exports and a `SubscriptionBillingService` convenience method.
Use the host resolver with canonical account/configuration/provider identity;
reject a mismatching resolved gateway before I/O. Revalidate current method
ownership, approved attempt transaction/reference, and subscription pointer
after query, and require exactly one affected method row before reporting
success. Do not write attempts, charges, subscriptions, vault references,
access, amounts, renewal timing, or events. Document how to invoke the same
entrypoint after newly committed approval and for explicit repair.

Finally test against real isolated PostgreSQL databases using the existing
`test_support::TestDatabase` harness. The seven scenarios live in
`crates/syrup-rail-postgres/src/paid_trial_dunning_tests/card_metadata.rs` and
its `fixture.rs` and `races.rs` children; they reuse actual paid-trial approval
and saved-method replacement operations. Provider fakes must count queries and
panic on mutation. A blocked-query fixture permits replacement/scrubbing on a
second connection and proves no lock spans I/O. Snapshot financial rows before
and after refresh, exercise wrong ownership and resolver/evidence identities,
missing or conflicting descriptors, timeout, repeat calls, and later retry.

## Concrete Steps

Run commands from `/home/aa/Documents/syrup-rail`. Start a Jig work record with
this file as its body. Run focused client and metadata tests during edits;
database tests launch the repository PostgreSQL harness. Run
`scripts/jig check fmt`, `scripts/jig check clippy`,
`scripts/jig check contract`, `scripts/jig check sqlx`, and finish backend
verification with `scripts/jig check test`. Connect receipts with
`scripts/jig work check`, `work evidence`, `work gates`, and `work finish`.
Review `git diff` and `git diff v0.5.2 -- crates/syrup-rail-postgres/schema`;
the latter must be empty. Do not import commits from master.

## Validation and Acceptance

An approved saved method with empty descriptors followed by an exact query
must produce Visa / 1111 / 10 / 2029 in the billing portal. Invalid or duplicate
metadata cannot change payment decisions. Missing data, timeout, malformed or
conflicting evidence must preserve existing data and all financial snapshots;
a later retry may succeed without a sale. Wrong scope, subscriber, account,
transaction, replaced method, and scrubbed data must never be written. All
configured gates must pass; inspect their coverage rather than relying only on
the exit status. Review all changed docs and public API exports.

## Idempotence and Recovery

Fill-only refresh is repeatable and can be retried separately from financial
commands. A failed or canceled query leaves no write transaction open. A failed
write rolls back; a canceled write drops its owned transaction. Preserve user
edits in `.beads/issues.jsonl`. Update this plan with actual evidence, close the
issue only after verification, and flush Beads with `br sync --flush-only`.

## Interfaces and Dependencies

Use existing `GatewayResolver`, `ResolvedGateway::query_transaction`,
`GatewayQueryRequest`, and `GatewayPaymentDescriptor` without a new provider
trait or dependency. PostgreSQL exports `RefreshPaymentMethodMetadata`,
`PaymentMethodMetadataRefreshOutcome`, `PaymentMethodMetadataRefreshError`,
and `refresh_payment_method_metadata`; the service delegates using its pool
and resolver. All public documentation describes bounded retries and host
authorization. No schema migration or host table writes are part of this work.

Revision 2026-09-07: recorded implemented APIs, actual test ownership, latest-attempt
supersession and both legacy lock domains. HTTP-error evidence detection now
recognizes the added card aliases too, preserving conservative submission
certainty. Gate freshness will be rechecked after all source and doc edits.

Verification update 2026-09-07: the unrestricted parallel workspace run passed
core (110), NMI adapter (24), and NMI client (221) tests, but PostgreSQL fixture
failures affected 115 tests. The full suite is being rerun with
`RUST_TEST_THREADS=1`, preserving all test cases and matching the repository
locked-test execution policy. An isolated temporary copy with unchanged v0.5.2
parsers failed all four new client regressions; the combined conflict/PAN test
fails first on the ignored cc_type alias, so this is not evidence of a baseline
PAN-parser defect. No worktree sources were changed for that negative control.

Completion audit: criterion 1 is covered by the
four `client::tests::card_metadata` tests plus the NMI adapter transport test;
core/adapter/client suites passed in the workspace run. Criterion 2 is covered
by an actual paid-trial approval with empty display, service refresh, and exact
portal assertions in `approved_empty_card_metadata_refresh_populates_portal_without_financial_changes`.
Criterion 3 uses full JSON snapshots of every attempt, processor charge and
subscription column, plus method identity snapshots and event counts, across
missing, failed, conflicting, repeated and canceled queries. Criterion 4 uses
wrong scope/subscriber/attempt and all resolver identity dimensions, mismatch
transaction/vault responses, canonical replacement with different and reused
vault references, actual canonical scrubbing, and completion of the competing
write while provider I/O is deliberately blocked. Both legacy domain locks are
independently held to prove refresh respects them; suppressed writes are errors.
Criterion 5: `git diff --exit-code v0.5.2 -- crates/syrup-rail-postgres/schema`
and the untracked-schema-file check were empty; `git log v0.5.2..HEAD` was empty.
Manifest and lockfile comparisons to v0.5.2 were also empty. Formatting, Clippy,
public API docs/doctests, source-size and schema-immutability checks passed.
Final plan-bound SQLx and full serialized test receipts passed in batch
`receipt_01M1XZ3YMQPZDH2R0F6JSG7GZY`: SQLx receipt
`receipt_01M1XYJ12FJZBB8B7H5W12TCNH`, full test receipt
`receipt_01M1XZ3YK0WA4TKHXRM1Q7BGD4`. Contract receipt
`receipt_01M1XY5XYSWMCY2FVD5RWEVXSQ` also matches the final worktree.

Final verification 2026-09-07: `RUST_TEST_THREADS=1 scripts/jig work check
--plan-id plan_01M1XX8A4BBTT18ZRC7HRH9B87 --tool jig.sqlx_check --tool jig.test`
completed successfully. It ran the complete configured workspace test command
without filters or ignored-test changes. The earlier parallel failure remains
recorded, and no unrelated source or test changes were made to hide it.

Work and Beads closure completed after fresh required gates passed. The working
tree remains uncommitted on `integration/0.5.3`; no tag, publication, host
database write, or master merge was performed.
