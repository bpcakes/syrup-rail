# Harden NMI payment evidence boundaries

This plan fixes the NMI duplicate-policy regression and the follow-on review
findings without weakening Syrup Rail's central invariant: only evidence that
proves a mutation never reached processing may authorize replacement
submission. The security baseline is `docs/security/threat-model.md`.

## Progress

- [x] Read repository, crate, security, and Fowler refactoring guidance.
- [x] Establish the pre-change baseline: 299 focused library tests pass.
- [x] Research NMI's documented duplicate-window and v5 HTTP contracts.
- [x] Refactor decision evidence so raw, canonical, and classified forms cannot
      disagree or depend on field order.
- [x] Make non-success payment HTTP handling fail closed when the body contains
      in-band transaction-decision evidence.
- [x] Carry provider-neutral gateway diagnostics through foreground
      application results without changing the shipped schema-v4 contract.
- [x] Add regression tests for parser, transport, adapter, and application
      boundaries.
- [x] Run targeted tests and standalone package checks on current Rust and the
      declared Rust 1.88 MSRV.
- [x] Run the final repository-required gates and record their evidence.

## Surprises & Discoveries

- `DecisionField` currently stores a first raw spelling while comparing a
  separately normalized spelling. `is_rate_limited` then reads the raw field,
  making certainty depend on duplicate-key order.
- The same field abstraction considers `420` and `430` non-conflicting because
  both classify as `Unknown`, even though they are different processor facts.
- NMI documents v5 HTTP 400 as a validation-error envelope and does not list
  422 for sale. It documents transaction results under HTTP 200.
- NMI's public payment field describes zero on the wire, but the affected
  processor configuration rejects it. The client contract therefore models
  only omission or a validated positive window and never sends zero.
- The comprehensive review found that generic HTTP status handling was acting
  as a mutation-certainty authority before the operation-specific body was
  interpreted. HTTP 400 body read failures, Classic non-success bodies, and
  payment-only 422 handling all needed the same proof boundary.
- Application regression coverage found a second transform in payment-method
  replacement that rebuilt a reconciled outcome while dropping diagnostics.
- Schema v4 shipped in 0.4.0. A new durable diagnostic column would require a
  forward schema version, so this patch keeps the existing exact response code
  durable and carries typed diagnostics on the foreground result. Replay
  diagnostics are explicitly outside this patch's persistence contract.

## Decision Log

- Preserve exact single provider values in `ProcessorEvidence`; canonicalize
  only semantically equivalent duplicate spellings so output is order
  independent.
- Treat noncanonical `301` spellings as diagnostic evidence only. Known
  pre-processing rate limiting requires every occurrence to be exactly the
  documented canonical `301` value.
- Surface a duplicate diagnostic if any bounded numeric response-code
  occurrence is 430, even when other occurrences conflict; status remains
  `Unknown` and the conflict is diagnosed separately.
- On non-success v5 payment responses, any in-band payment-decision field
  contradicts the validation-error HTTP contract and therefore yields
  indeterminate mutation certainty.
- Accept HTTP 400 as known-not-submitted only for NMI's documented v5
  validation envelope. Unreadable, oversized, malformed, extended, or
  processing-bearing payment responses remain indeterminate.
- Remove the zero-valued duplicate-check state. Both the explicit and
  deprecated construction paths now either omit `dup_seconds` or send a
  validated positive window.
- Carry diagnostics on `SubscriptionEnrollmentPaymentResult` and
  `HostChargePaymentResult` for the current gateway application. Do not modify
  immutable shipped schema artifacts in this patch.

## Outcomes & Retrospective

- Replaced the global zero hardcode with an account-bound policy whose only
  states are processor-configured omission and a validated positive window.
- Split query/report and payment-mutation HTTP interpretation so generic status
  mapping cannot overstate mutation certainty.
- Made exact canonical 301 proof independent of duplicate ordering and kept
  whitespace-padded or numeric JSON observations diagnostic-only.
- Propagated typed duplicate diagnostics through raw client, adapter, and all
  foreground subscription and host-charge result boundaries without changing
  shipped schema artifacts.
- The affected core, adapter, and Postgres suites pass (106, 16, and 225 tests),
  and the standalone NMI client passes 194 tests plus doctests on current Rust
  and Rust 1.88.
- Jig contract, full test, SQLx, formatting, and Clippy checks all pass with
  linked receipts for this plan/session.

## Context and orientation

`syrup-rail-nmi-client` owns bounded transport and untrusted response parsing.
`client/response/common.rs` resolves repeated decision fields;
`client/transport.rs` maps HTTP response certainty. `syrup-rail-nmi` maps raw
diagnostics into provider-neutral core values. `syrup-rail-postgres` applies
gateway outcomes and returns core payment-result types to hosts.

## Plan of work

1. Characterize field-order, noncanonical, conflicting-code, and non-2xx
   behavior with focused tests.
2. Apply Fowler's **Encapsulate Record** and **Combine Functions into
   Transform** to make decision resolution produce one coherent resolved value
   containing raw evidence, normalized observations, status, and diagnostics.
3. Apply **Split Phase** to distinguish HTTP transport success from
   operation-specific payment response interpretation.
4. Extend host-facing payment result records with foreground diagnostics and
   thread them through each subscription and host-charge application boundary.
5. Update documentation to state the researched provider contract, renewal
   interval guidance, canonical-certainty rule, and foreground diagnostic
   lifetime.

## Concrete steps

- Edit only current Rust sources, tests, README files, the changelog, crate
  guidance, and append-only Jig receipts. Do not edit shipped schema v1-v4.
- Run `cargo test -p syrup-rail-nmi-client` after parser/transport changes.
- Run `cargo test -p syrup-rail -p syrup-rail-nmi -p syrup-rail-postgres`
  after result propagation changes.
- Run `scripts/jig work check`, `scripts/jig work evidence`, and
  `scripts/jig work gates` before finishing.

## Validation and acceptance

- Both duplicate-key orderings produce identical certainty.
- Only exact canonical 301 evidence produces `MutationCertainty::NotSubmitted`.
- HTTP 400 with a decision field and all HTTP 422 sale responses are
  indeterminate; documented HTTP 400 validation envelopes remain
  `RequestRejected`.
- A 430 diagnostic reaches raw client, adapter, subscription payment result,
  and host-charge payment result tests.
- Existing public constructor signatures remain source compatible and schema
  v4 remains untouched. The deprecated constructor intentionally changes its
  broken wire behavior by omitting `dup_seconds`.

## Idempotence and recovery

All edits are ordinary source changes. Tests use isolated local fixtures. If a
step fails, retain the last compiling structure and revert only the current
unverified patch hunk; never reset the user's existing working tree.

## Interfaces and dependencies

No new runtime dependency, unsafe code, production background task, retry, SQL
migration, or production network call is introduced. A test-only Tokio feature
supports bounded adapter-server assertions. Edition 2024 and MSRV 1.88 remain
unchanged.
