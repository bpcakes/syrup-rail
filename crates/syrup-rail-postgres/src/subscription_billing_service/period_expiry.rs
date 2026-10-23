use chrono::{DateTime, Utc};
use syrup_rail::{
    PlanKey, RetireExpiredSubscriptionPeriod, SubscriberId, SubscriptionPeriodRetirementOutcome,
};

use super::*;

const BILLING_LOCK_TIMEOUT: Duration = Duration::from_millis(250);

impl SubscriptionBillingService {
    /// Retires one exact obsolete due period through the host-prepared billing
    /// transaction.
    ///
    /// See [`crate::retire_expired_subscription_period_in_transaction`] for the
    /// exact rechecks. A `Retired` outcome appends its
    /// [`syrup_rail::BillingEvent::SubscriptionPeriodExpired`] on the same
    /// transaction before commit; every other outcome commits without an event.
    /// The operation is independent of the host's expiry policy, performs no
    /// end-user admission and no provider I/O, and is safe to replay.
    pub async fn retire_expired_period(
        &self,
        command: RetireExpiredSubscriptionPeriod,
    ) -> Result<SubscriptionPeriodRetirementOutcome, SubscriptionBillingServiceError> {
        let mut transaction = self
            .coordinator
            .begin(
                BillingEventSubject::new(command.billing_scope_id(), command.subscriber_id()),
                BILLING_LOCK_TIMEOUT,
            )
            .await?;
        let outcome = match crate::period_expiry::retire_expired_subscription_period_on_connection(
            transaction.connection(),
            &command,
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(error) => {
                let _ = transaction.rollback().await;
                return Err(error.into());
            }
        };
        if let SubscriptionPeriodRetirementOutcome::Retired { event, .. } = &outcome
            && let Err(error) = transaction.append_event(event).await
        {
            let _ = transaction.rollback().await;
            return Err(error.into());
        }
        transaction.commit().await?;
        Ok(outcome)
    }

    /// Reads, without billing locks, whether the exact due renewal period has
    /// ended at the database clock. Final admission repeats the check under
    /// locks; this probe only avoids gateway work for obsolete periods. A
    /// lifecycle bound to the other gateway account mode is never retired by
    /// this service: it returns `None` so ordinary renewal routing reports
    /// the mode mismatch.
    pub(super) async fn expired_renewal_period_owner(
        &self,
        command: ChargeRenewal,
    ) -> Result<Option<(SubscriberId, PlanKey)>, SubscriptionBillingServiceError> {
        let mut transaction = self.pool.begin().await?;
        let row = sqlx::query_as::<_, (uuid::Uuid, String, String, i32, DateTime<Utc>)>(
            r#"
            SELECT subscriber_id, plan_key, recurring_period_kind, recurring_period_count,
                clock_timestamp()
            FROM billing_subscriptions
            WHERE billing_scope_id = $1 AND id = $2
                AND status IN ('active', 'past_due')
                AND next_renewal_at = $3
                AND required_gateway_account_mode = $4
            "#,
        )
        .bind(command.billing_scope_id().as_uuid())
        .bind(command.subscription_id().as_uuid())
        .bind(command.period_start_at())
        .bind(self.required_gateway_account_mode.as_str())
        .fetch_optional(&mut *transaction)
        .await?;
        transaction.commit().await?;
        let Some((subscriber_id, plan_key, period_kind, period_count, now)) = row else {
            return Ok(None);
        };
        let rule = crate::subscription_persistence::subscription_period_rule_from_scalars(
            crate::subscription_persistence::SubscriptionPeriodRuleScalars::new(
                &period_kind,
                period_count,
            ),
        )
        .map_err(|_| SubscriptionBillingServiceError::InvalidState(INVALID_SERVICE_STATE))?;
        let period = syrup_rail::next_billing_period(*command.period_start_at(), rule)
            .map_err(|_| SubscriptionBillingServiceError::InvalidState(INVALID_SERVICE_STATE))?;
        if !syrup_rail::SubscriptionPeriodExpiryPolicy::period_has_expired(*period.end_at(), now) {
            return Ok(None);
        }
        let plan_key = PlanKey::new(plan_key)
            .map_err(|_| SubscriptionBillingServiceError::InvalidState(INVALID_SERVICE_STATE))?;
        Ok(Some((SubscriberId::new(subscriber_id), plan_key)))
    }

    /// Retires a renewal's due period after expiry stopped its collection.
    ///
    /// The retirement rechecks everything under locks; an unexpired, changed,
    /// or still-unresolved period is left untouched and the renewal remains a
    /// no-sale result.
    pub(super) async fn retire_after_expired_renewal(
        &self,
        command: ChargeRenewal,
        subscriber_id: SubscriberId,
        plan_key: PlanKey,
    ) -> Result<(), SubscriptionBillingServiceError> {
        self.retire_expired_period(RetireExpiredSubscriptionPeriod::new(
            command.billing_scope_id(),
            subscriber_id,
            plan_key,
            command.subscription_id(),
            *command.period_start_at(),
        ))
        .await
        .map(|_| ())
    }
}
