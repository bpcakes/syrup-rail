# Syrup Rail PostgreSQL schema v7

Schema v7 is the current canonical contract. New hosts copy `install.sql`
byte-for-byte into an immutable host migration. Existing schema-v6 hosts apply
`upgrade_from_v6.sql` once, in one transaction. Do not run `install.sql` over
v6 and do not edit any shipped artifact under `schema/v1` through `schema/v6`.

Version 7 appends one nullable column and changes nothing else:

- `billing_payment_attempts.gateway_avs_response text` holds the provider's
  address-verification result code from the attempt's original response.

No row, constraint, index, trigger, view, or other column changes, and the
upgrade infers nothing about historical rows: they keep `NULL`. Unlike the
attempt's other gateway response columns, which
`billing_payment_attempts_gateway_text_check` bounds to 512 octets, the value
carries no CHECK constraint, so the upgrade stays catalog-only. This release
never writes it; a runtime that does must bound the value itself before
writing. Subscriber scrubbing retains it with `gateway_response_code`, because
it is a closed provider code and not the address it was checked against.

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
working after the upgrade commits. A runtime that writes
`gateway_avs_response` will require v7.

## Cutover

The upgrade is online. Adding a nullable column without a default changes only
the catalog: it neither rewrites nor scans the table. Writers do not stop.

1. Deploy a build that calls `assert_runtime_schema_v6_or_v7_compatible` and
   make it the oldest permitted rollback target. A build that calls
   `assert_runtime_schema_v6_compatible` refuses to start once v7 commits.
2. Apply `upgrade_from_v6.sql` in one host-owned transaction and commit it.
   `ALTER TABLE` takes an `ACCESS EXCLUSIVE` lock for the catalog change and
   holds it until that transaction commits, so commit promptly. While the
   request waits, every later statement on the attempt table queues behind it.
   The artifact's 100-millisecond `lock_timeout` stays under the runtime's
   250-millisecond billing lock timeout, so a blocked upgrade gives up before
   queued billing statements fail. On a timeout the transaction rolls back
   with no change and can be retried. The artifact refuses a database that is
   not schema v6 and a rerun.
3. Optionally confirm the result with `assert_runtime_schema_v7_compatible`.

## Recovery

Any failure before commit leaves schema v6 intact. After commit, any build
that accepts v7 at startup may run, including the bridge build as a rollback
target. Dropping the column, resetting the database, or rewriting a historical
migration is not a rollback and must not be attempted.
