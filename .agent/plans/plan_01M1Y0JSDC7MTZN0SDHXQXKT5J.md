# Harden saved-card metadata boundaries

Follow-up to syrup-rail-20u and its independent Claude/Codex review. Work on
integration/0.5.3 against v0.5.2 (9f30107); preserve all schema artifacts and
financial evidence. Follow docs/security/threat-model.md and crate guides.

## Progress

- [x] Research open questions and inspect brand writers/readers.
- [x] Fix boundary mismatches and local error/parser defects.
- [x] Cover brand persistence, partial retries, diagnostics, lifecycle attempts,
  concurrent refreshes, and preservation of financial rows.
- [x] Pass required Jig gates with fresh evidence.
- [x] Review working tree (three cycles completed).
- [x] Close this verified implementation/precommit-review checkpoint before
  staging changes, while the required Jig receipts still match the worktree.

After this checkpoint, commit the reviewed task changes and perform the user's
separate full-branch review phase (up to three cycles). Fix actionable findings
and verify each revised snapshot. Closing this implementation record does not
claim that the later commit or full-branch review has already happened.

## Surprises & Discoveries

Refresh used a presentation label where approval stores provider evidence.
Unknown brand classification was treated as positive mismatch evidence. The
Fowler scanner only flagged the private SQL row DTO; that is a non-finding.
The previous default parallel DB suite failed; its serialized rerun passed.

## Decision Log

Research: https://docs.nmi.com/reference/query documents an exact transaction
query and an example without vault linkage, and does not promise visibility
latency. Return NotFound for an absent observation and document bounded host
retry policy; Unchanged does not promise complete display. Allow only the
MissingPaymentMethodReference diagnostic with approved exact transaction
identity and current durable linkage. Reject all other diagnostics. Keep
mode verification at financial admission: this operation has one read-only
query and no financial authority. Hosts own metrics and retry scheduling.

Apply Extract Function to the known-brand predicate so both sides compare the
same domain values. Store bounded provider brand text, matching approval.
Do not add a new storage codec, public wrapper, trait, dependency or schema.
Use SQLx transaction ownership/drop for error rollback so cleanup cannot replace
the original error. Functional changes are explicitly separate from mechanical
extraction. Add regression tests before each correction where practical.

## Outcomes & Retrospective

Implementation complete. Twelve focused PostgreSQL scenarios and five parser
regressions pass after a fresh build; formatting and Clippy pass. Before the fix,
three new database regressions independently failed on Amex portal projection,
unknown-brand merge rejection, and missing-reference diagnostic rejection. The
mixed-case alias regression also failed before its correction. The unchanged
seven-scenario baseline passed. Full workspace gates passed: 110 core, 24
adapter, 222 client and 252 PostgreSQL tests, plus example and doctests.
Jig batch receipt_01M1Y1YZGNR9KWZBZQST3KERBG binds passing contract, SQLx and
test receipts to the first corrected source fingerprint. Later review changes
require fresh gate evidence before committing.
No publication or host backfill requested.

After round 1 corrections, all 15 focused PostgreSQL scenarios, seven client
parser regressions and the adapter regression pass. The full workspace suite
passes with 110 core, 24 adapter, 224 client and 255 PostgreSQL tests, plus
examples and doctests. Formatting, Clippy, contract and SQLx also pass.
Batch receipt_01M1Y45S24ZAQ1RJ5PQD179BWG binds the final three required gates
to the corrected worktree; all required gates are fresh and passed.

## Review cycles

Working-tree round 1 completed with independent Claude and Codex reports and
matching complete fingerprint 36e942cda9e2fd2e2999d5610a74df368ae223f9ce7242bc5547afbd7e800821.
Codex found that legitimate approved renewals without echoed vault evidence
were ineligible and superseded the older repair candidate. The correction uses
their durable method/subscription linkage while rejecting conflicting references.
Claude identified lifecycle-state exclusion, duplicated lock identities,
unknown observed brands, and XML case-equivalent duplicate inconsistency.
The new regressions failed before these corrections; the lock extraction
compiled independently as a behavior-preserving step. Tests now acquire locks
through their actual owners, and renewal-without-reference participates in
the real scrub/replacement race matrix.

The lifecycle decision follows the exact-query contract and NMI's documented
`canceled` condition (a void): durable approval supplies payment authority,
while Approved or diagnostic-free Unknown observations may supply display.
Explicit decline/failure and all diagnostics except a missing optional vault
reference remain rejected. Unknown brand text is bounded/sanitized by
GatewayDiagnostic and retained only as provider evidence; presentation is Other.

Retain timestamps: removing them would admit an away-and-back replacement
whose identity fields return to their original values. Conservative no-ops and
host retry limits are documented. Legacy scrub/approval lock keys diverge in
the pinned v0.5.2 release; preserve those deployed keys, share their owner
helpers, and enter both in refresh. Historical tracker fixes on unreleased
master are outside this release boundary. Non-exhaustive Unchanged semantics
and account activation remain documented host policy rather than new financial
admission. Approved resolved_at NULL is prohibited by the existing schema.

Working-tree round 2 completed with matching complete fingerprint
8392ccb5c7287f5a5e83ef4f027e98231cf30f03db4da44cc27a9e0019d54f1b.
Codex reported no actionable findings. Claude found missing durable cooldown
integration, discarded partial expiry, and conflated initial/in-flight rejection.
Three regressions failed on the prior behavior. Extracted the existing cooldown
reader and flag interpretation without behavior changes and compiled separately;
refresh now shares that reader and the established provider-throttle writer.
Active cooldowns stop before resolution; query rate limits extend provider
cooldown without changing attempts, charges, or subscriptions. Each valid expiry
component fills independently. ChangedDuringQuery is separate from Ineligible.
The first 18-scenario focused run passes after these corrections.

Retain FOR SHARE OF g: candidate equality does not protect configuration changes
between locked revalidation and commit. The bounded write transaction holds this
lock only after I/O. Add configuration-rotation and write-cancellation regressions.
Reject unconditional COALESCE in approval upserts: an approved replacement can
reuse a vault reference for a different card, so stale display must be invalidated.
Document and test that a later exact refresh repairs that latest approval while
the older command stays ineligible. End-user admission remains host-owned and
is now explicit in the operation's rustdoc as well as README. Timestamp-only
changes deliberately produce ChangedDuringQuery and have direct coverage.
Non-renewal approvals require vault evidence at upsert. Schema constraints reject
malformed provider keys/noncanonical transaction IDs; normal writers additionally
validate reference length and raw-card rejection. Corrupt canonical identity
errors remain defensive coverage rather than normal workflow scenarios; do not
disable schema invariants merely to fabricate them.
Versions remain 0.5.2/Unreleased until the separate release step; schema v4 and
Cargo manifests/lockfile remain unchanged.

Residual integration coverage: parser and adapter loopback tests use synthetic
NMI responses, while PostgreSQL acceptance tests compose core outcomes with real
approval workflows. This does not prove the original staging provider response
shape; a live host staging check remains separate. No NMI dependency is added to
the provider-neutral PostgreSQL crate. Cancellation at commit acknowledgement
remains potentially ambiguous, but display refresh can be retried independently
and never changes financial approval.

All three additional race scenarios pass, including cancellation after PostgreSQL
reports the refresh write connection waiting on a row lock. The initial test
synchronization failed because pg_stat_activity truncates long statement text;
the corrected oracle observes the dedicated connection's actual lock wait.
Formatting, Clippy and public API checks pass after the final source edits.
The final full gates run alongside working-tree round 3 on an isolated, frozen
copy of the same source. The review copy includes all original Git changes at
capture, with staging used there to avoid untracked-evidence size limits.

Working-tree round 3 completed with matching complete fingerprint
1a047f0b6e06d6929891a2c3657136b2be87598f6a841168ea43a7fd3272b285.
Codex found no actionable issue; Claude attested 30/30 evidence pages and found
that cooldown persistence failure erased the provider throttle classification.
Add RateLimitCooldownPersistenceFailed retaining both original errors, and a
failing database-trigger regression. The superseded full test run was stopped
after this finding so no gate could bind old test execution to revised source;
its interrupted receipt is not passing evidence. Run final gates after these edits.

The coarse GatewayError::RateLimited does not establish a query-only quota.
Keep the existing conservative provider cooldown policy, and explicitly document
that it pauses financial readiness and renewal dispatch across every account
with that provider key. Separate query quota storage would require a new schema
contract and unsupported provider certainty. Host limits remain mandatory.
Keep whole-observation conflict rejection and authorization-safe Ineligible;
document canonical-identity verification and normal replacement/re-approval as
the conflict remedy. Do not merge contradictory card evidence or clear fields
merely to bypass a conflict. Claude's suggestion that ordinary renewal calls
upsert_payment_method is unsupported: renewal.rs marks the attempt approved with
the existing method ID and does not call that helper or overwrite descriptors.
Mode checks cannot prove merchant identity; credential binding remains the host
resolver's contract, as for the existing financial paths. These questions do not
require a new provider call or user decision.

The three requested precommit review cycles are now used. Final corrections
receive focused and full verification before committing; the subsequent full
branch review phase reviews the complete final patch again.

Final implementation checkpoint: all 22 metadata scenarios are included in the
passing full workspace suite: 110 core, 24 adapter, 224 client and 262 PostgreSQL
tests, plus four example tests and all doctests. Final batch
receipt_01M1Y7DCWD0H45T0Z526C3S1YK binds passing contract
receipt_01M1Y6KDR20H520EB6AAFP9VM2, SQLx
receipt_01M1Y6NM0SR169FZJDV5JXCC6R and test
receipt_01M1Y7DCTPJ4WAPZ41M9QFAD47 to the final worktree. All required gates
are fresh. Formatting, Clippy and public API checks also pass. The focused
cooldown-persistence failure regression passes with both errors preserved.
This closes implementation and precommit review; the requested commit and
full-branch review phase follows separately.

## Execution and validation

Relevant code: postgres/src/payment_method_metadata{.rs,/storage.rs},
nmi-client/src/client/response/form.rs, and their card_metadata test modules.
Use real canonical approval/reservation helpers for replacement, renewal and
recovery tests; assert portal values, current method identity, full financial
snapshots, and bounded query counts. Synchronize race tests with notifications.
Run focused cargo tests, then RUST_TEST_THREADS=1 scripts/jig work check for
jig.contract_check, jig.sqlx_check and jig.test, plus fmt and clippy. Preserve
append-only state. Run context-free Claude and Codex comprehensive reviews of
each frozen snapshot, fix between rounds only, stop each phase when clean or
after three rounds. Branch scope pins v0.5.2 and requires a clean checkout.
Keep unrelated existing tracker edits outside the application commit.
