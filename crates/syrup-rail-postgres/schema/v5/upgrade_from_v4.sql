-- Forward-only Syrup Rail schema upgrade from version 4 to version 5.
--
-- Apply these bytes once inside a host-owned transaction while all schema-v4
-- billing writers are stopped. This ALTER TABLE takes ACCESS EXCLUSIVE and
-- scans all retained external-reversal attestations while the lock is held
-- through transaction end. Size and rehearse that maintenance window with
-- preflight_from_v4.sql. The transaction aborts when a tuple cannot be
-- represented by the typed runtime; remediate such financial evidence under
-- an audited host process before retrying this migration.

ALTER TABLE public.billing_external_reversal_attestations
    DROP CONSTRAINT billing_external_reversal_attestations_resolution_check,
    ADD CONSTRAINT billing_external_reversal_attestations_resolution_check
        CHECK (
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
        );
