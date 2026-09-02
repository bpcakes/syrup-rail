## Progress

- [x] Research NMI failure semantics and operator-review ownership.
- [x] Preserve valid sibling identifiers across all foreground parsers.
- [x] Add compatibility guidance and close review test gaps.
- [x] Run required verification and close the plan.

## Surprises & Discoveries

- NMI documents `failed` as a terminal transaction status/condition; generic response=3 suppression by that evidence is intentional.
- Host applications own reconciliation scheduling, metrics, alerting, and operator presentation; the crate already exposes bounded review pages and manual resolution.
- `finalize_foreground_identifiers` resolves fields independently and then unnecessarily erases both when either is invalid.
- Existing JSON container-shape tests encoded the coupled erasure behavior;
  they now prove the invalid field is dropped while its safe sibling survives.

## Decision Log

- Keep decision certainty and evidence retention separate: any identity anomaly downgrades payment status, but only the anomalous field is discarded.
- Fix the shared client helper so form, JSON, and XML cannot drift.
- Preserve the 0.5.1 safety change and document it explicitly for 0.5.0 hosts instead of reverting the core invariant.

## Outcomes & Retrospective

Foreground form, JSON, and XML parsing now preserves each independently valid
identifier while keeping any anomalous outcome `Unknown`. Determinate NMI
failure precedence is pinned for JSON, the provider-neutral status policy is
covered for all non-approved states, and PostgreSQL exercises an adapter-
reported approval vetoed by indeterminate provenance. Host compatibility and
operator-review monitoring duties are explicit. Focused tests, formatting,
Clippy, contract, SQLx, and serialized workspace tests all pass.

## Context and orientation

The NMI client parses untrusted wire identifiers into independent `ResolvedScalar` values. `finalize_foreground_identifiers` currently turns either field anomaly into `(None, None)`, losing safe reconciliation evidence. The provider-neutral adapter already admits identifier values independently. See `docs/security/threat-model.md`.

## Plan of work

Change the shared finalizer in `crates/syrup-rail-nmi-client/src/client/response/common.rs`, update cross-format identity tests, add JSON failure-precedence coverage, exercise the core approval downgrade through PostgreSQL application tests, and update host action guidance.

## Validation and acceptance

Every parser retains a valid transaction or vault sibling when the other field is invalid, while returning Unknown with the appropriate diagnostic. Determinate non-approved statuses remain unchanged by core diagnostics, JSON failure evidence supersedes generic response=3, and PostgreSQL observes Approved plus indeterminate provenance as Unknown. Run fmt, clippy, contract, SQLx, and serialized full tests.

## Idempotence and recovery

Source and documentation changes are reversible. Checks are safe to rerun. Do not stage or commit existing user changes.

## Interfaces and dependencies

No public signatures or schema artifacts change.
