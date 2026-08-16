# Syrup Rail PostgreSQL schema v3

Schema v3 is the current canonical contract. New hosts copy `install.sql`
byte-for-byte into an immutable host migration. Existing schema-v2 hosts stop
all billing writers and copy `upgrade_from_v2.sql` byte-for-byte into one
forward-only transactional migration. Do not run `install.sql` over v2 and do
not edit any shipped artifact under `schema/v1` or `schema/v2`.

Version 2 persisted attempt billing contact as one combined `billing_name` plus
`billing_email`. That projection was sufficient for receipts but not for
immutable replay identity because different first/last boundaries can produce
the same combined name. Version 3 renames the attempt column to
`billing_first_name` and adds `billing_last_name`; `billing_email` is unchanged.
New token-bearing attempts therefore persist the same normalized structured
contact that the provider adapter submits.

PostgreSQL cannot reconstruct the historical first/last boundary from a v2
combined name. The upgrade treats the complete old `billing_name` as the
canonical first name and leaves the last name absent. This preserves the exact
derived display name and retained contact content. A prepared v2 attempt can
resume only when its retry uses that canonical upgraded structure; another
split is an intentional idempotency conflict and no provider mutation occurs.
Submitted and terminal attempts never resubmit.

For the cutover, prebuild and verify the schema-v3-aware application, stop all
schema-v2 writers, apply `upgrade_from_v2.sql` in one database transaction,
start the new application, and then resume billing work. A failure before
commit rolls back to v2 and permits the stopped v2 application to resume. After
commit, roll forward; never restart a v2 writer against v3 because the attempt
column contract has changed.

The payment-method table intentionally retains `billing_name` and
`billing_email`: stored methods need only minimized receipt/display metadata and
cannot resume a token-bearing request. Account deletion scrubs both structured
attempt-name fields as well as the existing method display projection.

Schema v3 otherwise retains the complete v2 tables, constraints, functions,
views, and reader-facing index contracts. The Rust package exposes the exact
artifacts as `V3_INSTALL_SQL` and `V2_TO_V3_UPGRADE_SQL` behind the schema
contract test-support feature. Once released, both files are immutable.
