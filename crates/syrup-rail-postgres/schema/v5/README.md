# Syrup Rail PostgreSQL schema v5

Schema v5 is the current canonical contract. New hosts copy `install.sql`
byte-for-byte into an immutable host migration. Existing schema-v4 hosts stop
all billing writers, drain billing traffic, and copy `upgrade_from_v4.sql`
byte-for-byte into one forward-only transactional migration. Do not run
`install.sql` over v4, and do not edit any shipped artifact under `schema/v1`
through `schema/v4`.

Version 5 retains schema v4's durable required-gateway-mode snapshots,
mode-specific renewal indexes, and closed resolution-code vocabulary. It
tightens one existing external-reversal attestation CHECK constraint. Schema
v4 allowed two resolution tuples that the typed Rust model cannot represent:
an initial-current-grant conflict paired with a generic processor-charge
refund or void outcome. Version 5 encodes the complete typed tuple matrix.

Adding the replacement constraint validates every retained attestation once.
If an incompatible v4 row exists, PostgreSQL aborts the migration transaction.
Keep the v4 application stopped, investigate the evidence, and use an audited
host-owned repair process before retrying. Do not bypass the migration or
rewrite financial evidence without that review.

Run `preflight_from_v4.sql` well before the maintenance window. Its single
read-only result reports `retained_attestation_count`, the volume the migration
must scan, and `incompatible_attestation_count`, the rows that block the
cutover. If it reports blockers, run
`audit_incompatible_attestations_from_v4.sql`. The audit identifies each row by
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
when the first preflight reports blockers. Because v4 writers can add rows,
repeat the preflight after stopping them when an immediate zero-blocker result
is part of the host's cutover evidence.

The `ALTER TABLE` takes an `ACCESS EXCLUSIVE` lock and scans the complete
`billing_external_reversal_attestations` table while adding the validated
replacement CHECK constraint. That lock blocks readers and writers and remains
held until the host transaction commits or rolls back. Set host-appropriate
`lock_timeout` and `statement_timeout` values outside the checked-in artifact
so a deployment cannot wait or run longer than its approved maintenance
budget. The supported artifact keeps replacement and validation atomic: a
failed scan restores v4 exactly, and a committed cutover is immediately ready
for v5 startup.

For the cutover, prebuild and verify the schema-v5-aware application, drain
schema-v4 billing traffic, stop every v4 writer, apply `upgrade_from_v4.sql` in
one database transaction, start the new application, and then resume billing
work. A failure before commit rolls back to v4 and permits the stopped v4
application to resume. After commit, roll forward; do not restart a v4 writer
against v5.

After a successful cutover, `assert_runtime_schema_v5_compatible` verifies the
validated constraint and complete canonical catalog without scanning retained
attestations. Schema v5 otherwise retains the complete v4 tables, constraints,
functions, views, and reader-facing index contracts. The Rust package exposes
the exact artifacts as `V5_INSTALL_SQL` and `V4_TO_V5_UPGRADE_SQL` behind the
schema-contract test-support feature;
`V4_TO_V5_PREFLIGHT_SQL` exposes the read-only sizing query and
`V4_TO_V5_INCOMPATIBLE_ATTESTATION_AUDIT_SQL` exposes the minimized blocker
audit. Once released, all four artifacts and this guide are immutable.

The retained pending-evidence columns `check_count` and `last_checked_at`, and
`billing_gateway_lifecycle_pending_check_idx`, preserve the v4 catalog contract.
The v5 application no longer updates or uses them; `expires_at` controls pending
retention. Removing these legacy objects requires a future versioned schema
cutover with matching runtime assertions, rather than editing shipped artifacts.
