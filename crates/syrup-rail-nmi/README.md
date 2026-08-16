# syrup-rail-nmi

`syrup-rail-nmi` adapts the bounded NMI client to Syrup Rail's provider-neutral
gateway and lifecycle-evidence contracts.

```toml
[dependencies]
syrup-rail = "0.3.0"
syrup-rail-nmi = "0.3.0"
```

The adapter re-exports the matching raw client as
`syrup_rail_nmi::nmi_client`. Construct a short-lived account client from
host-owned credentials, wrap it in `NmiPaymentGateway`, and return that adapter
from the host's `GatewayResolver`. `NmiMutationReferenceFactory` creates stable
attempt-derived order identifiers in a two-letter host namespace.

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
