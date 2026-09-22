# Integrating Syrup Rail

This guide covers subscription terms, entitlement decisions, and the PostgreSQL
host boundary for the current source tree. Start with the
[repository README](../README.md) for installation and prerequisites.

## Subscription terms

Hosts select an explicit recurring start or a positive paid introductory
period. Both variants snapshot the recurring price, cadence, dunning policy,
and access policy when enrollment is admitted. The complete
[`subscription_terms` example](../crates/syrup-rail/examples/subscription_terms.rs)
is compiled with the workspace and can be run with
`cargo run -p syrup-rail --example subscription_terms`.

Hosts with subscriber-specific trial eligibility implement
`SubscriptionOfferStore::lock_enrollment_offer`. Syrup Rail supplies the same
typed reservation identity at the reservation and final-admission stages,
including the in-flight attempt ID. Eligibility queries over attempt history
must exclude that ID so the prepared enrollment does not disqualify itself.
The callback uses only the supplied transaction connection.

Each dunning delay is relative to the preceding submitted, determinate
automatic-renewal failure. In the example, collection occurs at the economic
period boundary, then one day after the first customer-payment failure, then
three days after the second. The example uses the checked
`DunningRetryDelay::days` and `DunningSchedule::from_delays` APIs rather than
unlabelled second counts. User recovery, unknown outcomes, provider throttling,
and failures before submission do not consume those steps; infrastructure
retries retain their separate bounded pacing.

`Entitlement::PastDue` is a payment-state fact, not an access denial by itself.
Use `Entitlement::permits_product_access()` for the canonical subscription
decision after the host has authenticated and authorized its subject.
`AllowedDuringDunning` continues both reads and protected writes, while
`Suspended` denies them. The compiled
[`entitlement_access` example](../crates/syrup-rail/examples/entitlement_access.rs)
shows the host security boundary and canonical method call.

Gateway account mode constrains payment mutations. Entitlement reads and
guards default to live paid subscriptions; test workers select `Test`, while
trusted administrative tooling can opt into both modes explicitly. Current-
subscription, billing-portal, and payment-history reads remain mode-neutral,
so hosts that mix modes still need a trusted environment or tenant partition
around those surfaces. Separate databases or merchant accounts remain the
simplest hard isolation boundary.

For a protected write, start an `EntitlementWriteTransaction` from the pool,
make any preparatory host writes through its connection, and pass it by value
to `require_entitlement_for_update`. The pending transaction cannot commit.
Only successful admission returns an `AdmittedEntitlementWriteTransaction`
with its entitlement locks held. Completed denial and database failure await a
full rollback; cancellation queues the owned transaction's rollback. Perform
and commit the host-owned protected mutation only with the admitted value, and
finish any nested savepoint before consuming that value. The compiled
[`host_integration` example](../crates/syrup-rail-postgres/examples/host_integration.rs)
shows this fail-closed ownership boundary.

`MarkUnpaid` makes the subscription terminal after the schedule is exhausted:
it removes renewal and recovery authority and grants no subscription
entitlement. The final transaction appends `SubscriptionPaymentFailed`
followed by `SubscriptionEnded { reason: NonPayment, .. }`. A host should
persist those provider-neutral events in its own transactional outbox and run
product-specific cleanup asynchronously; Syrup Rail does not call host
fulfillment integrations.

Every `SubscriptionPaymentFailed` carries one closed
`SubscriptionPaymentFailureOutcome`. Its `access()` projection is the canonical
product-access fact immediately after the failure; it already accounts for the
subscription's snapshotted access policy and causal failure history. In
particular, an immediate-suspension retry carries the original access boundary
even though automatic dunning remains open. Hosts must not reconstruct this
decision from current offer configuration.

`RemainPastDue` instead keeps the financial lifecycle open with no further
automatic payment scheduled. It does not emit `SubscriptionEnded`. When the
access policy is `ContinueUntilDunningExhausted`, the final
`SubscriptionPaymentFailed { outcome: DunningExhausted { exhausted_at,
access_ended_at } }` records both facts at the same boundary: the subscription
entitlement changes from `AllowedDuringDunning` to `Suspended`. Hosts that
mirror access outside Syrup Rail must consume the outcome's access projection
from their transactional outbox.

PostgreSQL 18 is the only supported database major, and schema v5 is the
current contract. New hosts install
[`schema/v5/install.sql`](../crates/syrup-rail-postgres/schema/v5/install.sql).
Existing schema-v4 hosts follow the checked-in
[`v4` to `v5` cutover guide](../crates/syrup-rail-postgres/schema/v5/README.md).
Hosts on an older schema must first follow the immutable versioned artifacts
to reach schema v4, then perform the v4-to-v5 cutover.

## PostgreSQL host integration

The compiled [`host_integration` example](../crates/syrup-rail-postgres/examples/host_integration.rs) shows the
host-owned offer lock, gateway resolution, end-user admission, transaction and
outbox boundary, service construction, authorized command construction, and
durable enrollment-result handling. It is intentionally provider-neutral and
does not install or run a database migrator. Its versioned event envelope
separates database-assigned first-write facts from the complete replay-stable
contract. Semantic-key conflicts are accepted only when schema version,
billing subject, event kind, semantic key, and payload all match; JSONB payload
equality is structural because PostgreSQL does not retain source bytes. The
example compares untouched persisted values before typed decoding, whose exact
round trip rejects unknown or normalized fields. Its `Debug` output exposes
only schema version and event kind, never subject identifiers or payload
values. The V1 phase and card labels are host-owned, so later core display
changes cannot alter already-versioned wire data. Card brands in customer and
event projections use a closed provider-neutral vocabulary; unknown provider
text becomes `other` rather than being copied into the host payload.

After the host has applied its v5 install or forward-only v4-to-v5 upgrade,
call `assert_runtime_schema_v5_compatible(&pool).await` during process startup and
before accepting billing traffic. The assertion checks the complete canonical
v5 catalog and fingerprint inside one repeatable-read, read-only transaction.
It first rejects every PostgreSQL major other than 18. Separately named
host-prefixed tables, constraints, indexes, functions, and triggers are valid
extension points, but canonical table and view columns are closed: adding even
a host-prefixed column to a canonical relation is unsupported and fails the
fingerprint check. The assertion also fails closed for v1, v2, v3, or other
canonical drift. It never executes install, upgrade, preflight, or audit SQL.
Hosts remain responsible for applying and coordinating their own migrations.
The compiled host integration example includes a default-feature helper for
this startup check.

An active `REINDEX CONCURRENTLY` may temporarily create invalid `_ccnew` or
`_ccold` indexes. The assertion tolerates only shadows whose lock owner is also
visible to the validating database role in `pg_stat_progress_create_index` as
a non-initializing concurrent reindex, and it rechecks that evidence before
committing. PostgreSQL hides those progress details across roles unless the
observer has statistics privileges, so use the same database role for startup
validation and maintenance when uninterrupted startup during reindexing is
required. Otherwise validation deliberately fails closed. A failed reindex can
leave a stale invalid shadow; drop that shadow according to PostgreSQL's
`REINDEX` recovery guidance before accepting billing traffic.

After the host has authenticated and authorized an exact billing scope,
subscriber, and plan, the same service also exposes `cancel`, `claim_discount`,
and `clear_discount`. Cancellation changes canonical state and appends its
typed event through the host transaction/outbox boundary before committing;
replays and semantic blockers append nothing. Discount claim and clear retain
their typed outcomes, use no gateway or provider I/O, and do not emit billing
events. The host integration example includes compiled helpers for all three
operations.

For a failed high-level billing command, hosts can branch on
`SubscriptionBillingServiceError::disposition()` instead of matching internal
error variants. The disposition enum is non-exhaustive, so consumer matches
must retain a conservative wildcard. `is_retryable()` means it is safe to
resubmit the **same idempotent command and key** later; it does not guarantee
success. A transient provider `Unavailable` result that is provably
not-submitted restores prepared work when that flow supports replay, just like
a transient final account-mode query. `retry_after()` returns an exact delay
only for admission denial.
Gateway and account cooldowns are temporarily unavailable but deliberately do
not receive a fabricated delay. Conflicts are not retryable as-is: reload and
rebuild against current authority, or reconcile the existing idempotency key.
Provider-free cancellation and discount transactions also preserve pool
acquisition timeouts and PostgreSQL's serialization, deadlock, lock-timeout,
and statement-timeout conditions as `StorageTemporarilyUnavailable`; replaying
those idempotent operations is safe. Generic storage faults and failures on
paths that may have crossed provider I/O remain `Internal` because their
outcome is ambiguous.

Host callback error wrappers also stop the ordinary `Error::source()` chain
before the arbitrary application error. Classify the outer service error
first; use `into_source()` only after destructuring an owned callback wrapper
in a protected path that deliberately inspects that potentially sensitive
value.

For customer billing pages, construct a `SubscriptionBillingPortalQuery` from
that same authorized exact identity and call `subscription_billing_portal`. It
returns the canonical `Entitlement`, including current terms and saved or
applied discounts, plus an optional masked-card display.
`subscription_payment_history_page` supplies bounded, cursor-paginated
exact-plan attempt history. These reads deliberately exclude provider
payment-method references, transaction identifiers, contacts, gateway
responses, and raw diagnostics; hosts still own presentation and
authorization.

For automatic renewal dispatches, a mode-specific worker calls
`due_renewals_page_for_mode(pool, required_mode, None)` and continues with its
returned `RenewalDispatchPageCursor` until no next cursor is present. Keep the
same required mode for the whole cursor chain; the cursor records it and rejects
cross-mode reuse. A central router that owns both modes can instead call
`due_renewals_page(pool, None)`. A dedicated mode-leading index prevents one
mode's work from consuming the other worker's bounded page.
PostgreSQL observes the first page's timestamp and the cursor reuses it for
every time-based due, cooldown, stale-update, and retry-window gate, while
strict `(next_payment_attempt_at, subscription_id)` ordering avoids offset and
timestamp-tie gaps or repeats for unchanged candidates. It is not a cross-page
MVCC snapshot: concurrently inserted, retimed, or newly unblocked candidates
behind the continuation key wait for a fresh scan. Persist or reconstruct
cursors only in trusted host code from a prior page—never accept a cursor from
an end user. This is not a lease or queue writer: the host writes its own
outbox/queue record and each eventual renewal still rechecks current canonical
state. `due_renewals` remains the compatible fixed-100 first-page helper; use
`due_renewals_for_mode` for a mode-specific fixed-100 first page. Each scan
first applies subscription/account/provider gates, then probes only each
eligible subscription's exact current-period attempt history; unrelated
historical attempts are not globally aggregated on every page.

Run `cargo check -p syrup-rail-postgres --example host_integration --locked` to
compile the integration boundary without contacting a database or provider.
