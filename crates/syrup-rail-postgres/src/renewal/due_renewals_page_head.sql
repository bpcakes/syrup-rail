-- Composition contract: this fragment uses $4/$10 and opens
-- eligible_subscriptions. The first-page statement appends the body directly;
-- the continuation inserts its $11/$12 keyset predicate first.
-- NOT MATERIALIZED removes the old mandatory full-candidate evaluation and
-- lets PostgreSQL stop an ordered index plan at the outer LIMIT. Plan choice
-- remains cost-based and must be rehearsed against representative host data.
WITH eligible_subscriptions AS NOT MATERIALIZED (
    SELECT
        subscriptions.billing_scope_id,
        subscriptions.id,
        subscriptions.next_renewal_at,
        subscriptions.next_payment_attempt_at,
        accounts.gateway_configuration_id
    FROM billing_subscriptions AS subscriptions
    JOIN billing_gateway_accounts AS accounts
        ON accounts.billing_scope_id = subscriptions.billing_scope_id
        AND accounts.id = subscriptions.gateway_account_id
    JOIN billing_gateway_provider_rate_limits AS provider_limits
        ON provider_limits.provider_key = accounts.provider_key
    WHERE subscriptions.status IN ('active', 'past_due')
        AND subscriptions.next_payment_attempt_at <= $10::timestamptz
        AND provider_limits.rate_limited_until <= $10::timestamptz
        AND (
            accounts.mutation_rate_limited_until IS NULL
            OR accounts.mutation_rate_limited_until <= $10::timestamptz
        )
        AND NOT EXISTS (
            SELECT 1
            FROM billing_payment_attempts AS update_attempts
            WHERE update_attempts.subscription_id = subscriptions.id
                AND update_attempts.attempt_kind = 'subscription_payment_method_update'
                AND update_attempts.status IN ('pending', 'unknown', 'review_required')
                AND NOT (
                    update_attempts.status = 'pending'
                    AND update_attempts.submitted_at IS NULL
                    AND update_attempts.created_at <= $10::timestamptz
                        - ($4::bigint * interval '1 second')
                )
        )
