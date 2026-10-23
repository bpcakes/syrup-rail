-- Forward-only Syrup Rail schema upgrade from version 5 to version 6.
--
-- Apply these bytes once inside a host-owned transaction while all schema-v5
-- billing writers are stopped. Version 6 widens two closed CHECK lists so
-- billing-period expiry has typed, durable dispositions:
-- `billing_payment_attempts.resolution_code` accepts
-- 'subscription_period_expired_before_charge' and
-- 'subscription_approved_period_expired', and an external-reversal
-- attestation may record 'subscription_approved_period_expired' as its prior
-- resolution. No column, row, index, or other constraint changes. Replacing
-- each constraint validates its whole table under the ACCESS EXCLUSIVE lock
-- taken by ALTER TABLE, so rehearse the duration on a production-sized copy.
-- Roll back before commit on any failure; after commit keep writers stopped
-- and roll forward with a schema-v6-aware build.

SET LOCAL lock_timeout = '5s';

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
        FROM information_schema.columns
        WHERE table_schema = 'public'
            AND table_name = 'billing_payment_attempts'
            AND column_name = 'billing_address_line1'
    )
    THEN
        RAISE EXCEPTION
            'schema-v6 upgrade requires schema v5; apply the earlier upgrades first';
    END IF;
    IF EXISTS (
        SELECT 1
        FROM pg_catalog.pg_constraint AS constraints
        JOIN pg_catalog.pg_class AS tables ON tables.oid = constraints.conrelid
        JOIN pg_catalog.pg_namespace AS namespaces ON namespaces.oid = tables.relnamespace
        WHERE namespaces.nspname = 'public'
            AND constraints.conname IN (
                'billing_payment_attempts_resolution_code_check',
                'billing_external_reversal_attestations_resolution_check'
            )
            AND pg_catalog.pg_get_constraintdef(constraints.oid)
                LIKE '%subscription_approved_period_expired%'
    )
    THEN
        RAISE EXCEPTION
            'schema-v6 upgrade is already applied or was manually altered; inspect the catalog and do not rerun this artifact';
    END IF;
END
$$;

ALTER TABLE public.billing_payment_attempts
    DROP CONSTRAINT billing_payment_attempts_resolution_code_check,
    ADD CONSTRAINT billing_payment_attempts_resolution_code_check
        CHECK (
            resolution_code IS NULL
            OR resolution_code IN (
                'subscription_initial_current_subscription_conflict',
                'subscription_initial_current_grant_conflict',
                'subscription_initial_externally_refunded',
                'subscription_initial_externally_voided',
                'processor_charge_externally_refunded',
                'processor_charge_externally_voided',
                'subscription_initial_prepared_attempt_expired',
                'subscription_renewal_retry_state_changed_before_charge',
                'gateway_live_readiness_failed_before_submission',
                'gateway_test_readiness_failed_before_submission',
                'gateway_malformed_before_submission',
                'gateway_request_rejected_before_submission',
                'gateway_configuration_before_submission',
                'gateway_unavailable_before_submission',
                'gateway_provider_rate_limited_before_submission',
                'gateway_account_rate_limited_before_submission',
                'gateway_account_mutation_cooldown_before_submission',
                'host_charge_approved_stale_state',
                'subscription_approved_renewal_stale_state',
                'subscription_approved_recovery_stale_state',
                'subscription_approved_recovery_inactive_replacement_method',
                'subscription_approved_payment_method_update_subscription_ineligible',
                'subscription_approved_payment_method_update_stale_state',
                'subscription_approved_payment_method_update_inactive_replacement_method',
                'subscription_period_expired_before_charge',
                'subscription_approved_period_expired'
            )
        );

ALTER TABLE public.billing_external_reversal_attestations
    DROP CONSTRAINT billing_external_reversal_attestations_resolution_check,
    ADD CONSTRAINT billing_external_reversal_attestations_resolution_check
        CHECK (
            prior_resolution_code IN (
                'subscription_initial_current_grant_conflict',
                'processor_charge_external_reversal_required',
                'subscription_approved_period_expired'
            )
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
        );
