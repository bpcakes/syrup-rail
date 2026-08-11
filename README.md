# Syrup Rail

Reusable subscription billing crates for Banana Pancakes applications.

## Packages

| Crate | Role |
| --- | --- |
| `syrup-rail` | Validated domain types and lifecycle policy |
| `syrup-rail-postgres` | Canonical PostgreSQL schema contract and SQLx orchestration |
| `syrup-rail-nmi` | NMI gateway and lifecycle-evidence adapter |
| `syrup-rail-nmi-client` | Bounded, retry-free raw NMI HTTP client |

## Subscription terms

Hosts select an explicit recurring start or a positive paid introductory
period. Both variants snapshot the recurring price, cadence, dunning policy,
and access policy when enrollment is admitted. The complete
[`subscription_terms` example](crates/syrup-rail/examples/subscription_terms.rs)
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
`Suspended` denies them. The compiled [`entitlement_access`
example](crates/syrup-rail/examples/entitlement_access.rs) shows the host
security boundary and canonical method call.

`MarkUnpaid` makes the subscription terminal after the schedule is exhausted:
it removes renewal and recovery authority and grants no subscription
entitlement. The final transaction appends `SubscriptionPaymentFailed`
followed by `SubscriptionEnded { reason: NonPayment, .. }`. A host should
persist those provider-neutral events in its own transactional outbox and run
product-specific cleanup asynchronously; Syrup Rail does not call host
fulfillment integrations.

`RemainPastDue` instead keeps the financial lifecycle open with no further
automatic payment scheduled. It does not emit `SubscriptionEnded`. When the
access policy is `ContinueUntilDunningExhausted`, the final
`SubscriptionPaymentFailed { disposition: DunningExhausted { exhausted_at } }`
is the host's access-revocation signal: the subscription entitlement changes
from `AllowedDuringDunning` to `Suspended` at `exhausted_at`. Hosts that mirror
access outside Syrup Rail must consume that disposition from their
transactional outbox.

PostgreSQL schema v2 is the current contract. New hosts install
[`schema/v2/install.sql`](crates/syrup-rail-postgres/schema/v2/install.sql),
while v1 hosts follow the checked-in
[`v1` to `v2` cutover guide](crates/syrup-rail-postgres/schema/v2/README.md).

## PostgreSQL host integration

The compiled [`host_integration`
example](crates/syrup-rail-postgres/examples/host_integration.rs) shows the
host-owned offer lock, gateway resolution, end-user admission, transaction and
outbox boundary, service construction, authorized command construction, and
durable enrollment-result handling. It is intentionally provider-neutral and
does not install or run a database migrator.

After the host has authenticated and authorized an exact billing scope,
subscriber, and plan, the same service also exposes `cancel`, `claim_discount`,
and `clear_discount`. Cancellation changes canonical state and appends its
typed event through the host transaction/outbox boundary before committing;
replays and semantic blockers append nothing. Discount claim and clear retain
their typed outcomes, use no gateway or provider I/O, and do not emit billing
events. The host integration example includes compiled helpers for all three
operations.

For customer billing pages, construct a `SubscriptionBillingPortalQuery` from
that same authorized exact identity and call `subscription_billing_portal`. It
returns the canonical `Entitlement`, including current terms and saved or
applied discounts, plus an optional masked-card display.
`subscription_payment_history_page` supplies bounded, cursor-paginated
exact-plan attempt history. These reads deliberately exclude provider
payment-method references, transaction identifiers, contacts, gateway
responses, and raw diagnostics; hosts still own presentation and
authorization.

For automatic renewal dispatches, call `due_renewals_page(pool, None)` and
continue with its returned `RenewalDispatchPageCursor` until no next cursor is
present. PostgreSQL observes the first page's timestamp and the cursor reuses
it for every time-based due, cooldown, stale-update, and retry-window gate,
while strict `(next_payment_attempt_at, subscription_id)` ordering avoids
offset and timestamp-tie gaps or repeats for unchanged candidates. It is not a
cross-page MVCC snapshot: concurrently inserted, retimed, or newly unblocked
candidates behind the continuation key wait for a fresh scan. Persist or
reconstruct cursors only in trusted host code from a prior page—never accept a
cursor from an end user. This is not a lease or queue writer: the host writes
its own outbox/queue record and each eventual renewal still rechecks current
canonical state. `due_renewals` remains the compatible fixed-100 first-page
helper.

Run `cargo check -p syrup-rail-postgres --example host_integration --locked` to
compile the integration boundary without contacting a database or provider.

## Development

- `scripts/jig doctor`
- `scripts/jig check test`
- `cargo test -p syrup-rail-nmi-client`

## Releasing

See [docs/releasing.md](docs/releasing.md) for the local preflight and the
manual, trusted-publishing workflow.

Private Cargo consumers pin one exact Git revision with
`git = "ssh://git@github.com/bpcakes/syrup-rail.git"` and set
`CARGO_NET_GIT_FETCH_WITH_CLI=true` so authentication uses the system Git client.
