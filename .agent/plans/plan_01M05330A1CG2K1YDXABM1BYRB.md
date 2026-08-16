# Preserve structured billing-contact replay identity

The payment ledger currently collapses `BillingContact.first_name` and
`last_name` into one display name. Recent replay hardening then reused that
receipt-oriented projection as immutable request identity. Two structurally
different contacts can therefore compare equal even though the gateway adapter
sends distinct `first_name` and `last_name` fields. This plan fixes the owning
model: attempt snapshots and persistence retain the normalized structured
contact, while display names remain derived presentation.

The PostgreSQL schema v2 shipped with version 0.2.0 and remains immutable. The
fix therefore introduces a complete schema-v3 install artifact and a
forward-only v2-to-v3 upgrade rather than editing `schema/v2/**`. The security
and privacy baseline is `docs/security/threat-model.md`.

## Progress

- [x] Reproduce the non-injective contact projection and identify every
  token-bearing replay matcher and provider submission path.
- [x] Choose lossless structured persistence instead of a matcher-only patch,
  encoded-name workaround, or contact hash.
- [x] Extend the core attempt-contact snapshot with exact normalized first and
  last name fields while retaining its derived display name.
- [x] Add schema v3, a transactional v2-to-v3 cutover, runtime conformance, and
  SQLx metadata for lossless attempt-contact persistence.
- [x] Cover projection-collision retries for initial enrollment, recovery,
  payment-method replacement, and host charge.
- [x] Update migration, runtime-schema, deletion, and privacy documentation.
- [x] Commit each coherent slice and pass formatting, Clippy, workspace tests,
  SQLx, contract, and Jig work gates.

## Surprises & Discoveries

- `BillingContactSnapshot` is the value embedded in every durable
  `PaymentAttemptRequest`; once it becomes lossless, existing request and
  reservation equality already enforce the invariant without four new policy
  branches.
- Payment-method rows need only a display name and email. Only payment-attempt
  rows can resume a token-bearing provider request, so schema v3 changes the
  attempt projection without expanding stored-method retention.
- A v2 row cannot reveal its historical first/last boundary. The v3 upgrade
  preserves the old combined display name as a canonical `billing_first_name`
  and sets `billing_last_name` to null. Display remains unchanged; a prepared
  retry using a different historical split fails closed unless it uses that
  canonical upgraded structure.
- The first full workspace run exposed two legacy fixtures that correctly
  stopped at schema v2 but then invoked the current production attempt loader.
  Their v2 catalog assertions remain in place; the fixtures now complete the
  v2-to-v3 cutover before exercising current operational code.

## Decision Log

- Decision: Make the durable attempt contact lossless and keep display name a
  derived value. Rationale: replay equality and provider request construction
  then consume one authoritative value instead of compensating for a lossy
  receipt projection at each workflow.
- Decision: Add schema v3 and keep every file under `schema/v2/**` byte-for-byte
  unchanged. Rationale: schema v2 is materialized persisted state and cannot be
  silently redefined after release.
- Decision: Rename the attempt column `billing_name` to
  `billing_first_name`, add `billing_last_name`, and retain `billing_email`.
  Rationale: the stored values match the provider-neutral command shape and do
  not duplicate a separately persisted display projection.
- Decision: Do not add hashing, encryption, or a new secret. The database
  already retains the same normalized name and email values; preserving their
  boundary adds structure, not new contact content. Hashing would still leak
  equality, introduce collision/key-lifecycle concerns, and leave provider
  request reconstruction unsolved.
- Decision: Require a stopped-writer v2-to-v3 cutover. Rationale: v2 code reads
  `billing_name` while v3 code reads the structured columns, so mixed binaries
  cannot safely share the catalog.

## Outcomes & Retrospective

`BillingContactSnapshot` now owns lossless normalized first/last/email identity
and derives its display name, so every workflow's existing request equality
enforces the provider request boundary without duplicated matcher policy.
Schema v3 persists that shape for token-bearing attempts, upgrades legacy names
deterministically, and keeps payment-method display metadata unchanged. Fresh
v3, v2-to-v3, and v1-to-v2-to-v3 catalogs converge on the same fingerprint;
deletion scrubs both structured fields.

Projection-collision regressions cover initial enrollment, recovery,
payment-method replacement, and host charge. The final repository receipts are
`receipt_01M054MK899KRYHW4H07X5CM9J` (format),
`receipt_01M054NCC63JW7PX91DWPYC66A` (Clippy),
`receipt_01M054VRBPKT0Q237ZC8WXAYCW` (plan-bound full tests),
`receipt_01M054WXQC72F8XZPRP5138QJZ` (plan-bound SQLx), and
`receipt_01M054PX52SDX9EBADQJRNVAY7` (plan-bound contract), grouped by work
check `receipt_01M054WXRMV8DVTNFBM3JEBRV1`. The earlier failed test receipt led
directly to the legacy-fixture correction recorded above.

## Context and orientation

Core contact and request snapshots live in
`crates/syrup-rail/src/attempt/snapshots.rs` and
`crates/syrup-rail/src/attempt.rs`. PostgreSQL row loading is centralized in
`crates/syrup-rail-postgres/src/attempts/persistence.rs`; operation-specific
inserts live in `src/attempts/{initial,recovery,payment_method_replacement,
renewal}.rs` and `src/host_charges.rs`. Account deletion scrubbing lives in
`src/deletion.rs`. Versioned schema artifacts live under
`crates/syrup-rail-postgres/schema`, and runtime conformance lives in
`src/schema_contract.rs` with fixtures under `src/schema_contract/tests`.

## Plan of work

First change the core snapshot so structured equality is the canonical domain
rule. Next add schema v3 and update every PostgreSQL read/write edge to persist
that model. Then add cross-workflow collision regressions, refresh SQLx
metadata and public cutover guidance, and run the full repository gates.

## Concrete steps

1. Add normalized `first_name` and `last_name` fields and accessors to
   `BillingContactSnapshot`; derive `name` from them and prove ambiguous full
   names remain distinct.
2. Copy the complete v2 install contract forward to v3, rename only the
   attempt contact column, append `billing_last_name`, and add a transactional
   `upgrade_from_v2.sql` whose resulting catalog matches a fresh v3 install.
3. Make test and SQLx databases install v3 by default while retaining explicit
   v1/v2 fixture constructors and immutable v2 conformance coverage.
4. Update attempt loaders, all inserts, deletion scrubbing, and relevant
   fixtures to use `billing_first_name`, `billing_last_name`, and
   `billing_email`.
5. Add same-combined-name/different-structure retries to all four foreground
   flows and assert conflict occurs before admission, resolution, or provider
   I/O.
6. Update schema exports, runtime assertion names, examples, READMEs,
   changelog, public API guidance, and crate ownership guidance.
7. Regenerate committed SQLx metadata, review the complete diff, commit each
   slice, and run every required gate.

## Validation and acceptance

- `cargo test -p syrup-rail`
- Focused PostgreSQL replay and schema-contract tests
- `scripts/jig check fmt`
- `scripts/jig check clippy`
- `scripts/jig check test`
- `scripts/jig check sqlx`
- `scripts/jig check contract`
- `scripts/jig work check --plan-id plan_01M05330A1CG2K1YDXABM1BYRB`
- `scripts/jig work evidence --plan-id plan_01M05330A1CG2K1YDXABM1BYRB`
- `scripts/jig work gates --plan-id plan_01M05330A1CG2K1YDXABM1BYRB`

Acceptance requires a clean tracked worktree after commits, with the preexisting
untracked `.agent/0.2.1-bug-findings.md` untouched.

## Idempotence and recovery

The v2-to-v3 artifact is one transactional forward migration. A failure before
commit leaves v2 intact and permits the stopped v2 application to resume. After
commit, roll forward with the v3-aware binary; do not restart a v2 writer. Test
databases are disposable. SQLx metadata regeneration is repeatable after query
changes stabilize.

## Interfaces and dependencies

`BillingContactSnapshot` remains value-safe to format and keeps its existing
display-name/email accessors while adding exact field accessors. No provider,
credential, token, cryptographic, or host-authorization dependency is added.
The PostgreSQL package exports v3 install and v2-to-v3 upgrade artifacts behind
the existing schema-contract test-support boundary and replaces the production
runtime assertion with `assert_runtime_schema_v3_compatible`.
