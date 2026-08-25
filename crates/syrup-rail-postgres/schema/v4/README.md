# Syrup Rail PostgreSQL schema v4

Schema v4 is the current canonical contract. New hosts copy `install.sql`
byte-for-byte into an immutable host migration. Existing schema-v3 hosts stop
all billing writers, drain billing traffic, and copy `upgrade_from_v3.sql`
byte-for-byte into one forward-only transactional migration. Do not run
`install.sql` over v3 and do not edit any shipped artifact under `schema/v1`,
`schema/v2`, or `schema/v3`.

Version 3 allowed two external-reversal resolution tuples that the typed Rust
model cannot represent: an initial-current-grant conflict paired with a generic
processor-charge refund or void outcome. Version 4 tightens the existing named
CHECK constraint to encode the complete typed tuple matrix.

Adding the replacement constraint validates every retained attestation once.
If an incompatible v3 row exists, PostgreSQL aborts the migration transaction.
Keep the v3 application stopped, investigate the evidence, and use an audited
host-owned repair process before retrying. Do not bypass the migration or
rewrite financial evidence without that review.

Run `preflight_from_v3.sql` well before the maintenance window. Its single
read-only result reports `retained_attestation_count`, the volume the migration
must scan, and `incompatible_attestation_count`, the rows that block the
cutover. If it reports blockers, run the checked-in read-only
`audit_incompatible_attestations_from_v3.sql`. The audit identifies each row by
its internal attempt and processor-charge IDs and reports only the three fields
of the incompatible tuple. It deliberately excludes gateway transaction IDs,
actor and reason text, account metadata, processor responses, and card-display
fields. Treat its output as protected financial evidence and keep access and
exports inside the host's authorized operator process.

Rehearse the exact upgrade artifact on a production-like copy with a
representative retained-attestation count, record its elapsed time, and budget
the maintenance window from that observation. This repository has no
host-independent row-count ceiling or downtime budget, so the host owns those
operational limits. Complete the audited review before scheduling a cutover
when the first preflight reports blockers. Because v3 writers can add rows,
repeat the preflight after stopping them when an immediate zero-blocker result
is part of the host's cutover evidence.

The `ALTER TABLE` takes an `ACCESS EXCLUSIVE` lock and scans the complete
`billing_external_reversal_attestations` table while adding the validated
replacement CHECK constraint. That lock blocks readers and writers and remains
held until the host transaction commits or rolls back. Set host-appropriate
`lock_timeout` and `statement_timeout` values outside the checked-in artifact
so a deployment cannot wait or run longer than its approved maintenance
budget. PostgreSQL can reduce reader-blocking scan time with a committed
`NOT VALID` constraint followed by `VALIDATE CONSTRAINT`, but that is a
different multi-transaction rollout: it creates an intermediate schema state
and needs its own compatibility, failure-recovery, and rollback protocol. This
repository does not supply that protocol. The supported artifact instead keeps
the constraint replacement and validation atomic so a failed scan restores v3
exactly and a committed cutover is immediately ready for v4 startup.

For the cutover, prebuild and verify the schema-v4-aware application, drain
schema-v3 billing traffic, stop all schema-v3 writers, apply
`upgrade_from_v3.sql` in one database transaction, start the new application,
and then resume billing work. A failure before commit rolls back to v3 and
permits the stopped v3 application to resume. After commit, roll forward; do
not restart a v3 writer against v4.

After a successful cutover, `assert_runtime_schema_v4_compatible` verifies the
validated constraint and complete canonical catalog without scanning retained
attestations. Schema v4 otherwise retains the complete v3 tables, constraints,
functions, views, and reader-facing index contracts. The Rust package exposes
the exact artifacts as `V4_INSTALL_SQL` and `V3_TO_V4_UPGRADE_SQL` behind the
schema-contract test-support feature; `V3_TO_V4_PREFLIGHT_SQL` exposes the
read-only sizing query and `V3_TO_V4_INCOMPATIBLE_ATTESTATION_AUDIT_SQL` exposes
the minimized blocker audit there as well. Once released, all four artifacts
and this guide are immutable.
