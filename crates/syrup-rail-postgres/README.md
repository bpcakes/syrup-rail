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
Schema v1 is immutable. Budget the stopped-writer maintenance window for a
full payment-attempt heap scan and transactional partial-index construction;
the detailed cutover guide explains the lock and rehearsal requirements.

After the host applies its migration and before it serves billing traffic,
verify the runtime catalog:

```rust,no_run
# async fn verify(pool: &sqlx::PgPool) -> Result<(), syrup_rail_postgres::SchemaConformanceError> {
syrup_rail_postgres::assert_runtime_schema_v2_compatible(pool).await?;
# Ok(())
# }
```

During `REINDEX CONCURRENTLY`, PostgreSQL exposes the command, phase, and
target details only to the maintenance role and statistics-privileged roles.
The runtime assertion tolerates `_ccnew` and `_ccold` shadows only when those
visible progress details and the backend's relation locks agree, then rechecks
the evidence before committing. Run maintenance and startup validation as the
same database role when startup must remain available during a reindex; a
cross-role observer fails closed. Drop stale invalid shadows left by failed
maintenance before serving billing traffic.

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

## Reconciliation phase order

The host owns the reconciliation scheduler. For each account returned by
`reconciliation_gateway_accounts`, run the local-only cleanup phases before
calling `claim_exact_reconciliation_attempts`:

1. `fail_stale_unsubmitted_payment_method_replacements`;
2. `fail_stale_unsubmitted_subscription_charges`;
3. `fail_stale_unsubmitted_subscription_enrollments`; and
4. `fail_stale_unsubmitted_host_charges` when host charges are configured.

The host-charge phase also requires the host's `HostChargeTargetStore`; it
releases the host-owned target and fails the canonical attempt in one database
transaction. Local cleanup never contacts the gateway. The cleanup functions
are safe to repeat, and the bounded phases should run on every scheduled pass
so locked work or a backlog is retried later. Exact provider queries are only
for attempts whose `submitted_at` proves that submission began.

`fail_stale_unsubmitted_host_charges` returns failed and skipped counts. A
`StaleTarget` or `Unchanged` callback outcome leaves that target and attempt
untouched, increments `skipped`, and does not prevent later candidates from
progressing. The host callback should record the target-specific incident for
operator follow-up; the account scheduler can continue its remaining local and
exact reconciliation phases.

An existing 0.2.0 host must add the subscription-charge phase, plus the
host-charge phase when host charges are configured, to its reconciliation loop
when upgrading to the next patch release. Omitting them leaves abandoned local
rows for foreground reads or later cleanup even though exact reconciliation
correctly excludes never-submitted attempts.

Customer billing portal/history queries, stable due-renewal pagination, and
other lower-level transaction-local operations remain available for hosts that
need to compose them into a larger application transaction. The protected-write
guard described below deliberately owns its top-level transaction instead.
Authentication, authorization, migrations, job queues, and event transport
remain host-owned.

Protected product writes use two ownership-enforced phases. Start an
`EntitlementWriteTransaction` from the pool and use its connection for any
preparatory host writes; that pending value has no commit operation.
`require_entitlement_for_update` returns an
`AdmittedEntitlementWriteTransaction` only when current paid or granted access
is admitted and keeps the relevant locks held for the host mutation. Completed
denials and SQL failures await rollback, while cancellation queues rollback of
the owned transaction, including any earlier host writes. Perform and commit
the host-owned protected mutation only through the admitted value, and finish
any nested savepoint before consuming that value with `commit` or `rollback`.

This package is proprietary software distributed under the terms in the
packaged `LICENSE` file.
