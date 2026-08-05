# Syrup Rail

Reusable subscription billing crates for Banana Pancakes applications.

## Packages

| Crate | Role |
| --- | --- |
| `syrup-rail` | Validated domain types and lifecycle policy |
| `syrup-rail-postgres` | Canonical PostgreSQL schema contract and SQLx orchestration |
| `syrup-rail-nmi` | NMI gateway and lifecycle-evidence adapter |
| `syrup-rail-nmi-client` | Bounded, retry-free raw NMI HTTP client |

## Development

- `scripts/jig doctor`
- `scripts/jig check test`
- `cargo test -p syrup-rail-nmi-client`

Private Cargo consumers pin one exact Git revision with
`git = "ssh://git@github.com/bpcakes/syrup-rail.git"` and set
`CARGO_NET_GIT_FETCH_WITH_CLI=true` so authentication uses the system Git client.
