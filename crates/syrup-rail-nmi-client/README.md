# syrup-rail-nmi-client

`syrup-rail-nmi-client` is a concrete asynchronous client for NMI payment APIs. It
supports account-mode lookup, sales, Customer Vault payment method storage,
individual transaction lookup, and paginated transaction reports. It does not
manage NMI plans or subscription schedules.

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

The not-submitted classification for HTTP 404 and 405 assumes the configured
URL is NMI's direct API endpoint: those statuses prove that NMI rejected the
endpoint or method before processing a payment. Do not put a forwarding reverse
proxy in front of mutation endpoints under this contract. If deployment ever
requires one, proxy-generated responses must be distinguishable and mapped to
indeterminate certainty before the proxy can be supported.

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
