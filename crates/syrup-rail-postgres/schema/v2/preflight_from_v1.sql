-- Read-only preflight for upgrading Syrup Rail schema v1 to v2.
--
-- Every returned row is a migration blocker: v1 says that the subscription
-- is past due, but neither supported v1 transition establishes its causal
-- payment failure. An operator-reviewed recovery that was manually failed
-- from an active optimistic subscription snapshot is valid v1 causal history
-- and cuts over without consuming a v2 automatic-dunning step.

SELECT
    subscriptions.id AS subscription_id,
    subscriptions.billing_scope_id,
    subscriptions.subscriber_id,
    subscriptions.plan_key,
    subscriptions.next_renewal_at,
    count(attempts.id) AS qualifying_failure_count
FROM public.billing_subscriptions AS subscriptions
LEFT JOIN public.billing_payment_attempts AS attempts
    ON attempts.subscription_id = subscriptions.id
    AND attempts.billing_scope_id = subscriptions.billing_scope_id
    AND attempts.subscriber_id = subscriptions.subscriber_id
    AND attempts.plan_key = subscriptions.plan_key
    AND attempts.attempt_kind = 'subscription_renewal'
    AND attempts.billing_period_start_at = subscriptions.next_renewal_at
    AND attempts.submitted_at IS NOT NULL
    AND attempts.status IN ('declined', 'failed')
    AND attempts.resolution_code IS NULL
WHERE subscriptions.status = 'past_due'
GROUP BY
    subscriptions.id,
    subscriptions.billing_scope_id,
    subscriptions.subscriber_id,
    subscriptions.plan_key,
    subscriptions.next_renewal_at
HAVING count(attempts.id) = 0
    AND NOT EXISTS (
        SELECT 1
        FROM public.billing_payment_attempts AS recovery_attempts
        WHERE recovery_attempts.subscription_id = subscriptions.id
            AND recovery_attempts.billing_scope_id =
                subscriptions.billing_scope_id
            AND recovery_attempts.subscriber_id = subscriptions.subscriber_id
            AND recovery_attempts.plan_key = subscriptions.plan_key
            AND recovery_attempts.attempt_kind = 'subscription_recovery'
            AND recovery_attempts.billing_period_start_at =
                subscriptions.next_renewal_at
            AND recovery_attempts.subscription_expected_status = 'active'
            AND recovery_attempts.submitted_at IS NOT NULL
            AND recovery_attempts.review_required_at IS NOT NULL
            AND recovery_attempts.status = 'failed'
            AND recovery_attempts.resolution_code IS NULL
    )
ORDER BY
    subscriptions.billing_scope_id,
    subscriptions.subscriber_id,
    subscriptions.plan_key,
    subscriptions.id;
