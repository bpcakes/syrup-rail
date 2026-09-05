# Syrup Rail PostgreSQL schema v6

Schema v6 is the current development contract. Released schema v5 and every
older artifact remain immutable. New hosts install `install.sql` byte-for-byte
through a host migration. Existing schema-v5 hosts apply `upgrade_from_v5.sql`
once in a single host-owned transaction. Older hosts first follow each shipped
cutover to reach v5. Never run a fresh install over an existing ledger.

Before scheduling the cutover, run `preflight_from_v5.sql` through a read-only
role. It reports the retained attempt population, the rows rewritten to
`absent`, the `review_required` rows that remain `unclassified`, and the
retained processor-charge and reversal-attestation populations. The latter two
counts are the rows classified as `structured` from their existing durable
financial records, supplied by constant column defaults without per-row updates.
Use the counts to size validation scans and decide whether
legacy operator review must be completed before cutover. The
`terminal_host_attempts_with_unclassified_evidence_count` also identifies declined
host attempts and unsubmitted failures without charges whose target admission
changes from safe in v5 to unsafe in v6. The audit includes their target IDs,
statuses, and resolution codes. These terminal attempts cannot be reclassified
by ordinary reconciliation, so their targets cannot be retried or released after
cutover. A recovery or compatibility policy for this population remains a
**0.6.0 release blocker**. The upgrade takes the cutover table locks and aborts
with SQLSTATE `23514` before changing schema when this population is nonempty.
Roll back the host transaction and remain on v5; do not clear evidence or remove
retained attempts to bypass the guard. A later empty query or manual failure
cannot recover these targets. When investigation is
required, run
`audit_unclassified_review_attempts_from_v5.sql` through an authorized operator
process. Its result contains internal identifiers and evidence-presence flags,
but no raw provider strings.

The upgrade adds `gateway_approval_evidence` to payment attempts, processor
charges, and external-reversal attestations. Its closed labels are
`unclassified`, `absent`, `text_only`, and `structured`. The raw NMI parser
derives these signals before reducing fields, and the NMI adapter translates
the typed summary; they never authorize an approved payment.
Raw provider fields remain unchanged. The processor-charge immutability trigger
protects classification with the rest of that observation.

Fresh-install charge and attestation columns default to `unclassified` so an
omitted classification fails closed. Retained attempts become `absent` only
when all decision/reference fields and response text are NULL. New empty
reservations default to `absent`. Every other retained attempt stays
`unclassified`, including local query notes: v5 could have overwritten provider
evidence with those notes, so their apparent local origin cannot establish the
history's safety. Retained processor charges and their reversal attestations
become `structured`: v5 created a charge only from an authoritative approved
outcome or a structured approval field, so the durable charge record supplies
this fact without reparsing provider strings. Raw fields are never rewritten.

For payment-bearing attempts, `Unclassified` always blocks the manual
no-financial-effect exit, even if a parser discarded the original fields. Local
notes and redaction cannot change this classification. Existing transaction
references retain their separate guard. A subscription payment-method update is
a zero-value vault operation: an operator may close it against its retained
subscription snapshot without applying the new method, while preserving its
processor evidence and any zero-value charge observation for reconciliation.
An exact provider query that returns a matching transaction can add a newly
classified observation to an unresolved attempt, but approval evidence remains
monotonic across observations. Empty or malformed query results do not prove
absence and never downgrade retained evidence. If
queries keep returning no record, an unclassified submitted attempt remains open
for investigation; this cutover does not provide a manual override. This also
applies to new v6 indeterminate mutation errors, including an empty transport
diagnostic. Until authoritative transaction or reversal evidence resolves the
attempt, the in-flight uniqueness rules prevent another enrollment for the same
subscriber/plan or another renewal/recovery for the same subscription. This hold
has no automatic time limit when the provider cannot establish the outcome.
Repeated empty queries are not proof of non-submission and do not authorize
resubmission. Hosts must alert on aged review items and investigate with the
provider; changing the classification or bypassing the unique index is not a
supported recovery path. Resolve legacy cases before cutover if that restriction
is operationally unacceptable.
Schema v5 itself wrote local reconciliation notes into `gateway_response_text`;
those rows deliberately remain unclassified because local text cannot prove the
absence of earlier provider evidence. The audit identifies them without exposing
the text so authorized operators can resolve them while still running v5.
The existing independently authorized reversal workflow remains available when
there is charge evidence. After the cutover, charge and attestation
classifications are immutable. This includes transactionless historical charges
that later acquire a transaction identity: their migrated `structured`
classification remains immutable. Third-party adapters must supply an explicit
classification when constructing processor evidence.

Before cutover, prebuild and test the v6-aware application, rehearse the exact
upgrade on a representative copy, and drain billing traffic. The preflight's
empty-attempt count is the row count for the migration's in-transaction
`UPDATE`; include its write and bloat cost in the rehearsal. Retained charges
and attestations use PostgreSQL's constant-default column addition, followed by
a default change for future inserts, so they require no per-row update.
Constraint validation can still scan those tables under the cutover locks. See
[PostgreSQL 18's column-default semantics](https://www.postgresql.org/docs/18/ddl-alter.html).
Stop **all** billing
writers, including reconciliation and operator workers. Set host-appropriate
`lock_timeout` and `statement_timeout`, then run the upgrade transaction. The
ALTERs take ACCESS EXCLUSIVE locks on the three evidence tables until commit;
constraint validation may scan retained rows. Budget the maintenance window
from the rehearsal. A failed transaction rolls back to v5 and permits v5 code
to resume. After commit, roll forward: do not restart v5 writers. This also excludes old
scrub workers: pre-0.6 scrub callers used a different payment-method advisory
lock domain. The shared v6 lock preserves the old approval writer key, but
cannot make an old scrub worker coordinate with a new writer.

Start the new application, call `assert_runtime_schema_v6_compatible(&pool)`
before accepting billing traffic, then resume work. The assertion requires
PostgreSQL 18, fingerprints the complete catalog, and performs no migrations.
The prior v5 assertion remains available for validating the old side of the
cutover; its success does not authorize running v6 queries on v5.

`V6_INSTALL_SQL`, `V5_TO_V6_PREFLIGHT_SQL`,
`V5_TO_V6_UNCLASSIFIED_REVIEW_AUDIT_SQL`, and `V5_TO_V6_UPGRADE_SQL` are
available through the explicit `schema-contract-test-support` feature. Once
released, this guide and the SQL artifacts are immutable.
