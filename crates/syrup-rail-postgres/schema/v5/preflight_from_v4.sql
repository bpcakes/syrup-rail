-- Read-only preflight for upgrading Syrup Rail schema v4 to v5.
--
-- Run this result query before scheduling the cutover to measure the retained
-- attestation scan and identify whether audited remediation is required. Run
-- it again after stopping schema-v4 writers if a zero-blocker observation is
-- required immediately before the transactional migration.

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
FROM public.billing_external_reversal_attestations;
