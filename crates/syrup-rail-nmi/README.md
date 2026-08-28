# syrup-rail-nmi

`syrup-rail-nmi` adapts the bounded NMI client to Syrup Rail's provider-neutral
gateway and lifecycle-evidence contracts.

```toml
[dependencies]
syrup-rail = "0.5.0"
syrup-rail-nmi = "0.5.0"
```

The adapter re-exports the matching raw client as
`syrup_rail_nmi::nmi_client`. Construct a short-lived account client from
host-owned credentials, wrap it in `NmiPaymentGateway`, and return that adapter
from the host's `GatewayResolver`. `NmiMutationReferenceFactory` creates stable
attempt-derived order identifiers in a two-letter host namespace.

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
`GatewayPaymentOutcome`. Foreground `syrup-rail-postgres` subscription and
host-charge results copy it to `gateway_diagnostics()`, so hosts can route on
the typed fact without parsing provider text. Those result diagnostics describe
the observation applied by the current call; they are not separately persisted
and a later attempt replay may not contain them. The exact response code remains
durable processor evidence. A current diagnostic never overrides the returned
durable attempt status or evidence; hosts must use those authoritative fields
when deciding whether submission is complete or reconciliation is required.
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
