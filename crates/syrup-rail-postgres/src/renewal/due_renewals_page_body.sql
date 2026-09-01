-- Composition contract: this fragment closes eligible_subscriptions and uses
-- $1-$3/$5-$12. Its caller appends the LIMIT placeholder after any mode and
-- continuation binds.
)
SELECT subscriptions.billing_scope_id, subscriptions.id,
    subscriptions.required_gateway_account_mode,
    subscriptions.next_renewal_at,
    subscriptions.next_payment_attempt_at,
    COALESCE(renewal_attempts.attempt_sequence_count, 0)::bigint
        AS attempt_sequence_count
FROM eligible_subscriptions AS subscriptions
LEFT JOIN LATERAL (
    SELECT
        COUNT(*) FILTER (
            WHERE attempts.attempt_kind = 'subscription_renewal'
                AND attempts.status = 'failed'
                AND attempts.resolution_code = ANY($1::text[])
                AND attempts.gateway_configuration_id IS NOT DISTINCT FROM
                    subscriptions.gateway_configuration_id
        ) AS automatic_infrastructure_attempt_count,
        MAX(attempts.resolved_at) FILTER (
            WHERE attempts.attempt_kind = 'subscription_renewal'
                AND attempts.status = 'failed'
                AND attempts.resolution_code = ANY($2::text[])
                AND attempts.gateway_configuration_id IS NOT DISTINCT FROM
                    subscriptions.gateway_configuration_id
        ) AS last_automatic_infrastructure_failure_at,
        COUNT(*) FILTER (
            WHERE attempts.attempt_kind = 'subscription_renewal'
                AND attempts.status = 'failed'
                AND attempts.resolution_code = ANY($3::text[])
        ) AS rate_limited_attempt_count,
        MAX(attempts.resolved_at) FILTER (
            WHERE attempts.attempt_kind = 'subscription_renewal'
                AND attempts.status = 'failed'
                AND attempts.resolution_code = ANY($3::text[])
        ) AS last_rate_limited_at,
        COUNT(*) AS attempt_sequence_count,
        BOOL_OR(
            attempts.status IN ('pending', 'unknown', 'review_required', 'approved')
            AND NOT (
                attempts.status = ANY($12::text[])
                AND attempts.submitted_at IS NULL
                AND attempts.created_at <= $10::timestamptz
                    - ($11::bigint * interval '1 second')
            )
        ) AS has_blocking_attempt
    FROM billing_payment_attempts AS attempts
    WHERE attempts.subscription_id = subscriptions.id
        AND attempts.billing_period_start_at = subscriptions.next_renewal_at
        AND attempts.attempt_kind IN ('subscription_renewal', 'subscription_recovery')
) AS renewal_attempts ON true
WHERE COALESCE(renewal_attempts.has_blocking_attempt, false) = false
    AND COALESCE(renewal_attempts.automatic_infrastructure_attempt_count, 0)
        < $5
    AND (
        renewal_attempts.last_automatic_infrastructure_failure_at IS NULL
        OR renewal_attempts.last_automatic_infrastructure_failure_at
            <= $10::timestamptz - ($6::bigint * interval '1 second')
    )
    AND (
        renewal_attempts.last_rate_limited_at IS NULL
        OR renewal_attempts.last_rate_limited_at <= $10::timestamptz
            - (
                CASE WHEN COALESCE(
                    renewal_attempts.rate_limited_attempt_count,
                    0
                ) >= $7 THEN $8::bigint ELSE $9::bigint END
                * interval '1 second'
            )
    )
ORDER BY subscriptions.next_payment_attempt_at ASC, subscriptions.id ASC
