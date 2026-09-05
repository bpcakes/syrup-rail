-- Read-only preflight for upgrading Syrup Rail schema v5 to v6.
--
-- Run before scheduling the cutover, and again after stopping schema-v5
-- writers, to size the attempt backfill and inventory review-required rows
-- that the conservative evidence migration cannot classify as absent.

SELECT
    count(*) AS retained_attempt_count,
    count(*) FILTER (
        WHERE gateway_transaction_id IS NULL
            AND gateway_payment_method_reference IS NULL
            AND gateway_response IS NULL
            AND gateway_response_code IS NULL
            AND gateway_condition IS NULL
            AND gateway_response_text IS NULL
    ) AS attempts_classified_absent_count,
    count(*) FILTER (
        WHERE status = 'review_required'
            AND NOT (
                gateway_transaction_id IS NULL
                AND gateway_payment_method_reference IS NULL
                AND gateway_response IS NULL
                AND gateway_response_code IS NULL
                AND gateway_condition IS NULL
                AND gateway_response_text IS NULL
            )
    ) AS review_required_unclassified_count,
    (
        SELECT count(*)
        FROM public.billing_processor_charges
    ) AS retained_processor_charge_count,
    (
        SELECT count(*)
        FROM public.billing_external_reversal_attestations
    ) AS retained_external_reversal_attestation_count
FROM public.billing_payment_attempts;
