-- Canonical v4 rows, with distinct ownership for each subscriber in one account.
INSERT INTO billing_gateway_provider_rate_limits(provider_key) VALUES ('nmi');
INSERT INTO billing_gateway_accounts(id, billing_scope_id, provider_key, gateway_configuration_id)
VALUES (md5('account')::uuid, md5('scope')::uuid, 'nmi', md5('configuration')::uuid);

INSERT INTO billing_payment_methods(
    id, billing_scope_id, subscriber_id, gateway_account_id,
    gateway_payment_method_reference, status
)
SELECT md5('method' || n)::uuid, md5('scope')::uuid,
       md5('subscriber' || n)::uuid, md5('account')::uuid, 'vault_' || n, 'active'
FROM generate_series(1, 4096) AS n;

INSERT INTO billing_subscriptions(
    id, billing_scope_id, subscriber_id, plan_key, status, gateway_account_id,
    payment_method_id, amount_cents, current_period_start_at, current_period_end_at,
    next_renewal_at, initial_transaction_id, phase, recurring_period_kind,
    recurring_period_count, dunning_retry_delays_seconds, dunning_exhaustion,
    past_due_access, next_payment_attempt_at, required_gateway_account_mode
)
SELECT md5('subscription' || n)::uuid, md5('scope')::uuid,
       md5('subscriber' || n)::uuid, 'plan', 'active', md5('account')::uuid,
       md5('method' || n)::uuid, 2000, now(), now() + interval '1 month',
       now() + interval '1 month', 'transaction_' || n, 'recurring', 'calendar_months',
       1, ARRAY[]::bigint[], 'remain_past_due', 'suspend_immediately',
       now() + interval '1 month', 'live'
FROM generate_series(1, 4096) AS n;

INSERT INTO billing_payment_attempts(
    id, billing_scope_id, subscriber_id, plan_key, subscription_id, payment_method_id,
    attempt_kind, status, idempotency_key, request_fingerprint, amount_cents,
    billing_period_start_at, billing_period_end_at, gateway_account_id,
    gateway_configuration_id, gateway_order_id, gateway_transaction_id,
    submitted_at, resolved_at, subscription_expected_payment_method_id,
    subscription_expected_initial_transaction_id, subscription_expected_status,
    required_gateway_account_mode
)
SELECT md5('attempt' || n)::uuid, md5('scope')::uuid,
       md5('subscriber' || n)::uuid, 'plan', md5('subscription' || n)::uuid,
       md5('method' || n)::uuid, 'subscription_renewal', 'approved', 'key_' || n,
       'fingerprint', 2000, now(), now() + interval '1 month', md5('account')::uuid,
       md5('configuration')::uuid, 'order_' || n, 'txn_' || n, now(), now(),
       md5('method' || n)::uuid, 'transaction_' || n, 'active', 'live'
FROM generate_series(1, 4096) AS n;

-- Historical statuses retain a saved method too. Their current-reference
-- check must not be narrowed to the partial active/past_due owner index.
UPDATE billing_subscriptions
SET status = 'canceled', canceled_at = clock_timestamp(), next_payment_attempt_at = NULL
WHERE initial_transaction_id = 'transaction_1';
UPDATE billing_subscriptions
SET status = 'unpaid', unpaid_at = clock_timestamp(), next_payment_attempt_at = NULL
WHERE initial_transaction_id = 'transaction_2';
UPDATE billing_subscriptions SET status = 'past_due', next_payment_attempt_at = NULL
WHERE initial_transaction_id = 'transaction_3';

ANALYZE billing_subscriptions;
ANALYZE billing_payment_methods;
ANALYZE billing_gateway_accounts;
ANALYZE billing_payment_attempts;
