# Syrup Rail 0.6.0 public API

Syrup Rail's four crates are released at one version and form one layered API.
Every root export is explicit: adding or removing a public symbol requires an
intentional facade edit rather than being pulled in by a wildcard re-export.

## Primary host surface

Most applications should construct `syrup_rail_postgres::SubscriptionBillingService`
and use its enrollment, recovery, renewal, payment-method replacement,
subscriber cancellation, discount, reconciliation, and optional host-charge
methods. The host supplies four policy boundaries:

- `SubscriptionOfferStore` owns host plan rows and subscriber-specific offer
  eligibility;
- `GatewayResolver` constructs a short-lived provider adapter from host-owned
  credentials;
- `EndUserMutationAdmission` owns authenticated-host abuse controls; and
- `BillingTransactionCoordinator` locks the authorized host subject first and
  appends typed events to the host outbox on the same transaction.

The compiled `syrup-rail-postgres` `host_integration` example is the canonical
composition guide. `SubscriptionBillingServiceError::disposition()` is the
stable operational classification boundary; callers retain a wildcard because
the error and disposition enums are non-exhaustive.
`GatewayNotSubmittedError` is deliberately closed and exhaustive instead:
every new variant is a semver-breaking change that forces persistence adapters
to classify retry safety, durable resolution, cooldown scope, and host-target
effects together.

`SubscriptionBillingService` requires an exact live gateway account by
default. A host can require `GatewayAccountMode::Test` for a trusted test
deployment with `with_required_gateway_account_mode`; the exact requirement
also rejects an observed live account before submission.
The required mode is persisted on each attempt and on the subscription created
by an approved enrollment. Prepared retries under the other deployment mode
fail closed, while terminal replays remain readable. Renewal dispatches expose
the subscription mode so hosts can route each job to a matching service;
renewal, recovery, and payment-method replacement reservation revalidate it
before creating new provider work. Subscription mode is write-once authority:
there is no supported re-authorization API that changes an existing
subscription from test to live or vice versa. Route it to a matching service.
Because account-mode lookup and mutation are separate provider requests,
serialization in one host process cannot cover out-of-process Merchant Portal
toggles. Use separate test and production merchant accounts for hard isolation.
The high-level service queries mode before final admission, carries the expected
mode in a non-cloneable capability, and queries again when that capability is
consumed immediately before provider submission. Automatic renewals check
before reservation, recheck the resulting durable attempt before admission,
and consume the capability for a final query immediately before the sale. New
host charges perform their early check before creating or admitting work;
flows with an existing token-free reservation verify against that durable
attempt.
The final check narrows the race window but does not make the separate NMI
requests atomic. It changes the provider request shape from `query → mutation`
to `query → query → mutation` for initial enrollments and prepared host-charge
replays. Renewals use `query → reservation → query → admission → query → mutation`;
recoveries, payment-method replacements, and fresh host charges retain their
existing readiness boundaries. Because the final
query runs after durable admission but before the mutation endpoint, a
transient failure atomically restores resumable enrollment, recovery,
payment-method replacement, and host-charge attempts to prepared state. Retry
those operations with the same command and idempotency key. Automatic renewal
retains terminal not-submitted handling because it does not resume prepared
attempts.
The same restoration applies to a mutation-transport
`GatewayNotSubmittedError::NotTransmitted`: both paths prove that provider
submission did not occur, so both retain the canonical idempotency key for
same-key retry when the flow supports prepared replay.
Both modes execute the real provider API. The mode check determines whether
submission is authorized; NMI's account setting determines whether that real
request is simulated or processed live.

A `GatewayReadiness` error does not imply that no attempt was committed. For
subscriber mutations and prepared host-charge retries whose readiness query
runs against a token-free reservation, transient `Unavailable` keeps the
prepared attempt pending, while a determinate `RequestRejected`, `Malformed`,
or `Configuration` failure records the exact terminal resolution before
returning the typed error. New host charges and automatic renewals verify before
reservation, so their readiness errors create no attempt. Reissue only the same
command and idempotency key to recover or resume a canonical prepared result;
terminal replay completes before host admission, gateway resolution, readiness,
or provider mutation. Do not replace the key merely because the first call
returned `Err`.

The high-level result shape intentionally differs for determinate account-mode
mismatch. At the pre-admission readiness boundary, subscriber enrollment,
recovery, and payment-method replacement return the canonical terminal failed
payment as `Ok`, preserving their existing subscriber-flow contract. A mismatch
caught by the final post-admission check returns
`Err(GatewayNotSubmitted(AccountModeMismatch))` after persisting that same
terminal outcome. A host charge returns `Err(GatewayReadiness)` after atomically
resolving the never-submitted attempt and releasing its host-owned target. In
every case the durable outcome is terminal and no provider mutation was
submitted.

At the low-level enrollment boundary, both reservation and submission
admission report `GatewayAccountModeChanged` without resolving the prepared
attempt. The mode check runs before the host offer callback, so a wrong-mode
worker cannot invoke host admission policy or destroy work owned by the
matching deployment.
Recovery, renewal, and payment-method replacement reservations likewise return
their typed `GatewayAccountModeChanged` conflicts. Their admission values can
only be created by a successful matching-mode reservation, so a later identity
mismatch is an internal invalid state rather than a second typed mode conflict.

Mode does not split the subscriber/plan write aggregate. Existing
subscriptions and grants block enrollment across modes, saved discount claims
are shared, and cancellation/deletion blockers remain mode-neutral. Hosts that
need test work to have no effect on live subscriber lifecycle must isolate the
tenant/database or use synthetic test subscribers.

## Read and scheduler surface

Use `subscription_billing_portal` and `subscription_payment_history_page` for
authorized customer billing pages. A present payment-method display always has
at least one renderable, normalized field; normalized absence is represented by
`None`, not an empty inner value. Use `due_renewals_page_for_mode` for a
bounded, trusted-host renewal scan owned by one account-mode worker, and retain
that mode for the entire cursor chain. The typed cursor records the filter and
returns `CursorModeMismatch` if reused with another mode or an all-mode scan.
Use `due_renewals_page` only for a central router that intentionally returns
both modes. A dedicated mode-leading index filters before the SQL limit, so
work for another mode cannot consume the worker's page. A cursor is not
end-user input, a queue lease, or a cross-page database snapshot.

`EntitlementQuery` and `EntitlementGuard` default to live paid subscriptions;
use `with_required_gateway_account_mode(Test)` for test workers or the explicit
`across_gateway_account_modes` opt-in for trusted cross-mode tooling.
Host-issued grants remain mode-neutral. Current-subscription, portal, and
history reads are intentionally cross-mode and still require a trusted
environment or tenant partition when their mode matters.

The host also owns reconciliation scheduling. For every result from
`reconciliation_gateway_accounts`, run the local cleanup functions for stale
unsubmitted payment-method replacements, subscription charges, enrollments,
and configured host charges before `claim_exact_reconciliation_attempts`.
These phases perform no provider I/O and are safe to repeat; bounded phases
belong in every scheduled pass so skipped locks and backlogs make progress.
Host-charge cleanup requires the host's `HostChargeTargetStore` and changes the
target and canonical attempt atomically. Its summary reports target outcomes
that were safely skipped; a durable scheduling claim places those rows behind
unclaimed work without changing their financial outcome, so the host can alert
on that target while unrelated work and the account's remaining reconciliation
phases continue. Exact provider queries are reserved for attempts with
`submitted_at IS NOT NULL`.

The host also owns operator-review scheduling and alerting. Page
`attempt_review_page` on a bounded cadence, monitor both backlog size and the
age of the oldest item, and route authorized decisions through the exported
operator-review workflows. Syrup Rail owns the durable review state and atomic
resolution primitives; it does not run workers, send alerts, authenticate
operators, or provide an operator interface.
`fail_review_required_attempt` is the bounded exit for an authorized operator
who has independently established that an attempt with no gateway reference
and no approval evidence had no financial effect. Attempts with either kind of
evidence remain open for stronger reconciliation or reversal evidence rather
than being expired automatically.

Hosts upgrading from 0.2.0 must add the subscription-charge and host-charge
cleanup phases to their existing loop when applicable.

Use `assert_runtime_schema_v5_compatible` after host migrations and before
serving billing traffic. Version 0.6.0 supports PostgreSQL 18 and schema v5 only;
the assertion is read-only and does not install or upgrade a schema. It
tolerates concurrent-reindex shadows only when the validating role can observe
the matching `pg_stat_progress_create_index` details; cross-role maintenance is
fail-closed unless the observer has PostgreSQL statistics privileges.

## Typed domain and advanced transaction-local surface

The `syrup-rail` crate owns validated values, immutable command snapshots,
gateway ports, and closed lifecycle facts used by the primary facade. Its root
exports are supported because hosts need them to construct commands or
implement the required ports.

The lower-level `syrup-rail-postgres` functions are supported composition
points for hosts that already own a larger SQL transaction. Their transaction,
lock-order, replay, and provider-I/O constraints are part of the API contract;
prefer the high-level service unless that composition is required.
Every lower-level `submit_admitted_*` function requires a non-cloneable
`ModeVerifiedGateway` minted by `verify_gateway_account_mode`. Mint it as late
as practical before submission and consume it immediately. Within these
ledger-aware submission functions, the capability's mode and gateway identity
must match the durable attempt, so callers cannot bypass the account-mode guard
while applying an admitted attempt. `ResolvedGateway` still exposes raw gateway
mutations for host-owned composition outside these ledger-aware APIs; those
calls do not inherit the attempt-bound guard or ledger guarantees.
`GatewayPaymentOutcome::status()` is nevertheless the authoritative payment
decision for every adapter. Attaching an indeterminate, conflicting, malformed,
unrecognized, missing, unmapped, or processor-duplicate decision diagnostic
monotonically forces `Unknown`; replacing diagnostics cannot restore a terminal
status. Identity diagnostics instead describe field usability and quarantine
the corresponding field from the outcome. A missing required identity leaves
an approval visible so its workflow can park the incomplete approval. An
invalid or conflicting identity prevents the approval from remaining
authoritative while leaving a determinate decline or failure terminal;
third-party adapters do not need to duplicate that status policy.
Reconciliation never restores a missing or quarantined identity into an
approving observation. Every payment workflow compares new identity evidence
with the durable attempt under its canonical application lock. A conflict
leaves the attempt unresolved; an otherwise approving conflicting observation
is recorded in the processor-charge ledger as reconciliation-required rather
than mutating subscription or host-target state. For unresolved non-approved
observations the application may retain a compatible durable identity, but the
current observation supplies the complete decision and descriptor bundles;
fields from separate processor observations are not combined into synthetic
approval evidence. Already-terminal attempts compare the raw observation with
their durable evidence so a sparse observation cannot become an exact replay;
identity conflicts annotate the current call without rewriting that durable
winner, and a conflicting approval is retained for external reversal. If that
charge was first recorded as reconciliation-required, replay after the attempt
becomes approved promotes it into the external-reversal review queue. Parking a
late approval likewise preserves an established attempt observation rather
than replacing it with sparse or conflicting fields. The raw approval remains
available in the processor-charge ledger. A payment-method-reference conflict
on the same known processor transaction instead annotates the call without
reclassifying its existing applied charge; terminal late-approval paths retain
the same diagnostics while preserving their reversal evidence.
Enrollment and host-charge application results expose the current call's
diagnostics through `observation_diagnostics()`. Those annotations do not
reinterpret an already durable attempt status: for example, replaying a
duplicate observation after an approval was durably applied returns the
approved result annotated with that observation. This separates uncertain
provider observations from authoritative durable replay instead of weakening
the adapter boundary.
Low-level recovery, renewal, and payment-method replacement reservation
functions likewise require an explicit `GatewayAccountMode`; there is no
implicit live-mode reservation API. Enrollment, host-charge, recovery,
renewal, and payment-method replacement reservation outcomes distinguish a prepared
attempt's `GatewayAccountModeChanged` conflict from other gateway
configuration drift; the high-level service intentionally normalizes both to
`GatewayConfigurationChanged`. Typed low-level mismatch errors preserve the
required and observed modes for protected operator diagnostics; the
subscriber-facing facade and durable evidence deliberately use generic copy.

`HostChargeTargetStore::ensure_submission_admitted` is a repeat-safe
revalidation hook, not a one-shot business transition. A transient final
account-mode query can
restore an admitted host charge to prepared state without releasing its target;
same-key retry then invokes `ensure_submission_admitted` again for the same
attempt. Return the same admitted snapshot while the target and expected charge
remain unchanged, and apply one-shot paid/failed effects only in
`apply_transition`.
Target callbacks retain the target-before-attempt lock order. Consequently an
approved evidence replay may invoke `Paid` before the canonical attempt is
locked. Keep that transition monotonic: a target already advanced to reversed
returns `StaleTarget` and must not move backward. Once the attempt lock proves
the original approval committed, Syrup Rail treats that refused callback as a
canonical replay; first-time approval still requires `Applied` or
`ExactReplay`.
Determinate failures before provider submission use the distinct
`ReleasedBeforeSubmission` transition; `PaymentFailed` means the mutation was
submitted. An already-active cooldown is determinate and releases an existing
claim. A transient `Unavailable` result that is provably not submitted instead
restores prepared work and retains the claim when the flow supports same-key
replay.
`HostChargeTargetStore::preflight_target` is a side-effect-free snapshot hook.
The ledger resolves terminal replay and wrong-mode prepared replay before
invoking it. A same-mode prepared replay invokes it again and requires the
current snapshot even for an idempotent contender, so changed host-owned
economics become a conflict before gateway I/O.

Gateway lifecycle reconciliation reports
`GatewayLifecycleApplyOutcome::HostTargetTransitionSkipped` when a host refuses
a full-reversal transition. The attempt update is rolled back, first-seen
evidence is durably staged before the provider cursor may advance, and the
batch continues. Count these through
`GatewayLifecycleReconciliationSummary::skipped()` and repair the host target;
a first-seen outcome also contributes to `staged()`. A retry of an already
staged row contributes only to `skipped()`; in both cases the evidence remains
safe to retry. More generally, `staged()` counts newly inserted pending rows,
not classifications: redelivery of an already-staged no-match or ambiguous
report also contributes zero.

Transient final account-mode query failures emit a warning on the
`syrup_rail::gateway_control_plane` tracing target after the prepared-state
restoration commits. Operators should count or alert on repeated restoration
events; the attempt remains unresolved so the durable payment ledger does not
misrepresent a provider outcome that never occurred.
Cooldown persistence that discovers a rotated, removed, or mismatched gateway
identity emits a warning on `syrup_rail::gateway_cooldown` and deliberately
skips the advisory throttle instead of stranding the financial attempt. Alert
on repeated events for the same account or configuration: they can indicate a
systematically stale resolver identity rather than an ordinary rotation.
An error on the same tracing target means provider-scoped cooldown storage is
missing. Treat it as a schema/configuration invariant breach; the financial
attempt is deliberately left unresolved rather than resolving without the
required shared throttle.
Also monitor repeated advances of durable account/provider
`rate_limited_until`: replaying a still-unapplied throttled outcome deliberately
refreshes the fail-safe cooldown window and can pace unrelated work sharing the
same merchant boundary.
NMI throttle scopes intentionally differ by signal. An HTTP 429 is documented
as a system-wide transport throttle and does not prove whether the mutation was
processed, so `RateLimitedIndeterminate` applies provider-wide cooldown. An
in-band payment response code 301 proves non-submission for one resolved
merchant account, so `GatewayNotSubmittedError::RateLimited` applies only that
account's cooldown. This precise split is available on the mutation path. The
account-mode query API exposes one `GatewayError::RateLimited` category; because
the NMI query adapter cannot retain the original signal, readiness failures use
conservative provider-wide cooldown.
Host-target refusals emit a warning on `syrup_rail::host_charge_target` with the
attempt, target, boundary, and typed outcome. Alert on these events: the
financial attempt deliberately remains unresolved until the host repairs or
explicitly reconciles its target state.

## Events and compatibility

`BillingEvent` is deliberately closed and exhaustive. A host mapper should fail
to compile when a future event requires a new durable representation. It is not
a Serde wire schema. Map it into a host-owned envelope with an event ID,
database-observed occurrence time, subject, host kind/version, semantic key,
and minimized payload. The host integration example provides an exhaustive
version-1 mapping. Its envelope separates first-write facts (`event_id` and
`occurred_at`) from a typed replay contract. On a semantic-key conflict, every
replay-contract field must match: schema version, billing subject, event kind,
semantic key, and payload. The first-write facts need not match. JSONB equality
is structural rather than byte-for-byte because PostgreSQL normalizes JSONB.
The example's `append_host_billing_event_v1` helper demonstrates the complete
atomic path: insert with `ON CONFLICT DO NOTHING`, select the conflicting row
on the same transaction connection, compare its untouched split columns and
payload, and only then reconstruct the private typed payload. Reconstruction
must round-trip exactly, so unknown or normalized fields fail closed. The V1 DTO
also owns its phase, end-reason, and card-brand enums; future changes to core
display labels cannot silently rewrite this durable wire version. Its `Debug`
implementations expose only schema version and event kind so subject
identifiers, masked card data, and other payload values cannot leak through
ambient formatting.

Private fields plus checked constructors prevent invalid domain states.
Ordinary `Debug` and `Display` output for secrets, provider identifiers,
contacts, diagnostics, and card-display values is redacted or value-free.
Explicit `expose` methods are boundary operations and must not be used for
ambient logging. Provider card-brand evidence is reduced to the closed
`PaymentCardBrand` vocabulary before it enters a customer display or host
event; unknown nonempty values become `Other`, and the original provider text
is not retained by those projections. Provider adapters retain exact evidence
and conformance-test their documented labels; the NMI adapter includes the
provider's `diners` spelling for `DinersClub`.

Host callback failures can contain arbitrary application data. The public
host-error wrappers therefore keep ordinary `Display` and `Debug` value-free
and terminate the standard `Error::source()` chain before the arbitrary host
error. A host that deliberately needs its original error must consume the
wrapper through `into_source()` and handle the result as sensitive data. For a
high-level service failure, classify it before consuming it, then destructure
only the callback variants in an explicitly protected operator path:

```rust,ignore
let disposition = error.disposition();
match error {
    SubscriptionBillingServiceError::BillingTransaction(error) => {
        let sensitive_source = error.into_source();
        inspect_in_protected_operator_telemetry(disposition, sensitive_source);
    }
    SubscriptionBillingServiceError::BillingEvent(error) => {
        let sensitive_source = error.into_source();
        inspect_in_protected_operator_telemetry(disposition, sensitive_source);
    }
    _ => {}
}
```

Generic error reporters and `source()` walkers intentionally stop at the
redacted wrapper; this differs from 0.1 and is not a missing source link.

Breaking public API or event-shape changes require a semver release and a
changelog migration note. Schema artifacts and queued host wire events have
separate forward-compatibility obligations and must never rely on Rust debug or
enum serialization layout.

## Documentation enforcement

The closed event API, high-level billing service, and host transaction boundary
enable the missing-rustdoc warning directly in their owning Rust modules. The
CI documentation command promotes warnings to errors, without shipping a hard
lint level that could make a future compiler lint expansion break consumers.
These consequential downstream integration contracts currently have no
missing documentation.

The older low-level surface predates that policy and is too broad for
mechanical one-line comments to improve it. For this small project, the gate is
intentionally simple: `scripts/check-public-api.sh` verifies that every
expected root facade exists and is readable, rejects wildcard public
re-exports, and builds warning-free documentation and runs doctests with both
default features and all features. It does not maintain a compiler-diagnostic
debt snapshot or parse human compiler output. Add meaningful documentation at
an owning abstraction, and expand `warn(missing_docs)` to another module only after that module is
ready to stay clean.
