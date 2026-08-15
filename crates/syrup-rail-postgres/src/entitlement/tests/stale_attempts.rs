use syrup_rail::PastDueAction;

use super::*;

#[tokio::test]
async fn stale_local_recovery_does_not_request_provider_confirmation() -> Result<(), Box<dyn Error>>
{
    let database = TestDatabase::start("ent_stale").await?;
    let result = async {
        let (scope, subscriber, subscription) =
            insert_paid_subscription(&database.pool, "past_due").await?;
        let attempt_id = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO billing_payment_attempts (
                id, billing_scope_id, subscriber_id, plan_key, subscription_id,
                payment_method_id, attempt_kind, status, idempotency_key,
                request_fingerprint, amount_cents, currency,
                billing_period_start_at, billing_period_end_at,
                gateway_account_id, gateway_configuration_id, gateway_order_id,
                subscription_expected_payment_method_id,
                subscription_expected_initial_transaction_id,
                subscription_expected_status
            )
            SELECT
                $1, subscriptions.billing_scope_id, subscriptions.subscriber_id,
                subscriptions.plan_key, subscriptions.id,
                subscriptions.payment_method_id, 'subscription_recovery',
                'pending', $2, $3, subscriptions.amount_cents,
                subscriptions.currency, subscriptions.next_renewal_at,
                subscriptions.next_renewal_at + interval '1 month',
                subscriptions.gateway_account_id,
                accounts.gateway_configuration_id, $4,
                subscriptions.payment_method_id,
                subscriptions.initial_transaction_id, 'past_due'
            FROM billing_subscriptions AS subscriptions
            INNER JOIN billing_gateway_accounts AS accounts
                ON accounts.id = subscriptions.gateway_account_id
                AND accounts.billing_scope_id = subscriptions.billing_scope_id
            WHERE subscriptions.id = $5
            "#,
        )
        .bind(attempt_id)
        .bind(format!("recovery_{}", attempt_id.simple()))
        .bind(format!("recovery_fingerprint_{}", attempt_id.simple()))
        .bind(format!("recovery_order_{}", attempt_id.simple()))
        .bind(subscription)
        .execute(&database.pool)
        .await?;
        let query = EntitlementQuery::new(
            BillingScopeId::new(scope),
            SubscriberId::new(subscriber),
            PlanKey::new("base_subscription")?,
        );

        assert!(matches!(
            entitlement(&database.pool, &query).await?,
            Entitlement::PastDue {
                next_action: PastDueAction::ConfirmRecoveryPayment,
                ..
            }
        ));
        sqlx::query(
            r#"
            UPDATE billing_payment_attempts
            SET created_at = clock_timestamp() - interval '31 minutes',
                updated_at = clock_timestamp() - interval '31 minutes'
            WHERE id = $1
            "#,
        )
        .bind(attempt_id)
        .execute(&database.pool)
        .await?;
        assert!(matches!(
            entitlement(&database.pool, &query).await?,
            Entitlement::PastDue {
                next_action: PastDueAction::RecoverPayment,
                ..
            }
        ));
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}
