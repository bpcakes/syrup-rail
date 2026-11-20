-- Forward-only Syrup Rail schema upgrade from version 6 to version 7.
--
-- Apply these bytes once inside a host-owned transaction. Version 7 appends
-- one nullable column, billing_payment_attempts.gateway_avs_response, which
-- holds the provider's address-verification result code for the attempt's
-- original response. It also replaces billing_host_charge_ledger_admission so
-- a pending host-charge attempt must have no AVS result before submission, as
-- it already must have no other gateway response. No row, constraint, index,
-- trigger, view, or other column changes, and the upgrade infers nothing about
-- historical rows: they keep NULL.
--
-- Adding a nullable column without a default and replacing a function change
-- only the catalog: neither rewrites nor scans the table. ALTER TABLE still
-- takes an ACCESS EXCLUSIVE lock for that catalog change, and every later
-- statement on the
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

CREATE OR REPLACE FUNCTION public.billing_host_charge_ledger_admission(
    p_billing_scope_id uuid,
    p_subscriber_id uuid,
    p_host_charge_target_id uuid,
    p_mode text,
    p_idempotency_key text DEFAULT NULL,
    p_attempt_id uuid DEFAULT NULL
)
RETURNS text
LANGUAGE plpgsql
STABLE
SET search_path = pg_catalog, public
AS $$
DECLARE
    has_contender boolean;
    has_unsafe_attempt boolean;
    has_unsafe_charge boolean;
BEGIN
    IF p_mode NOT IN ('reserve', 'submit', 'release') THEN
        RAISE EXCEPTION 'unsupported host charge ledger admission mode';
    END IF;

    IF (
        p_mode = 'reserve'
        AND (
            p_idempotency_key IS NULL
            OR length(btrim(p_idempotency_key)) = 0
            OR p_attempt_id IS NOT NULL
        )
    ) OR (
        p_mode = 'submit'
        AND (
            p_idempotency_key IS NOT NULL
            OR p_attempt_id IS NULL
        )
    ) OR (
        p_mode = 'release'
        AND (
            p_idempotency_key IS NOT NULL
            OR p_attempt_id IS NOT NULL
        )
    ) THEN
        RAISE EXCEPTION 'invalid host charge ledger admission arguments';
    END IF;

    WITH target_attempts AS (
        SELECT
            attempts.*,
            (
                p_mode = 'reserve'
                AND attempts.idempotency_key = p_idempotency_key
            ) AS contender,
            (
                p_mode = 'submit'
                AND attempts.id = p_attempt_id
                AND attempts.status = 'pending'
                AND attempts.submitted_at IS NULL
                AND attempts.resolved_at IS NULL
                AND attempts.gateway_lifecycle_status = 'unknown'
                AND attempts.refunded_amount_cents = 0
                AND attempts.gateway_transaction_id IS NULL
                AND attempts.gateway_payment_method_reference IS NULL
                AND attempts.gateway_response IS NULL
                AND attempts.gateway_response_code IS NULL
                AND attempts.gateway_response_text IS NULL
                AND attempts.gateway_avs_response IS NULL
                AND attempts.gateway_condition IS NULL
                AND attempts.resolution_code IS NULL
                AND attempts.payment_type IS NULL
                AND attempts.card_brand IS NULL
                AND attempts.card_last4 IS NULL
                AND attempts.card_exp_month IS NULL
                AND attempts.card_exp_year IS NULL
                AND attempts.gateway_lifecycle_action IS NULL
                AND attempts.gateway_lifecycle_at IS NULL
                AND attempts.gateway_lifecycle_reconciled_at IS NULL
                AND NOT EXISTS (
                    SELECT 1
                    FROM public.billing_processor_charges AS pending_charges
                    WHERE pending_charges.attempt_id = attempts.id
                )
            ) AS allowed_pending,
            (
                attempts.gateway_lifecycle_status = 'unknown'
                AND attempts.refunded_amount_cents = 0
                AND (
                    attempts.status = 'declined'
                    OR (
                        attempts.status = 'failed'
                        AND attempts.submitted_at IS NULL
                    )
                    OR (
                        attempts.status IN ('declined', 'failed')
                        AND EXISTS (
                            SELECT 1
                            FROM public.billing_processor_charges
                                AS attempt_charges
                            WHERE attempt_charges.attempt_id = attempts.id
                        )
                    )
                )
            ) AS terminal_safe
        FROM public.billing_payment_attempts AS attempts
        WHERE attempts.billing_scope_id = p_billing_scope_id
            AND attempts.subscriber_id = p_subscriber_id
            AND attempts.host_charge_target_id = p_host_charge_target_id
            AND attempts.attempt_kind = 'host_charge'
    )
    SELECT
        COALESCE(bool_or(contender), false),
        COALESCE(bool_or(NOT (
            contender
            OR allowed_pending
            OR terminal_safe
        )), false)
    INTO has_contender, has_unsafe_attempt
    FROM target_attempts;

    IF p_mode = 'submit' AND NOT EXISTS (
        SELECT 1
        FROM public.billing_payment_attempts AS attempts
        WHERE attempts.id = p_attempt_id
            AND attempts.billing_scope_id = p_billing_scope_id
            AND attempts.subscriber_id = p_subscriber_id
            AND attempts.host_charge_target_id = p_host_charge_target_id
            AND attempts.attempt_kind = 'host_charge'
            AND attempts.status = 'pending'
            AND attempts.submitted_at IS NULL
            AND attempts.resolved_at IS NULL
            AND attempts.gateway_lifecycle_status = 'unknown'
            AND attempts.refunded_amount_cents = 0
            AND attempts.gateway_transaction_id IS NULL
            AND attempts.gateway_payment_method_reference IS NULL
            AND attempts.gateway_response IS NULL
            AND attempts.gateway_response_code IS NULL
            AND attempts.gateway_response_text IS NULL
            AND attempts.gateway_avs_response IS NULL
            AND attempts.gateway_condition IS NULL
            AND attempts.resolution_code IS NULL
            AND attempts.payment_type IS NULL
            AND attempts.card_brand IS NULL
            AND attempts.card_last4 IS NULL
            AND attempts.card_exp_month IS NULL
            AND attempts.card_exp_year IS NULL
            AND attempts.gateway_lifecycle_action IS NULL
            AND attempts.gateway_lifecycle_at IS NULL
            AND attempts.gateway_lifecycle_reconciled_at IS NULL
            AND NOT EXISTS (
                SELECT 1
                FROM public.billing_processor_charges AS pending_charges
                WHERE pending_charges.attempt_id = attempts.id
            )
    ) THEN
        RETURN 'unsafe';
    END IF;

    SELECT EXISTS (
        SELECT 1
        FROM public.billing_processor_charges AS charges
        INNER JOIN public.billing_payment_attempts AS attempts
            ON attempts.id = charges.attempt_id
        WHERE charges.billing_scope_id = p_billing_scope_id
            AND attempts.subscriber_id = p_subscriber_id
            AND charges.host_charge_target_id = p_host_charge_target_id
            AND NOT (
                (p_mode = 'reserve' AND attempts.idempotency_key =
                    p_idempotency_key)
                OR (p_mode = 'submit' AND attempts.id = p_attempt_id)
            )
            AND NOT (
                charges.progression_state = 'externally_reversed'
                AND charges.externally_reversed_at IS NOT NULL
                AND charges.attempt_kind = 'host_charge'
                AND attempts.status IN ('declined', 'failed')
                AND EXISTS (
                    SELECT 1
                    FROM public.billing_external_reversal_attestations
                        AS attestations
                    WHERE attestations.processor_charge_id = charges.id
                        AND attestations.attempt_id = charges.attempt_id
                        AND attestations.prior_resolution_code =
                            'processor_charge_external_reversal_required'
                        AND charges.state_code =
                            attestations.prior_resolution_code
                        AND (
                            (
                                attestations.reversal_kind = 'refund'
                                AND attestations.final_resolution_code =
                                    'processor_charge_externally_refunded'
                            )
                            OR (
                                attestations.reversal_kind = 'void'
                                AND attestations.final_resolution_code =
                                    'processor_charge_externally_voided'
                            )
                        )
                        AND attestations.gateway_account_id =
                            charges.gateway_account_id
                        AND attestations.gateway_account_id =
                            attempts.gateway_account_id
                        AND attestations.gateway_configuration_id =
                            attempts.gateway_configuration_id
                        AND attestations.gateway_order_id =
                            charges.gateway_order_id
                        AND attestations.gateway_order_id =
                            attempts.gateway_order_id
                        AND attestations.amount_cents =
                            charges.amount_cents
                        AND attestations.amount_cents =
                            attempts.amount_cents
                        AND attestations.currency = charges.currency
                        AND attestations.currency = attempts.currency
                        AND attestations.gateway_transaction_id =
                            charges.gateway_transaction_id
                        AND attestations.gateway_payment_method_reference
                            IS NOT DISTINCT FROM
                            charges.gateway_payment_method_reference
                        AND attestations.gateway_response
                            IS NOT DISTINCT FROM charges.gateway_response
                        AND attestations.gateway_response_code
                            IS NOT DISTINCT FROM
                            charges.gateway_response_code
                        AND attestations.gateway_response_text
                            IS NOT DISTINCT FROM
                            charges.gateway_response_text
                        AND attestations.gateway_condition
                            IS NOT DISTINCT FROM charges.gateway_condition
                        AND attestations.payment_type
                            IS NOT DISTINCT FROM charges.payment_type
                        AND attestations.card_brand
                            IS NOT DISTINCT FROM charges.card_brand
                        AND attestations.card_last4
                            IS NOT DISTINCT FROM charges.card_last4
                        AND attestations.card_exp_month
                            IS NOT DISTINCT FROM charges.card_exp_month
                        AND attestations.card_exp_year
                            IS NOT DISTINCT FROM charges.card_exp_year
                )
            )
    )
    INTO has_unsafe_charge;

    IF has_unsafe_attempt OR has_unsafe_charge THEN
        RETURN 'unsafe';
    END IF;

    IF p_mode = 'reserve' AND has_contender THEN
        RETURN 'idempotent_contender';
    END IF;

    RETURN 'safe';
END
$$;
