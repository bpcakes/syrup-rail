# syrup-rail-postgres

`syrup-rail-postgres` provides Syrup Rail's canonical provider-neutral ledger,
SQLx operations, and high-level subscription billing service. Version 0.5
supports PostgreSQL 18 only and uses schema v4.

```toml
[dependencies]
syrup-rail = "0.5.2"
syrup-rail-postgres = "0.5.2"
```

New hosts install `schema/v4/install.sql` through their normal migration
system. Existing schema-v3 hosts separately commit
`schema/v4/prepare_from_v3.sql` and `schema/v4/validate_from_v3.sql`, run
`schema/v4/index_from_v3.sql` outside a transaction, and finally commit
`schema/v4/upgrade_from_v3.sql` while following the versioned cutover guide.
Schemas v1, v2, and v3 are immutable. The detailed versioned guides explain
the required lock, maintenance, and rehearsal boundaries.

After the host applies its migration and before it serves billing traffic,
verify the runtime catalog:

```rust,no_run
# async fn verify(pool: &sqlx::PgPool) -> Result<(), syrup_rail_postgres::SchemaConformanceError> {
syrup_rail_postgres::assert_runtime_schema_v4_compatible(pool).await?;
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
requests for those flows. Renewals, recoveries, payment-method replacements,
and fresh host charges already performed a final readiness query, so their
request counts do not increase. A transient failure of the new final query
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

For an approved subscription whose current saved method has incomplete card
display, call `SubscriptionBillingService::refresh_payment_method_metadata`
with `RefreshPaymentMethodMetadata::new(billing_scope_id, subscriber_id,
attempt_id)`. The standalone `refresh_payment_method_metadata` operation takes
a pool and host `GatewayResolver` for hosts that do not use the service. Use the
latest approved attempt for that saved method. The host must authorize the
scope and subscriber before calling either entrypoint.
Saved methods can be shared across plans. Use the latest approval for the method
across those plans, even if its original subscription has since switched methods;
another subscription for the same account and subscriber must still use it.
Renewals may approve without echoed vault-reference evidence; their durable
method and subscription linkage is sufficient. A conflicting retained reference
is rejected, and an older approval cannot bypass a newer approved renewal.

Invoke refresh after the approval transaction commits, or enqueue that same
command for explicit historical repair. One invocation performs at most one
read-only exact transaction query, with a 10-second provider timeout and no
internal retry. A complete display returns `Unchanged` without contacting the
provider. Existing durable account/provider cooldowns return `CooldownActive`
before gateway resolution. A rate-limited query extends the existing shared
provider cooldown and returns `Query(RateLimited)`; a failure to persist that
cooldown returns `RateLimitCooldownPersistenceFailed`, retaining both errors.
Back off for either result even when the storage cause is transient.
The provider cooldown pauses financial readiness and renewal dispatch for
**every gateway account with that provider key**, including other billing scopes.
The gateway's coarse rate-limit error does not establish a query-only quota, so
refresh follows the existing conservative policy. NMI documents HTTP 429 as a
[system-wide limit spanning Payment and Query APIs](https://docs.nmi.com/reference/rate-limiting).
The shared cooldown lasts 60 seconds, exposed as
`syrup_rail::GATEWAY_MUTATION_RATE_LIMIT_RETRY_AFTER_SECONDS`.
Budget background backfills
across that provider and prioritize payment traffic. Scheduling/concurrency
limits remain host-owned.
Query errors are independent of payment success: keep the approved
payment result and retry only the refresh operation with host-owned bounded
scheduling and concurrency. Never resubmit a sale or replay financial approval
to backfill display.
Refresh calls `PaymentGateway::query_payment_method_metadata`, whose result has
no financial approval authority. NMI enriches this observation with expiry while
keeping its financial sale and `query_transaction` descriptor normalization
compatible with 0.5.2. An old parked approval can therefore reconcile before its
saved method is enriched. Existing JSON financial expiry remains intact. Other
providers inherit a default projection of their ordinary exact query and may
override the metadata operation independently. Financial evidence equality
remains strict; hosts retain normalized `ProcessorEvidence` for financial retries.
The separate refresh error type does not use the financial service's
`disposition()` method. A host may retry the same refresh with bounded backoff
for storage SQLSTATE `40001`, `40P01`, `55P03`, or `57014`; other storage failures
need investigation. The refresh error's ordinary `Display` and `Debug` omit
details, but its SQLx error source retains database diagnostics for explicit
inspection. Use the top-level format for logs; automatic source-chain formatting
can expose database values. A commit acknowledgement can be ambiguous, so a retry
rechecks current display. Refresh briefly shares the approval lock domain and
locks the gateway-account identity through commit. It can therefore delay both
approval application and account configuration/cooldown writes; keep its
scheduling below payment traffic to avoid contention.

`NotFound` means the exact query returned no transaction, not that display is
complete or the approved payment failed. Hosts may retry with a bounded budget;
the provider gives no visibility-delay guarantee. `Unchanged` means either the
display was already complete or a matching observation supplied no additional
safe fields. Hosts should use the local portal projection to assess remaining
display gaps and record refresh outcomes/errors in their own metrics. Neither
`Updated` nor `Unchanged` alone promises complete display.
Hosts must cap calls per subscriber and across the account, including calls
from user-facing endpoints. Stop automatic retries after that budget; do not
loop until all display fields are present. The resolver may deny unavailable
or deactivated accounts. Refresh itself is a historical read/repair operation
and does not apply financial account-activation admission.

Refresh fills only absent card brand, last four, and expiration fields on the
still-current active method. It checks exact account, subject, transaction,
vault linkage, and method/subscription identity again under locks after the
query. Conflicting evidence leaves display unchanged; a replaced, disabled,
scrubbed, or superseded method returns `Ineligible` before provider I/O. An
initially eligible candidate that changes during I/O returns `ChangedDuringQuery`;
select the latest approved attempt before a bounded retry. Lifecycle-only
reconciliation of that attempt does not invalidate otherwise eligible display.
Charge and attempt
evidence, vault references, access, amounts, renewal
dates, contacts, and events are not changed. Billing portal reads then use the
refreshed local method. This does not backfill historical attempt descriptors.
Each validated expiry component can fill its own absent field. A later approval
that reuses a vault reference replaces the method's display evidence, which may
clear these fields: the vault reference alone cannot prove that the underlying
card stayed the same. Refresh that latest approval to restore its display;
retaining old fields unconditionally could display the previous card.
`Ineligible` deliberately does not distinguish wrong identity from an obsolete
candidate. Verify the supplied scope/subscriber/attempt from authorized canonical
history before treating it as a terminal backfill result. For `EvidenceRejected`,
inspect the conflict through authorized operator tooling; repeated identical
queries cannot repair conflicting stored fields. If the card changed, use the
normal payment-method replacement/re-approval workflow and refresh its latest
approval. Do not clear canonical display columns merely to bypass that guard.

The method retains bounded provider brand text, as the approval writer does;
the core presentation boundary alone converts it to `PaymentCardBrand`.
Unrecognized existing brand text stays untouched and cannot establish a brand
conflict that blocks other absent fields. Known brand, last-four, and expiry
disagreements still reject the entire refresh. Exact transaction correlation
does not require a repeated vault reference: an absent reference and the
`MissingPaymentMethodReference` diagnostic are allowed. All other diagnostics
and conflicting returned references reject the observation. Account-mode
verification remains part of financial admission; refresh does not add a
profile query to its single exact-query budget.

The approved ledger attempt remains the payment authority. A diagnostic-free
`Unknown` query observation can supply display after later lifecycle changes
such as a void; it cannot change the approved payment or entitlement. Explicit
decline/failure observations and contradictory diagnostics are rejected.
The timestamp comparison is deliberately conservative: it detects replacement
away from and back to the same method while the query is in flight. Unrelated
updates may therefore produce `ChangedDuringQuery`; the host's bounded retry policy
applies.

The 0.5.3 card-display repair uses existing schema-v4 columns. Hosts apply no
schema migration for it. Synthetic parser and PostgreSQL fixtures verify the
library behavior; host staging retests and invocation for existing accounts
remain a separate host integration step.

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
