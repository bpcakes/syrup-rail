# syrup-rail-postgres

`syrup-rail-postgres` provides Syrup Rail's canonical provider-neutral ledger,
SQLx operations, and high-level subscription billing service. Version 0.5
supports PostgreSQL 18 only and uses schema v5.

```toml
[dependencies]
syrup-rail = "0.5.0"
syrup-rail-postgres = "0.5.0"
```

New hosts install `schema/v5/install.sql` through their normal migration
system. Existing schema-v4 hosts run the read-only v5 preflight and audit,
remediate incompatible retained attestations through an authorized process,
then apply `schema/v5/upgrade_from_v4.sql` transactionally. Schemas v1 through
v4 are immutable. The detailed versioned guides explain the required lock,
maintenance, audit, and rehearsal boundaries.

After the host applies its migration and before it serves billing traffic,
verify the runtime catalog:

```rust,no_run
# async fn verify(pool: &sqlx::PgPool) -> Result<(), syrup_rail_postgres::SchemaConformanceError> {
syrup_rail_postgres::assert_runtime_schema_v5_compatible(pool).await?;
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

The service requires a live gateway account by default. A host exercising a
dedicated test environment can call
`with_required_gateway_account_mode(GatewayAccountMode::Test)` to require an
exact test account instead. That setting permits test-mode mutations but still
rejects an observed live account before submission; bind it only to trusted
deployment configuration. A single NMI account can therefore be used only for
a carefully serialized staging/live cutover, but NMI mode lookup and sale are
separate requests and a Merchant Portal user can change the same account-wide
switch out of process. Use separate NMI test and production merchant accounts
when test/live isolation matters. The service performs an early account-mode
query before final admission and a mandatory second query immediately before
provider submission. The second query narrows the race window and makes the
safety check structural for supported low-level submitters; it cannot make two
NMI requests atomic. Initial enrollments and prepared host-charge replays change
from `query → mutation` to `query → query → mutation`, roughly 50% more provider
requests for those flows. Renewals check before reservation, after reservation,
and at submission, so their successful path uses three mode queries. Recoveries,
payment-method replacements, and fresh host charges retain their existing
readiness boundaries. A transient failure of the final query
occurs after durable admission, but the provider mutation endpoint was not
contacted. Resumable enrollment, recovery, payment-method replacement, and
host-charge attempts are therefore atomically restored to prepared state and
can retry with the same command and idempotency key. Automatic renewal retains
terminal not-submitted handling because its scheduler does not resume prepared
attempts.
Approved enrollments also persist the required mode on the subscription.
Renewal dispatches expose it for host routing, and renewal, recovery, and
payment-method replacement reservations reject a service configured for the
other mode before creating new provider work. A mode-specific scheduler should
use `due_renewals_page_for_mode`; the returned cursor records that mode and
rejects cross-mode reuse. Its dedicated mode-leading index filters before the
bounded SQL page limit. The unfiltered
`due_renewals_page` remains available to a central router that owns both modes.
Entitlement queries and guards default to live paid subscriptions; test workers
select `Test`, while trusted administrative tooling can opt into both modes
with `across_gateway_account_modes`. Current-subscription, portal, and history
projections remain mode-neutral, so a host that mixes modes must enforce its
own trusted production access partition around those reads.

Supported low-level provider submission functions consume an opaque
`ModeVerifiedGateway` created by `verify_gateway_account_mode`; they do not
accept a raw gateway. The value captures early readiness and the expected mode,
then revalidates that mode when consumed. A successful test-mode capability
still executes the real NMI API request, whose processing is controlled by
NMI's account-wide TEST mode.
All matching reservation constructors require the trusted account mode
explicitly rather than defaulting to live.
When a transient final mode query restores a host charge for same-key retry,
`HostChargeTargetStore::ensure_submission_admitted` is invoked again for that
attempt.
Hosts must make that callback repeat-safe and reserve one-shot paid/failed
business effects for `apply_transition`.
Terminal host-charge replay and prepared replay owned by the other deployment
mode resolve by idempotency before `HostChargeTargetStore::preflight_target`.
A same-mode prepared replay invokes the snapshot callback again so a changed
target charge becomes an idempotency conflict before gateway I/O.

Errors returned by host transaction, event, charge-target, and operator-review
callbacks remain opaque through ordinary formatting and the standard
`Error::source()` chain. A host can deliberately recover its original callback
error only by classifying the outer service error, destructuring an owned
callback-error variant, and consuming that wrapper with `into_source()` in a
protected diagnostic path.

`GatewayReadiness` can be returned after a token-free attempt was committed.
At that boundary, transient unavailability leaves the attempt pending for the
same-key retry, while determinate readiness failures first persist their exact
terminal resolution. Replaying the same command and idempotency key returns or
resumes the canonical attempt before host admission or gateway I/O; an error is
not permission to substitute a new key.

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
`StaleTarget`, `Unchanged`, concurrent change, or contended-row outcome leaves
the target and financial attempt state untouched, increments `skipped`, and
does not prevent later candidates from progressing. Candidate selection uses
the attempt's `updated_at` as a durable scheduling claim, so previously skipped
rows sort behind unclaimed work and become retryable after the claim interval.
The host callback should record the target-specific incident for operator
follow-up; the account scheduler can continue its remaining local and exact
reconciliation phases.

An existing 0.2.0 host must add the subscription-charge phase, plus the
host-charge phase when host charges are configured, to its reconciliation loop
when upgrading to 0.3.0. Omitting them leaves abandoned local
rows for foreground reads or later cleanup even though exact reconciliation
correctly excludes never-submitted attempts. The existing enrollment phase also
repairs never-submitted initial attempts that 0.2.0 may already have parked as
`review_required`. The schema-v3 cutover separately preserves historical
combined attempt names as canonical first-name values and adds lossless
last-name persistence for new attempts.

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
