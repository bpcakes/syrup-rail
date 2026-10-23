# Syrup Rail PostgreSQL schema v6

Schema v6 is the current canonical contract for Syrup Rail 0.5.5. New hosts
copy `install.sql` byte-for-byte into an immutable host migration. Existing
schema-v5 hosts apply `upgrade_from_v5.sql` once, in one transaction, while
all billing writers are stopped. Do not run `install.sql` over v5 and do not
edit any shipped artifact under `schema/v1` through `schema/v5`.

Version 6 gives billing-period expiry typed, durable dispositions. It widens
two closed CHECK lists and changes nothing else:

- `billing_payment_attempts_resolution_code_check` also accepts
  `subscription_period_expired_before_charge`, for a prepared renewal or
  recovery rejected before provider submission because its period ended, and
  `subscription_approved_period_expired`, for a provider-approved charge that
  arrived after its period ended and is parked for external reversal.
- `billing_external_reversal_attestations_resolution_check` also accepts
  `subscription_approved_period_expired` as the prior resolution, so the
  expiry reason survives a verified external reversal.

No column, row, index, trigger, or other constraint changes, and the upgrade
infers nothing about historical rows. Both new values are written only by
0.5.5 code paths that the host enables explicitly through
`SubscriptionPeriodExpiryPolicy::RejectExpiredPeriods` or the retirement
operation.

## Cutover

Prebuild and verify the schema-v6-aware 0.5.5 API and workers before starting.
Rehearse the upgrade on a restored production-sized copy and measure lock
waits and duration: replacing each constraint validates every row of
`billing_payment_attempts` and `billing_external_reversal_attestations` under
the `ACCESS EXCLUSIVE` lock taken by `ALTER TABLE`. If the measured scan does
not fit the approved maintenance window, stop and plan a staged cutover as a
separately reviewed change; the v6 startup assertion rejects `NOT VALID`
canonical constraints, so a validated constraint is required before 0.5.5
accepts billing work.

During the maintenance window:

1. Stop every billing writer: API processes, renewal and reconciliation
   workers, and any job that can call Syrup Rail. The writer stop is what
   enforces the cutover. An already-running 0.5.4 process could keep writing
   because the widened lists accept every existing value, and a restarted
   0.5.4 process fails its v5 startup assertion against the v6 catalog.
2. Optionally confirm the pre-cutover state with
   `assert_runtime_schema_v5_compatible`.
3. Apply `upgrade_from_v5.sql` in one host-owned transaction and commit it.
   The artifact sets a 5-second `lock_timeout`; if it times out, the
   transaction rolls back with no change and can be retried. It refuses to run
   on a database without the schema-v5 address columns or on one whose
   constraints already accept the version-6 values.
4. Start the 0.5.5 build. It must call `assert_runtime_schema_v6_compatible`
   before accepting billing work.

## Recovery

Any failure before commit rolls the whole upgrade back and leaves schema v5
intact. After commit, keep writers stopped and roll forward with a
schema-v6-aware build. Restoring an old image, narrowing the constraints,
resetting the database, or rewriting a historical migration is not a rollback
and must not be attempted.
