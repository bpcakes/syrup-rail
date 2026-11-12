use sqlx::PgConnection;
use syrup_rail::{
    BillingDeletionBlocker, BillingDeletionBlockers, DeletionBlockerQuery, PaymentAttemptKind,
};

use crate::attempts::LocalAttemptPolicy;

/// Reports the canonical financial rows that prevent host account deletion.
///
/// The query is subscriber-wide across plans and uses the caller's transaction.
/// Host-specific orders and fulfillment blockers remain the host's responsibility.
pub async fn billing_deletion_blockers(
    connection: &mut PgConnection,
    query: DeletionBlockerQuery,
) -> Result<BillingDeletionBlockers, sqlx::Error> {
    let billing_scope_id = query.billing_scope_id().into_uuid();
    let subscriber_id = query.subscriber_id().into_uuid();
    let payment_method_update_policy =
        LocalAttemptPolicy::for_kind(PaymentAttemptKind::SubscriptionPaymentMethodUpdate);
    let initial_policy = LocalAttemptPolicy::for_kind(PaymentAttemptKind::SubscriptionInitial);
    let subscription_charge_policy =
        LocalAttemptPolicy::for_kind(PaymentAttemptKind::SubscriptionRenewal);
    let host_charge_policy = LocalAttemptPolicy::for_kind(PaymentAttemptKind::HostCharge);
    let row = sqlx::query!(
        r#"
        SELECT
            EXISTS (
                SELECT 1
                FROM billing_subscriptions
                WHERE billing_scope_id = $1
                    AND subscriber_id = $2
                    AND status IN ('active', 'past_due')
            ) AS "active_subscription!",
            EXISTS (
                SELECT 1
                FROM billing_payment_attempts
                WHERE billing_scope_id = $1
                    AND subscriber_id = $2
                    AND status IN ('pending', 'unknown', 'review_required')
                    AND NOT (
                        submitted_at IS NULL
                        AND status = ANY($7::text[])
                        AND (
                            (
                                attempt_kind = 'subscription_payment_method_update'
                                AND created_at <= clock_timestamp()
                                    - ($3::bigint * interval '1 second')
                            )
                            OR (
                                attempt_kind = 'subscription_initial'
                                AND created_at <= clock_timestamp()
                                    - ($4::bigint * interval '1 second')
                            )
                            OR (
                                attempt_kind IN (
                                    'subscription_renewal',
                                    'subscription_recovery'
                                )
                                AND created_at <= clock_timestamp()
                                    - ($5::bigint * interval '1 second')
                            )
                            OR (
                                attempt_kind = 'host_charge'
                                AND created_at <= clock_timestamp()
                                    - ($6::bigint * interval '1 second')
                            )
                        )
                    )
            ) AS "unresolved_payment!"
        "#,
        billing_scope_id,
        subscriber_id,
        payment_method_update_policy.stale_after_seconds(),
        initial_policy.stale_after_seconds(),
        subscription_charge_policy.stale_after_seconds(),
        host_charge_policy.stale_after_seconds(),
        LocalAttemptPolicy::expirable_status_values(),
    )
    .fetch_one(connection)
    .await?;
    Ok(BillingDeletionBlockers::new(
        [
            row.active_subscription
                .then_some(BillingDeletionBlocker::ActiveSubscription),
            row.unresolved_payment
                .then_some(BillingDeletionBlocker::UnresolvedPayment),
        ]
        .into_iter()
        .flatten(),
    ))
}
