-- Shared identity and projection for both physical payment-history page shapes.
-- The exact-plan identity occupies $1-$3. A continuation inserts its complete
-- $4/$5 keyset after this fragment; the first page inserts no cursor predicate.
SELECT
    id,
    attempt_kind,
    status,
    amount_cents,
    currency,
    billing_period_start_at,
    billing_period_end_at,
    submitted_at,
    resolved_at,
    created_at
FROM billing_payment_attempts
WHERE billing_scope_id = $1
    AND subscriber_id = $2
    AND plan_key = $3
    AND attempt_kind <> 'host_charge'
