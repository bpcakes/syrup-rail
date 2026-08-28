# Close NMI comprehensive-review findings

Research the independent review questions, fix remaining payment-evidence
certainty and durable-result contract defects, add regressions, run repository
gates, and execute comprehensive review round 2.

## Progress

- [x] Verify current source, security baseline, and crate invariants.
- [x] Research NMI's documented v5 validation envelope.
- [x] Resolve the intended current-observation diagnostic and replay contract.
- [x] Make anomalous `status` fields fail closed while preserving exact generic
      HTTP metadata.
- [x] Keep equivalent decision evidence order-independent without synthesizing
      provider values.
- [x] Exclude call-scoped diagnostics from durable-result equality.
- [x] Declare the adapter test's Tokio `io-util` requirement explicitly.
- [x] Run full repository and standalone/MSRV verification.
- [x] Run comprehensive review round 2 and close its actionable gaps.
- [x] Run comprehensive review round 3 and adjudicate final findings.

## Decisions

- NMI's official example documents `type=validationError`,
  `error_code=E_INVALID_SUBMISSION`, a non-empty message, `ref_id`, and a
  non-empty field-level `details` array. Unknown extensions and empty or omitted
  details remain indeterminate; availability does not outweigh mutation
  certainty at this boundary.
- A current duplicate diagnostic may annotate an approved durable replay. It
  never changes durable status or evidence and may disappear on a later replay.
  Equality therefore compares only durable fields.
- Same-status lifecycle aliases preserve the economic decision but do not
  fabricate one exact lifecycle value when the provider supplied conflicting
  normalized facts.
- NMI documents v5 401/403 as authentication or permission/account rejection.
  Together with 404/405 they prove pre-processing only when the configured URL
  is the direct NMI endpoint; the public contract excludes indistinguishable
  proxy-generated responses.
- v5 documents JSON responses, but response formats are untrusted evidence at
  the mutation-certainty boundary. Recognizable Classic form evidence from a
  v5 endpoint, and any non-empty top-level JSON array, therefore fail closed.
- The deprecated legacy constructor stays source-compatible for downstream
  clients, but its warning explicitly calls out the wire change. There is no
  valid compatibility behavior to preserve because NMI rejects zero and the
  positive-window API still exposes supported merchant overrides.
- A Classic vault validation may create durable vault state. Its undocumented
  HTTP 400/422 responses therefore remain indeterminate rather than authorizing
  an immediate replacement attempt.
- Different response codes remain conflicting even when they map to the same
  coarse payment status. The exact code is durable evidence and can affect host
  policy, so the client does not discard that ambiguity.
- The documented in-band `301` rate limit is a closed-envelope proof, not an
  evidence blacklist. Any unknown field, processing container, descriptor, or
  unexpected response text makes the mutation indeterminate.
- Non-success JSON objects preserve a status-derived not-submitted result only
  when they are bounded generic NMI error metadata. Unknown or extended object
  shapes fail closed just like non-empty arrays.

## Validation

- Focused raw-client, core, and adapter suites pass with 196, 106, and 16 tests.
- The payment-method-replacement replay regression passes with whole-result
  durable equality restored.
- Round 2 found and fixed cross-format v5 payment evidence, non-empty unknown
  JSON envelopes, a latent occurrence-vector invariant, and a lost
  identical-approved-replay assertion. The raw client now passes 199 tests and
  the restored Postgres replay regression passes.
- Round 3 found and fixed incomplete `301` proof, unknown JSON-object handling,
  synthetic report normalization, and call-site-selected Classic certainty.
  The raw client passes 202 tests before the final repository gates.
- Final Jig contract, full test, and SQLx gates pass under batch receipt
  `receipt_01M143WFSW3KBE69Y6H3HYG0JY`; formatting and Clippy pass under
  `receipt_01M143WP4BNJ05QJJ1FHD8CDMB` and
  `receipt_01M143XDMKSQ3QQH77HY9B0ZT2`.
- The extracted NMI client package passes 202 tests plus doctests on current
  Rust and Rust 1.88. The NMI adapter's 16 tests also pass on Rust 1.88.
