# Preserve approval information at its source

This living ExecPlan follows `.agent/PLANS.md` and `docs/security/threat-model.md`. It continues the uncommitted schema-v6 work in plan_01M1RVVQ0GGTQ4YR4DW21D5BZ0 and Bead syrup-rail-zhe. Do not commit or push without a user request.

## Purpose

Approval signals must survive decision conflicts, discarded duplicate fields, missing identities, storage, and local reconciliation notes. A submitted attempt with no saved provider response must still permit independently authorized manual closure after negative exact queries. An indeterminate provider error must never certify absence merely because it uses the error channel.

## Progress

- [x] Research source contracts, reproduction probes, error transport, and official NMI response-code documentation.
- [x] Derive approval signals before lossless raw decision reduction and map them without reparsing in NMI.
- [x] Make core review policy classification-only, normalize empty legacy construction, and derive error evidence from mutation certainty.
- [x] Adjust unreleased v6 defaults/backfill and preserve raw evidence during negative exact queries.
- [x] Add cross-boundary recovery, error, duplicate/status, migration, and replay regressions.
- [x] Run formatting, Clippy, public API, SQLx, contract, and full workspace tests (repeat after subsequent repairs).
- [x] Repeat independent native and Claude Opus comprehensive reviews, fix actionable findings, and finish with a clean review.

## Surprises & Discoveries

The raw decision reducer sees status and every duplicate occurrence, but PaymentOutcomeParts only retained reduced status and three selected raw fields. Classifying in the adapter after this reduction necessarily loses information. The local exact-query writer also overwrites provider response text with local notes. Unclassified formerly recomputed its guard from those text fields, so notes strengthened the guard and scrubbing could weaken it. The mutation error helper accepted only diagnostic text, erasing the distinction between proven non-submission and indeterminate receipt.

Official NMI transaction-processing documentation identifies 100 as approved and distinguishes processor/communication errors from a decline. It does not establish that an arbitrary error proves no financial effect. Source: https://docs.nmi.com/reference/transactions-processing . The source reproductions establish the repository-specific paths; provider truth and host operator authorization remain outside this change.

## Decision Log

Compute conservative structured and text approval signals before raw fields are discarded, using the same decision classifier as authoritative reduction. Carry an explicit raw-client summary through PaymentOutcomeParts; the NMI adapter only translates enum variants. Keep authoritative payment status separate.

Unclassified always blocks the manual no-financial-effect exit; raw field presence must no longer influence policy. Empty ProcessorEvidence construction represents absence of observed approval signals. New database attempts likewise default to absent. Backfill only entirely NULL retained attempt evidence as absent. Legacy local query notes may conceal overwritten provider text and remain unclassified, as does other retained evidence; charge and attestation history is not reclassified. V6 is unreleased in this worktree, so adjust its install and upgrade together; v1-v5 remain immutable.

Derive error evidence from GatewayMutationError itself. Proven non-submission yields absent; indeterminate receipt yields unclassified even if the diagnostic is empty. Preserve existing nonempty provider response text when exact-query notes are recorded so error evidence is not erased. Empty text may be replaced by a local note without changing classification. No core code interprets provider text.

## Outcomes & Retrospective

Completed after six successful Opus review passes plus independent native reviews (one initial quota failure and one canceled premature launch were not counted as completed reviews). Final merged review has no actionable findings. The final Opus pass's remaining comments concern documented migration restrictions, conservative empty-text/descriptor semantics without a demonstrated failing path, and intentional immutable replay/default behavior. Staging-only concerns are outside the complete-working-tree review scope; no commit was requested. Opus performed static inspection; local commands supply the test evidence.

Formatting, Clippy, public API checks, the full workspace test suite, contract checks, and SQLx verification passed. Final gate batch: receipt_01M1S4PC85GSFFBS0N3530B3D1; tests receipt_01M1S4NPTH29GHX0GCWFDRZ6WM; SQLx receipt_01M1S4PC4HENDKN20YV7PHKMZV. Intermittent Docker PortNotExposed startup failures in disposable fixtures required retries; the final complete run passed. Fresh and upgraded v6 catalogs still match fingerprint 0x37feb10042e9893a. Shipped v1-v5 artifacts remain untouched.

The root cause was structural information loss and ambiguous ownership, amplified by missed persistence branches and inaccurate fixtures. Approval signals now originate before raw reduction, travel as explicit evidence, and remain independent of mutable diagnostic text. Error certainty is projected before destructuring. Missing/unidentified observations cannot certify absence or erase prior signals. Local query writers preserve nonempty evidence, and the migration never infers safety from legacy notes that may conceal overwritten text. The deliberate rollout cost is that retained unclassified attempts without a matching provider record stay open; the v6 guide requires investigation before cutover when that restriction is unacceptable.

## Context and Plan of Work

Raw response parsing lives in crates/syrup-rail-nmi-client/src/client/response/{common,form,json,xml}.rs. common.rs owns scalar occurrence collection and decision reduction. A raw-client approval enum and merge rule carry conservative information into responses.rs. Move the existing approval-text expression to that owner; remove the NMI adapter's duplicate code/state parser.

Core ProcessorEvidence and GatewayMutationError live in crates/syrup-rail/src/{gateway.rs,gateway/port.rs}. PostgreSQL application modules currently share mutation_error_evidence; replace that text-only helper with the core error-owned projection. attempts/transitions.rs already persists classification atomically. reconciliation.rs owns negative exact-query notes and must retain the provider bundle. schema/v6 owns current defaults, upgrade, and matching runtime fingerprint.

## Validation and Acceptance

From /Users/aa/Documents/syrup-rail, run focused raw-client, NMI, core, PostgreSQL recovery/error and schema tests. Assert status-only approval and conflicting duplicates retain signals without promoting Unknown to Approved; ambiguous empty evidence stays protected; local notes do not change the guard; indeterminate errors cannot certify absence; fresh and upgraded catalogs match. Run scripts/jig check fmt, scripts/jig check clippy, scripts/check-public-api.sh, and RUST_TEST_THREADS=1 scripts/jig work check --plan-id plan_01M1RZ5HNX397HQ3T3YCEQVAWS. The work gate includes contract, full workspace tests, and SQLx. Use work evidence/gates/finish when complete.

Run comprehensive review on the whole working tree with native Codex and the cc companion using --scope working-tree --model opus; wait for results, validate/deduplicate, and repeat repairs and relevant checks until no actionable findings remain. Do not turn stylistic preferences or intentional immutable-history compatibility into defects.

## Recovery and Compatibility

Only disposable local test databases are mutated. Hosts stop all writers and apply v5-to-v6 once in one transaction. Failed cutover rolls back to v5; after commit roll forward. No provider requests with real credentials, commits, pushes, or application deployment are authorized. Preserve existing staged edits and do not rewrite shipped artifacts.

Revision 2026-09-05: source-first structural diagnosis and initial implementation decisions.

Review iteration 3: Opus identified text-only Classic pending decisions, the payment-method stale auto-failure branch, and loss of prior signals during identity-quarantined reconciliation. Reproductions confirmed all three. Missing decisions now remain unclassified at the shared reducer (empty local reservations remain absent). The payment-method branch checks typed protection, preserves provider text, and no longer invents a provider condition. Unidentified observations join conservative signals; only an accepted transaction identity permits replacing classification. Added Classic, repeated-negative-query payment-method, and unidentified-to-identified reconciliation controls. Focused regressions passed after shortening test-project names to the harness limit. Repeat complete reviews and gates on this iteration.

Review iteration 4b completed after all iteration-4 gates passed. Accepted the migration-history finding: old local notes could have overwritten provider evidence, so the backfill must leave them unclassified. This supersedes the earlier local-note exception; only all-NULL retained observations become absent. Empty diagnostics are omitted from new error evidence and negative-query writers fill empty text while preserving nonempty text byte-for-byte. Corrected processor-duplicate/indeterminate/unknown application fixtures to match the adapter's Unclassified summary. Kept intentional immutable-history replay and v5-column-order install layout; replaced the misleading upgrade header in the install file. Repeat focused migration/error checks, full gates, and native/Opus review.

Review iteration 5: Opus found no actionable production correctness defect. Rejected staging-only concerns (the authorized scope is the complete working tree; no commit requested), a speculative rustfmt failure (fmt passes), and documented compatibility/identity/history design notes. Native full tests exposed one stale expectation after correcting the indeterminate fixture: the old test expected manual closure. Updated it to assert retained ReviewRequired/Unclassified state, matching the implemented policy. The other failure was Docker PortNotExposed during fixture startup. Added the deprecated compatibility exception to the core guide. Final focused check and complete native/Opus review plus gates follow.

Final revision 2026-09-05: merged native/Opus review clean, complete gate retry passed; implementation left uncommitted for the user.
