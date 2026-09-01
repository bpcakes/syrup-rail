-- Forward-only Syrup Rail schema-v4 finalization after the separately
-- committed prepare_from_v3.sql and validate_from_v3.sql artifacts.
--
-- Apply these bytes once inside a host-owned transaction while schema-v3
-- billing writers are stopped and the 0.4.0 application is ready to start.
-- Removing the compatibility defaults makes every schema-v4 writer state its
-- trusted required mode explicitly.

-- Fail before changing any canonical name if either non-transactional build
-- was omitted, interrupted, or left invalid. This keeps the old all-mode index
-- intact and lets the operator clean up and rerun index_from_v3.sql.
DO $$
BEGIN
    IF to_regclass('public.billing_subscriptions_due_v4_idx') IS NULL
        AND EXISTS (
            SELECT 1
            FROM information_schema.columns
            WHERE table_schema = 'public'
                AND (
                    (table_name = 'billing_payment_attempts'
                        AND column_name = 'required_gateway_account_mode')
                    OR (table_name = 'billing_subscriptions'
                        AND column_name = 'required_gateway_account_mode')
                )
                AND column_default IS NULL
        )
    THEN
        RAISE EXCEPTION
            'schema-v4 finalization is already applied or was manually altered; inspect the catalog and do not rerun this artifact';
    END IF;

    IF NOT EXISTS (
        SELECT 1
        FROM pg_catalog.pg_index AS indexes
        JOIN pg_catalog.pg_class AS index_relations
            ON index_relations.oid = indexes.indexrelid
        JOIN pg_catalog.pg_namespace AS namespaces
            ON namespaces.oid = index_relations.relnamespace
        WHERE namespaces.nspname = 'public'
            AND index_relations.relname = 'billing_subscriptions_due_v4_idx'
            AND indexes.indisready
            AND indexes.indisvalid
    ) THEN
        RAISE EXCEPTION
            'schema-v4 all-mode renewal index was not built successfully; outside a transaction run DROP INDEX CONCURRENTLY IF EXISTS public.billing_subscriptions_due_v4_idx, then rerun index_from_v3.sql';
    END IF;

    IF NOT EXISTS (
        SELECT 1
        FROM pg_catalog.pg_index AS indexes
        JOIN pg_catalog.pg_class AS index_relations
            ON index_relations.oid = indexes.indexrelid
        JOIN pg_catalog.pg_namespace AS namespaces
            ON namespaces.oid = index_relations.relnamespace
        WHERE namespaces.nspname = 'public'
            AND index_relations.relname = 'billing_subscriptions_due_mode_idx'
            AND indexes.indisready
            AND indexes.indisvalid
    ) THEN
        RAISE EXCEPTION
            'schema-v4 mode-specific renewal index was not built successfully; outside a transaction run DROP INDEX CONCURRENTLY IF EXISTS public.billing_subscriptions_due_mode_idx, then rerun index_from_v3.sql';
    END IF;
END
$$;

-- The replacement was built concurrently while v3 writers continued. Swap it
-- into the canonical name now that writers are stopped; these are catalog-only
-- operations and do not rebuild the index in the final outage.
DROP INDEX public.billing_subscriptions_due_idx;
ALTER INDEX public.billing_subscriptions_due_v4_idx
    RENAME TO billing_subscriptions_due_idx;

ALTER TABLE public.billing_payment_attempts
    ALTER COLUMN required_gateway_account_mode DROP DEFAULT;

ALTER TABLE public.billing_subscriptions
    ALTER COLUMN required_gateway_account_mode DROP DEFAULT;

-- Append the durable subscription mode to the public current-subscription
-- projection after the column exists and has been validated.
CREATE OR REPLACE VIEW public.billing_current_subscriptions AS
SELECT
    id,
    billing_scope_id,
    gateway_account_id,
    subscriber_id,
    plan_key,
    status,
    payment_method_id,
    amount_cents,
    currency,
    current_period_start_at,
    current_period_end_at,
    next_renewal_at,
    initial_transaction_id,
    canceled_at,
    created_at,
    updated_at,
    CASE status
        WHEN 'active' THEN 0
        WHEN 'past_due' THEN 1
        ELSE 2
    END AS current_subscription_rank,
    phase,
    recurring_period_kind,
    recurring_period_count,
    trial_amount_cents,
    trial_period_kind,
    trial_period_count,
    dunning_retry_delays_seconds,
    dunning_exhaustion,
    past_due_access,
    next_payment_attempt_at,
    unpaid_at,
    required_gateway_account_mode
FROM public.billing_subscriptions
WHERE status IN ('active', 'past_due')
    OR (
        status = 'canceled'
        AND current_period_end_at > now()
    );
