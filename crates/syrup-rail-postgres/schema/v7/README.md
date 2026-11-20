# Syrup Rail PostgreSQL schema v7

Schema v7 is the current canonical contract. New hosts copy `install.sql`
byte-for-byte into an immutable host migration. Existing schema-v6 hosts apply
`upgrade_from_v6.sql` once, in one transaction. Do not run `install.sql` over
v6 and do not edit any shipped artifact under `schema/v1` through `schema/v6`.

Version 7 appends one nullable column and changes nothing else:

- `billing_payment_attempts.gateway_avs_response text` holds the provider's
  address-verification result code from the attempt's original response.

No row, constraint, index, trigger, view, or other column changes, and the
upgrade infers nothing about historical rows: they keep `NULL`. Like the
attempt's other gateway response columns, the value carries no CHECK
constraint; the runtime bounds it before writing. Subscriber scrubbing retains
it with `gateway_response_code`, because it is a closed provider code and not
the address it was checked against.

## Runtime compatibility

| Startup assertion | v6 catalog | v7 catalog |
| --- | --- | --- |
| `assert_runtime_schema_v6_compatible` | accepts | rejects |
| `assert_runtime_schema_v6_or_v7_compatible` | accepts | accepts |
| `assert_runtime_schema_v7_compatible` | rejects | accepts |

The runtime in this release reads and writes only schema-v6 columns, by name,
so it is correct on either catalog and calls
`assert_runtime_schema_v6_or_v7_compatible`. A runtime that writes
`gateway_avs_response` will require v7.

## Cutover

The upgrade is online. Adding a nullable column without a default changes only
the catalog: it neither rewrites nor scans the table. Writers do not stop.

1. Deploy a build that calls `assert_runtime_schema_v6_or_v7_compatible` and
   make it the oldest permitted rollback target. A build that calls
   `assert_runtime_schema_v6_compatible` refuses to start once v7 commits.
2. Apply `upgrade_from_v6.sql` in one host-owned transaction and commit it.
   `ALTER TABLE` briefly takes an `ACCESS EXCLUSIVE` lock for the catalog
   change; the artifact's 5-second `lock_timeout` bounds how long other
   statements queue behind it. On a timeout the transaction rolls back with no
   change and can be retried. The artifact refuses a database that is not
   schema v6 and a rerun.
3. Optionally confirm the result with `assert_runtime_schema_v7_compatible`.

## Recovery

Any failure before commit leaves schema v6 intact. After commit, any build
that accepts v7 at startup may run, including the bridge build as a rollback
target. Dropping the column, resetting the database, or rewriting a historical
migration is not a rollback and must not be attempted.
