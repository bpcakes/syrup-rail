Address the two accepted low findings from the v0.5.2 branch review. Reuse transaction-local database timeouts for candidate/cooldown reads and end that transaction before gateway resolution. Add deterministic blocked-read and connection-release regressions plus generic/actual-plan coverage with a dense method history. Compare master, preserve public APIs and schema v4, run focused and full backend gates, then comprehensive focused and branch reviews before committing. Exclude the pre-existing mixed Beads export.

## Completion evidence

The candidate/cooldown reads share a short transaction with the existing 250 ms lock and 5 s statement limits. It ends before gateway resolution and provider I/O. The private cooldown reader accepts either the existing pool caller or the transaction connection. Public APIs, dependency versions, and shipped schema artifacts are unchanged by this follow-up; pinned master 6fccde09 does not contain the metadata operation or these fixes.

New regressions cover both DDL-blocked reads, SQLSTATE 55P03 before any provider I/O, same-connection reuse and timeout restoration, and connection/table-lock release before host resolution and during provider I/O. Removing the new timeout setup made the blocked-read regression fail as intended, then the source was restored. Generic and executed plans cover 4,096 methods with 1,025 approvals concentrated on one method. Latest-attempt queries visited that method's 1,025 rows and took 3.709 ms unlocked / 3.848 ms locked locally; superseded-attempt queries visited one row. These measurements are fixture results, not a production capacity guarantee.

Validation: formatting, Clippy, agent guides, Rust file limits, schema immutability, and all required Jig gates passed. The final serial full suite passed 636 tests plus six doctests, followed by SQLx verification. The initial default parallel invocation failed 171 tests solely with PostgreSQL container-startup timeouts; rerunning with RUST_TEST_THREADS=1 passed. Initial new-test failures were fixture-name length and EXPLAIN numeric decoding mistakes, corrected before the final checks. Full gate output: /tmp/syrup-read-fix-gates-serial.json; freshness evidence: /tmp/syrup-read-fix-final-evidence.json.

## Comprehensive review disposition

One focused round and one full-branch round completed with independent Claude and Codex reviewers. No further 0.5.3 code correction was established after source adjudication. Codex reported no actionable findings in either round.

Claude's focused timeout concern describes the same query already bounded during write revalidation; bounded host retry and capacity rehearsal are documented. Its four-connection server-limit hypothesis is disproven by the pinned harness: the budget is a database-lease semaphore and the managed server uses max_connections=300. The sparse generic-plan assertion passed as well as the dense fixture.

In the full-branch pass, RowNotFound remains an investigation error as documented; a concurrently deleted row cannot disappear after FOR UPDATE, so returning ChangedDuringQuery for a zero-row UPDATE would hide an invariant or host-trigger failure. Provider brands pass through GatewayDiagnostic's existing raw-card-data sanitizer. Display eligibility for retained canceled/unpaid subscriptions is distinct from collection authority and is covered explicitly.

The history-index option is deferred to syrup-rail-bn4 for capacity-driven next-schema work; the public decorator/capability redesign remains syrup-rail-cis for 0.6.0. No unrelated master fix or schema redesign was imported into this patch.

Open questions: consuming-host invocation/decorator forwarding and real NMI staging acceptance remain host work (syrup-rail-20u).

Test gaps: larger production histories, full decorator conformance, direct pre-read statement-timeout/cancellation and nonzero baseline cases, and commit-acknowledgement fault injection remain nonblocking limits. Provider fixtures are synthetic.

Review notes:
- Reviewers requested: Claude, Codex.
- Claude review: completed in both rounds; file access restricted.
- Codex review: completed in both rounds.
- Focused scope fingerprint: verified, 1f4b7a037f2bbae291e8797442f8a042880b0be4a170bc5a678a20aaac94170f.
- Full-branch scope: v0.5.2 (9f30107e) through candidate 6d8c944d, all application/crate-documentation files matching the tested working tree byte for byte.
- Full-branch scope fingerprint: verified, 403ec89ae78239a6f4ae09ded2b356c14d77714d13f504c33977750c7bea7cac.
- Evidence coverage: reviewer-attested; 32/32 pages reported reviewed with valid receipts. Receipts attest page access and claimed coverage, not review quality.
- The mixed Beads export was excluded from reviewer checkouts and application staging.

Application commit: bb68a30. Its complete Git tree matches the reviewed candidate 6d8c944d; the verification record is committed separately. Capacity-index detail was added to syrup-rail-bn4 comment 16 and exported; the mixed Beads export remains unstaged.
