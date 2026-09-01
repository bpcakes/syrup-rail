-- Forward-only schema-v4 renewal-index build after validate_from_v3.sql.
--
-- Run every statement in this artifact outside an explicit transaction while
-- schema-v3 writers continue. PostgreSQL's concurrent index builds avoid
-- blocking their ordinary inserts and updates. The covering replacement keeps
-- the all-mode scan index-only after required_gateway_account_mode becomes a
-- projected dispatch field. If a build leaves an invalid index, follow the
-- cutover guide's concurrent cleanup before retrying.

CREATE INDEX CONCURRENTLY IF NOT EXISTS billing_subscriptions_due_v4_idx
ON public.billing_subscriptions (next_payment_attempt_at, id)
INCLUDE (
    billing_scope_id,
    gateway_account_id,
    next_renewal_at,
    required_gateway_account_mode
)
WHERE status IN ('active', 'past_due')
    AND next_payment_attempt_at IS NOT NULL;

CREATE INDEX CONCURRENTLY IF NOT EXISTS billing_subscriptions_due_mode_idx
ON public.billing_subscriptions (
    required_gateway_account_mode,
    next_payment_attempt_at,
    id
)
INCLUDE (billing_scope_id, gateway_account_id, next_renewal_at)
WHERE status IN ('active', 'past_due')
    AND next_payment_attempt_at IS NOT NULL;
