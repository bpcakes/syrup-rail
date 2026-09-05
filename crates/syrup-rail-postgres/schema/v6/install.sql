-- Syrup Rail canonical PostgreSQL schema, version 6.
--
-- Hosts materialize this file byte-for-byte in an immutable migration, then
-- add host identity, actor, credential, and target bindings separately.

CREATE FUNCTION public.billing_canonical_gateway_transaction_id(value text)
RETURNS text
LANGUAGE sql
IMMUTABLE
PARALLEL SAFE
STRICT
SET search_path = pg_catalog
AS $$
    SELECT NULLIF(btrim(
        value,
        U&'\0009\000A\000B\000C\000D\0020\0085\00A0\1680\2000\2001\2002\2003\2004\2005\2006\2007\2008\2009\200A\2028\2029\202F\205F\3000'
    ), '')
$$;

CREATE TABLE public.billing_gateway_provider_rate_limits (
    provider_key text PRIMARY KEY,
    rate_limited_until timestamptz NOT NULL DEFAULT '-infinity',
    CONSTRAINT billing_gateway_provider_rate_limits_key_check
        CHECK (provider_key ~ '^[a-z0-9][a-z0-9_-]{0,63}$')
);

CREATE TABLE public.billing_gateway_accounts (
    id uuid PRIMARY KEY,
    billing_scope_id uuid NOT NULL,
    provider_key text NOT NULL,
    gateway_configuration_id uuid NOT NULL,
    mutation_rate_limited_until timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT billing_gateway_accounts_scope_key UNIQUE (billing_scope_id),
    CONSTRAINT billing_gateway_accounts_configuration_key
        UNIQUE (gateway_configuration_id),
    CONSTRAINT billing_gateway_accounts_id_scope_key
        UNIQUE (id, billing_scope_id),
    CONSTRAINT billing_gateway_accounts_id_scope_provider_key
        UNIQUE (id, billing_scope_id, provider_key),
    CONSTRAINT billing_gateway_accounts_provider_fk
        FOREIGN KEY (provider_key)
        REFERENCES public.billing_gateway_provider_rate_limits(provider_key)
        ON DELETE RESTRICT,
    CONSTRAINT billing_gateway_accounts_timestamp_order_check
        CHECK (updated_at >= created_at)
);

CREATE TABLE public.billing_payment_methods (
    id uuid PRIMARY KEY,
    billing_scope_id uuid NOT NULL,
    subscriber_id uuid NOT NULL,
    gateway_account_id uuid NOT NULL,
    gateway_payment_method_reference text NOT NULL,
    status text NOT NULL,
    payment_type text,
    card_brand text,
    card_last4 text,
    card_exp_month smallint,
    card_exp_year smallint,
    billing_name text,
    billing_email text,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT billing_payment_methods_id_owner_account_key
        UNIQUE (id, billing_scope_id, subscriber_id, gateway_account_id),
    CONSTRAINT billing_payment_methods_id_owner_key
        UNIQUE (id, billing_scope_id, subscriber_id),
    CONSTRAINT billing_payment_methods_status_check
        CHECK (status IN ('active', 'disabled')),
    CONSTRAINT billing_payment_methods_reference_check
        CHECK (length(btrim(gateway_payment_method_reference)) > 0),
    CONSTRAINT billing_payment_methods_card_last4_check
        CHECK (card_last4 IS NULL OR card_last4 ~ '^[0-9]{4}$'),
    CONSTRAINT billing_payment_methods_gateway_text_check
        CHECK (
            (payment_type IS NULL OR octet_length(payment_type) <= 512)
            AND (card_brand IS NULL OR octet_length(card_brand) <= 512)
        ),
    CONSTRAINT billing_payment_methods_card_exp_month_check
        CHECK (card_exp_month IS NULL OR card_exp_month BETWEEN 1 AND 12),
    CONSTRAINT billing_payment_methods_card_exp_year_check
        CHECK (card_exp_year IS NULL OR card_exp_year >= 2000),
    CONSTRAINT billing_payment_methods_timestamp_order_check
        CHECK (updated_at >= created_at),
    CONSTRAINT billing_payment_methods_account_scope_fk
        FOREIGN KEY (gateway_account_id, billing_scope_id)
        REFERENCES public.billing_gateway_accounts(id, billing_scope_id)
        ON DELETE RESTRICT
);

CREATE UNIQUE INDEX billing_payment_methods_owner_reference_idx
ON public.billing_payment_methods (
    gateway_account_id,
    subscriber_id,
    gateway_payment_method_reference
);

CREATE INDEX billing_payment_methods_scope_subscriber_idx
ON public.billing_payment_methods (billing_scope_id, subscriber_id, updated_at DESC);

CREATE TABLE public.billing_subscriptions (
    id uuid PRIMARY KEY,
    billing_scope_id uuid NOT NULL,
    subscriber_id uuid NOT NULL,
    plan_key text NOT NULL,
    status text NOT NULL,
    gateway_account_id uuid NOT NULL,
    payment_method_id uuid NOT NULL,
    amount_cents integer NOT NULL,
    currency text NOT NULL DEFAULT 'USD',
    current_period_start_at timestamptz NOT NULL,
    current_period_end_at timestamptz NOT NULL,
    next_renewal_at timestamptz NOT NULL,
    initial_transaction_id text NOT NULL,
    canceled_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    phase text NOT NULL,
    recurring_period_kind text NOT NULL,
    recurring_period_count integer NOT NULL,
    trial_amount_cents integer,
    trial_period_kind text,
    trial_period_count integer,
    dunning_retry_delays_seconds bigint[] NOT NULL,
    dunning_exhaustion text NOT NULL,
    past_due_access text NOT NULL,
    next_payment_attempt_at timestamptz,
    unpaid_at timestamptz,
    required_gateway_account_mode text NOT NULL,
    CONSTRAINT billing_subscriptions_id_owner_account_plan_key
        UNIQUE (
            id,
            billing_scope_id,
            subscriber_id,
            gateway_account_id,
            plan_key
        ),
    CONSTRAINT billing_subscriptions_id_owner_plan_key
        UNIQUE (id, billing_scope_id, subscriber_id, plan_key),
    CONSTRAINT billing_subscriptions_plan_key_check
        CHECK (plan_key ~ '^[a-z0-9][a-z0-9_-]{0,63}$'),
    CONSTRAINT billing_subscriptions_status_check
        CHECK (status IN ('active', 'past_due', 'canceled', 'unpaid')),
    CONSTRAINT billing_subscriptions_amount_check
        CHECK (amount_cents > 0),
    CONSTRAINT billing_subscriptions_currency_check
        CHECK (currency = upper(currency) AND length(currency) = 3),
    CONSTRAINT billing_subscriptions_required_gateway_account_mode_check
        CHECK (required_gateway_account_mode IN ('live', 'test')),
    CONSTRAINT billing_subscriptions_period_check
        CHECK (
            current_period_end_at > current_period_start_at
            AND next_renewal_at = current_period_end_at
        ),
    CONSTRAINT billing_subscriptions_canceled_state_check
        CHECK (
            (status = 'canceled' AND canceled_at IS NOT NULL)
            OR (status <> 'canceled' AND canceled_at IS NULL)
        ),
    CONSTRAINT billing_subscriptions_unpaid_state_check
        CHECK (
            (status = 'unpaid' AND unpaid_at IS NOT NULL)
            OR (status <> 'unpaid' AND unpaid_at IS NULL)
        ),
    CONSTRAINT billing_subscriptions_terms_check
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
    CONSTRAINT billing_subscriptions_dunning_schedule_check
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
    CONSTRAINT billing_subscriptions_payment_schedule_check
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
        ),
    CONSTRAINT billing_subscriptions_initial_transaction_check
        CHECK (
            public.billing_canonical_gateway_transaction_id(
                initial_transaction_id
            ) IS NOT NULL
            AND initial_transaction_id =
                public.billing_canonical_gateway_transaction_id(
                    initial_transaction_id
                )
        ),
    CONSTRAINT billing_subscriptions_timestamp_order_check
        CHECK (updated_at >= created_at),
    CONSTRAINT billing_subscriptions_account_scope_fk
        FOREIGN KEY (gateway_account_id, billing_scope_id)
        REFERENCES public.billing_gateway_accounts(id, billing_scope_id)
        ON DELETE RESTRICT,
    CONSTRAINT billing_subscriptions_payment_method_owner_fk
        FOREIGN KEY (
            payment_method_id,
            billing_scope_id,
            subscriber_id,
            gateway_account_id
        )
        REFERENCES public.billing_payment_methods(
            id,
            billing_scope_id,
            subscriber_id,
            gateway_account_id
        )
        ON DELETE RESTRICT
);

CREATE UNIQUE INDEX billing_subscriptions_current_owner_plan_idx
ON public.billing_subscriptions (billing_scope_id, subscriber_id, plan_key)
WHERE status IN ('active', 'past_due');

CREATE UNIQUE INDEX billing_subscriptions_initial_transaction_idx
ON public.billing_subscriptions (gateway_account_id, initial_transaction_id);

CREATE INDEX billing_subscriptions_due_idx
ON public.billing_subscriptions (next_payment_attempt_at, id)
INCLUDE (
    billing_scope_id,
    gateway_account_id,
    next_renewal_at,
    required_gateway_account_mode
)
WHERE status IN ('active', 'past_due')
    AND next_payment_attempt_at IS NOT NULL;

CREATE INDEX billing_subscriptions_due_mode_idx
ON public.billing_subscriptions (
    required_gateway_account_mode,
    next_payment_attempt_at,
    id
)
INCLUDE (billing_scope_id, gateway_account_id, next_renewal_at)
WHERE status IN ('active', 'past_due')
    AND next_payment_attempt_at IS NOT NULL;

CREATE TABLE public.billing_payment_attempts (
    id uuid PRIMARY KEY,
    billing_scope_id uuid NOT NULL,
    subscriber_id uuid NOT NULL,
    plan_key text,
    host_charge_target_id uuid,
    subscription_id uuid,
    payment_method_id uuid,
    attempt_kind text NOT NULL,
    status text NOT NULL,
    idempotency_key text NOT NULL,
    request_fingerprint text NOT NULL,
    amount_cents integer NOT NULL,
    currency text NOT NULL DEFAULT 'USD',
    billing_period_start_at timestamptz,
    billing_period_end_at timestamptz,
    gateway_account_id uuid NOT NULL,
    gateway_configuration_id uuid NOT NULL,
    gateway_order_id text NOT NULL,
    gateway_transaction_id text,
    gateway_payment_method_reference text,
    gateway_response text,
    gateway_response_code text,
    gateway_response_text text,
    gateway_condition text,
    payment_type text,
    card_brand text,
    card_last4 text,
    card_exp_month smallint,
    card_exp_year smallint,
    submitted_at timestamptz,
    resolved_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    gateway_lifecycle_status text NOT NULL DEFAULT 'unknown',
    gateway_lifecycle_action text,
    gateway_lifecycle_at timestamptz,
    gateway_lifecycle_reconciled_at timestamptz,
    refunded_amount_cents integer NOT NULL DEFAULT 0,
    billing_first_name text,
    billing_email text,
    resolution_code text,
    review_required_at timestamptz,
    payment_method_update_expected_payment_method_id uuid,
    payment_method_update_expected_initial_transaction_id text,
    subscription_expected_payment_method_id uuid,
    subscription_expected_initial_transaction_id text,
    subscription_expected_status text,
    subscription_initial_discount_claim_id uuid,
    subscription_initial_discount_code_id uuid,
    subscription_initial_discount_code_snapshot text,
    subscription_initial_discount_label_snapshot text,
    subscription_initial_discount_kind text,
    subscription_initial_discount_amount_off_cents integer,
    subscription_initial_discount_percent_off_bps integer,
    subscription_initial_discount_currency text,
    subscription_initial_discount_duration text,
    subscription_initial_discount_duration_months integer,
    subscription_initial_discount_base_amount_cents integer,
    subscription_initial_discount_discounted_amount_cents integer,
    subscription_initial_terms_version smallint,
    subscription_initial_start_kind text,
    subscription_initial_trial_amount_cents integer,
    subscription_initial_trial_period_kind text,
    subscription_initial_trial_period_count integer,
    subscription_initial_recurring_base_amount_cents integer,
    subscription_initial_recurring_period_kind text,
    subscription_initial_recurring_period_count integer,
    subscription_initial_dunning_retry_delays_seconds bigint[],
    subscription_initial_dunning_exhaustion text,
    subscription_initial_past_due_access text,
    billing_last_name text,
    required_gateway_account_mode text NOT NULL,
    CONSTRAINT billing_payment_attempts_id_scope_account_key
        UNIQUE (id, billing_scope_id, gateway_account_id),
    CONSTRAINT billing_payment_attempts_id_owner_plan_key
        UNIQUE (id, billing_scope_id, subscriber_id, plan_key),
    CONSTRAINT billing_payment_attempts_kind_check
        CHECK (
            attempt_kind IN (
                'host_charge',
                'subscription_initial',
                'subscription_renewal',
                'subscription_recovery',
                'subscription_payment_method_update'
            )
        ),
    CONSTRAINT billing_payment_attempts_status_check
        CHECK (
            status IN (
                'pending',
                'approved',
                'declined',
                'unknown',
                'review_required',
                'failed'
            )
        ),
    CONSTRAINT billing_payment_attempts_plan_target_shape_check
        CHECK (
            (
                attempt_kind = 'host_charge'
                AND plan_key IS NULL
                AND host_charge_target_id IS NOT NULL
            )
            OR (
                attempt_kind <> 'host_charge'
                AND plan_key IS NOT NULL
                AND plan_key ~ '^[a-z0-9][a-z0-9_-]{0,63}$'
                AND host_charge_target_id IS NULL
            )
        ),
    CONSTRAINT billing_payment_attempts_amount_check
        CHECK (
            (
                attempt_kind = 'subscription_payment_method_update'
                AND amount_cents = 0
            )
            OR (
                attempt_kind <> 'subscription_payment_method_update'
                AND amount_cents > 0
            )
        ),
    CONSTRAINT billing_payment_attempts_currency_check
        CHECK (currency = upper(currency) AND length(currency) = 3),
    CONSTRAINT billing_payment_attempts_idempotency_key_check
        CHECK (length(btrim(idempotency_key)) > 0),
    CONSTRAINT billing_payment_attempts_request_fingerprint_check
        CHECK (length(btrim(request_fingerprint)) > 0),
    CONSTRAINT billing_payment_attempts_gateway_order_check
        CHECK (length(btrim(gateway_order_id)) > 0),
    CONSTRAINT billing_payment_attempts_required_gateway_account_mode_check
        CHECK (required_gateway_account_mode IN ('live', 'test')),
    CONSTRAINT billing_payment_attempts_gateway_transaction_check
        CHECK (
            NOT gateway_transaction_id IS DISTINCT FROM
                public.billing_canonical_gateway_transaction_id(
                    gateway_transaction_id
                )
        ),
    CONSTRAINT billing_payment_attempts_gateway_text_check
        CHECK (
            (gateway_response IS NULL OR octet_length(gateway_response) <= 512)
            AND (
                gateway_response_code IS NULL
                OR octet_length(gateway_response_code) <= 512
            )
            AND (
                gateway_response_text IS NULL
                OR octet_length(gateway_response_text) <= 512
            )
            AND (
                gateway_condition IS NULL
                OR octet_length(gateway_condition) <= 512
            )
            AND (payment_type IS NULL OR octet_length(payment_type) <= 512)
            AND (card_brand IS NULL OR octet_length(card_brand) <= 512)
            AND (
                gateway_lifecycle_action IS NULL
                OR octet_length(gateway_lifecycle_action) <= 512
            )
        ),
    CONSTRAINT billing_payment_attempts_card_last4_check
        CHECK (card_last4 IS NULL OR card_last4 ~ '^[0-9]{4}$'),
    CONSTRAINT billing_payment_attempts_card_exp_month_check
        CHECK (card_exp_month IS NULL OR card_exp_month BETWEEN 1 AND 12),
    CONSTRAINT billing_payment_attempts_card_exp_year_check
        CHECK (card_exp_year IS NULL OR card_exp_year >= 2000),
    CONSTRAINT billing_payment_attempts_period_check
        CHECK (
            (
                billing_period_start_at IS NULL
                AND billing_period_end_at IS NULL
            )
            OR (
                billing_period_start_at IS NOT NULL
                AND billing_period_end_at IS NOT NULL
                AND billing_period_end_at > billing_period_start_at
            )
        ),
    CONSTRAINT billing_payment_attempts_relationship_shape_check
        CHECK (
            (
                attempt_kind = 'host_charge'
                AND subscription_id IS NULL
                AND payment_method_id IS NULL
                AND billing_period_start_at IS NULL
                AND billing_period_end_at IS NULL
            )
            OR (
                attempt_kind = 'subscription_initial'
                AND billing_period_start_at IS NULL
                AND billing_period_end_at IS NULL
                AND (
                    (
                        status IN (
                            'approved',
                            'declined',
                            'failed',
                            'review_required'
                        )
                        AND payment_method_id IS NOT NULL
                    )
                    OR (
                        status <> 'approved'
                        AND subscription_id IS NULL
                        AND payment_method_id IS NULL
                    )
                )
            )
            OR (
                attempt_kind IN (
                    'subscription_renewal',
                    'subscription_recovery'
                )
                AND subscription_id IS NOT NULL
                AND payment_method_id IS NOT NULL
                AND billing_period_start_at IS NOT NULL
                AND billing_period_end_at IS NOT NULL
            )
            OR (
                attempt_kind = 'subscription_payment_method_update'
                AND subscription_id IS NOT NULL
                AND payment_method_id IS NOT NULL
                AND billing_period_start_at IS NULL
                AND billing_period_end_at IS NULL
            )
        ),
    CONSTRAINT billing_payment_attempts_method_update_snapshot_check
        CHECK (
            (
                attempt_kind = 'subscription_payment_method_update'
                AND payment_method_update_expected_payment_method_id IS NOT NULL
                AND payment_method_update_expected_initial_transaction_id
                    IS NOT NULL
                AND length(btrim(
                    payment_method_update_expected_initial_transaction_id
                )) > 0
            )
            OR (
                attempt_kind <> 'subscription_payment_method_update'
                AND payment_method_update_expected_payment_method_id IS NULL
                AND payment_method_update_expected_initial_transaction_id
                    IS NULL
            )
        ),
    CONSTRAINT billing_payment_attempts_method_update_transaction_check
        CHECK (
            NOT payment_method_update_expected_initial_transaction_id
                IS DISTINCT FROM
                public.billing_canonical_gateway_transaction_id(
                    payment_method_update_expected_initial_transaction_id
                )
        ),
    CONSTRAINT billing_payment_attempts_subscription_snapshot_check
        CHECK (
            (
                attempt_kind IN (
                    'subscription_renewal',
                    'subscription_recovery'
                )
                AND subscription_expected_payment_method_id IS NOT NULL
                AND subscription_expected_initial_transaction_id IS NOT NULL
                AND length(btrim(
                    subscription_expected_initial_transaction_id
                )) > 0
                AND subscription_expected_status IS NOT NULL
                AND subscription_expected_status IN ('active', 'past_due')
            )
            OR (
                attempt_kind NOT IN (
                    'subscription_renewal',
                    'subscription_recovery'
                )
                AND subscription_expected_payment_method_id IS NULL
                AND subscription_expected_initial_transaction_id IS NULL
                AND subscription_expected_status IS NULL
            )
        ),
    CONSTRAINT billing_payment_attempts_subscription_transaction_check
        CHECK (
            NOT subscription_expected_initial_transaction_id IS DISTINCT FROM
                public.billing_canonical_gateway_transaction_id(
                    subscription_expected_initial_transaction_id
                )
        ),
    CONSTRAINT billing_payment_attempts_resolution_code_check
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
        ),
    CONSTRAINT billing_payment_attempts_resolved_state_check
        CHECK (
            (
                status IN ('approved', 'declined', 'failed')
                AND resolved_at IS NOT NULL
            )
            OR status IN ('pending', 'unknown', 'review_required')
        ),
    CONSTRAINT billing_payment_attempts_review_required_at_check
        CHECK (status <> 'review_required' OR review_required_at IS NOT NULL),
    CONSTRAINT billing_payment_attempts_lifecycle_status_check
        CHECK (
            gateway_lifecycle_status IN (
                'unknown',
                'pending_settlement',
                'settled',
                'voided',
                'refunded',
                'chargeback'
            )
        ),
    CONSTRAINT billing_payment_attempts_lifecycle_amount_check
        CHECK (
            (
                gateway_lifecycle_status IN (
                    'unknown',
                    'pending_settlement',
                    'voided'
                )
                AND refunded_amount_cents = 0
            )
            OR (
                gateway_lifecycle_status = 'settled'
                AND refunded_amount_cents >= 0
                AND refunded_amount_cents < amount_cents
            )
            OR (
                gateway_lifecycle_status = 'refunded'
                AND amount_cents > 0
                AND refunded_amount_cents = amount_cents
            )
            OR (
                gateway_lifecycle_status = 'chargeback'
                AND refunded_amount_cents >= 0
                AND refunded_amount_cents <= amount_cents
            )
        ),
    CONSTRAINT billing_payment_attempts_initial_discount_snapshot_check
        CHECK (
            (
                subscription_initial_discount_claim_id IS NULL
                AND subscription_initial_discount_code_id IS NULL
                AND subscription_initial_discount_code_snapshot IS NULL
                AND subscription_initial_discount_label_snapshot IS NULL
                AND subscription_initial_discount_kind IS NULL
                AND subscription_initial_discount_amount_off_cents IS NULL
                AND subscription_initial_discount_percent_off_bps IS NULL
                AND subscription_initial_discount_currency IS NULL
                AND subscription_initial_discount_duration IS NULL
                AND subscription_initial_discount_duration_months IS NULL
                AND subscription_initial_discount_base_amount_cents IS NULL
                AND subscription_initial_discount_discounted_amount_cents
                    IS NULL
            )
            OR (
                attempt_kind = 'subscription_initial'
                AND subscription_initial_discount_claim_id IS NOT NULL
                AND subscription_initial_discount_code_id IS NOT NULL
                AND subscription_initial_discount_code_snapshot IS NOT NULL
                AND length(btrim(
                    subscription_initial_discount_code_snapshot
                )) > 0
                AND subscription_initial_discount_kind IS NOT NULL
                AND (
                    (
                        subscription_initial_discount_kind = 'amount_off'
                        AND subscription_initial_discount_amount_off_cents
                            IS NOT NULL
                        AND subscription_initial_discount_amount_off_cents > 0
                        AND subscription_initial_discount_percent_off_bps
                            IS NULL
                    )
                    OR (
                        subscription_initial_discount_kind = 'percent_off'
                        AND subscription_initial_discount_amount_off_cents
                            IS NULL
                        AND subscription_initial_discount_percent_off_bps
                            IS NOT NULL
                        AND subscription_initial_discount_percent_off_bps
                            BETWEEN 1 AND 9999
                    )
                )
                AND subscription_initial_discount_currency IS NOT NULL
                AND subscription_initial_discount_currency =
                    upper(subscription_initial_discount_currency)
                AND length(subscription_initial_discount_currency) = 3
                AND subscription_initial_discount_duration IS NOT NULL
                AND (
                    (
                        subscription_initial_discount_duration = 'indefinite'
                        AND subscription_initial_discount_duration_months
                            IS NULL
                    )
                    OR (
                        subscription_initial_discount_duration =
                            'limited_months'
                        AND subscription_initial_discount_duration_months
                            IS NOT NULL
                        AND subscription_initial_discount_duration_months
                            BETWEEN 1 AND 36
                    )
                )
                AND subscription_initial_discount_base_amount_cents IS NOT NULL
                AND subscription_initial_discount_base_amount_cents > 0
                AND subscription_initial_discount_discounted_amount_cents
                    IS NOT NULL
                AND subscription_initial_discount_discounted_amount_cents > 0
                AND subscription_initial_discount_discounted_amount_cents <=
                    subscription_initial_discount_base_amount_cents
            )
        ),
    CONSTRAINT billing_payment_attempts_initial_terms_check
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
        ),
    CONSTRAINT billing_payment_attempts_timestamp_order_check
        CHECK (updated_at >= created_at),
    CONSTRAINT billing_payment_attempts_account_scope_fk
        FOREIGN KEY (gateway_account_id, billing_scope_id)
        REFERENCES public.billing_gateway_accounts(id, billing_scope_id)
        ON DELETE RESTRICT,
    CONSTRAINT billing_payment_attempts_payment_method_owner_fk
        FOREIGN KEY (
            payment_method_id,
            billing_scope_id,
            subscriber_id,
            gateway_account_id
        )
        REFERENCES public.billing_payment_methods(
            id,
            billing_scope_id,
            subscriber_id,
            gateway_account_id
        )
        ON DELETE RESTRICT,
    CONSTRAINT billing_payment_attempts_subscription_owner_fk
        FOREIGN KEY (
            subscription_id,
            billing_scope_id,
            subscriber_id,
            gateway_account_id,
            plan_key
        )
        REFERENCES public.billing_subscriptions(
            id,
            billing_scope_id,
            subscriber_id,
            gateway_account_id,
            plan_key
        )
        ON DELETE SET NULL (subscription_id)
);

CREATE UNIQUE INDEX billing_payment_attempts_owner_idempotency_idx
ON public.billing_payment_attempts (
    billing_scope_id,
    subscriber_id,
    idempotency_key
);

CREATE UNIQUE INDEX billing_payment_attempts_gateway_order_idx
ON public.billing_payment_attempts (gateway_account_id, gateway_order_id);

CREATE UNIQUE INDEX billing_payment_attempts_gateway_transaction_idx
ON public.billing_payment_attempts (
    gateway_account_id,
    gateway_transaction_id
)
WHERE public.billing_canonical_gateway_transaction_id(
    gateway_transaction_id
) IS NOT NULL;

CREATE UNIQUE INDEX billing_payment_attempts_initial_inflight_idx
ON public.billing_payment_attempts (
    billing_scope_id,
    subscriber_id,
    plan_key
)
WHERE attempt_kind = 'subscription_initial'
    AND (
        status IN ('pending', 'unknown')
        OR (
            status = 'review_required'
            AND resolution_code IS DISTINCT FROM
                'subscription_initial_current_subscription_conflict'
        )
    );

CREATE UNIQUE INDEX billing_payment_attempts_host_charge_once_idx
ON public.billing_payment_attempts (
    billing_scope_id,
    subscriber_id,
    host_charge_target_id
)
WHERE attempt_kind = 'host_charge'
    AND status IN ('pending', 'unknown', 'review_required', 'approved');

CREATE UNIQUE INDEX billing_payment_attempts_subscription_billing_inflight_idx
ON public.billing_payment_attempts (subscription_id)
WHERE attempt_kind IN ('subscription_renewal', 'subscription_recovery')
    AND status IN ('pending', 'unknown', 'review_required');

CREATE UNIQUE INDEX billing_payment_attempts_subscription_period_once_idx
ON public.billing_payment_attempts (subscription_id, billing_period_start_at)
WHERE attempt_kind IN ('subscription_renewal', 'subscription_recovery')
    AND status IN ('pending', 'unknown', 'review_required', 'approved');

CREATE UNIQUE INDEX billing_payment_attempts_method_update_inflight_idx
ON public.billing_payment_attempts (subscription_id)
WHERE attempt_kind = 'subscription_payment_method_update'
    AND status IN ('pending', 'unknown', 'review_required');

CREATE INDEX billing_payment_attempts_subscription_period_idx
ON public.billing_payment_attempts (subscription_id, billing_period_start_at)
WHERE subscription_id IS NOT NULL;

CREATE INDEX billing_payment_attempts_subscription_history_idx
ON public.billing_payment_attempts (
    billing_scope_id,
    subscriber_id,
    plan_key,
    created_at DESC,
    id DESC
)
WHERE attempt_kind <> 'host_charge';

CREATE INDEX billing_payment_attempts_payment_method_idx
ON public.billing_payment_attempts (payment_method_id)
WHERE payment_method_id IS NOT NULL;

CREATE INDEX billing_payment_attempts_unknown_idx
ON public.billing_payment_attempts (gateway_account_id, created_at, id)
WHERE status = 'unknown';

CREATE INDEX billing_payment_attempts_lifecycle_reconcile_idx
ON public.billing_payment_attempts (
    gateway_account_id,
    gateway_lifecycle_reconciled_at,
    resolved_at,
    id
)
WHERE status = 'approved'
    AND public.billing_canonical_gateway_transaction_id(
        gateway_transaction_id
    ) IS NOT NULL;

CREATE INDEX billing_payment_attempts_review_idx
ON public.billing_payment_attempts (review_required_at, id)
WHERE status = 'review_required';

CREATE INDEX billing_payment_attempts_initial_discount_claim_idx
ON public.billing_payment_attempts (subscription_initial_discount_claim_id)
WHERE subscription_initial_discount_claim_id IS NOT NULL;

CREATE FUNCTION public.billing_set_attempt_review_required_at()
RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, public
AS $$
BEGIN
    IF NEW.status = 'review_required' AND NEW.review_required_at IS NULL THEN
        NEW.review_required_at := clock_timestamp();
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER billing_payment_attempt_review_required_at
BEFORE INSERT OR UPDATE OF status
ON public.billing_payment_attempts
FOR EACH ROW
EXECUTE FUNCTION public.billing_set_attempt_review_required_at();

CREATE TABLE public.billing_processor_charges (
    id uuid PRIMARY KEY DEFAULT uuidv7(),
    attempt_id uuid NOT NULL,
    billing_scope_id uuid NOT NULL,
    gateway_account_id uuid NOT NULL,
    gateway_order_id text NOT NULL,
    gateway_transaction_id text,
    gateway_payment_method_reference text,
    gateway_response text,
    gateway_response_code text,
    gateway_response_text text,
    gateway_condition text,
    payment_type text,
    card_brand text,
    card_last4 text,
    card_exp_month smallint,
    card_exp_year smallint,
    charge_role text NOT NULL DEFAULT 'additional',
    progression_state text NOT NULL DEFAULT 'pending',
    state_code text,
    observed_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    reconciliation_required_at timestamptz,
    external_reversal_required_at timestamptz,
    applied_at timestamptz,
    externally_reversed_at timestamptz,
    attempt_kind text NOT NULL,
    plan_key text,
    host_charge_target_id uuid,
    amount_cents integer NOT NULL,
    currency text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT billing_processor_charges_id_attempt_key
        UNIQUE (id, attempt_id),
    CONSTRAINT billing_processor_charges_gateway_order_check
        CHECK (length(btrim(gateway_order_id)) > 0),
    CONSTRAINT billing_processor_charges_gateway_transaction_check
        CHECK (
            NOT gateway_transaction_id IS DISTINCT FROM
                public.billing_canonical_gateway_transaction_id(
                    gateway_transaction_id
                )
        ),
    CONSTRAINT billing_processor_charges_gateway_text_check
        CHECK (
            (gateway_response IS NULL OR octet_length(gateway_response) <= 512)
            AND (
                gateway_response_code IS NULL
                OR octet_length(gateway_response_code) <= 512
            )
            AND (
                gateway_response_text IS NULL
                OR octet_length(gateway_response_text) <= 512
            )
            AND (
                gateway_condition IS NULL
                OR octet_length(gateway_condition) <= 512
            )
            AND (payment_type IS NULL OR octet_length(payment_type) <= 512)
            AND (card_brand IS NULL OR octet_length(card_brand) <= 512)
        ),
    CONSTRAINT billing_processor_charges_kind_check
        CHECK (
            attempt_kind IN (
                'host_charge',
                'subscription_initial',
                'subscription_renewal',
                'subscription_recovery',
                'subscription_payment_method_update'
            )
        ),
    CONSTRAINT billing_processor_charges_dimension_shape_check
        CHECK (
            (
                attempt_kind = 'host_charge'
                AND plan_key IS NULL
                AND host_charge_target_id IS NOT NULL
                AND amount_cents > 0
            )
            OR (
                attempt_kind = 'subscription_payment_method_update'
                AND plan_key IS NOT NULL
                AND plan_key ~ '^[a-z0-9][a-z0-9_-]{0,63}$'
                AND host_charge_target_id IS NULL
                AND amount_cents = 0
            )
            OR (
                attempt_kind IN (
                    'subscription_initial',
                    'subscription_renewal',
                    'subscription_recovery'
                )
                AND plan_key IS NOT NULL
                AND plan_key ~ '^[a-z0-9][a-z0-9_-]{0,63}$'
                AND host_charge_target_id IS NULL
                AND amount_cents > 0
            )
        ),
    CONSTRAINT billing_processor_charges_currency_check
        CHECK (currency = upper(currency) AND length(currency) = 3),
    CONSTRAINT billing_processor_charges_role_check
        CHECK (charge_role IN ('primary', 'additional')),
    CONSTRAINT billing_processor_charges_progression_check
        CHECK (
            progression_state IN (
                'pending',
                'reconciliation_required',
                'external_reversal_required',
                'applied',
                'externally_reversed'
            )
        ),
    CONSTRAINT billing_processor_charges_state_code_check
        CHECK (state_code IS NULL OR length(btrim(state_code)) > 0),
    CONSTRAINT billing_processor_charges_state_timestamps_check
        CHECK (
            (
                progression_state = 'pending'
                AND reconciliation_required_at IS NULL
                AND external_reversal_required_at IS NULL
                AND applied_at IS NULL
                AND externally_reversed_at IS NULL
            )
            OR (
                progression_state = 'reconciliation_required'
                AND reconciliation_required_at IS NOT NULL
                AND external_reversal_required_at IS NULL
                AND applied_at IS NULL
                AND externally_reversed_at IS NULL
            )
            OR (
                progression_state = 'external_reversal_required'
                AND external_reversal_required_at IS NOT NULL
                AND applied_at IS NULL
                AND externally_reversed_at IS NULL
            )
            OR (
                progression_state = 'applied'
                AND applied_at IS NOT NULL
                AND externally_reversed_at IS NULL
            )
            OR (
                progression_state = 'externally_reversed'
                AND externally_reversed_at IS NOT NULL
            )
        ),
    CONSTRAINT billing_processor_charges_transaction_identity_check
        CHECK (
            (
                public.billing_canonical_gateway_transaction_id(
                    gateway_transaction_id
                ) IS NULL
                AND progression_state IN (
                    'pending',
                    'reconciliation_required'
                )
            )
            OR public.billing_canonical_gateway_transaction_id(
                gateway_transaction_id
            ) IS NOT NULL
        ),
    CONSTRAINT billing_processor_charges_reversal_eligibility_check
        CHECK (
            progression_state NOT IN (
                'external_reversal_required',
                'externally_reversed'
            )
            OR (
                public.billing_canonical_gateway_transaction_id(
                    gateway_transaction_id
                ) IS NOT NULL
                AND amount_cents > 0
                AND attempt_kind IN (
                    'host_charge',
                    'subscription_initial',
                    'subscription_renewal',
                    'subscription_recovery'
                )
            )
        ),
    CONSTRAINT billing_processor_charges_card_last4_check
        CHECK (card_last4 IS NULL OR card_last4 ~ '^[0-9]{4}$'),
    CONSTRAINT billing_processor_charges_card_exp_month_check
        CHECK (card_exp_month IS NULL OR card_exp_month BETWEEN 1 AND 12),
    CONSTRAINT billing_processor_charges_card_exp_year_check
        CHECK (card_exp_year IS NULL OR card_exp_year >= 2000),
    CONSTRAINT billing_processor_charges_timestamp_order_check
        CHECK (updated_at >= created_at),
    CONSTRAINT billing_processor_charges_account_scope_fk
        FOREIGN KEY (gateway_account_id, billing_scope_id)
        REFERENCES public.billing_gateway_accounts(id, billing_scope_id)
        ON DELETE RESTRICT,
    CONSTRAINT billing_processor_charges_attempt_scope_account_fk
        FOREIGN KEY (attempt_id, billing_scope_id, gateway_account_id)
        REFERENCES public.billing_payment_attempts(
            id,
            billing_scope_id,
            gateway_account_id
        )
        ON DELETE RESTRICT
);

CREATE UNIQUE INDEX billing_processor_charges_attempt_transaction_idx
ON public.billing_processor_charges (attempt_id, gateway_transaction_id)
WHERE public.billing_canonical_gateway_transaction_id(
    gateway_transaction_id
) IS NOT NULL;

CREATE UNIQUE INDEX billing_processor_charges_attempt_transactionless_idx
ON public.billing_processor_charges (attempt_id)
WHERE public.billing_canonical_gateway_transaction_id(
    gateway_transaction_id
) IS NULL;

CREATE UNIQUE INDEX billing_processor_charges_gateway_transaction_idx
ON public.billing_processor_charges (
    gateway_account_id,
    gateway_transaction_id
)
WHERE public.billing_canonical_gateway_transaction_id(
    gateway_transaction_id
) IS NOT NULL;

CREATE UNIQUE INDEX billing_processor_charges_primary_attempt_idx
ON public.billing_processor_charges (attempt_id)
WHERE charge_role = 'primary';

CREATE INDEX billing_processor_charges_account_created_idx
ON public.billing_processor_charges (gateway_account_id, created_at, id);

CREATE INDEX billing_processor_charges_account_order_idx
ON public.billing_processor_charges (gateway_account_id, gateway_order_id);

CREATE INDEX billing_processor_charges_scope_created_idx
ON public.billing_processor_charges (
    gateway_account_id,
    billing_scope_id,
    created_at,
    id
);

CREATE INDEX billing_processor_charges_pending_work_idx
ON public.billing_processor_charges (gateway_account_id, observed_at, id)
WHERE progression_state = 'pending';

CREATE INDEX billing_processor_charges_review_idx
ON public.billing_processor_charges (external_reversal_required_at, id)
WHERE progression_state = 'external_reversal_required';

CREATE FUNCTION public.billing_set_processor_charge_attempt_dimensions()
RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, public
AS $$
DECLARE
    owning_attempt public.billing_payment_attempts%ROWTYPE;
BEGIN
    IF TG_OP = 'UPDATE' THEN
        IF NEW.attempt_id IS DISTINCT FROM OLD.attempt_id
            OR NEW.billing_scope_id IS DISTINCT FROM OLD.billing_scope_id
            OR NEW.gateway_account_id IS DISTINCT FROM OLD.gateway_account_id
            OR NEW.gateway_order_id IS DISTINCT FROM OLD.gateway_order_id
            OR NEW.attempt_kind IS DISTINCT FROM OLD.attempt_kind
            OR NEW.plan_key IS DISTINCT FROM OLD.plan_key
            OR NEW.host_charge_target_id IS DISTINCT FROM
                OLD.host_charge_target_id
            OR NEW.amount_cents IS DISTINCT FROM OLD.amount_cents
            OR NEW.currency IS DISTINCT FROM OLD.currency
        THEN
            RAISE EXCEPTION
                'processor charge application dimensions are immutable';
        END IF;
        RETURN NEW;
    END IF;

    SELECT * INTO STRICT owning_attempt
    FROM public.billing_payment_attempts
    WHERE id = NEW.attempt_id;

    IF NEW.billing_scope_id IS DISTINCT FROM owning_attempt.billing_scope_id
        OR NEW.gateway_account_id IS DISTINCT FROM
            owning_attempt.gateway_account_id
        OR NEW.gateway_order_id IS DISTINCT FROM
            owning_attempt.gateway_order_id
    THEN
        RAISE EXCEPTION
            'processor charge provenance does not match owning attempt';
    END IF;

    NEW.attempt_kind := owning_attempt.attempt_kind;
    NEW.plan_key := owning_attempt.plan_key;
    NEW.host_charge_target_id := owning_attempt.host_charge_target_id;
    NEW.amount_cents := owning_attempt.amount_cents;
    NEW.currency := owning_attempt.currency;
    RETURN NEW;
END
$$;

CREATE FUNCTION public.billing_guard_processor_charge_evidence_update()
RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, public
AS $$
BEGIN
    IF NEW.gateway_payment_method_reference IS DISTINCT FROM
            OLD.gateway_payment_method_reference
        OR NEW.gateway_response IS DISTINCT FROM OLD.gateway_response
        OR NEW.gateway_response_code IS DISTINCT FROM
            OLD.gateway_response_code
        OR NEW.gateway_response_text IS DISTINCT FROM
            OLD.gateway_response_text
        OR NEW.gateway_condition IS DISTINCT FROM OLD.gateway_condition
        OR NEW.payment_type IS DISTINCT FROM OLD.payment_type
        OR NEW.card_brand IS DISTINCT FROM OLD.card_brand
        OR NEW.card_last4 IS DISTINCT FROM OLD.card_last4
        OR NEW.card_exp_month IS DISTINCT FROM OLD.card_exp_month
        OR NEW.card_exp_year IS DISTINCT FROM OLD.card_exp_year
        OR (
            NEW.gateway_transaction_id IS DISTINCT FROM
                OLD.gateway_transaction_id
            AND NOT (
                public.billing_canonical_gateway_transaction_id(
                    OLD.gateway_transaction_id
                ) IS NULL
                AND public.billing_canonical_gateway_transaction_id(
                    NEW.gateway_transaction_id
                ) IS NOT NULL
                AND NEW.gateway_transaction_id =
                    public.billing_canonical_gateway_transaction_id(
                        NEW.gateway_transaction_id
                    )
            )
        )
    THEN
        RAISE EXCEPTION 'processor charge evidence is immutable';
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER billing_processor_charge_attempt_dimensions
BEFORE INSERT OR UPDATE OF
    attempt_id,
    billing_scope_id,
    gateway_account_id,
    gateway_order_id,
    attempt_kind,
    plan_key,
    host_charge_target_id,
    amount_cents,
    currency
ON public.billing_processor_charges
FOR EACH ROW
EXECUTE FUNCTION public.billing_set_processor_charge_attempt_dimensions();

CREATE TRIGGER billing_processor_charge_evidence_immutable
BEFORE UPDATE OF
    gateway_transaction_id,
    gateway_payment_method_reference,
    gateway_response,
    gateway_response_code,
    gateway_response_text,
    gateway_condition,
    payment_type,
    card_brand,
    card_last4,
    card_exp_month,
    card_exp_year
ON public.billing_processor_charges
FOR EACH ROW
EXECUTE FUNCTION public.billing_guard_processor_charge_evidence_update();

CREATE TABLE public.billing_external_reversal_attestations (
    attempt_id uuid NOT NULL,
    processor_charge_id uuid NOT NULL,
    actor_id uuid NOT NULL,
    reversal_kind text NOT NULL,
    reason text NOT NULL,
    prior_resolution_code text NOT NULL,
    final_resolution_code text NOT NULL,
    gateway_account_id uuid NOT NULL,
    gateway_configuration_id uuid NOT NULL,
    gateway_order_id text NOT NULL,
    amount_cents integer NOT NULL,
    currency text NOT NULL,
    gateway_transaction_id text NOT NULL,
    gateway_payment_method_reference text,
    gateway_response text,
    gateway_response_code text,
    gateway_response_text text,
    gateway_condition text,
    payment_type text,
    card_brand text,
    card_last4 text,
    card_exp_month smallint,
    card_exp_year smallint,
    attested_at timestamptz NOT NULL,
    CONSTRAINT billing_external_reversal_attestations_pkey
        PRIMARY KEY (attempt_id, gateway_transaction_id),
    CONSTRAINT billing_external_reversal_attestations_charge_key
        UNIQUE (processor_charge_id),
    CONSTRAINT billing_external_reversal_attestations_account_txn_key
        UNIQUE (gateway_account_id, gateway_transaction_id),
    CONSTRAINT billing_external_reversal_attestations_kind_check
        CHECK (reversal_kind IN ('refund', 'void')),
    CONSTRAINT billing_external_reversal_attestations_reason_check
        CHECK (
            reason = btrim(reason)
            AND char_length(reason) BETWEEN 1 AND 500
        ),
    CONSTRAINT billing_external_reversal_attestations_resolution_check
        CHECK (
            (
                prior_resolution_code =
                    'subscription_initial_current_grant_conflict'
                AND (
                    (
                        reversal_kind = 'refund'
                        AND final_resolution_code =
                            'subscription_initial_externally_refunded'
                    )
                    OR (
                        reversal_kind = 'void'
                        AND final_resolution_code =
                            'subscription_initial_externally_voided'
                    )
                )
            )
            OR (
                prior_resolution_code =
                    'processor_charge_external_reversal_required'
                AND (
                    (
                        reversal_kind = 'refund'
                        AND final_resolution_code IN (
                            'subscription_initial_externally_refunded',
                            'processor_charge_externally_refunded'
                        )
                    )
                    OR (
                        reversal_kind = 'void'
                        AND final_resolution_code IN (
                            'subscription_initial_externally_voided',
                            'processor_charge_externally_voided'
                        )
                    )
                )
            )
        ),
    CONSTRAINT billing_external_reversal_attestations_order_check
        CHECK (length(btrim(gateway_order_id)) > 0),
    CONSTRAINT billing_external_reversal_attestations_amount_check
        CHECK (amount_cents > 0),
    CONSTRAINT billing_external_reversal_attestations_currency_check
        CHECK (currency = upper(currency) AND length(currency) = 3),
    CONSTRAINT billing_external_reversal_attestations_transaction_check
        CHECK (
            public.billing_canonical_gateway_transaction_id(
                gateway_transaction_id
            ) IS NOT NULL
            AND gateway_transaction_id =
                public.billing_canonical_gateway_transaction_id(
                    gateway_transaction_id
                )
        ),
    CONSTRAINT billing_external_reversal_attestations_gateway_text_check
        CHECK (
            (gateway_response IS NULL OR octet_length(gateway_response) <= 512)
            AND (
                gateway_response_code IS NULL
                OR octet_length(gateway_response_code) <= 512
            )
            AND (
                gateway_response_text IS NULL
                OR octet_length(gateway_response_text) <= 512
            )
            AND (
                gateway_condition IS NULL
                OR octet_length(gateway_condition) <= 512
            )
            AND (payment_type IS NULL OR octet_length(payment_type) <= 512)
            AND (card_brand IS NULL OR octet_length(card_brand) <= 512)
        ),
    CONSTRAINT billing_external_reversal_attestations_card_last4_check
        CHECK (card_last4 IS NULL OR card_last4 ~ '^[0-9]{4}$'),
    CONSTRAINT billing_external_reversal_attestations_card_exp_month_check
        CHECK (card_exp_month IS NULL OR card_exp_month BETWEEN 1 AND 12),
    CONSTRAINT billing_external_reversal_attestations_card_exp_year_check
        CHECK (card_exp_year IS NULL OR card_exp_year >= 2000),
    CONSTRAINT billing_external_reversal_attestations_attempt_fk
        FOREIGN KEY (attempt_id)
        REFERENCES public.billing_payment_attempts(id)
        ON DELETE RESTRICT,
    CONSTRAINT billing_external_reversal_attestations_charge_attempt_fk
        FOREIGN KEY (processor_charge_id, attempt_id)
        REFERENCES public.billing_processor_charges(id, attempt_id)
        ON DELETE RESTRICT,
    CONSTRAINT billing_external_reversal_attestations_account_fk
        FOREIGN KEY (gateway_account_id)
        REFERENCES public.billing_gateway_accounts(id)
        ON DELETE RESTRICT
);

CREATE INDEX billing_external_reversal_attestations_actor_time_idx
ON public.billing_external_reversal_attestations (actor_id, attested_at DESC);

CREATE TABLE public.billing_gateway_lifecycle_pending_updates (
    id uuid PRIMARY KEY DEFAULT uuidv7(),
    billing_scope_id uuid NOT NULL,
    gateway_account_id uuid NOT NULL,
    gateway_transaction_id text,
    gateway_order_id text,
    gateway_condition text,
    gateway_lifecycle_status text NOT NULL,
    gateway_lifecycle_action text,
    gateway_lifecycle_at timestamptz,
    refunded_amount_cents integer,
    first_seen_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    last_checked_at timestamptz,
    check_count integer NOT NULL DEFAULT 0,
    expires_at timestamptz NOT NULL DEFAULT now() + interval '7 days',
    CONSTRAINT billing_gateway_lifecycle_pending_target_check
        CHECK (
            gateway_transaction_id IS NOT NULL
            OR gateway_order_id IS NOT NULL
        ),
    CONSTRAINT billing_gateway_lifecycle_pending_transaction_check
        CHECK (
            NOT gateway_transaction_id IS DISTINCT FROM
                public.billing_canonical_gateway_transaction_id(
                    gateway_transaction_id
                )
        ),
    CONSTRAINT billing_gateway_lifecycle_pending_order_check
        CHECK (
            gateway_order_id IS NULL
            OR length(btrim(gateway_order_id)) > 0
        ),
    CONSTRAINT billing_gateway_lifecycle_pending_gateway_text_check
        CHECK (
            (
                gateway_condition IS NULL
                OR octet_length(gateway_condition) <= 512
            )
            AND (
                gateway_lifecycle_action IS NULL
                OR octet_length(gateway_lifecycle_action) <= 512
            )
        ),
    CONSTRAINT billing_gateway_lifecycle_pending_state_check
        CHECK (
            (
                gateway_lifecycle_status IN (
                    'unknown',
                    'pending_settlement',
                    'voided'
                )
                AND refunded_amount_cents IS NULL
            )
            OR (
                gateway_lifecycle_status = 'settled'
                AND (
                    refunded_amount_cents IS NULL
                    OR refunded_amount_cents > 0
                )
            )
            OR (
                gateway_lifecycle_status = 'refunded'
                AND refunded_amount_cents IS NOT NULL
                AND refunded_amount_cents > 0
            )
            OR (
                gateway_lifecycle_status = 'chargeback'
                AND (
                    refunded_amount_cents IS NULL
                    OR refunded_amount_cents > 0
                )
            )
        ),
    CONSTRAINT billing_gateway_lifecycle_pending_check_count_check
        CHECK (check_count >= 0),
    CONSTRAINT billing_gateway_lifecycle_pending_expiry_check
        CHECK (expires_at >= first_seen_at),
    CONSTRAINT billing_gateway_lifecycle_pending_timestamp_order_check
        CHECK (updated_at >= first_seen_at),
    CONSTRAINT billing_gateway_lifecycle_pending_account_scope_fk
        FOREIGN KEY (gateway_account_id, billing_scope_id)
        REFERENCES public.billing_gateway_accounts(id, billing_scope_id)
        ON DELETE RESTRICT
);

CREATE UNIQUE INDEX billing_gateway_lifecycle_pending_dedupe_idx
ON public.billing_gateway_lifecycle_pending_updates (
    gateway_account_id,
    COALESCE(gateway_transaction_id, ''),
    COALESCE(gateway_order_id, ''),
    COALESCE(gateway_condition, ''),
    gateway_lifecycle_status,
    COALESCE(gateway_lifecycle_action, ''),
    COALESCE(gateway_lifecycle_at, '-infinity'),
    COALESCE(refunded_amount_cents, -1)
);

CREATE INDEX billing_gateway_lifecycle_pending_check_idx
ON public.billing_gateway_lifecycle_pending_updates (
    gateway_account_id,
    last_checked_at,
    first_seen_at,
    id
);

CREATE INDEX billing_gateway_lifecycle_pending_cleanup_idx
ON public.billing_gateway_lifecycle_pending_updates (
    gateway_account_id,
    expires_at,
    check_count,
    first_seen_at,
    id
);

CREATE TABLE public.billing_gateway_lifecycle_quarantines (
    id uuid PRIMARY KEY DEFAULT uuidv7(),
    billing_scope_id uuid NOT NULL,
    gateway_account_id uuid NOT NULL,
    gateway_transaction_id text,
    gateway_order_id text,
    reason_code text NOT NULL,
    first_seen_at timestamptz NOT NULL DEFAULT now(),
    last_seen_at timestamptz NOT NULL DEFAULT now(),
    occurrence_count bigint NOT NULL DEFAULT 1,
    resolved_at timestamptz,
    last_operator_alerted_at timestamptz,
    CONSTRAINT billing_gateway_lifecycle_quarantines_transaction_check
        CHECK (
            NOT gateway_transaction_id IS DISTINCT FROM
                public.billing_canonical_gateway_transaction_id(
                    gateway_transaction_id
                )
        ),
    CONSTRAINT billing_gateway_lifecycle_quarantines_order_check
        CHECK (
            gateway_order_id IS NULL
            OR length(btrim(gateway_order_id)) > 0
        ),
    CONSTRAINT billing_gateway_lifecycle_quarantines_reason_check
        CHECK (
            reason_code IN (
                'ambiguous_reversal_success',
                'invalid_refund_economics',
                'malformed_report_structure'
            )
        ),
    CONSTRAINT billing_gateway_lifecycle_quarantines_target_check
        CHECK (
            gateway_transaction_id IS NOT NULL
            OR gateway_order_id IS NOT NULL
            OR reason_code = 'malformed_report_structure'
        ),
    CONSTRAINT billing_gateway_lifecycle_quarantines_count_check
        CHECK (occurrence_count > 0),
    CONSTRAINT billing_gateway_lifecycle_quarantines_time_check
        CHECK (
            last_seen_at >= first_seen_at
            AND (
                resolved_at IS NULL
                OR resolved_at >= first_seen_at
            )
            AND (
                last_operator_alerted_at IS NULL
                OR last_operator_alerted_at >= first_seen_at
            )
        ),
    CONSTRAINT billing_gateway_lifecycle_quarantines_account_scope_fk
        FOREIGN KEY (gateway_account_id, billing_scope_id)
        REFERENCES public.billing_gateway_accounts(id, billing_scope_id)
        ON DELETE RESTRICT
);

CREATE UNIQUE INDEX billing_gateway_lifecycle_quarantines_dedupe_idx
ON public.billing_gateway_lifecycle_quarantines (
    gateway_account_id,
    COALESCE(gateway_transaction_id, ''),
    COALESCE(gateway_order_id, ''),
    reason_code
);

CREATE INDEX billing_gateway_lifecycle_quarantines_review_idx
ON public.billing_gateway_lifecycle_quarantines (first_seen_at, id)
WHERE resolved_at IS NULL;

CREATE INDEX billing_gateway_lifecycle_quarantines_unresolved_idx
ON public.billing_gateway_lifecycle_quarantines (
    billing_scope_id,
    gateway_account_id
)
WHERE resolved_at IS NULL;

CREATE TABLE public.billing_gateway_lifecycle_quarantine_resolutions (
    id uuid PRIMARY KEY DEFAULT uuidv7(),
    quarantine_id uuid NOT NULL,
    actor_id uuid NOT NULL,
    reason text NOT NULL,
    observed_occurrence_count bigint NOT NULL,
    observed_last_seen_at timestamptz NOT NULL,
    resolved_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT billing_gateway_lifecycle_resolution_observation_key
        UNIQUE (
            quarantine_id,
            observed_occurrence_count,
            observed_last_seen_at
        ),
    CONSTRAINT billing_gateway_lifecycle_resolution_reason_check
        CHECK (
            reason = btrim(reason)
            AND char_length(reason) BETWEEN 1 AND 500
        ),
    CONSTRAINT billing_gateway_lifecycle_resolution_count_check
        CHECK (observed_occurrence_count > 0),
    CONSTRAINT billing_gateway_lifecycle_resolution_time_check
        CHECK (resolved_at >= observed_last_seen_at),
    CONSTRAINT billing_gateway_lifecycle_resolution_quarantine_fk
        FOREIGN KEY (quarantine_id)
        REFERENCES public.billing_gateway_lifecycle_quarantines(id)
        ON DELETE RESTRICT
);

CREATE INDEX billing_gateway_lifecycle_resolution_actor_time_idx
ON public.billing_gateway_lifecycle_quarantine_resolutions (
    actor_id,
    resolved_at DESC
);

CREATE TABLE public.billing_reconciliation_cursors (
    gateway_account_id uuid NOT NULL,
    billing_scope_id uuid NOT NULL,
    provider_key text NOT NULL,
    cursor_key text NOT NULL,
    last_successful_end_at timestamptz NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT billing_reconciliation_cursors_pkey
        PRIMARY KEY (gateway_account_id, provider_key, cursor_key),
    CONSTRAINT billing_reconciliation_cursors_provider_key_check
        CHECK (provider_key ~ '^[a-z0-9][a-z0-9_-]{0,63}$'),
    CONSTRAINT billing_reconciliation_cursors_cursor_key_check
        CHECK (cursor_key ~ '^[a-z0-9][a-z0-9_-]{0,63}$'),
    CONSTRAINT billing_reconciliation_cursors_timestamp_order_check
        CHECK (updated_at >= created_at),
    CONSTRAINT billing_reconciliation_cursors_account_provider_fk
        FOREIGN KEY (
            gateway_account_id,
            billing_scope_id,
            provider_key
        )
        REFERENCES public.billing_gateway_accounts(
            id,
            billing_scope_id,
            provider_key
        )
        ON DELETE RESTRICT
);

CREATE TABLE public.billing_subscription_discount_codes (
    id uuid PRIMARY KEY,
    billing_scope_id uuid NOT NULL,
    plan_key text NOT NULL,
    code_normalized text NOT NULL,
    display_code text NOT NULL,
    label text,
    status text NOT NULL,
    discount_kind text NOT NULL,
    amount_off_cents integer,
    percent_off_bps integer,
    currency text NOT NULL DEFAULT 'USD',
    duration text NOT NULL,
    duration_months integer,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT billing_subscription_discount_codes_id_scope_plan_key
        UNIQUE (id, billing_scope_id, plan_key),
    CONSTRAINT billing_subscription_discount_codes_plan_key_check
        CHECK (plan_key ~ '^[a-z0-9][a-z0-9_-]{0,63}$'),
    CONSTRAINT billing_subscription_discount_codes_code_check
        CHECK (
            length(btrim(code_normalized)) > 0
            AND length(btrim(display_code)) > 0
        ),
    CONSTRAINT billing_subscription_discount_codes_active_length_check
        CHECK (
            status <> 'active'
            OR (
                char_length(code_normalized) >= 5
                AND char_length(display_code) >= 5
            )
        ),
    CONSTRAINT billing_subscription_discount_codes_status_check
        CHECK (status IN ('active', 'disabled')),
    CONSTRAINT billing_subscription_discount_codes_value_check
        CHECK (
            (
                discount_kind = 'amount_off'
                AND amount_off_cents IS NOT NULL
                AND amount_off_cents > 0
                AND percent_off_bps IS NULL
            )
            OR (
                discount_kind = 'percent_off'
                AND amount_off_cents IS NULL
                AND percent_off_bps IS NOT NULL
                AND percent_off_bps BETWEEN 1 AND 9999
            )
        ),
    CONSTRAINT billing_subscription_discount_codes_currency_check
        CHECK (currency = upper(currency) AND length(currency) = 3),
    CONSTRAINT billing_subscription_discount_codes_duration_check
        CHECK (
            (
                duration = 'indefinite'
                AND duration_months IS NULL
            )
            OR (
                duration = 'limited_months'
                AND duration_months IS NOT NULL
                AND duration_months BETWEEN 1 AND 36
            )
        ),
    CONSTRAINT billing_subscription_discount_codes_timestamp_order_check
        CHECK (updated_at >= created_at)
);

CREATE UNIQUE INDEX billing_subscription_discount_codes_scope_plan_code_idx
ON public.billing_subscription_discount_codes (
    billing_scope_id,
    plan_key,
    code_normalized
);

CREATE INDEX billing_subscription_discount_codes_lookup_idx
ON public.billing_subscription_discount_codes (
    billing_scope_id,
    plan_key,
    status,
    code_normalized
);

CREATE TABLE public.billing_subscription_discount_claims (
    id uuid PRIMARY KEY,
    billing_scope_id uuid NOT NULL,
    subscriber_id uuid NOT NULL,
    plan_key text NOT NULL,
    discount_code_id uuid NOT NULL,
    code_snapshot text NOT NULL,
    label_snapshot text,
    discount_kind text NOT NULL,
    amount_off_cents integer,
    percent_off_bps integer,
    currency text NOT NULL,
    duration text NOT NULL,
    duration_months integer,
    base_amount_cents integer NOT NULL,
    discounted_amount_cents integer NOT NULL,
    status text NOT NULL,
    claimed_at timestamptz NOT NULL DEFAULT now(),
    applied_at timestamptz,
    applied_subscription_id uuid,
    applied_payment_attempt_id uuid,
    superseded_at timestamptz,
    CONSTRAINT billing_subscription_discount_claims_id_owner_plan_key
        UNIQUE (id, billing_scope_id, subscriber_id, plan_key),
    CONSTRAINT billing_subscription_discount_claims_plan_key_check
        CHECK (plan_key ~ '^[a-z0-9][a-z0-9_-]{0,63}$'),
    CONSTRAINT billing_subscription_discount_claims_code_check
        CHECK (length(btrim(code_snapshot)) > 0),
    CONSTRAINT billing_subscription_discount_claims_value_check
        CHECK (
            (
                discount_kind = 'amount_off'
                AND amount_off_cents IS NOT NULL
                AND amount_off_cents > 0
                AND percent_off_bps IS NULL
            )
            OR (
                discount_kind = 'percent_off'
                AND amount_off_cents IS NULL
                AND percent_off_bps IS NOT NULL
                AND percent_off_bps BETWEEN 1 AND 9999
            )
        ),
    CONSTRAINT billing_subscription_discount_claims_currency_check
        CHECK (currency = upper(currency) AND length(currency) = 3),
    CONSTRAINT billing_subscription_discount_claims_duration_check
        CHECK (
            (
                duration = 'indefinite'
                AND duration_months IS NULL
            )
            OR (
                duration = 'limited_months'
                AND duration_months IS NOT NULL
                AND duration_months BETWEEN 1 AND 36
            )
        ),
    CONSTRAINT billing_subscription_discount_claims_amounts_check
        CHECK (
            base_amount_cents > 0
            AND discounted_amount_cents > 0
            AND discounted_amount_cents <= base_amount_cents
        ),
    CONSTRAINT billing_subscription_discount_claims_status_check
        CHECK (status IN ('saved', 'applied', 'superseded', 'expired')),
    CONSTRAINT billing_subscription_discount_claims_status_fields_check
        CHECK (
            (
                status = 'applied'
                AND applied_at IS NOT NULL
                AND applied_subscription_id IS NOT NULL
                AND applied_payment_attempt_id IS NOT NULL
                AND superseded_at IS NULL
            )
            OR (
                status = 'superseded'
                AND applied_at IS NULL
                AND applied_subscription_id IS NULL
                AND applied_payment_attempt_id IS NULL
                AND superseded_at IS NOT NULL
            )
            OR (
                status IN ('saved', 'expired')
                AND applied_at IS NULL
                AND applied_subscription_id IS NULL
                AND applied_payment_attempt_id IS NULL
                AND superseded_at IS NULL
            )
        ),
    CONSTRAINT billing_subscription_discount_claims_code_fk
        FOREIGN KEY (discount_code_id, billing_scope_id, plan_key)
        REFERENCES public.billing_subscription_discount_codes(
            id,
            billing_scope_id,
            plan_key
        )
        ON DELETE RESTRICT,
    CONSTRAINT billing_subscription_discount_claims_subscription_fk
        FOREIGN KEY (
            applied_subscription_id,
            billing_scope_id,
            subscriber_id,
            plan_key
        )
        REFERENCES public.billing_subscriptions(
            id,
            billing_scope_id,
            subscriber_id,
            plan_key
        )
        ON DELETE RESTRICT,
    CONSTRAINT billing_subscription_discount_claims_attempt_fk
        FOREIGN KEY (
            applied_payment_attempt_id,
            billing_scope_id,
            subscriber_id,
            plan_key
        )
        REFERENCES public.billing_payment_attempts(
            id,
            billing_scope_id,
            subscriber_id,
            plan_key
        )
        ON DELETE RESTRICT
);

CREATE UNIQUE INDEX billing_subscription_discount_claims_saved_once_idx
ON public.billing_subscription_discount_claims (
    billing_scope_id,
    subscriber_id,
    plan_key
)
WHERE status = 'saved';

CREATE INDEX billing_subscription_discount_claims_owner_status_idx
ON public.billing_subscription_discount_claims (
    billing_scope_id,
    subscriber_id,
    plan_key,
    status,
    claimed_at DESC
);

CREATE TABLE public.billing_subscription_discounts (
    subscription_id uuid PRIMARY KEY,
    billing_scope_id uuid NOT NULL,
    subscriber_id uuid NOT NULL,
    plan_key text NOT NULL,
    discount_claim_id uuid,
    discount_code_id uuid,
    code_snapshot text NOT NULL,
    label_snapshot text,
    discount_kind text NOT NULL,
    amount_off_cents integer,
    percent_off_bps integer,
    currency text NOT NULL,
    duration text NOT NULL,
    duration_months integer,
    base_amount_cents integer NOT NULL,
    discounted_amount_cents integer NOT NULL,
    periods_total integer,
    periods_applied integer NOT NULL DEFAULT 1,
    status text NOT NULL,
    applied_at timestamptz NOT NULL DEFAULT now(),
    completed_at timestamptz,
    CONSTRAINT billing_subscription_discounts_plan_key_check
        CHECK (plan_key ~ '^[a-z0-9][a-z0-9_-]{0,63}$'),
    CONSTRAINT billing_subscription_discounts_code_check
        CHECK (length(btrim(code_snapshot)) > 0),
    CONSTRAINT billing_subscription_discounts_value_check
        CHECK (
            (
                discount_kind = 'amount_off'
                AND amount_off_cents IS NOT NULL
                AND amount_off_cents > 0
                AND percent_off_bps IS NULL
            )
            OR (
                discount_kind = 'percent_off'
                AND amount_off_cents IS NULL
                AND percent_off_bps IS NOT NULL
                AND percent_off_bps BETWEEN 1 AND 9999
            )
        ),
    CONSTRAINT billing_subscription_discounts_currency_check
        CHECK (currency = upper(currency) AND length(currency) = 3),
    CONSTRAINT billing_subscription_discounts_amounts_check
        CHECK (
            base_amount_cents > 0
            AND discounted_amount_cents > 0
            AND discounted_amount_cents <= base_amount_cents
        ),
    CONSTRAINT billing_subscription_discounts_status_check
        CHECK (status IN ('active', 'completed')),
    CONSTRAINT billing_subscription_discounts_duration_periods_check
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
        ),
    CONSTRAINT billing_subscription_discounts_subscription_fk
        FOREIGN KEY (
            subscription_id,
            billing_scope_id,
            subscriber_id,
            plan_key
        )
        REFERENCES public.billing_subscriptions(
            id,
            billing_scope_id,
            subscriber_id,
            plan_key
        )
        ON DELETE RESTRICT,
    CONSTRAINT billing_subscription_discounts_claim_fk
        FOREIGN KEY (
            discount_claim_id,
            billing_scope_id,
            subscriber_id,
            plan_key
        )
        REFERENCES public.billing_subscription_discount_claims(
            id,
            billing_scope_id,
            subscriber_id,
            plan_key
        )
        ON DELETE RESTRICT,
    CONSTRAINT billing_subscription_discounts_code_fk
        FOREIGN KEY (discount_code_id, billing_scope_id, plan_key)
        REFERENCES public.billing_subscription_discount_codes(
            id,
            billing_scope_id,
            plan_key
        )
        ON DELETE RESTRICT
);

CREATE INDEX billing_subscription_discounts_owner_applied_idx
ON public.billing_subscription_discounts (
    billing_scope_id,
    subscriber_id,
    plan_key,
    applied_at DESC
);

CREATE TABLE public.billing_subscription_grants (
    id uuid PRIMARY KEY,
    billing_scope_id uuid NOT NULL,
    subscriber_id uuid NOT NULL,
    plan_key text NOT NULL,
    grant_kind text NOT NULL,
    reason text NOT NULL,
    starts_at timestamptz NOT NULL DEFAULT now(),
    ends_at timestamptz NOT NULL,
    granted_by_actor_id uuid NOT NULL,
    revoked_at timestamptz,
    revoked_by_actor_id uuid,
    revocation_reason text,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT billing_subscription_grants_id_owner_plan_key
        UNIQUE (id, billing_scope_id, subscriber_id, plan_key),
    CONSTRAINT billing_subscription_grants_plan_key_check
        CHECK (plan_key ~ '^[a-z0-9][a-z0-9_-]{0,63}$'),
    CONSTRAINT billing_subscription_grants_kind_check
        CHECK (grant_kind IN ('testing', 'promotion')),
    CONSTRAINT billing_subscription_grants_period_check
        CHECK (starts_at < ends_at),
    CONSTRAINT billing_subscription_grants_reason_check
        CHECK (
            reason = btrim(reason)
            AND char_length(reason) BETWEEN 1 AND 500
        ),
    CONSTRAINT billing_subscription_grants_revocation_check
        CHECK (
            (
                revoked_at IS NULL
                AND revoked_by_actor_id IS NULL
                AND revocation_reason IS NULL
            )
            OR (
                revoked_at IS NOT NULL
                AND revoked_by_actor_id IS NOT NULL
                AND revocation_reason IS NOT NULL
                AND revocation_reason = btrim(revocation_reason)
                AND char_length(revocation_reason) BETWEEN 1 AND 500
                AND revoked_at >= starts_at
            )
        ),
    CONSTRAINT billing_subscription_grants_timestamp_order_check
        CHECK (updated_at >= created_at)
);

CREATE INDEX billing_subscription_grants_current_lookup_idx
ON public.billing_subscription_grants (
    billing_scope_id,
    subscriber_id,
    plan_key,
    ends_at DESC
)
WHERE revoked_at IS NULL;

ALTER TABLE public.billing_payment_attempts
    ADD CONSTRAINT billing_payment_attempts_initial_discount_claim_fk
    FOREIGN KEY (
        subscription_initial_discount_claim_id,
        billing_scope_id,
        subscriber_id,
        plan_key
    )
    REFERENCES public.billing_subscription_discount_claims(
        id,
        billing_scope_id,
        subscriber_id,
        plan_key
    )
    ON DELETE RESTRICT;

ALTER TABLE public.billing_payment_attempts
    ADD CONSTRAINT billing_payment_attempts_initial_discount_code_fk
    FOREIGN KEY (
        subscription_initial_discount_code_id,
        billing_scope_id,
        plan_key
    )
    REFERENCES public.billing_subscription_discount_codes(
        id,
        billing_scope_id,
        plan_key
    )
    ON DELETE RESTRICT;

CREATE VIEW public.billing_current_subscriptions AS
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
    unpaid_at,
    required_gateway_account_mode
FROM public.billing_subscriptions
WHERE status IN ('active', 'past_due')
    OR (
        status = 'canceled'
        AND current_period_end_at > now()
    );

CREATE VIEW public.billing_payment_facts AS
SELECT
    id AS attempt_id,
    billing_scope_id,
    subscriber_id,
    plan_key,
    host_charge_target_id,
    attempt_kind,
    status,
    amount_cents,
    currency,
    created_at,
    resolved_at,
    gateway_lifecycle_status,
    refunded_amount_cents
FROM public.billing_payment_attempts;

CREATE VIEW public.billing_active_discount_facts AS
SELECT
    id AS discount_code_id,
    billing_scope_id,
    plan_key,
    code_normalized,
    display_code,
    label
FROM public.billing_subscription_discount_codes
WHERE status = 'active';

-- Complete the v6 attempt-evidence definition before creating policies that
-- use the classification. The column remains physically after every v5 column.
ALTER TABLE public.billing_payment_attempts
    ADD COLUMN gateway_approval_evidence text NOT NULL DEFAULT 'unclassified'
        CONSTRAINT billing_payment_attempts_approval_evidence_check
        CHECK (gateway_approval_evidence IN ('unclassified', 'absent', 'text_only', 'structured'));

-- Only genuinely empty retained observations establish absence. Old local
-- query notes may have overwritten provider text and cannot prove its absence.
UPDATE public.billing_payment_attempts
SET gateway_approval_evidence = 'absent'
WHERE gateway_transaction_id IS NULL
    AND gateway_payment_method_reference IS NULL
    AND gateway_response IS NULL
    AND gateway_response_code IS NULL
    AND gateway_condition IS NULL
    AND gateway_response_text IS NULL;

-- New reservations have no observation yet. All provider/error writes must
-- explicitly persist their classification with the evidence bundle.
ALTER TABLE public.billing_payment_attempts
    ALTER COLUMN gateway_approval_evidence SET DEFAULT 'absent';

CREATE FUNCTION public.billing_host_charge_ledger_admission(
    p_billing_scope_id uuid,
    p_subscriber_id uuid,
    p_host_charge_target_id uuid,
    p_mode text,
    p_idempotency_key text DEFAULT NULL,
    p_attempt_id uuid DEFAULT NULL
)
RETURNS text
LANGUAGE plpgsql
STABLE
SET search_path = pg_catalog, public
AS $$
DECLARE
    has_contender boolean;
    has_unsafe_attempt boolean;
    has_unsafe_charge boolean;
BEGIN
    IF p_mode NOT IN ('reserve', 'submit', 'release') THEN
        RAISE EXCEPTION 'unsupported host charge ledger admission mode';
    END IF;

    IF (
        p_mode = 'reserve'
        AND (
            p_idempotency_key IS NULL
            OR length(btrim(p_idempotency_key)) = 0
            OR p_attempt_id IS NOT NULL
        )
    ) OR (
        p_mode = 'submit'
        AND (
            p_idempotency_key IS NOT NULL
            OR p_attempt_id IS NULL
        )
    ) OR (
        p_mode = 'release'
        AND (
            p_idempotency_key IS NOT NULL
            OR p_attempt_id IS NOT NULL
        )
    ) THEN
        RAISE EXCEPTION 'invalid host charge ledger admission arguments';
    END IF;

    WITH target_attempts AS (
        SELECT
            attempts.*,
            (
                p_mode = 'reserve'
                AND attempts.idempotency_key = p_idempotency_key
            ) AS contender,
            (
                p_mode = 'submit'
                AND attempts.id = p_attempt_id
                AND attempts.status = 'pending'
                AND attempts.submitted_at IS NULL
                AND attempts.resolved_at IS NULL
                AND attempts.gateway_lifecycle_status = 'unknown'
                AND attempts.refunded_amount_cents = 0
                AND attempts.gateway_approval_evidence = 'absent'
                AND attempts.gateway_transaction_id IS NULL
                AND attempts.gateway_payment_method_reference IS NULL
                AND attempts.gateway_response IS NULL
                AND attempts.gateway_response_code IS NULL
                AND attempts.gateway_response_text IS NULL
                AND attempts.gateway_condition IS NULL
                AND attempts.resolution_code IS NULL
                AND attempts.payment_type IS NULL
                AND attempts.card_brand IS NULL
                AND attempts.card_last4 IS NULL
                AND attempts.card_exp_month IS NULL
                AND attempts.card_exp_year IS NULL
                AND attempts.gateway_lifecycle_action IS NULL
                AND attempts.gateway_lifecycle_at IS NULL
                AND attempts.gateway_lifecycle_reconciled_at IS NULL
                AND NOT EXISTS (
                    SELECT 1
                    FROM public.billing_processor_charges AS pending_charges
                    WHERE pending_charges.attempt_id = attempts.id
                )
            ) AS allowed_pending,
            (
                attempts.gateway_lifecycle_status = 'unknown'
                AND attempts.refunded_amount_cents = 0
                AND (
                    (
                        attempts.gateway_approval_evidence = 'absent'
                        AND (
                            attempts.status = 'declined'
                            OR (
                                attempts.status = 'failed'
                                AND attempts.submitted_at IS NULL
                            )
                        )
                    )
                    OR (
                        attempts.status IN ('declined', 'failed')
                        AND EXISTS (
                            SELECT 1
                            FROM public.billing_processor_charges
                                AS attempt_charges
                            WHERE attempt_charges.attempt_id = attempts.id
                        )
                    )
                )
            ) AS terminal_safe
        FROM public.billing_payment_attempts AS attempts
        WHERE attempts.billing_scope_id = p_billing_scope_id
            AND attempts.subscriber_id = p_subscriber_id
            AND attempts.host_charge_target_id = p_host_charge_target_id
            AND attempts.attempt_kind = 'host_charge'
    )
    SELECT
        COALESCE(bool_or(contender), false),
        COALESCE(bool_or(NOT (
            contender
            OR allowed_pending
            OR terminal_safe
        )), false)
    INTO has_contender, has_unsafe_attempt
    FROM target_attempts;

    IF p_mode = 'submit' AND NOT EXISTS (
        SELECT 1
        FROM public.billing_payment_attempts AS attempts
        WHERE attempts.id = p_attempt_id
            AND attempts.billing_scope_id = p_billing_scope_id
            AND attempts.subscriber_id = p_subscriber_id
            AND attempts.host_charge_target_id = p_host_charge_target_id
            AND attempts.attempt_kind = 'host_charge'
            AND attempts.status = 'pending'
            AND attempts.submitted_at IS NULL
            AND attempts.resolved_at IS NULL
            AND attempts.gateway_lifecycle_status = 'unknown'
            AND attempts.refunded_amount_cents = 0
            AND attempts.gateway_approval_evidence = 'absent'
            AND attempts.gateway_transaction_id IS NULL
            AND attempts.gateway_payment_method_reference IS NULL
            AND attempts.gateway_response IS NULL
            AND attempts.gateway_response_code IS NULL
            AND attempts.gateway_response_text IS NULL
            AND attempts.gateway_condition IS NULL
            AND attempts.resolution_code IS NULL
            AND attempts.payment_type IS NULL
            AND attempts.card_brand IS NULL
            AND attempts.card_last4 IS NULL
            AND attempts.card_exp_month IS NULL
            AND attempts.card_exp_year IS NULL
            AND attempts.gateway_lifecycle_action IS NULL
            AND attempts.gateway_lifecycle_at IS NULL
            AND attempts.gateway_lifecycle_reconciled_at IS NULL
            AND NOT EXISTS (
                SELECT 1
                FROM public.billing_processor_charges AS pending_charges
                WHERE pending_charges.attempt_id = attempts.id
            )
    ) THEN
        RETURN 'unsafe';
    END IF;

    SELECT EXISTS (
        SELECT 1
        FROM public.billing_processor_charges AS charges
        INNER JOIN public.billing_payment_attempts AS attempts
            ON attempts.id = charges.attempt_id
        WHERE charges.billing_scope_id = p_billing_scope_id
            AND attempts.subscriber_id = p_subscriber_id
            AND charges.host_charge_target_id = p_host_charge_target_id
            AND NOT (
                (p_mode = 'reserve' AND attempts.idempotency_key =
                    p_idempotency_key)
                OR (p_mode = 'submit' AND attempts.id = p_attempt_id)
            )
            AND NOT (
                charges.progression_state = 'externally_reversed'
                AND charges.externally_reversed_at IS NOT NULL
                AND charges.attempt_kind = 'host_charge'
                AND attempts.status IN ('declined', 'failed')
                AND EXISTS (
                    SELECT 1
                    FROM public.billing_external_reversal_attestations
                        AS attestations
                    WHERE attestations.processor_charge_id = charges.id
                        AND attestations.attempt_id = charges.attempt_id
                        AND attestations.prior_resolution_code =
                            'processor_charge_external_reversal_required'
                        AND charges.state_code =
                            attestations.prior_resolution_code
                        AND (
                            (
                                attestations.reversal_kind = 'refund'
                                AND attestations.final_resolution_code =
                                    'processor_charge_externally_refunded'
                            )
                            OR (
                                attestations.reversal_kind = 'void'
                                AND attestations.final_resolution_code =
                                    'processor_charge_externally_voided'
                            )
                        )
                        AND attestations.gateway_account_id =
                            charges.gateway_account_id
                        AND attestations.gateway_account_id =
                            attempts.gateway_account_id
                        AND attestations.gateway_configuration_id =
                            attempts.gateway_configuration_id
                        AND attestations.gateway_order_id =
                            charges.gateway_order_id
                        AND attestations.gateway_order_id =
                            attempts.gateway_order_id
                        AND attestations.amount_cents =
                            charges.amount_cents
                        AND attestations.amount_cents =
                            attempts.amount_cents
                        AND attestations.currency = charges.currency
                        AND attestations.currency = attempts.currency
                        AND attestations.gateway_transaction_id =
                            charges.gateway_transaction_id
                        AND attestations.gateway_payment_method_reference
                            IS NOT DISTINCT FROM
                            charges.gateway_payment_method_reference
                        AND attestations.gateway_response
                            IS NOT DISTINCT FROM charges.gateway_response
                        AND attestations.gateway_response_code
                            IS NOT DISTINCT FROM
                            charges.gateway_response_code
                        AND attestations.gateway_response_text
                            IS NOT DISTINCT FROM
                            charges.gateway_response_text
                        AND attestations.gateway_condition
                            IS NOT DISTINCT FROM charges.gateway_condition
                        AND attestations.payment_type
                            IS NOT DISTINCT FROM charges.payment_type
                        AND attestations.card_brand
                            IS NOT DISTINCT FROM charges.card_brand
                        AND attestations.card_last4
                            IS NOT DISTINCT FROM charges.card_last4
                        AND attestations.card_exp_month
                            IS NOT DISTINCT FROM charges.card_exp_month
                        AND attestations.card_exp_year
                            IS NOT DISTINCT FROM charges.card_exp_year
                )
            )
    )
    INTO has_unsafe_charge;

    IF has_unsafe_attempt OR has_unsafe_charge THEN
        RETURN 'unsafe';
    END IF;

    IF p_mode = 'reserve' AND has_contender THEN
        RETURN 'idempotent_contender';
    END IF;

    RETURN 'safe';
END
$$;

-- Complete the v6 immutable-evidence definitions. These columns remain
-- physically after every v5 column on fresh and upgraded hosts.

ALTER TABLE public.billing_processor_charges
    ADD COLUMN gateway_approval_evidence text NOT NULL DEFAULT 'unclassified'
        CONSTRAINT billing_processor_charges_approval_evidence_check
        CHECK (gateway_approval_evidence IN ('unclassified', 'absent', 'text_only', 'structured'));

ALTER TABLE public.billing_external_reversal_attestations
    ADD COLUMN gateway_approval_evidence text NOT NULL DEFAULT 'unclassified'
        CONSTRAINT billing_external_reversal_attestations_approval_evidence_check
        CHECK (gateway_approval_evidence IN ('unclassified', 'absent', 'text_only', 'structured'));

CREATE OR REPLACE FUNCTION public.billing_guard_processor_charge_evidence_update()
RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, public
AS $$
BEGIN
    IF NEW.gateway_approval_evidence IS DISTINCT FROM OLD.gateway_approval_evidence
        OR NEW.gateway_payment_method_reference IS DISTINCT FROM
            OLD.gateway_payment_method_reference
        OR NEW.gateway_response IS DISTINCT FROM OLD.gateway_response
        OR NEW.gateway_response_code IS DISTINCT FROM
            OLD.gateway_response_code
        OR NEW.gateway_response_text IS DISTINCT FROM
            OLD.gateway_response_text
        OR NEW.gateway_condition IS DISTINCT FROM OLD.gateway_condition
        OR NEW.payment_type IS DISTINCT FROM OLD.payment_type
        OR NEW.card_brand IS DISTINCT FROM OLD.card_brand
        OR NEW.card_last4 IS DISTINCT FROM OLD.card_last4
        OR NEW.card_exp_month IS DISTINCT FROM OLD.card_exp_month
        OR NEW.card_exp_year IS DISTINCT FROM OLD.card_exp_year
        OR (
            NEW.gateway_transaction_id IS DISTINCT FROM
                OLD.gateway_transaction_id
            AND NOT (
                public.billing_canonical_gateway_transaction_id(
                    OLD.gateway_transaction_id
                ) IS NULL
                AND public.billing_canonical_gateway_transaction_id(
                    NEW.gateway_transaction_id
                ) IS NOT NULL
                AND NEW.gateway_transaction_id =
                    public.billing_canonical_gateway_transaction_id(
                        NEW.gateway_transaction_id
                    )
            )
        )
    THEN
        RAISE EXCEPTION 'processor charge evidence is immutable';
    END IF;
    RETURN NEW;
END
$$;

DROP TRIGGER billing_processor_charge_evidence_immutable ON public.billing_processor_charges;
CREATE TRIGGER billing_processor_charge_evidence_immutable
BEFORE UPDATE OF
    gateway_approval_evidence,
    gateway_transaction_id,
    gateway_payment_method_reference,
    gateway_response,
    gateway_response_code,
    gateway_response_text,
    gateway_condition,
    payment_type,
    card_brand,
    card_last4,
    card_exp_month,
    card_exp_year
ON public.billing_processor_charges
FOR EACH ROW
EXECUTE FUNCTION public.billing_guard_processor_charge_evidence_update();

