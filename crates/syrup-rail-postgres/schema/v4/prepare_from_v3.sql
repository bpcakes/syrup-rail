-- Forward-only schema-v4 preparation for a canonical schema-v3 database.
--
-- Apply and commit this artifact before validation. The constant default makes
-- the metadata-only column addition fast and keeps stopped-or-draining v3
-- writers deterministic if they omit the new column. The NOT VALID check is
-- enforced for new writes without scanning historical payment attempts while
-- this statement holds ACCESS EXCLUSIVE.

ALTER TABLE public.billing_payment_attempts
    ADD COLUMN required_gateway_account_mode text NOT NULL DEFAULT 'live';

ALTER TABLE public.billing_payment_attempts
    ADD CONSTRAINT billing_payment_attempts_required_gateway_account_mode_check
    CHECK (required_gateway_account_mode IN ('live', 'test')) NOT VALID;

-- Existing subscriptions were created by schema-v3 runtimes, which only
-- submitted live transactions. Persist that trusted historical policy so a
-- later test-configured runtime cannot reinterpret their renewal authority.
ALTER TABLE public.billing_subscriptions
    ADD COLUMN required_gateway_account_mode text NOT NULL DEFAULT 'live';

ALTER TABLE public.billing_subscriptions
    ADD CONSTRAINT billing_subscriptions_required_gateway_account_mode_check
    CHECK (required_gateway_account_mode IN ('live', 'test')) NOT VALID;

-- Schema v4 distinguishes both directions of an account-mode mismatch in the
-- durable ledger. Replace v3's narrower vocabulary without scanning existing
-- rows; validation is performed by the separate validation artifact.
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
            'subscription_approved_payment_method_update_inactive_replacement_method'
        )
    ) NOT VALID;
