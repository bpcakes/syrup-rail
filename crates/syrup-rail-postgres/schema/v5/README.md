# Syrup Rail PostgreSQL schema v5

Schema v5 is the current canonical contract for Syrup Rail 0.5.4. New hosts
copy `install.sql` byte-for-byte into an immutable host migration. Existing
schema-v4 hosts apply `upgrade_from_v4.sql` once, in one transaction, while
all billing writers are stopped. Do not run `install.sql` over v4 and do not
edit any shipped artifact under `schema/v1`, `schema/v2`, `schema/v3`, or
`schema/v4`.

Version 5 appends six nullable columns to both `billing_payment_methods` and
`billing_payment_attempts`: `billing_address_line1`, `billing_address_line2`,
`billing_address_city`, `billing_address_region`,
`billing_address_postal_code`, and `billing_address_country`. The validated
`billing_payment_methods_billing_address_valid` and
`billing_payment_attempts_billing_address_valid` constraints require either
all six to be NULL, or a nonblank first line and an uppercase two-letter
country, with every present field nonblank and at most 255 octets. The
constraints do not validate a country-specific region or postal-code format.

Historical rows keep NULL addresses; the upgrade infers nothing. A stored
method receives an address only when a customer-confirmed enrollment,
recovery, or payment-method replacement carrying one is approved, and a
renewal copies the selected method's address into its attempt when it is
reserved. Existing subscribers therefore stay addressless until they complete
one of those customer-present operations. Addressless renewals keep their
previous provider request.

## Cutover

Prebuild and verify the schema-v5-aware 0.5.4 API and workers before starting.
Rehearse the upgrade on a restored production-sized copy and measure lock
waits and duration: adding the columns is metadata-only, but validating each
constraint scans its table under the `ACCESS EXCLUSIVE` lock taken by
`ALTER TABLE`. If the measured scan does not fit the approved maintenance
window, stop and plan a staged cutover as a separately reviewed change; the v5
startup assertion rejects `NOT VALID` canonical constraints, so a validated
constraint is required before 0.5.4 accepts billing work.

During the maintenance window:

1. Stop every billing writer: API processes, renewal and reconciliation
   workers, and any job that can call Syrup Rail. The writer stop is what
   enforces the cutover. An already-running v4 process could keep writing
   because the new columns are nullable, and a restarted v4 process fails its
   startup assertion against the v5 catalog.
2. Optionally confirm the pre-cutover state with
   `assert_runtime_schema_v4_compatible`.
3. Apply `upgrade_from_v4.sql` in one host-owned transaction and commit it.
   The artifact sets a 5-second `lock_timeout`; if it times out, the
   transaction rolls back with no change and can be retried. It refuses to run
   on a database that already has address columns.
4. Start the 0.5.4 build. It must call `assert_runtime_schema_v5_compatible`
   before accepting billing work.

## Recovery

Any failure before commit rolls the whole upgrade back and leaves schema v4
intact. After commit, keep writers stopped and roll forward with a
schema-v5-aware build. Restoring an old image, dropping the new columns or
constraints, resetting the database, or rewriting a historical migration is
not a rollback and must not be attempted.

## Privacy

The addresses are billing PII. `scrub_subscriber_billing_data` clears all six
columns on methods and attempts while holding the payment-method approval
domain, so a concurrent approval or renewal reservation cannot restore them.
Provider-side copies are outside local deletion: NMI stores the address in the
Customer Vault and in its transaction history, and this library offers no
vault update or deletion. Provider-side deletion remains a host and operator
responsibility that the host's privacy notice must describe.
