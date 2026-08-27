-- Forward-only schema-v4 validation after prepare_from_v3.sql has committed.
--
-- PostgreSQL validates this check under SHARE UPDATE EXCLUSIVE rather than
-- retaining the preparation phase's ACCESS EXCLUSIVE lock for the ledger scan.

ALTER TABLE public.billing_payment_attempts
    VALIDATE CONSTRAINT billing_payment_attempts_required_gateway_account_mode_check;

ALTER TABLE public.billing_subscriptions
    VALIDATE CONSTRAINT billing_subscriptions_required_gateway_account_mode_check;

ALTER TABLE public.billing_payment_attempts
    VALIDATE CONSTRAINT billing_payment_attempts_resolution_code_check;
