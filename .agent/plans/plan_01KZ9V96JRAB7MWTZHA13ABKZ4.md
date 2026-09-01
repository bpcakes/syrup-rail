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

- `scripts/jig doctor --summary`
- `scripts/jig check test`
- `cargo test -p syrup-rail-nmi-client`

Private consumers pin one exact Git revision through `git@github.com:bpcakes/syrup-rail.git`.


Dup-unifier implementation pass: extract the shared cooldown commit transaction while preserving subscription/host-charge errors and diagnostics; compose EntitlementQuery and EntitlementGuard over one private selector while preserving their nominal public APIs and Debug behavior; validate core, PostgreSQL, contract, formatting, clippy, SQLx, and workspace tests.

Dup-unifier eligible-candidate consolidation completed. Added a private shared entitlement selector while preserving distinct public EntitlementQuery and EntitlementGuard nominal APIs, const builders/accessors, and Debug shape. Extracted the subscription/host-charge rate-limit cooldown transaction, durability recheck, persistence outcome, and diagnostics into one shared core while retaining domain-specific terminal errors in thin wrappers. Added regression coverage for selector parity/type separation and the non-durable cooldown disposition. Verification: focused core and PostgreSQL tests passed; jig fmt receipt_01M11NHM142C1ABMDGVWXCZ691, clippy receipt_01M11NJM1VT68JTN4EPDNZJ06B, contract receipt_01M11NJVDNP2S80MK63QNMY8MV, SQLx receipt_01M11NN6HZK8CXXVPHNAN9KHEQ, and serialized full tests receipt_01M11P80C9NNG9W4J9BX3FPWHP all passed. The first parallel test attempt hit only testcontainer StartupTimeout failures; the serialized retry passed.