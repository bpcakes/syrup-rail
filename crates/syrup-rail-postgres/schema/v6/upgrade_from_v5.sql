-- Forward-only schema v5 to v6. Apply once in a host-owned transaction
-- after stopping every billing writer. Retained attempts fail closed unless
-- empty; retained charges and reversal attestations carry approval provenance.

ALTER TABLE public.billing_payment_attempts
    ADD COLUMN gateway_approval_evidence text NOT NULL DEFAULT 'unclassified'
        CONSTRAINT billing_payment_attempts_approval_evidence_check
        CHECK (gateway_approval_evidence IN ('unclassified', 'absent', 'text_only', 'structured'));

ALTER TABLE public.billing_processor_charges
    ADD COLUMN gateway_approval_evidence text NOT NULL DEFAULT 'structured'
        CONSTRAINT billing_processor_charges_approval_evidence_check
        CHECK (gateway_approval_evidence IN ('unclassified', 'absent', 'text_only', 'structured'));

ALTER TABLE public.billing_external_reversal_attestations
    ADD COLUMN gateway_approval_evidence text NOT NULL DEFAULT 'structured'
        CONSTRAINT billing_external_reversal_attestations_approval_evidence_check
        CHECK (gateway_approval_evidence IN ('unclassified', 'absent', 'text_only', 'structured'));

-- The existence of a retained processor-charge row is itself durable proof
-- that v5 observed an authoritative approval or a structured approval field.
-- Reconstruct that fact without reparsing provider strings, and keep existing
-- reversal attestations aligned with their source charges.
-- PostgreSQL's constant ADD COLUMN default supplies the retained value without
-- updating every tuple. Changing the INSERT default does not change old rows.
-- New writers must still classify explicitly; omissions remain fail-closed.
ALTER TABLE public.billing_processor_charges
    ALTER COLUMN gateway_approval_evidence SET DEFAULT 'unclassified';

ALTER TABLE public.billing_external_reversal_attestations
    ALTER COLUMN gateway_approval_evidence SET DEFAULT 'unclassified';

-- Only genuinely empty retained observations establish absence. Old local
-- query notes may have overwritten provider text and cannot prove its absence.
UPDATE public.billing_payment_attempts
SET gateway_approval_evidence = 'absent'
WHERE gateway_transaction_id IS NULL
    AND gateway_payment_method_reference IS NULL
    AND gateway_response IS NULL
    AND gateway_response_code IS NULL
    AND gateway_condition IS NULL
    AND gateway_response_text IS NULL;

-- New reservations have no observation yet. All provider/error writes must
-- explicitly persist their classification with the evidence bundle.
ALTER TABLE public.billing_payment_attempts
    ALTER COLUMN gateway_approval_evidence SET DEFAULT 'absent';

CREATE OR REPLACE FUNCTION public.billing_guard_processor_charge_evidence_update()
RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, public
AS $$
BEGIN
    IF NEW.gateway_approval_evidence IS DISTINCT FROM OLD.gateway_approval_evidence
        OR NEW.gateway_payment_method_reference IS DISTINCT FROM
            OLD.gateway_payment_method_reference
        OR NEW.gateway_response IS DISTINCT FROM OLD.gateway_response
        OR NEW.gateway_response_code IS DISTINCT FROM
            OLD.gateway_response_code
        OR NEW.gateway_response_text IS DISTINCT FROM
            OLD.gateway_response_text
        OR NEW.gateway_condition IS DISTINCT FROM OLD.gateway_condition
        OR NEW.payment_type IS DISTINCT FROM OLD.payment_type
        OR NEW.card_brand IS DISTINCT FROM OLD.card_brand
        OR NEW.card_last4 IS DISTINCT FROM OLD.card_last4
        OR NEW.card_exp_month IS DISTINCT FROM OLD.card_exp_month
        OR NEW.card_exp_year IS DISTINCT FROM OLD.card_exp_year
        OR (
            NEW.gateway_transaction_id IS DISTINCT FROM
                OLD.gateway_transaction_id
            AND NOT (
                public.billing_canonical_gateway_transaction_id(
                    OLD.gateway_transaction_id
                ) IS NULL
                AND public.billing_canonical_gateway_transaction_id(
                    NEW.gateway_transaction_id
                ) IS NOT NULL
                AND NEW.gateway_transaction_id =
                    public.billing_canonical_gateway_transaction_id(
                        NEW.gateway_transaction_id
                    )
            )
        )
    THEN
        RAISE EXCEPTION 'processor charge evidence is immutable';
    END IF;
    RETURN NEW;
END
$$;

DROP TRIGGER billing_processor_charge_evidence_immutable ON public.billing_processor_charges;
CREATE TRIGGER billing_processor_charge_evidence_immutable
BEFORE UPDATE OF
    gateway_approval_evidence,
    gateway_transaction_id,
    gateway_payment_method_reference,
    gateway_response,
    gateway_response_code,
    gateway_response_text,
    gateway_condition,
    payment_type,
    card_brand,
    card_last4,
    card_exp_month,
    card_exp_year
ON public.billing_processor_charges
FOR EACH ROW
EXECUTE FUNCTION public.billing_guard_processor_charge_evidence_update();

-- Rebind ledger admission to the v6 evidence classification. Every path that
-- infers no financial effect must require explicit absence.
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
                AND attempts.gateway_approval_evidence = 'absent'
                AND attempts.gateway_transaction_id IS NULL
                AND attempts.gateway_payment_method_reference IS NULL
                AND attempts.gateway_response IS NULL
                AND attempts.gateway_response_code IS NULL
                AND attempts.gateway_response_text IS NULL
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
                    (
                        attempts.gateway_approval_evidence = 'absent'
                        AND (
                            attempts.status = 'declined'
                            OR (
                                attempts.status = 'failed'
                                AND attempts.submitted_at IS NULL
                            )
                        )
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
            AND attempts.gateway_approval_evidence = 'absent'
            AND attempts.gateway_transaction_id IS NULL
            AND attempts.gateway_payment_method_reference IS NULL
            AND attempts.gateway_response IS NULL
            AND attempts.gateway_response_code IS NULL
            AND attempts.gateway_response_text IS NULL
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
                        AND attestations.gateway_approval_evidence
                            IS NOT DISTINCT FROM
                            charges.gateway_approval_evidence
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
