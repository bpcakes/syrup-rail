-- Read-only audit of schema-v4 external-reversal attestations that block v5.
--
-- The result deliberately exposes only canonical internal identifiers and the
-- incompatible resolution tuple. Keep access to and exports of this protected
-- financial evidence inside the host's authorized operator process.

SELECT
    attempt_id,
    processor_charge_id,
    reversal_kind,
    prior_resolution_code,
    final_resolution_code
FROM public.billing_external_reversal_attestations
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
ORDER BY attempt_id, processor_charge_id;
