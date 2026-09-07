-- Read-only preflight for upgrading Syrup Rail schema v4 to v5.
-- Run before scheduling the cutover and again after stopping all v4 writers.
-- Counts cover tuple-validation blockers, retained evidence backfill, review
-- items and terminal host attempts whose admission cannot survive the cutover.

WITH attestation_counts AS (
    SELECT
        count(*) AS retained_attestation_count,
        count(*) FILTER (
            WHERE NOT (
                (
                    prior_resolution_code =
                        'subscription_initial_current_grant_conflict'
                    AND (
                        (
                            reversal_kind = 'refund'
                            AND final_resolution_code =
                                'subscription_initial_externally_refunded'
                        )
                        OR (
                            reversal_kind = 'void'
                            AND final_resolution_code =
                                'subscription_initial_externally_voided'
                        )
                    )
                )
                OR (
                    prior_resolution_code =
                        'processor_charge_external_reversal_required'
                    AND (
                        (
                            reversal_kind = 'refund'
                            AND final_resolution_code IN (
                                'subscription_initial_externally_refunded',
                                'processor_charge_externally_refunded'
                            )
                        )
                        OR (
                            reversal_kind = 'void'
                            AND final_resolution_code IN (
                                'subscription_initial_externally_voided',
                                'processor_charge_externally_voided'
                            )
                        )
                    )
                )
            )
        ) AS incompatible_attestation_count
    FROM public.billing_external_reversal_attestations
), evidence_counts AS (
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
        count(*) FILTER (
            WHERE attempt_kind = 'host_charge'
                AND (status = 'declined' OR (status = 'failed' AND submitted_at IS NULL))
                AND gateway_lifecycle_status = 'unknown'
                AND refunded_amount_cents = 0
                AND NOT EXISTS (
                    SELECT 1 FROM public.billing_processor_charges AS charges
                    WHERE charges.attempt_id = billing_payment_attempts.id
                )
                AND NOT (
                    gateway_transaction_id IS NULL
                    AND gateway_payment_method_reference IS NULL
                    AND gateway_response IS NULL
                    AND gateway_response_code IS NULL
                    AND gateway_condition IS NULL
                    AND gateway_response_text IS NULL
                )
        ) AS terminal_host_attempts_with_unclassified_evidence_count,
        (
            SELECT count(*)
            FROM public.billing_processor_charges
        ) AS retained_processor_charge_count
    FROM public.billing_payment_attempts
)
SELECT attestation_counts.*, evidence_counts.*
FROM attestation_counts CROSS JOIN evidence_counts;
