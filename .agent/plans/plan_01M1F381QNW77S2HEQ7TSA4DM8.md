## Progress

- [x] Research NMI decision semantics and inspect core consumers.
- [x] Encode approval-certainty policy in the provider-neutral outcome.
- [x] Add exhaustive policy and provider regression tests.
- [x] Run required verification and close the plan.

## Surprises & Discoveries

- NMI documents Query `failed` as terminal failure and Classic response code 300 as gateway rejection; both legitimately resolve generic response=3.
- Identity diagnostics and duplicate provenance must remain compatible with Approved so approved processor evidence can be retained and reconciled.
- Decision-certainty diagnostics must veto Approved independent of adapter behavior.
- A cached effective status preserves the existing public `const fn status()`
  contract; a manual `Debug` implementation keeps the original effective-only
  representation and does not expose the internal provider status.

## Decision Log

- Fix the invariant in `syrup-rail::GatewayPaymentOutcome`, the provider-neutral trust boundary.
- Preserve the public const status accessor by retaining provider-reported status privately and recalculating effective status whenever diagnostics are replaced.
- Keep generic-error suppression when determinate failure evidence is present and pin all supported evidence fields.

## Outcomes & Retrospective

`GatewayPaymentOutcome` now owns the approval-certainty invariant. Every
decision-certainty diagnostic downgrades effective approval, while identity and
duplicate provenance remain compatible with approved evidence needed for safe
parking and reconciliation. Diagnostic replacement is deterministic. The NMI
generic-error policy is pinned across `condition=failed`, `status=failed`, and
`response_code=300`. Focused crate tests, formatting, Clippy, the repository
contract, SQLx verification, and the serialized workspace test gate all pass.

## Context and orientation

The NMI adapter maps untrusted provider responses into `GatewayPaymentOutcome`. Downstream PostgreSQL code treats `Approved` as authority to retain financial evidence and project state. Diagnostics are payload-free provenance. Decision-certainty diagnostics cannot coexist with effective Approved, while identity and duplicate diagnostics may require retaining an otherwise authoritative approval for safe review. See `docs/security/threat-model.md`.

## Plan of work

Update `crates/syrup-rail/src/gateway.rs` so the effective status is recalculated from the original provider status and canonical diagnostics. Classify every diagnostic exhaustively. Extend core and NMI client tests, then run focused tests and all repository gates.

## Validation and acceptance

An Approved outcome becomes Unknown for every decision-certainty diagnostic, remains Approved for identity/duplicate diagnostics, and restores deterministically if diagnostics are replaced. Generic response=3 plus condition/status failed or response_code=300 remains Failed without indeterminate provenance. Run fmt, clippy, contract, SQLx, and serialized full tests.

## Idempotence and recovery

Edits are source-only and reversible. Re-running tests and Jig checks is safe. Do not stage or commit user changes.

## Interfaces and dependencies

No public method signatures change. `GatewayPaymentOutcome::status()` and part-consuming methods continue returning effective status.
