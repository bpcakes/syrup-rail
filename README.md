# Syrup Rail

Syrup Rail provides Rust crates for subscription billing. The core crate owns
validated subscription terms, lifecycle policy, and entitlement decisions; the
PostgreSQL crate adds durable billing operations, and the NMI crates connect
those operations to a payment gateway. Host applications own authentication,
authorization, pricing, gateway credentials, and customer-facing presentation.

## Crates

| Crate | Role |
| --- | --- |
| [`syrup-rail`](crates/syrup-rail/README.md) | Provider-neutral domain types and lifecycle policy |
| [`syrup-rail-postgres`](crates/syrup-rail-postgres/README.md) | PostgreSQL schema contract and SQLx billing operations |
| [`syrup-rail-nmi`](crates/syrup-rail-nmi/README.md) | NMI gateway and lifecycle-evidence adapter |
| [`syrup-rail-nmi-client`](crates/syrup-rail-nmi-client/README.md) | Bounded, retry-free NMI HTTP client |

Use only the layers your host needs, and keep all Syrup Rail dependencies on
the same version or Git revision. The NMI adapter re-exports the matching raw
client as `syrup_rail_nmi::nmi_client`.

## Getting started

The current source tree is the unreleased `0.6.0` version. The last released
version is `0.5.3`; see its [versioned documentation](https://docs.rs/crate/syrup-rail/0.5.3).
To use the current source, pin every Syrup Rail dependency to the same Git
revision. This committed revision contains the `0.6.0` implementation:

```toml
[dependencies]
syrup-rail = { git = "https://github.com/bpcakes/syrup-rail.git", rev = "14783547c5c55fd4592ce3c35bb3517251fb58b0" }
syrup-rail-postgres = { git = "https://github.com/bpcakes/syrup-rail.git", rev = "14783547c5c55fd4592ce3c35bb3517251fb58b0" }
syrup-rail-nmi = { git = "https://github.com/bpcakes/syrup-rail.git", rev = "14783547c5c55fd4592ce3c35bb3517251fb58b0" } # NMI hosts only
```

For the raw NMI client without the billing adapter, use
`syrup-rail-nmi-client` at the same revision instead of `syrup-rail-nmi`.

The minimum supported Rust version is 1.88. The current PostgreSQL integration
supports PostgreSQL 18 and schema v5. New hosts apply the
[versioned install SQL](crates/syrup-rail-postgres/schema/v5/install.sql);
hosts on schema v4 use the [v5 cutover guide](crates/syrup-rail-postgres/schema/v5/README.md).
The library does not run migrations for the host.

The compiled [host integration example](crates/syrup-rail-postgres/examples/host_integration.rs)
shows service wiring, host authorization, gateway resolution, and the
transactional event boundary. It can be checked without a database or gateway:

```console
cargo check -p syrup-rail-postgres --example host_integration --locked
```

For the domain model, run the compiled
[subscription terms example](crates/syrup-rail/examples/subscription_terms.rs):

```console
cargo run -p syrup-rail --example subscription_terms
```

These commands run from a checkout of this repository.

## Subscription terms

The [subscription and entitlement guide](docs/integration.md#subscription-terms)
covers paid trials, dunning, access policy, and protected writes.

## PostgreSQL host integration

The [host integration guide](docs/integration.md#postgresql-host-integration)
covers service wiring, billing events, reconciliation, and renewal dispatch.
The [public API guide](docs/public-api.md) describes the supported service
facade and lower-level composition points.

## Development

Run `scripts/jig doctor` to check local prerequisites, then
`scripts/jig check test` for the workspace test gate. Use
`scripts/check-public-api.sh` to verify the public facade and documentation.
The [release guide](docs/releasing.md) covers package and publication checks.

## Help

Use [GitHub Issues](https://github.com/bpcakes/syrup-rail/issues) for questions
and bug reports.

## License

Syrup Rail is source-available under the Elastic License 2.0 (`Elastic-2.0`).
See [LICENSE](LICENSE) for the terms and [NOTICE.md](NOTICE.md) for ownership,
scope, and third-party notices.
