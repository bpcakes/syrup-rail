-- Read-only audit of schema-v4 review-required and terminal host attempts that remain
-- unclassified after the v5 cutover.
--
-- The result exposes only canonical internal identifiers, lifecycle facts,
-- and evidence-presence flags. Keep access and exports inside the host's
-- authorized operator process; raw provider evidence is deliberately omitted.

SELECT
    id AS attempt_id,
    billing_scope_id,
    subscriber_id,
    attempt_kind,
    host_charge_target_id,
    status,
    resolution_code,
    created_at,
    submitted_at IS NOT NULL AS was_submitted,
    gateway_transaction_id IS NOT NULL AS has_transaction_id,
    gateway_payment_method_reference IS NOT NULL AS has_payment_method_reference,
    gateway_response IS NOT NULL AS has_response,
    gateway_response_code IS NOT NULL AS has_response_code,
    gateway_condition IS NOT NULL AS has_condition,
    gateway_response_text IS NOT NULL AS has_response_text
FROM public.billing_payment_attempts
WHERE (
        status = 'review_required'
        OR (
            attempt_kind = 'host_charge'
            AND (status = 'declined' OR (status = 'failed' AND submitted_at IS NULL))
            AND gateway_lifecycle_status = 'unknown'
            AND refunded_amount_cents = 0
            AND NOT EXISTS (
                SELECT 1 FROM public.billing_processor_charges AS charges
                WHERE charges.attempt_id = billing_payment_attempts.id
            )
        )
    )
    AND NOT (
        gateway_transaction_id IS NULL
        AND gateway_payment_method_reference IS NULL
        AND gateway_response IS NULL
        AND gateway_response_code IS NULL
        AND gateway_condition IS NULL
        AND gateway_response_text IS NULL
    )
ORDER BY created_at, id;
