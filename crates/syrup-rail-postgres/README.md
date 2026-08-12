# syrup-rail-postgres

`syrup-rail-postgres` provides Syrup Rail's canonical provider-neutral ledger,
SQLx operations, and high-level subscription billing service. Version 0.2
supports PostgreSQL 18 only and uses schema v2.

```toml
[dependencies]
syrup-rail = "0.2.0"
syrup-rail-postgres = "0.2.0"
```

New hosts install `schema/v2/install.sql` through their normal migration
system. Hosts upgrading from 0.1 must stop every 0.1 billing writer, run the
checked-in v1 preflight and retry-reclassification audit, apply
`schema/v2/upgrade_from_v1.sql` transactionally, and roll forward with 0.2.
Schema v1 is immutable.

After the host applies its migration and before it serves billing traffic,
verify the runtime catalog:

```rust,no_run
# async fn verify(pool: &sqlx::PgPool) -> Result<(), syrup_rail_postgres::SchemaConformanceError> {
syrup_rail_postgres::assert_runtime_schema_v2_compatible(pool).await?;
# Ok(())
# }
```

`SubscriptionBillingService` is the primary mutation facade. Hosts supply
offer locking, gateway resolution, abuse admission, and a transaction
coordinator that locks the authorized billing subject first and appends every
typed `BillingEvent` to the host outbox on the same connection. The packaged
`host_integration` example includes concrete service wiring and a versioned,
redacted host-owned event-envelope mapping. It separates first-write metadata
from the replay-stable value and demonstrates the complete atomic
insert/conflict-read/raw-structural-comparison/typed-reconstruction path on the
same transaction. Version 1 owns its nested enum labels instead of delegating
them to core display methods. The example keeps subject identifiers and
payload values out of `Debug` output; its card payload uses the core canonical
brand vocabulary and never copies an unknown provider string into the host
event.

Errors returned by host transaction, event, charge-target, and operator-review
callbacks remain opaque through ordinary formatting and the standard
`Error::source()` chain. A host can deliberately recover its original callback
error only by classifying the outer service error, destructuring an owned
callback-error variant, and consuming that wrapper with `into_source()` in a
protected diagnostic path.

Customer billing portal/history queries, stable due-renewal pagination, and the
lower-level transaction-local operations remain available for hosts that need
to compose them into a larger application transaction. Authentication,
authorization, migrations, job queues, and event transport remain host-owned.

This package is proprietary software distributed under the terms in the
packaged `LICENSE` file.
