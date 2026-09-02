# Harden NMI outcome provenance after review

Model processor uncertainty and diagnostic provenance together, prove current
adapter mappings exhaustively, document conservative fallback and retained
reconciliation evidence, then run repository gates.

## Progress

- [x] Research NMI response-code and exact-query semantics in current official
  documentation.
- [x] Classify generic provider errors and ambiguous processor/communication
  codes without treating them as known non-submission.
- [x] Preserve typed diagnostics across the raw-client and provider-neutral
  boundaries with canonical set semantics.
- [x] Prevent an unmapped future provider diagnostic from retaining an
  authoritatively approved provider-neutral status.
- [x] Pin the intentional precedence of determinate failure evidence over a
  generic `response=3` signal.
- [x] Prove stale empty exact-query observations route indeterminate sales and
  renewals to review without consuming dunning.
- [x] Rerun repository gates after the final review fixes and record receipts.

## Surprises & Discoveries

- NMI's response-code table labels `400`, `420`, `421`, `440`, and `441`, but
  does not guarantee that those outcomes had no financial effect. Source:
  <https://docs.nmi.com/reference/response-codes>.
- NMI documents exact transaction lookup by order ID, but does not specify a
  consistency horizon or make an empty result a final no-transaction proof.
  Automatically failing a stale empty result would therefore weaken the
  repository threat-model requirement for indeterminate mutations. Source:
  <https://docs.nmi.com/reference/query>.
- Provider-neutral diagnostics are observation-local and intentionally not
  separately persisted. The durable attempt retains the exact processor
  fields, while reconciliation must remain provider-neutral and cannot safely
  reinterpret NMI codes.
- A known duplicate diagnostic can legitimately annotate a later approved
  reconciled result. Consequently, diagnostics cannot all force `Unknown`;
  only diagnostics whose policy explicitly requires conservative admission may
  veto an approved status.

## Decision Log

- Keep ambiguous NMI processor and communication errors `Unknown`. Do not add
  an automatic stale-empty-query failure path without a provider finality
  guarantee.
- Keep sales and renewal/dunning attempts in operator review after a stale
  empty exact query. Document this operational behavior before the unshipped
  `0.5.1` release and cover it at the application and reconciliation layers.
- Enforce the future-diagnostic fallback in `GatewayPaymentOutcome` rather than
  only in the NMI adapter. This makes every adapter using the provider-neutral
  type receive the same fail-closed refinement rule.
- Make the diagnostic-to-status policy exhaustive inside the core crate so a
  new provider-neutral diagnostic requires an explicit policy decision at
  compile time.
- Retain `PaymentOutcomeDiagnostic::ALL` as supported adapter-facing API: its
  public documentation explicitly exists so independent adapters can prove
  coverage while keeping a non-exhaustive fallback.

## Outcomes & Retrospective

- The original response reducer defect was structural: it flattened certainty
  and provenance into a coarse status too early. The shared `DecisionEvidence`
  reducer remains the correct long-term repair.
- The final review exposed a second structural gap: the compatibility fallback
  preserved anomaly provenance but was disconnected from approval admission.
  `GatewayPaymentOutcome::with_diagnostics` now closes that gap centrally.
- The apparent reconciliation defect was an undocumented policy consequence,
  not a safe candidate for automatic failure. Focused database regressions now
  make the intended review and no-dunning behavior executable.

## Root cause and decisions

- The terminal-code defect was structural: heterogeneous NMI decision fields
  were flattened to `PaymentStatus` before their certainty and provenance were
  reduced. `response=3` therefore looked like a terminal failure even though
  NMI defines it as either transaction-data or system error.
- Response-code status and diagnostics now come from one `DecisionEvidence`
  classification. NMI documents `400`, `440`, and `441` as processor errors and
  `420` and `421` as communication errors, without a no-financial-effect
  guarantee, so they remain reconcilable. Bare `response=3` and generic
  `status`/`condition=error` evidence receive the same typed provenance;
  determinate gateway/configuration failures remain terminal.
- Raw-client diagnostics remain non-exhaustive for semver compatibility. The
  adapter retains a conservative `UnmappedProviderDiagnostic` fallback for a
  newer independently upgraded client, while `PaymentOutcomeDiagnostic::ALL`
  proves every diagnostic known to the current client maps specifically.
- Valid customer-vault evidence is retained when an approved response lacks a
  transaction identity. The result is `Unknown` and cannot yield
  `ApprovedProcessorEvidence`, so the reference is reconciliation evidence and
  cannot authorize activation.
- Adapter identifier admission is field-local: rejecting one identifier no
  longer erases its valid sibling, and only an approved outcome is downgraded
  by rejected identity evidence. Determinate declines and failures retain their
  status while exposing the rejected-field diagnostic.
- Diagnostics have set semantics at both published boundaries: canonical
  ordering and deduplication prevent hosts or direct client consumers from
  depending on incidental parser order or repeated evidence. Declaration order
  is the single canonical-order source, so no hand-maintained rank can drift.
- The client reducer also emits typed response-code diagnostics in semantic
  order, so duplicate JSON key order cannot change the resulting evidence.
- Each `DecisionField` owns the `DecisionFieldKind` used to classify it, so
  callers cannot accidentally reinterpret a status or condition as another
  wire field category.
- Generic provider-error provenance is aggregated independently from status
  reduction. It therefore survives pending, unrecognized, or conflicting
  companion fields and is suppressed only when determinate failure evidence or
  a more specific indeterminate/duplicate response-code diagnostic supersedes it.

## Validation

- Focused `syrup-rail`, `syrup-rail-nmi`, and `syrup-rail-nmi-client` suites:
  passed (341 unit tests plus doctests).
- The two new focused PostgreSQL regressions passed independently.
- Final formatting passed (`receipt_01M1F19ZNE0Z75RTFKQZ1KGJTJ`).
- Final Clippy with warnings denied passed (`receipt_01M1F1AKJAECC21PH4B13N2CF6`).
- Plan-linked contract validation passed (`receipt_01M1F1CQJGEX9ZBGD1MVK8KVBW`).
- Plan-linked SQLx validation passed (`receipt_01M1F1DZZQAEYQYW455Q3EHXQD`).
- `git diff --check` passed.
- The configured parallel `jig.test` lane passed every non-database test but
  exhausted local container startup capacity for 82 PostgreSQL scenarios; all
  failures were `ContainerStart(WaitContainer(StartupTimeout))`, with no failed
  assertion (`receipt_01M1F0QX0V4Y0A3GB1XKQVKEWZ`).
- The authoritative serialized/locked full workspace suite passed, including
  all 227 PostgreSQL tests (`receipt_01M1F1ZGX65XNE23TGRRE6CWNP`).
- The required `jig.test` gate also passed with `RUST_TEST_THREADS=1`, preserving
  the configured gate identity while avoiding local container oversubscription
  (`receipt_01M1F2GGJ7Z09GR2EYTNA0GWJJ`; batch
  `receipt_01M1F2GGM03GY6V1XC8MBR5K5W`). All three required plan gates are fresh.
