-- READ-ONLY RETRY RECLASSIFICATION AUDIT
WITH retry_counts AS (
    SELECT
        subscriptions.id AS subscription_id,
        subscriptions.billing_scope_id,
        subscriptions.subscriber_id,
        subscriptions.plan_key,
        subscriptions.status AS subscription_status,
        count(attempts.id) FILTER (
            WHERE attempts.attempt_kind IN (
                    'subscription_renewal',
                    'subscription_recovery'
                )
                AND attempts.status IN ('declined', 'failed')
                AND (
                    attempts.resolution_code IS NULL
                    OR attempts.resolution_code NOT IN (
                        'subscription_renewal_retry_state_changed_before_charge',
                        'gateway_live_readiness_failed_before_submission',
                        'gateway_malformed_before_submission',
                        'gateway_request_rejected_before_submission',
                        'gateway_configuration_before_submission',
                        'gateway_unavailable_before_submission',
                        'gateway_provider_rate_limited_before_submission',
                        'gateway_account_mutation_cooldown_before_submission'
                    )
                )
        ) AS v1_terminal_failure_count,
        count(attempts.id) FILTER (
            WHERE attempts.attempt_kind = 'subscription_renewal'
                AND attempts.submitted_at IS NOT NULL
                AND attempts.status IN ('declined', 'failed')
                AND attempts.resolution_code IS NULL
        ) AS v2_automatic_failure_count
    FROM public.billing_subscriptions AS subscriptions
    LEFT JOIN public.billing_payment_attempts AS attempts
        ON attempts.subscription_id = subscriptions.id
        AND attempts.billing_scope_id = subscriptions.billing_scope_id
        AND attempts.subscriber_id = subscriptions.subscriber_id
        AND attempts.plan_key = subscriptions.plan_key
        AND attempts.billing_period_start_at = subscriptions.next_renewal_at
    WHERE subscriptions.status IN ('active', 'past_due')
    GROUP BY
        subscriptions.id,
        subscriptions.billing_scope_id,
        subscriptions.subscriber_id,
        subscriptions.plan_key,
        subscriptions.status
)
SELECT *
FROM retry_counts
WHERE v1_terminal_failure_count >= 5
    AND v2_automatic_failure_count < 5
ORDER BY billing_scope_id, subscriber_id, plan_key, subscription_id;
