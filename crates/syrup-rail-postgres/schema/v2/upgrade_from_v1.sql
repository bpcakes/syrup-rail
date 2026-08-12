-- Forward-only Syrup Rail schema upgrade from version 1 to version 2.
--
-- Apply these bytes once inside a host-owned transaction while all schema-v1
-- billing writers are stopped. The read-only preflight_from_v1.sql can expose
-- causal-history blockers before entering the maintenance window, but the
-- validation below remains authoritative. Version 1 could move an active
-- subscription to past_due when an operator manually failed a submitted
-- recovery attempt. That recovery is valid causal history for the legacy
-- status, but it does not consume a version-2 automatic-dunning step.

ALTER TABLE public.billing_subscriptions
    ADD COLUMN phase text,
    ADD COLUMN recurring_period_kind text,
    ADD COLUMN recurring_period_count integer,
    ADD COLUMN trial_amount_cents integer,
    ADD COLUMN trial_period_kind text,
    ADD COLUMN trial_period_count integer,
    ADD COLUMN dunning_retry_delays_seconds bigint[],
    ADD COLUMN dunning_exhaustion text,
    ADD COLUMN past_due_access text,
    ADD COLUMN next_payment_attempt_at timestamptz,
    ADD COLUMN unpaid_at timestamptz;

ALTER TABLE public.billing_payment_attempts
    ADD COLUMN subscription_initial_terms_version smallint,
    ADD COLUMN subscription_initial_start_kind text,
    ADD COLUMN subscription_initial_trial_amount_cents integer,
    ADD COLUMN subscription_initial_trial_period_kind text,
    ADD COLUMN subscription_initial_trial_period_count integer,
    ADD COLUMN subscription_initial_recurring_base_amount_cents integer,
    ADD COLUMN subscription_initial_recurring_period_kind text,
    ADD COLUMN subscription_initial_recurring_period_count integer,
    ADD COLUMN subscription_initial_dunning_retry_delays_seconds bigint[],
    ADD COLUMN subscription_initial_dunning_exhaustion text,
    ADD COLUMN subscription_initial_past_due_access text;

DO $$
BEGIN
    IF EXISTS (
        SELECT 1
        FROM public.billing_subscriptions AS subscriptions
        WHERE subscriptions.status = 'past_due'
            AND NOT EXISTS (
                SELECT 1
                FROM public.billing_payment_attempts AS attempts
                WHERE attempts.subscription_id = subscriptions.id
                    AND attempts.billing_scope_id =
                        subscriptions.billing_scope_id
                    AND attempts.subscriber_id = subscriptions.subscriber_id
                    AND attempts.plan_key = subscriptions.plan_key
                    AND attempts.attempt_kind = 'subscription_renewal'
                    AND attempts.billing_period_start_at =
                        subscriptions.next_renewal_at
                    AND attempts.submitted_at IS NOT NULL
                    AND attempts.status IN ('declined', 'failed')
                    AND attempts.resolution_code IS NULL
            )
            AND NOT EXISTS (
                SELECT 1
                FROM public.billing_payment_attempts AS attempts
                WHERE attempts.subscription_id = subscriptions.id
                    AND attempts.billing_scope_id =
                        subscriptions.billing_scope_id
                    AND attempts.subscriber_id = subscriptions.subscriber_id
                    AND attempts.plan_key = subscriptions.plan_key
                    AND attempts.attempt_kind = 'subscription_recovery'
                    AND attempts.billing_period_start_at =
                        subscriptions.next_renewal_at
                    AND attempts.subscription_expected_status = 'active'
                    AND attempts.submitted_at IS NOT NULL
                    AND attempts.review_required_at IS NOT NULL
                    AND attempts.status = 'failed'
                    AND attempts.resolution_code IS NULL
            )
    ) THEN
        RAISE EXCEPTION
            'billing_v1_to_v2_missing_past_due_failure_history';
    END IF;
END
$$;

WITH automatic_failure_history AS (
    SELECT
        subscriptions.id AS subscription_id,
        count(attempts.id) AS failure_count,
        max(attempts.resolved_at) AS latest_resolved_at
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
    GROUP BY subscriptions.id
)
UPDATE public.billing_subscriptions AS subscriptions
SET phase = 'recurring',
    recurring_period_kind = 'calendar_months',
    recurring_period_count = 1,
    trial_amount_cents = NULL,
    trial_period_kind = NULL,
    trial_period_count = NULL,
    dunning_retry_delays_seconds =
        ARRAY[86400, 86400, 86400, 86400]::bigint[],
    dunning_exhaustion = 'remain_past_due',
    past_due_access = 'suspend_immediately',
    next_payment_attempt_at = CASE subscriptions.status
        WHEN 'active' THEN subscriptions.next_renewal_at
        WHEN 'past_due' THEN CASE
            WHEN history.failure_count >= 5 THEN NULL
            WHEN history.failure_count = 0 THEN
                subscriptions.next_renewal_at
            ELSE greatest(
                subscriptions.next_renewal_at,
                history.latest_resolved_at + interval '24 hours'
            )
        END
        ELSE NULL
    END,
    unpaid_at = NULL
FROM automatic_failure_history AS history
WHERE history.subscription_id = subscriptions.id;

UPDATE public.billing_payment_attempts
SET subscription_initial_terms_version = 1,
    subscription_initial_start_kind = 'recurring_immediately',
    subscription_initial_trial_amount_cents = NULL,
    subscription_initial_trial_period_kind = NULL,
    subscription_initial_trial_period_count = NULL,
    subscription_initial_recurring_base_amount_cents = coalesce(
        subscription_initial_discount_base_amount_cents,
        amount_cents
    ),
    subscription_initial_recurring_period_kind = 'calendar_months',
    subscription_initial_recurring_period_count = 1,
    subscription_initial_dunning_retry_delays_seconds =
        ARRAY[86400, 86400, 86400, 86400]::bigint[],
    subscription_initial_dunning_exhaustion = 'remain_past_due',
    subscription_initial_past_due_access = 'suspend_immediately'
WHERE attempt_kind = 'subscription_initial';

ALTER TABLE public.billing_subscriptions
    ALTER COLUMN phase SET NOT NULL,
    ALTER COLUMN recurring_period_kind SET NOT NULL,
    ALTER COLUMN recurring_period_count SET NOT NULL,
    ALTER COLUMN dunning_retry_delays_seconds SET NOT NULL,
    ALTER COLUMN dunning_exhaustion SET NOT NULL,
    ALTER COLUMN past_due_access SET NOT NULL;

ALTER TABLE public.billing_subscriptions
    DROP CONSTRAINT billing_subscriptions_status_check,
    DROP CONSTRAINT billing_subscriptions_canceled_state_check;

ALTER TABLE public.billing_subscriptions
    ADD CONSTRAINT billing_subscriptions_status_check
        CHECK (status IN ('active', 'past_due', 'canceled', 'unpaid')),
    ADD CONSTRAINT billing_subscriptions_canceled_state_check
        CHECK (
            (status = 'canceled' AND canceled_at IS NOT NULL)
            OR (status <> 'canceled' AND canceled_at IS NULL)
        ),
    ADD CONSTRAINT billing_subscriptions_unpaid_state_check
        CHECK (
            (status = 'unpaid' AND unpaid_at IS NOT NULL)
            OR (status <> 'unpaid' AND unpaid_at IS NULL)
        ),
    ADD CONSTRAINT billing_subscriptions_terms_check
        CHECK (
            phase IN ('paid_trial', 'recurring')
            AND recurring_period_kind IN ('fixed_days', 'calendar_months')
            AND recurring_period_count BETWEEN 1 AND 65535
            AND dunning_exhaustion IN ('remain_past_due', 'mark_unpaid')
            AND past_due_access IN (
                'suspend_immediately',
                'continue_until_dunning_exhausted'
            )
            AND (
                (
                    trial_amount_cents IS NULL
                    AND trial_period_kind IS NULL
                    AND trial_period_count IS NULL
                )
                OR (
                    trial_amount_cents IS NOT NULL
                    AND trial_amount_cents > 0
                    AND trial_period_kind IS NOT NULL
                    AND trial_period_kind IN (
                        'fixed_days',
                        'calendar_months'
                    )
                    AND trial_period_count IS NOT NULL
                    AND trial_period_count BETWEEN 1 AND 65535
                )
            )
            AND (
                phase = 'recurring'
                OR (
                    phase = 'paid_trial'
                    AND trial_amount_cents IS NOT NULL
                )
            )
        ),
    ADD CONSTRAINT billing_subscriptions_dunning_schedule_check
        CHECK (
            CASE
                WHEN cardinality(dunning_retry_delays_seconds) = 0 THEN
                    dunning_retry_delays_seconds = ARRAY[]::bigint[]
                WHEN array_ndims(dunning_retry_delays_seconds) = 1
                    AND array_lower(dunning_retry_delays_seconds, 1) = 1
                THEN
                    cardinality(dunning_retry_delays_seconds) <= 16
                    AND array_position(
                        dunning_retry_delays_seconds,
                        NULL
                    ) IS NULL
                    AND 0 < ALL(dunning_retry_delays_seconds)
                    AND 4294967295 >= ALL(dunning_retry_delays_seconds)
                ELSE false
            END
        ),
    ADD CONSTRAINT billing_subscriptions_payment_schedule_check
        CHECK (
            (
                status = 'active'
                AND next_payment_attempt_at IS NOT NULL
                AND next_payment_attempt_at = next_renewal_at
            )
            OR (
                status = 'past_due'
                AND (
                    next_payment_attempt_at IS NULL
                    OR next_payment_attempt_at >= next_renewal_at
                )
            )
            OR (
                status IN ('canceled', 'unpaid')
                AND next_payment_attempt_at IS NULL
            )
        );

DROP INDEX public.billing_subscriptions_due_idx;

CREATE INDEX billing_subscriptions_due_idx
ON public.billing_subscriptions (next_payment_attempt_at, id)
INCLUDE (billing_scope_id, gateway_account_id, next_renewal_at)
WHERE status IN ('active', 'past_due')
    AND next_payment_attempt_at IS NOT NULL;

CREATE INDEX billing_payment_attempts_subscription_history_idx
ON public.billing_payment_attempts (
    billing_scope_id,
    subscriber_id,
    plan_key,
    created_at DESC,
    id DESC
)
WHERE attempt_kind <> 'host_charge';

ALTER TABLE public.billing_payment_attempts
    ADD CONSTRAINT billing_payment_attempts_initial_terms_check
        CHECK (
            (
                attempt_kind <> 'subscription_initial'
                AND subscription_initial_terms_version IS NULL
                AND subscription_initial_start_kind IS NULL
                AND subscription_initial_trial_amount_cents IS NULL
                AND subscription_initial_trial_period_kind IS NULL
                AND subscription_initial_trial_period_count IS NULL
                AND subscription_initial_recurring_base_amount_cents IS NULL
                AND subscription_initial_recurring_period_kind IS NULL
                AND subscription_initial_recurring_period_count IS NULL
                AND subscription_initial_dunning_retry_delays_seconds IS NULL
                AND subscription_initial_dunning_exhaustion IS NULL
                AND subscription_initial_past_due_access IS NULL
            )
            OR (
                attempt_kind = 'subscription_initial'
                AND subscription_initial_terms_version IS NOT NULL
                AND subscription_initial_terms_version IN (1, 2)
                AND subscription_initial_start_kind IS NOT NULL
                AND subscription_initial_start_kind IN (
                    'paid_trial',
                    'recurring_immediately'
                )
                AND subscription_initial_recurring_base_amount_cents IS NOT NULL
                AND subscription_initial_recurring_base_amount_cents > 0
                AND subscription_initial_recurring_period_kind IS NOT NULL
                AND subscription_initial_recurring_period_kind IN (
                    'fixed_days',
                    'calendar_months'
                )
                AND subscription_initial_recurring_period_count IS NOT NULL
                AND subscription_initial_recurring_period_count
                    BETWEEN 1 AND 65535
                AND subscription_initial_dunning_retry_delays_seconds
                    IS NOT NULL
                AND (
                    CASE
                        WHEN cardinality(
                            subscription_initial_dunning_retry_delays_seconds
                        ) = 0
                        THEN subscription_initial_dunning_retry_delays_seconds =
                            ARRAY[]::bigint[]
                        WHEN array_ndims(
                            subscription_initial_dunning_retry_delays_seconds
                        ) = 1
                        AND array_lower(
                            subscription_initial_dunning_retry_delays_seconds,
                            1
                        ) = 1
                        THEN cardinality(
                            subscription_initial_dunning_retry_delays_seconds
                        ) <= 16
                            AND array_position(
                                subscription_initial_dunning_retry_delays_seconds,
                                NULL
                            ) IS NULL
                            AND 0 < ALL(
                                subscription_initial_dunning_retry_delays_seconds
                            )
                            AND 4294967295 >= ALL(
                                subscription_initial_dunning_retry_delays_seconds
                            )
                        ELSE false
                    END
                )
                AND subscription_initial_dunning_exhaustion IS NOT NULL
                AND subscription_initial_dunning_exhaustion IN (
                    'remain_past_due',
                    'mark_unpaid'
                )
                AND subscription_initial_past_due_access IS NOT NULL
                AND subscription_initial_past_due_access IN (
                    'suspend_immediately',
                    'continue_until_dunning_exhausted'
                )
                AND (
                    subscription_initial_terms_version = 2
                    OR (
                        subscription_initial_terms_version = 1
                        AND subscription_initial_start_kind =
                            'recurring_immediately'
                        AND subscription_initial_recurring_period_kind =
                            'calendar_months'
                        AND subscription_initial_recurring_period_count = 1
                        AND subscription_initial_dunning_retry_delays_seconds =
                            ARRAY[86400, 86400, 86400, 86400]::bigint[]
                        AND subscription_initial_dunning_exhaustion =
                            'remain_past_due'
                        AND subscription_initial_past_due_access =
                            'suspend_immediately'
                    )
                )
                AND (
                    (
                        subscription_initial_start_kind =
                            'recurring_immediately'
                        AND subscription_initial_trial_amount_cents IS NULL
                        AND subscription_initial_trial_period_kind IS NULL
                        AND subscription_initial_trial_period_count IS NULL
                    )
                    OR (
                        subscription_initial_start_kind = 'paid_trial'
                        AND subscription_initial_trial_amount_cents IS NOT NULL
                        AND subscription_initial_trial_amount_cents > 0
                        AND subscription_initial_trial_period_kind IS NOT NULL
                        AND subscription_initial_trial_period_kind IN (
                            'fixed_days',
                            'calendar_months'
                        )
                        AND subscription_initial_trial_period_count IS NOT NULL
                        AND subscription_initial_trial_period_count
                            BETWEEN 1 AND 65535
                    )
                )
            )
        );

ALTER TABLE public.billing_subscription_discounts
    DROP CONSTRAINT billing_subscription_discounts_duration_periods_check;

ALTER TABLE public.billing_subscription_discounts
    ADD CONSTRAINT billing_subscription_discounts_duration_periods_check
        CHECK (
            (
                duration = 'indefinite'
                AND duration_months IS NULL
                AND periods_total IS NULL
                AND periods_applied BETWEEN 0 AND 1
                AND status = 'active'
                AND completed_at IS NULL
            )
            OR (
                duration = 'limited_months'
                AND duration_months IS NOT NULL
                AND duration_months BETWEEN 1 AND 36
                AND periods_total = duration_months
                AND (
                    (
                        periods_applied BETWEEN 0 AND periods_total - 1
                        AND status = 'active'
                        AND completed_at IS NULL
                    )
                    OR (
                        periods_applied = periods_total
                        AND status = 'completed'
                        AND completed_at IS NOT NULL
                    )
                )
            )
        );

CREATE OR REPLACE VIEW public.billing_current_subscriptions AS
SELECT
    id,
    billing_scope_id,
    gateway_account_id,
    subscriber_id,
    plan_key,
    status,
    payment_method_id,
    amount_cents,
    currency,
    current_period_start_at,
    current_period_end_at,
    next_renewal_at,
    initial_transaction_id,
    canceled_at,
    created_at,
    updated_at,
    CASE status
        WHEN 'active' THEN 0
        WHEN 'past_due' THEN 1
        ELSE 2
    END AS current_subscription_rank,
    phase,
    recurring_period_kind,
    recurring_period_count,
    trial_amount_cents,
    trial_period_kind,
    trial_period_count,
    dunning_retry_delays_seconds,
    dunning_exhaustion,
    past_due_access,
    next_payment_attempt_at,
    unpaid_at
FROM public.billing_subscriptions
WHERE status IN ('active', 'past_due')
    OR (
        status = 'canceled'
        AND current_period_end_at > now()
    );
