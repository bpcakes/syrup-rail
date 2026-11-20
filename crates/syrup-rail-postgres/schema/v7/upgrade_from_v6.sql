-- Forward-only Syrup Rail schema upgrade from version 6 to version 7.
--
-- Apply these bytes once inside a host-owned transaction. Version 7 appends
-- one nullable column, billing_payment_attempts.gateway_avs_response, which
-- holds the provider's address-verification result code for the attempt's
-- original response. No row, constraint, index, trigger, view, or other
-- column changes, and the upgrade infers nothing about historical rows: they
-- keep NULL.
--
-- Adding a nullable column without a default changes only the catalog: it
-- neither rewrites nor scans the table. ALTER TABLE still takes an ACCESS
-- EXCLUSIVE lock for that catalog change, and every later statement on the
-- table queues behind the waiting request. The 100-millisecond lock_timeout
-- below stays under the runtime's 250-millisecond billing lock timeout, so a
-- blocked upgrade gives up before queued billing statements fail; on a timeout
-- the transaction rolls back with no change and can be retried. Writers do not
-- need to stop once every serving build accepts schema v6 or v7 at startup:
-- those builds name their attempt columns explicitly and read rows by column
-- name, so they keep working while and after this commits. Earlier schema-v6
-- builds returned attempts with a wildcard in the reconciliation claim and
-- must not be serving. A runtime that starts afterwards must accept schema v7
-- at startup: assert_runtime_schema_v6_or_v7_compatible or
-- assert_runtime_schema_v7_compatible, not assert_runtime_schema_v6_compatible.

SET LOCAL lock_timeout = '100ms';

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
        FROM pg_catalog.pg_constraint AS constraints
        JOIN pg_catalog.pg_class AS tables ON tables.oid = constraints.conrelid
        JOIN pg_catalog.pg_namespace AS namespaces ON namespaces.oid = tables.relnamespace
        WHERE namespaces.nspname = 'public'
            AND tables.relname = 'billing_payment_attempts'
            AND constraints.conname = 'billing_payment_attempts_resolution_code_check'
            AND constraints.convalidated
            AND pg_catalog.strpos(
                pg_catalog.pg_get_constraintdef(constraints.oid),
                'subscription_approved_period_expired'
            ) > 0
    )
    THEN
        RAISE EXCEPTION
            'schema-v7 upgrade requires schema v6; apply the earlier upgrades first';
    END IF;
    IF EXISTS (
        SELECT 1
        FROM information_schema.columns
        WHERE table_schema = 'public'
            AND table_name = 'billing_payment_attempts'
            AND column_name = 'gateway_avs_response'
    )
    THEN
        RAISE EXCEPTION
            'schema-v7 upgrade is already applied or was manually altered; inspect the catalog and do not rerun this artifact';
    END IF;
END
$$;

ALTER TABLE public.billing_payment_attempts
    ADD COLUMN gateway_avs_response text;
