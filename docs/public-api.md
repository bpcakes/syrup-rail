# Syrup Rail 0.2 public API

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

A `GatewayReadiness` error does not imply that no attempt was committed. For
subscriber and host-charge mutations whose readiness query fails after a
token-free reservation, transient `Unavailable` keeps the prepared attempt
pending, while a determinate `RequestRejected`, `Malformed`, or `Configuration`
failure records the exact terminal resolution before returning the typed
error. Reissue only the same command and idempotency key to recover or resume
the canonical result; terminal replay completes before host admission, gateway
resolution, readiness, or provider mutation. Do not replace the key merely
because the first call returned `Err`.

## Read and scheduler surface

Use `subscription_billing_portal` and `subscription_payment_history_page` for
authorized customer billing pages. A present payment-method display always has
at least one renderable, normalized field; normalized absence is represented by
`None`, not an empty inner value. Use `due_renewals_page` for a bounded,
trusted-host renewal scan. Its cursor is not end-user input, a queue lease, or
a cross-page database snapshot.

The host also owns reconciliation scheduling. For every result from
`reconciliation_gateway_accounts`, run the local cleanup functions for stale
unsubmitted payment-method replacements, subscription charges, enrollments,
and configured host charges before `claim_exact_reconciliation_attempts`.
These phases perform no provider I/O and are safe to repeat; bounded phases
belong in every scheduled pass so skipped locks and backlogs make progress.
Host-charge cleanup requires the host's `HostChargeTargetStore` and changes the
target and canonical attempt atomically. Its summary reports target outcomes
that were safely skipped, so the host can alert on that target while unrelated
work and the account's remaining reconciliation phases continue. Exact
provider queries are reserved for attempts with `submitted_at IS NOT NULL`.
Hosts upgrading from 0.2.0 must add the subscription-charge and host-charge
cleanup phases to their existing loop when applicable.

Use `assert_runtime_schema_v2_compatible` after host migrations and before
serving billing traffic. Version 0.2 supports PostgreSQL 18 and schema v2 only;
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
re-exports, builds warning-free all-feature documentation, and runs all-feature
doctests. It does not maintain a compiler-diagnostic debt snapshot or parse
human compiler output. Add meaningful documentation at an owning abstraction,
and expand `warn(missing_docs)` to another module only after that module is
ready to stay clean.
