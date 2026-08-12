# Syrup Rail PostgreSQL schema v2

Schema v2 is the current canonical contract. New hosts copy `install.sql`
byte-for-byte into an immutable host migration. Existing schema-v1 hosts copy
`upgrade_from_v1.sql` byte-for-byte into one forward-only transactional
migration; they must not run `install.sql` over v1.

Both installation paths include reader-facing keyset indexes whose complete
shapes are part of the runtime schema contract: table and access method,
uniqueness, ordered keys and null behavior, operator classes, included
columns, predicates, and planner/write readiness. Renewal dispatch forces its
candidate CTE to fold and uses `(next_payment_attempt_at, id)`, making an
early-stopping ordered index plan available without the old unconditional
full-candidate materialization. PostgreSQL plan choice remains cost-based;
rehearse it against representative host data. Exact-plan payment history uses
`(billing_scope_id, subscriber_id, plan_key, created_at, id)`. Change either a
reader order or its index shape in the same pre-release schema version; after
release, add a forward migration instead of rewriting these artifacts.

Before an existing host enters maintenance, run the checked-in read-only
`preflight_from_v1.sql`. Every returned subscription is a blocker whose
`past_due` state lacks both forms of causal history supported by v1: a
submitted determinate automatic-renewal failure, or an operator-reviewed
recovery manually failed from an `active` optimistic subscription snapshot.
The latter is identified by its retained `review_required_at`, terminal
`failed` status, and absent resolution code. Investigate and repair any
returned state under the v1 application contract without inventing processor
evidence. An empty result is advisory only: the upgrade repeats the validation
authoritatively.

Version 1 could create the second form when an operator manually failed an
active recovery. Version 2 accepts that state directly. It preserves the
recovery's earliest resolution timestamp as the immediate-suspension boundary,
does not count the recovery as automatic dunning, and sets
`next_payment_attempt_at` to the existing economic period anchor. The first v2
automatic renewal can therefore be dispatched immediately after cutover when
that anchor is already due. Its result advances dunning from automatic failure
count one even though the subscription was already `past_due`, and later
cancellation or terminal events retain the earlier legacy suspension boundary.
No synthetic renewal attempt or processor evidence is created.

Schema v2 intentionally does not preserve v1's effective retry exhaustion when
that exhaustion depended on subscriber-initiated recovery failures. Version 1
counted determinate renewal and recovery failures together toward its global
five-attempt ceiling. Version 2 counts only submitted determinate automatic
renewal failures against the subscription's dunning schedule. A `past_due`
subscription that stopped dispatching under v1 can therefore receive another
automatic renewal attempt after the upgrade. Recovery failures remain durable
financial history, but they neither consume nor delay automatic dunning in v2.

Before entering maintenance, operators can run the checked-in read-only
`audit_retry_reclassification_from_v1.sql` against v1. It lists `active` and
`past_due` subscriptions for which the old combined count reached the global
ceiling while the new automatic-renewal count is below five, including a zero
automatic count. Returned rows are informational cutover candidates, not
migration blockers; another unresolved attempt, provider cooldown, or
infrastructure cap can still prevent immediate dispatch.

For the cutover, prebuild and verify the 0.2 application, stop every 0.1
billing writer, apply `upgrade_from_v1.sql` in one database transaction, start
0.2, and only then resume dispatch and reconciliation. A failure before commit
rolls back to v1 and permits 0.1 to resume. After commit, keep writers stopped
until 0.2 is restored and roll forward; never restart 0.1 against v2. Durable
pending, unknown, and review-required attempts are preserved and reconciled by
0.2 rather than drained or resubmitted.

Size the maintenance window for both the data rewrite and index construction.
The upgrade replaces the renewal-dispatch index and builds the exact-plan
payment-history index inside the same transaction. PostgreSQL scans the full
`billing_payment_attempts` heap to build that partial index, then stores only
non-host-charge entries. On a large ledger it can be the dominant step, and
`CREATE INDEX` holds a lock that conflicts with billing writes until the
transaction commits. Measure the production table and rehearse the artifact
against a representative copy. Do not edit the packaged migration to use
`CONCURRENTLY`: PostgreSQL cannot run that form inside this atomic upgrade, and
the documented cutover already requires all billing writers to be stopped.

Current policy governs creation of new authority; the durable attempt snapshot
governs completion of authority granted before the cutover. In particular, 0.2
creates recovery attempts only for `past_due` subscriptions, while a preserved
0.1 recovery whose exact expected status is `active` may still be admitted or
reconciled. It must retain the same subscription, period, method, transaction,
and status snapshot; the cutover does not grant fresh recovery authority to an
active subscription.

Prepared v1 initial-enrollment attempts are backfilled with their historical
monthly cadence and default dunning/access policy. Until those prepared
attempts have resolved or expired, the host must continue presenting matching
offer terms for the affected plan and idempotency key; changing those terms is
an intentional replay conflict. Already-submitted initial attempts reconcile
from their durable snapshot and do not require a live offer lookup.

The Rust package exports the exact artifacts as `V2_INSTALL_SQL`,
`V1_TO_V2_PREFLIGHT_SQL`, `V1_TO_V2_RETRY_RECLASSIFICATION_AUDIT_SQL`, and
`V1_TO_V2_UPGRADE_SQL`. After release, all four SQL files are immutable
distribution artifacts. Hosts may add separately named host objects after
installation, subject to the canonical conformance rules.
