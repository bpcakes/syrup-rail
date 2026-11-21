# Syrup Rail PostgreSQL schema v7

Schema v7 is the current canonical contract. New hosts copy `install.sql`
byte-for-byte into an immutable host migration. Existing schema-v6 hosts apply
`upgrade_from_v6.sql` once, in one transaction. Do not run `install.sql` over
v6 and do not edit any shipped artifact under `schema/v1` through `schema/v6`.

Version 7 appends one nullable column and replaces one function to cover it:

- `billing_payment_attempts.gateway_avs_response varchar(64)` holds the provider's
  address-verification result code from the attempt's original response.
- `billing_host_charge_ledger_admission` additionally requires a pending
  host-charge attempt to have no AVS result before `submit` admission, as it
  already requires for the other gateway response columns.

No row, constraint, index, trigger, view, or other column changes, and the
upgrade infers nothing about historical rows: they keep `NULL`.

The `varchar(64)` type bounds the value to 64 characters on every write. That
matches `MAX_GATEWAY_DIAGNOSTIC_CODE_BYTES`, the 64-byte bound the domain
diagnostic observation already applies to AVS codes, so no value the domain
accepts can fail the write. NMI AVS result codes are one or two characters. The attempt's other
gateway response columns are bounded by the
`billing_payment_attempts_gateway_text_check` constraint instead. The new
column uses a type bound because adding a column whose type has a length limit
verifies no existing rows, while adding a CHECK scans the table under the
`ALTER TABLE` lock. The bound limits length, not vocabulary. This release never
writes the column; a runtime that does must recognize the value from the
provider adapter's closed AVS result-code vocabulary (never persisting raw
provider text or address fragments), because subscriber scrubbing retains it
with `gateway_response_code` as a closed decision code rather than the address
it was checked against.

## Runtime compatibility

| Startup assertion | v6 catalog | v7 catalog |
| --- | --- | --- |
| `assert_runtime_schema_v6_compatible` | accepts | rejects |
| `assert_runtime_schema_v6_or_v7_compatible` | accepts | accepts |
| `assert_runtime_schema_v7_compatible` | rejects | accepts |

The runtime in this release reads and writes only schema-v6 columns, by name,
so it is correct on either catalog and calls
`assert_runtime_schema_v6_or_v7_compatible`. No runtime query projects
attempts with `*`, so statements a pooled connection prepared against v6 keep
working after the upgrade commits. Serving roles require table-level `SELECT`
on `billing_payment_attempts` (not v6-only column grants), because catalog
validation and the canonical trigger and admission functions inspect the
table's full column set. A runtime that writes `gateway_avs_response` will
require v7.

## Cutover

The upgrade is online. Adding a nullable column without a default and
replacing the admission function change only the catalog: neither rewrites nor
scans the table. Writers do not stop.

1. Deploy a build that calls `assert_runtime_schema_v6_or_v7_compatible` and
   make it the oldest permitted rollback target. A build that calls
   `assert_runtime_schema_v6_compatible` refuses to start once v7 commits.
2. Apply `upgrade_from_v6.sql` in one host-owned transaction and commit it.
   `ALTER TABLE` takes an `ACCESS EXCLUSIVE` lock for the catalog change and
   holds it until that transaction commits, so commit promptly. Run nothing
   else in that transaction: the lock and the artifact's `lock_timeout` both
   last until it ends, so a migration runner that batches several migrations
   into one transaction must give this artifact a transaction of its own.
   While the request waits, every later statement on the attempt table queues
   behind it.
   The artifact's 100-millisecond `lock_timeout` stays under the runtime's
   250-millisecond billing lock timeout, so a blocked upgrade gives up before
   queued billing statements fail. On a timeout the transaction rolls back
   with no change and can be retried. A running autovacuum of
   `billing_payment_attempts` also blocks the lock, and the timeout expires
   before PostgreSQL would cancel that vacuum for the waiting `ALTER TABLE`
   (after `deadlock_timeout`, 1 second by default), so retries keep timing out
   until it finishes; an anti-wraparound vacuum is never cancelled. Check
   `pg_stat_progress_vacuum` for the table and apply once it is idle. The
   artifact refuses a database that is not schema v6 and a rerun.
3. Optionally confirm the result with `assert_runtime_schema_v7_compatible`.

## Recovery

Any failure before commit leaves schema v6 intact. After commit, any build
that accepts v7 at startup may run, including the bridge build as a rollback
target. Dropping the column, resetting the database, or rewriting a historical
migration is not a rollback and must not be attempted.
