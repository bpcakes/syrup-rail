# syrup-rail-nmi-client

Use `Client::query_payment_method_metadata` for enriched saved-card display.
It returns `PaymentMethodMetadata`, which exposes correlation, status,
diagnostics and descriptor parts without financial response evidence. It uses
the same bounded transport and exact-selector validation as `query_transaction`.
The existing financial sale/query APIs retain their 0.5.2 descriptor semantics;
newly recognized XML expiry is confined to the metadata query. Metadata queries
also accept repeated brands that differ only in case, retaining a spelling the
provider sent; financial queries keep the original case-sensitive duplicate
rule. Existing JSON expiry parsing remains unchanged.

`syrup-rail-nmi-client` is a concrete asynchronous client for NMI payment APIs. It
supports account-mode lookup, sales, Customer Vault payment method storage,
individual transaction lookup, and paginated transaction reports. It does not
manage NMI plans or subscription schedules.

```toml
[dependencies]
syrup-rail-nmi-client = "0.6.0"
```

The client never retries mutations. `MutationError::Indeterminate` and
`MutationError::RateLimitedIndeterminate` mean the request may have reached NMI,
so callers must reconcile before attempting a replacement charge. The latter
also tells callers to stop application-level provider I/O after an HTTP 429.
`MutationError::RateLimited` is emitted only for NMI's documented in-band
response-code 301 without transaction or lifecycle evidence and therefore has
`NotSubmitted` certainty. Other not-submitted variants are limited to local
validation/configuration failures, connection-acquisition failures before the
HTTP sender receives the request, and HTTP responses that establish an endpoint
or method rejection. Errors after a connection is acquired remain
indeterminate. Query failures use the separate `QueryError` type and may be
retried under the caller's policy.

Duplicate-check behavior is fixed when an account-bound client is constructed:

```rust
#![deny(deprecated)]

use syrup_rail_nmi_client::{
    Client, ClientFactory, ConfigurationError, Credentials, DuplicateCheck,
    Endpoint,
};

fn account_client(
    factory: &ClientFactory,
    endpoint: Endpoint,
    credentials: Credentials,
) -> Result<Client, ConfigurationError> {
    factory.client_with_duplicate_check(
        endpoint,
        credentials,
        DuplicateCheck::ProcessorConfigured,
    )
}
```

`DuplicateCheck::ProcessorConfigured` omits NMI's optional `dup_seconds` field
and delegates to the account's configured processor policy.
`DuplicateCheck::Window` sends an explicit positive window when the account
owner has verified support for per-transaction overrides; NMI may reject it
when the processor configuration forbids merchant overrides.
Zero is not a valid duplicate-check window and this client never sends
`dup_seconds=0`. The deprecated `ClientFactory::client` constructor now uses
`ProcessorConfigured`, correcting the invalid override sent by releases
through 0.4.0.

`ProcessorConfigured` retains the account's defense-in-depth policy, but any
heuristic duplicate window can reject a legitimate later payment that the
processor considers a duplicate. NMI's public documentation does not define
the matching criteria, so do not assume that a distinct Syrup Rail order ID
prevents that rejection. Choose and validate any explicit positive-window
override per account rather than assuming that every processor accepts it.

When NMI returns the general error `response=3` with [duplicate response code
`430`](https://docs.nmi.com/reference/response-codes), the two decision fields
do not prove non-submission. The client returns
`PaymentStatus::Unknown` with a
`PaymentOutcomeDiagnostic::DuplicateTransactionAtProcessor` diagnostic;
additional diagnostics can describe contradictory decision evidence or
malformed identity fields. Callers must reconcile the durable attempt before
any replacement charge. NMI documents `430` only as a duplicate at the
processor and does not guarantee that no transaction or financial evidence
exists.

NMI likewise documents response codes `400`, `440`, and `441` as processor
errors, plus `420` and `421` as communication errors, without guaranteeing that
the attempted payment had no financial effect. They remain
`PaymentStatus::Unknown` and carry
`PaymentOutcomeDiagnostic::IndeterminatePaymentOutcome`; reconcile them before
any replacement charge. A bare `response=3` or generic `error` state has the
same conservative provenance when no detailed decision resolves it. In
contrast, the more specific gateway or account configuration codes `300`,
`410`, `411`, `460`, and `461` remain determinate failures. The broad
`response=3` field is compatible with either category and does not override the
detailed response code. It is omitted from diagnostics only when compatible
failure evidence wins the complete reduction; if malformed or conflicting
sibling evidence keeps the aggregate outcome unknown, the indeterminate
provenance remains attached.

Raw identity conflicts clear the complete identity bundle because neither
identifier can be associated safely with the response. They make approvals
unknown, but do not overturn an otherwise coherent decline or determinate
failure. A merely missing identity is different: it does not make a present
sibling contradictory. A later adapter may also reject an otherwise
unambiguous identifier under stricter provider-neutral syntax; the NMI adapter
applies the same quarantine rule.

For an order-ID-only exact query, the client accepts a coherent decline or
determinate failure only after the response echoes the requested order ID and
contains exactly one transaction record. A missing or contradictory response
transaction ID remains quarantined and diagnosed, but does not erase that
independent selector binding. The parser distinguishes an absent transaction
ID from a present but invalid one before quarantining the identity bundle, so
these conditions retain distinct diagnostics. Approved query results still
require a usable transaction ID.

`PaymentOutcome::diagnostics()` has set semantics: values are deduplicated and
canonically ordered. That order is not provider chronology or policy
precedence; route by diagnostic membership rather than sequence.

Keep the processor's `duplicateTime` shorter than the host's shortest normal
billing or renewal interval and its minimum replacement-charge interval. If
reconciliation has not found a transaction, the safe default is still to wait
until that duplicate window expires before submitting a replacement. A
separately constructed `Window` client may be used for an explicitly authorized
recovery path only when the account permits merchant overrides; changing the
client does not turn an indeterminate prior attempt into a known
non-submission.

[NMI's sandbox and account Test Mode](https://docs.nmi.com/reference/testing-methods)
simulate transactions without sending them to a payment processor, so they
cannot verify processor-specific duplicate override behavior. Before rollout,
the account owner must inspect the [effective processor
settings](https://docs.nmi.com/reference/add-processor-service)
(`enableDuplicateChecking`, `allowMerchantOverride`, and `duplicateTime`) and
exercise the selected policy in a controlled pre-production account that uses
the same processor configuration. Never use production credentials or real
payment data for that validation. NMI's public payment and processor-setting
documentation does not define a stable response code for a forbidden merchant
override, so the client cannot safely promote an in-band rejection to
`ConfigurationError`. Such a rejection can therefore present as an ordinary
failed payment decision and enter host dunning policy; the pre-production
rollout probe is the configuration gate rather than runtime error inference.

The not-submitted classification for HTTP 401, 403, 404, and 405 assumes the
configured URL is NMI's direct API endpoint. NMI documents 401 and 403 as
authentication or permission/account rejection; 404 and 405 reject the
endpoint or method. Under that direct-endpoint contract they occur before
payment processing. Do not put a forwarding reverse proxy in front of mutation
endpoints under this contract. If deployment ever requires one, proxy-generated
responses must be distinguishable and mapped to indeterminate certainty before
the proxy can be supported.

NMI documents v5 payment HTTP 400 as a `validationError` envelope with
`E_INVALID_SUBMISSION`, a message, and non-empty field-level details, while
payment decisions are returned under HTTP 200. Only that bounded, unambiguous
envelope proves request rejection. An extended, unrecognized, malformed,
unreadable, oversized, or processing-bearing 400 body remains indeterminate.
Classic transaction
validation is documented as an in-band form response under HTTP 200, so a
Classic HTTP 400 is not treated as proof of non-submission either. Decision or
identity evidence in either documented response format on any other non-success
mutation response overrides an HTTP status-derived configuration classification
and remains indeterminate. Only a bounded generic JSON error object containing
recognized error metadata can preserve that status-derived classification;
unknown or extended objects and non-empty top-level arrays fail closed. A
canonical numeric `status` value may repeat the outer HTTP status in a generic
error envelope; unknown, malformed, conflicting, noncanonical, or mismatched
`status` values remain payment evidence. HTTP
422 is not documented for v5 sale and likewise remains indeterminate, without
changing Classic query/report handling. The known pre-processing rate-limit
classification requires the exact textual in-band pair `response=3` and
`response_code=301` in every occurrence and a closed set of fields from NMI's
documented rate-limit response. Unknown fields, card descriptors, processing
containers, unexpected response text, numeric JSON values, surrounding
whitespace, and numeric aliases such as `0301` or `+301` are useful only as
lower-trust diagnostic evidence and cannot authorize a replacement mutation.
When duplicate fields use semantically equivalent spellings, the exposed
evidence deterministically selects a spelling the provider actually sent; an
internal normalization token is never persisted as provider evidence.

`ClientFactory` owns a credential-free connection pool and one eight-report
admission budget shared by every account client and factory clone. A report
permit is held from before submission through bounded body collection and XML
parsing. Calls beyond the eight-report capacity fail locally as
`QueryError::Unavailable` before submission instead of joining an unbounded
waiter queue. Cancellation after parsing starts releases capacity only when
that blocking parse actually stops.
The report-specific cap limits simultaneously buffered 4 MiB report bodies to
32 MiB before XML-tree overhead without making ordinary payment mutations or
small queries queue behind report work. A separate transport policy retains at
most 32 idle connections per host, so report-memory tuning cannot change
payment connection reuse. Each account-bound `Client` owns its private API and
query keys in zeroizing buffers, and keys are never installed as shared default
headers. Provider-supplied text and identifiers are returned as `SensitiveText`,
whose formatting is value-free and whose directly owned buffer is zeroized on
drop.

`SensitiveText` is a disclosure guard, not a trust or validation marker.
Provider text and identifiers remain untrusted; callers must validate and
sanitize exposed values before logging or persistence.

Payment outcomes include payload-free `PaymentOutcomeDiagnostic` values when
provider decision evidence or identifiers are missing, unrecognized, invalid,
or conflicting. Callers may safely use these enums for operational logging and
metrics without exposing the underlying provider values.

Paginated transaction reports likewise carry a payload-free
`TransactionReportDiagnostic` when one well-formed transaction contains
conflicting, invalid, or oversized authority fields. The client retains only
independently resolved identifiers for correlation and clears all condition and
action evidence from that transaction. Malformed XML, an ambiguous response
envelope, or a transaction layout that makes page membership unknowable still
fails the complete page atomically.

Report XML parsing runs on Tokio's blocking executor under the same
eight-report bound, so a near-limit document does not monopolize an async
runtime worker. The admission permit moves into that work and remains held if
its awaiting future is cancelled.

NMI paginates reports by transaction and does not document a separate limit on
the actions accumulated by one transaction. Action cardinality is therefore
independent of the 100-transaction page size. The parser accepts more than 100
actions while bounding aggregate expanded action-record storage to one
additional 4 MiB budget across the report. It rejects an oversized response
before building the XML tree and diagnoses only the transaction that cannot
fit the remaining action-record budget.

An approved sale or exact transaction query always includes a transaction
identifier. Approved Customer Vault creation and payment-method storage also
include a Customer Vault identifier; otherwise the client returns an unknown
outcome with a payload-free diagnostic. Caller-provided request fields have
per-field byte limits and every encoded outbound request has a 16 KiB aggregate
budget that is checked before serialization and network I/O.

Mutation futures are not cancellation-safe. Once a mutation future has been
polled, dropping it does not prove that NMI did not receive or process the
request. The caller must reconcile that mutation and must not blindly retry it.

These controls prevent accidental disclosure through ordinary formatting and
reduce the lifetime of directly controlled secret buffers. They do not erase
copies made by HTTP, JSON, TLS, allocators, the operating system, or callers
after explicit exposure. In particular, v5 request construction copies payment
tokens and Customer Vault identifiers into `serde_json` and request-body
buffers that cannot be reliably zeroized. The controls do not protect against
a compromised process or memory forensics.

This package is source-available under the Elastic License 2.0
(`Elastic-2.0`). See [LICENSE](https://github.com/bpcakes/syrup-rail/blob/master/crates/syrup-rail-nmi-client/LICENSE) for the terms and
[NOTICE.md](https://github.com/bpcakes/syrup-rail/blob/master/crates/syrup-rail-nmi-client/NOTICE.md) for ownership and third-party notices.

`PaymentOutcomeParts::approval_evidence` retains conservative approval signals
from every structured decision and response-text occurrence before duplicate
reduction, identity quarantine, or text truncation. `Structured` and `TextOnly`
never upgrade the authoritative `PaymentStatus`; `Unclassified` means malformed
or unrecognized evidence prevents certifying absence. Consumers must preserve
this summary rather than reclassifying the selected raw fields. This added field
requires callers constructing outcome-part fixtures to provide the summary.
