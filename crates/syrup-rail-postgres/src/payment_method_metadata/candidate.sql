SELECT g.id AS account_id, g.gateway_configuration_id AS configuration_id,
       g.provider_key, a.gateway_transaction_id AS transaction_id,
       m.gateway_payment_method_reference AS reference, m.id AS method_id,
       s.id AS subscription_id, s.plan_key,
       m.updated_at AS method_updated_at, s.updated_at AS subscription_updated_at,
       a.updated_at AS attempt_updated_at,
       m.card_brand, m.card_last4, m.card_exp_month, m.card_exp_year
FROM billing_payment_attempts AS a
JOIN billing_gateway_accounts AS g
  ON g.id = a.gateway_account_id AND g.billing_scope_id = a.billing_scope_id
JOIN billing_payment_methods AS m
  ON m.id = a.payment_method_id AND m.billing_scope_id = a.billing_scope_id
 AND m.subscriber_id = a.subscriber_id AND m.gateway_account_id = a.gateway_account_id
 AND (m.gateway_payment_method_reference = a.gateway_payment_method_reference
      OR (a.attempt_kind = 'subscription_renewal'
          AND a.gateway_payment_method_reference IS NULL))
JOIN billing_subscriptions AS s
  ON s.id = a.subscription_id AND s.billing_scope_id = a.billing_scope_id
 AND s.subscriber_id = a.subscriber_id AND s.gateway_account_id = a.gateway_account_id
 AND s.plan_key = a.plan_key
WHERE a.billing_scope_id = $1 AND a.subscriber_id = $2 AND a.id = $3
  AND a.status = 'approved' AND a.gateway_transaction_id IS NOT NULL
  AND m.status = 'active'
  AND m.gateway_payment_method_reference <> 'erased:' || m.id::text
  AND EXISTS (
      SELECT 1 FROM billing_subscriptions AS current_subscription
      WHERE current_subscription.billing_scope_id = a.billing_scope_id
        AND current_subscription.subscriber_id = a.subscriber_id
        AND current_subscription.gateway_account_id = a.gateway_account_id
        AND current_subscription.payment_method_id = m.id
  )
  AND NOT EXISTS (
      SELECT 1 FROM billing_payment_attempts AS newer
      WHERE newer.payment_method_id = m.id AND newer.status = 'approved'
        AND (newer.resolved_at, newer.id) > (a.resolved_at, a.id)
  )
