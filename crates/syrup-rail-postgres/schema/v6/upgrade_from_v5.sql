-- Forward-only schema v5 to v6. Apply once in a host-owned transaction
-- after stopping every billing writer. Retained observations stay unclassified.

ALTER TABLE public.billing_payment_attempts
    ADD COLUMN gateway_approval_evidence text NOT NULL DEFAULT 'unclassified'
        CONSTRAINT billing_payment_attempts_approval_evidence_check
        CHECK (gateway_approval_evidence IN ('unclassified', 'absent', 'text_only', 'structured'));

ALTER TABLE public.billing_processor_charges
    ADD COLUMN gateway_approval_evidence text NOT NULL DEFAULT 'unclassified'
        CONSTRAINT billing_processor_charges_approval_evidence_check
        CHECK (gateway_approval_evidence IN ('unclassified', 'absent', 'text_only', 'structured'));

ALTER TABLE public.billing_external_reversal_attestations
    ADD COLUMN gateway_approval_evidence text NOT NULL DEFAULT 'unclassified'
        CONSTRAINT billing_external_reversal_attestations_approval_evidence_check
        CHECK (gateway_approval_evidence IN ('unclassified', 'absent', 'text_only', 'structured'));

-- The existence of a retained processor-charge row is itself durable proof
-- that v5 observed an authoritative approval or a structured approval field.
-- Reconstruct that fact without reparsing provider strings, and keep existing
-- reversal attestations aligned with their source charges.
UPDATE public.billing_processor_charges
SET gateway_approval_evidence = 'structured';

UPDATE public.billing_external_reversal_attestations
SET gateway_approval_evidence = 'structured';

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
