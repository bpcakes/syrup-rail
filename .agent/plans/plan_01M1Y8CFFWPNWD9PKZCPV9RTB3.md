# Repair shared-method refresh eligibility after full-branch review

Follow-up to full-branch review round 1 of integration/0.5.3, base
9f30107eaf13f4a84fa4a1aaa04299246fedcc09, reviewed HEAD 54c582e.
Claude and Codex completed against a clean, matching, complete fingerprint
eb372ea2d0f15b5752a229f7321d9922a73cdbca1a0fb3857d8ebc2ece56826d.

## Progress

- [x] Research and adjudicate review findings.
- [x] Add canonical two-plan and exact-sentinel regressions before the SQL fix.
- [x] Separate latest method approval provenance from current use, retaining
  global supersession and existing lock domains.
- [x] Strengthen timeout classification/bounds and document bounded storage retry.
- [x] Pass focused and full required gates. Commit and branch review follow this checkpoint.

After this implementation checkpoint, continue full-branch review (round 2 of
at most 3) on the resulting clean committed snapshot.

## Decisions

Codex identified a real cross-plan dead end: A and B share M; B owns the latest
approval for M and replaces it with N. M remains active for A but neither
approval is eligible. Separate approval provenance from an EXISTS check that M
is still referenced by a same-account/same-subscriber subscription. Preserve
NOT EXISTS for any later approval globally; narrowing it to current plans
would allow old-card evidence to refill M. The global account/subscriber method
domain serializes all supported pointer-changing approvals, and scrub's own
domain is also held. Keep original attempt/subscription ownership and row checks.
Test both successful repair and the last current reference disappearing during I/O.

Claude's exact scrub sentinel concern is valid for provider-neutral references;
compare against erased:<method UUID>, not every erased: prefix. Add a legitimate
prefix reference regression through real approval.

Research https://docs.nmi.com/reference/rate-limiting explicitly identifies HTTP
429 as system-wide throttling that can span Payment and Query APIs. The existing
GatewayNotSubmittedPolicy::for_readiness_error already maps the same coarse query
error to provider cooldown. Retain that established 60-second policy rather than
invent an independent query quota or an unsafe opt-out. Document the public
GATEWAY_MUTATION_RATE_LIMIT_RETRY_AFTER_SECONDS constant and primary-source basis.
NMI's https://docs.nmi.com/reference/query documents cc_exp; ccexp appears as a
payment request field. No evidence establishes the suggested additional Classic
response aliases, so do not add speculative inputs. Captured staging sale bodies
remain a separate integration check.

Keep fail-closed supersession even for unusable newer approval evidence. A later
approval invalidates old descriptor authority. Ineligible and conflict remedies
are already documented. Account activation remains a host resolver policy for
historical reads; mode lookup does not authenticate merchant credentials.

Strengthen the Never reply test to require QueryTimedOut within a finite outer
budget. Document bounded same-refresh retries for explicit transient SQLSTATEs;
refresh has no financial submission authority, and fill-only revalidation makes
ambiguous commit acknowledgement retryable as a display operation. Do not apply
the financial service error-disposition enum to this separate error type.

## Validation

Use actual enrollment/replacement workflows for two plans on one subscriber and
account. Assert globally superseded A never queries, latest B queries its exact
transaction, only M's display changes for A, and financial rows stay identical.
Run focused PostgreSQL tests, fmt, Clippy, public API, contract, SQLx, and the full
serialized workspace suite. Preserve schemas v1-v4, manifests, dependencies and
append-only state. Keep unrelated Beads changes outside commits.

## Results

Both new regression tests failed against the old candidate SQL (Ineligible
instead of Updated), then passed after the fix. The cross-plan case also covers
the last reference disappearing during provider I/O and confirms the older
approval never reaches the provider. The race setup uses distinct provider
transaction IDs for its two real replacements; reusing one ID correctly caused
canonical approval to reject the fixture. The final focused run passed both
new tests. Public API/doctests and formatting also passed. All gates passed on the final source snapshot: 110 core, 24 NMI adapter,
224 NMI client, and 264 PostgreSQL unit tests (622 total), four example tests,
and four doctests. PostgreSQL tests took 682.73 seconds. Contract and SQLx checks
passed; Clippy, formatting, public API and diff whitespace checks also passed.

The plan-bound batch receipt is receipt_01M1Y9P7VV870Z98R42ZQJA71D; individual
receipts are contract receipt_01M1Y8YAZMQNBTK4F7TXX3G4AJ, SQLx
receipt_01M1Y8ZY527Q8D21ZRWXFHC8D3, and test
receipt_01M1Y9P7T6CAPVTS51YMA3PRF1. Jig reported all required gates fresh before
staging. Versioned schemas and dependency files remain unchanged, and existing
workflow history remains append-only. This closes only the implementation
checkpoint; the correction commit and full-branch round 2 remain the next steps.
