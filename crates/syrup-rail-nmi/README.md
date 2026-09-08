# syrup-rail-nmi

`syrup-rail-nmi` adapts the bounded NMI client to Syrup Rail's provider-neutral
gateway and lifecycle-evidence contracts.

```toml
[dependencies]
syrup-rail = "0.5.2"
syrup-rail-nmi = "0.5.2"
```

The adapter re-exports the matching raw client as
`syrup_rail_nmi::nmi_client`. Construct a short-lived account client from
host-owned credentials, wrap it in `NmiPaymentGateway`, and return that adapter
from the host's `GatewayResolver`. `NmiMutationReferenceFactory` creates stable
attempt-derived order identifiers in a two-letter host namespace.

`query_payment_method_metadata` queries card display, including `cc_exp`, through
the same bounded exact NMI query and selector checks as `query_transaction`.
Its `GatewayPaymentMethodMetadata` result cannot authorize a payment. The
PostgreSQL saved-method refresh uses it after approval, or for existing methods.
Financial sale/query descriptors retain their 0.5.2 interpretation so additional
display fields do not invalidate immutable replay evidence. Classic financial
responses still use the original brand aliases and omit expiry; XML financial
queries also omit expiry, while existing JSON financial expiry is preserved.
Metadata queries additionally accept case-only brand duplicates and retain a
provider-supplied spelling. Financial queries keep the 0.5.2 case-sensitive
duplicate rule; customer display derives the canonical `PaymentCardBrand`.
HTTP-error classification is intentionally more conservative: Classic `cc_type`
and `cc_exp` fields count as possible payment-processing evidence even though
the financial descriptor does not retain them. Such responses stay indeterminate
instead of authorizing a same-key retry.

Construct each raw account client with
`ClientFactory::client_with_duplicate_check` and an explicit
`DuplicateCheck`. `ProcessorConfigured` retains the account's processor-level
duplicate policy. An explicit positive `Window` requires the processor to
permit merchant overrides. This client does not model or send the invalid
`dup_seconds=0` value. A processor heuristic can also reject a legitimate
later payment that it considers a duplicate, and NMI does not publish stable
matching criteria. Keep that account-level choice out of per-payment call
sites.

An NMI duplicate response code `430` remains an unknown outcome requiring exact
reconciliation; the raw client exposes
`DuplicateTransactionAtProcessor`, and the adapter maps it to the
provider-neutral `GatewayPaymentDiagnostic::ProcessorReportedDuplicate` on
`GatewayPaymentOutcome`. Processor-error codes `400`, `440`, and `441`,
communication-error codes `420` and `421`, and an otherwise unresolved generic
provider error likewise remain unknown because NMI does not guarantee that they
had no financial effect; the adapter exposes
`GatewayPaymentDiagnostic::IndeterminatePaymentOutcome`. Every other raw or
adapter-discovered payment-evidence anomaly also crosses that boundary as a
payload-free provider-neutral diagnostic, so hosts never need to reconstruct
decision safety from provider response text. A missing identifier does not
erase a valid sibling, but an invalid, conflicting, or adapter-rejected
identifier quarantines the complete NMI identity bundle. This prevents a
parseable sibling from becoming durable authority after its diagnostic
provenance is no longer present. Identity quarantine does not turn an otherwise
determinate decline or failure into an unknown outcome; approvals still fail
closed. Diagnostics are deduplicated and canonically ordered, but order has no
chronology or precedence semantics; use `has_diagnostic()` for routing by
membership. Foreground
`syrup-rail-postgres` subscription and host-charge results copy diagnostics to
`observation_diagnostics()`. Those result diagnostics describe the observation
applied by the current call; they are not separately persisted and a later
attempt replay may not contain them. Exact response fields remain durable
processor evidence. The 0.5.0 `gateway_diagnostics()` names remain as deprecated
compatibility aliases. The gateway outcome's effective status already includes
the core diagnostic-certainty policy; a diagnostic copied onto a foreground
result does not rewrite an earlier durable replay result. Hosts must use the
returned status and evidence when deciding whether submission is complete or
reconciliation is required. NMI
documents exact order-ID lookup but does not define an empty query result as
final. A stale empty observation therefore remains operator review for sales
and renewal/dunning attempts rather than becoming a determinate failure.
For a nonempty order-only query, the raw client accepts a coherent decline or
determinate failure only after the response echoes that order ID and contains
exactly one transaction record; an unusable response transaction ID remains a
diagnostic rather than weakening the independently bound non-approved decision.
The duplicate is not mapped to a card decline or known non-submission. Keep the
processor duplicate window shorter than the shortest normal billing or renewal
interval and the minimum replacement-charge interval, then wait out that window
by default after an indeterminate attempt.
NMI's sandbox does not contact a processor, so hosts must verify duplicate
override support against the effective processor configuration before rollout.

The raw client and adapter never retry a payment mutation. Indeterminate
outcomes must be reconciled from the durable attempt identity before any
replacement charge is considered. Unknown lifecycle vocabulary is quarantined
rather than guessed, and ordinary formatting remains free of credentials,
provider identifiers, response text, and payment-token values.

This crate does not persist credentials, install a database schema, authorize
subscribers, or own subscription schedules. Those responsibilities remain in
the host and the matching `syrup-rail-postgres` integration.

This package is proprietary software distributed under the terms in the
packaged `LICENSE` file.
