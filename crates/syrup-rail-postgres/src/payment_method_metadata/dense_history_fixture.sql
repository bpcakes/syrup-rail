-- Concentrate 1,024 prior approvals on method 1. Each economic period and
-- provider identity is distinct; txn_1 remains its latest approved attempt.
INSERT INTO billing_payment_attempts(
    id, billing_scope_id, subscriber_id, plan_key, subscription_id, payment_method_id,
    attempt_kind, status, idempotency_key, request_fingerprint, amount_cents,
    billing_period_start_at, billing_period_end_at, gateway_account_id,
    gateway_configuration_id, gateway_order_id, gateway_transaction_id,
    submitted_at, resolved_at, subscription_expected_payment_method_id,
    subscription_expected_initial_transaction_id, subscription_expected_status,
    required_gateway_account_mode
)
SELECT md5('history_attempt' || n)::uuid, a.billing_scope_id, a.subscriber_id,
       a.plan_key, a.subscription_id, a.payment_method_id, a.attempt_kind, a.status,
       'history_key_' || n, a.request_fingerprint, a.amount_cents,
       a.billing_period_start_at - n * interval '1 week',
       a.billing_period_start_at - (n - 1) * interval '1 week',
       a.gateway_account_id, a.gateway_configuration_id, 'history_order_' || n,
       'history_txn_' || n, a.submitted_at - n * interval '1 week',
       a.resolved_at - n * interval '1 week', a.subscription_expected_payment_method_id,
       a.subscription_expected_initial_transaction_id, a.subscription_expected_status,
       a.required_gateway_account_mode
FROM billing_payment_attempts AS a CROSS JOIN generate_series(1, 1024) AS n
WHERE a.gateway_transaction_id = 'txn_1';

ANALYZE billing_payment_attempts;
