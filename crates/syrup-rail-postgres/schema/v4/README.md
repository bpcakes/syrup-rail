# Syrup Rail PostgreSQL schema v4

Schema v4 is the current canonical contract for Syrup Rail 0.4. New hosts copy
`install.sql` byte-for-byte into an immutable host migration. Existing
schema-v3 hosts use the four forward-only stages below. The concurrent index
stage is intentionally non-transactional; the other three commit separately.
Do not run `install.sql` over v3 and do not edit any shipped artifact under
`schema/v1`, `schema/v2`, or `schema/v3`.

Version 4 adds the non-null `required_gateway_account_mode` snapshot to every
payment attempt and subscription, plus a distinct
`gateway_test_readiness_failed_before_submission` resolution. The snapshot
records the trusted service policy that authorized any future provider
submission as either `live` or `test`. It does not claim that NMI's separate
account-mode query and transaction request are atomic.

Every historical v3 attempt and subscription is backfilled as `live`. That is
deterministic because all released service versions rejected test-mode accounts
before any provider mutation. New test-authorized subscriptions retain that
policy for renewal, recovery, and payment-method replacement work even after a
shared NMI account is switched back to live mode.

## Required availability sign-off

By default, schedule all four stages in a full billing maintenance window.
Before `prepare_from_v3.sql`, the deployment owner must explicitly accept and
schedule the entire prepare → validate → concurrent-index → finalize window as
one availability-controlled operation. Keeping v3 writers active through the
first three stages is an exception that requires written sign-off that process
restarts are prevented for the entire window. A v3 process restart after
preparation cannot pass its startup schema assertion, and there is no rollback
artifact; recovery is to finish the remaining stages and roll forward. Do not
begin preparation without that operational sign-off.

## Staged v3 cutover

Prebuild and verify the schema-v4-aware 0.4 application before beginning.
Rehearse every stage against representative payment-attempt volume and set
explicit migration lock and statement timeouts appropriate for the host.

The committed prepare, validate, and index states are migration-only catalog states;
none is a supported application runtime. No production schema assertion is
expected to pass between preparation and finalization. Keep billing stopped for
the default four-stage maintenance operation, and start 0.4 only after the v4
assertion passes. Under the explicitly signed-off online-build exception, keep
the existing v3 process running without restarting through prepare, validate,
and index; stop all v3 billing writers for finalization. The risk window starts
when `prepare_from_v3.sql` commits, not when finalization begins: an OOM, node
drain, or unrelated restart in an intermediate state leaves the v3 process
unable to pass its startup schema assertion.

First apply `prepare_from_v3.sql` in its own transaction and commit it. It adds
both columns with metadata-only constant `live` defaults, adds their closed
`live`/`test` checks as `NOT VALID`, and replaces the resolution-code check with
schema v4's expanded vocabulary as `NOT VALID`. The checks are enforced for new
writes, but this short stage does not scan historical rows while holding its
`ACCESS EXCLUSIVE` locks. A v3 writer may temporarily continue because an
omitted mode receives the only historically valid value, `live`;
avoid a process restart because the v3 catalog assertion intentionally rejects
the prepared, no-longer-canonical catalog.

Next apply `validate_from_v3.sql` in a separate transaction and commit it.
PostgreSQL scans existing attempts under `SHARE UPDATE EXCLUSIVE`, allowing
ordinary reads and writes to continue while proving every row satisfies the
expanded constraints. Retry this stage if it times out; do not proceed until
all constraints are valid.

Next run every statement in `index_from_v3.sql` **outside an explicit
transaction** while the v3 writers continue. It uses two `CREATE INDEX
CONCURRENTLY` statements: one builds the mode-leading renewal-dispatch index,
and the other builds the covering v4 replacement for the all-mode dispatch
index. This keeps both table scans outside the final billing outage. If a build
fails and leaves an invalid index, run `DROP INDEX CONCURRENTLY IF EXISTS` for
`public.billing_subscriptions_due_mode_idx` and/or
`public.billing_subscriptions_due_v4_idx` outside a transaction, then rerun the
artifact. Confirm that both indexes are valid before finalization; the
finalization artifact enforces this precondition before changing the canonical
index name. The v4 startup assertion independently fails closed if either
canonical result is missing or invalid after the name swap.

Finally stop every schema-v3 billing writer, apply `upgrade_from_v3.sql` in its
own transaction, and commit it immediately before starting the 0.4 application.
This fast finalization swaps the prebuilt covering index into the canonical
all-mode index name and drops both compatibility defaults, forcing every
schema-v4 payment-attempt and subscription writer to state its trusted required
mode explicitly. The Syrup Rail enrollment writer supplies the approved
attempt's mode. Because the preceding concurrent stage already built both
renewal-dispatch indexes, finalization contains only catalog changes and the
current-subscription view replacement; still rehearse its lock acquisition
against production traffic.
After finalization, roll forward; never restart a v3 writer because omitted
mode writes now fail.

A failure before any stage commits rolls that stage back. After preparation
commits, retry validation. Clean up an invalid concurrent index as described
above before retrying its stage. After the index is valid, retry finalization.
The runtime is schema v4 only after all four stages have completed and
`assert_runtime_schema_v4_compatible` succeeds.

Schema v4 otherwise retains the complete v3 tables, constraints, functions,
views, and reader-facing index contracts. The Rust package exposes the exact
artifacts as `V4_INSTALL_SQL`, `V3_TO_V4_PREPARE_SQL`,
`V3_TO_V4_VALIDATE_SQL`, `V3_TO_V4_INDEX_SQL`, and `V3_TO_V4_UPGRADE_SQL`
behind the schema-contract test-support feature. Once released, all files in
this directory are immutable.
