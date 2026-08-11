use super::*;

const BILLING_LOCK_TIMEOUT: Duration = Duration::from_millis(250);

impl SubscriptionBillingService {
    /// Cancels one exact subscriber-owned subscription lifecycle.
    ///
    /// Admission runs before any database work. When cancellation changes
    /// canonical state, its event is appended on the same host-prepared
    /// transaction before that transaction commits. Replays and semantic
    /// blockers commit without an event. This operation performs no provider
    /// resolution or provider I/O.
    pub async fn cancel(
        &self,
        command: CancelSubscription,
    ) -> Result<CancelSubscriptionOutcome, SubscriptionBillingServiceError> {
        self.admit_subscriber_mutation(
            command.billing_scope_id(),
            command.subscriber_id(),
            EndUserMutationOperation::SubscriptionCancel,
        )
        .await?;

        let mut transaction = self
            .coordinator
            .begin(
                BillingEventSubject::new(command.billing_scope_id(), command.subscriber_id()),
                BILLING_LOCK_TIMEOUT,
            )
            .await?;
        let outcome = match crate::cancellation::cancel_subscription_on_connection(
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
        if let CancelSubscriptionOutcome::Canceled { event, .. } = &outcome
            && let Err(error) = transaction.append_event(event).await
        {
            let _ = transaction.rollback().await;
            return Err(error.into());
        }
        transaction.commit().await?;
        Ok(outcome)
    }

    /// Claims an eligible discount code for one exact subscriber aggregate.
    ///
    /// Admission happens before database work and the offer lock, claim, and
    /// commit use one local transaction. The operation does not resolve a
    /// gateway or perform provider I/O.
    pub async fn claim_discount(
        &self,
        command: SubscriptionDiscountClaim,
    ) -> Result<SubscriptionDiscountClaimOutcome, SubscriptionBillingServiceError> {
        self.admit_subscriber_mutation(
            command.billing_scope_id(),
            command.subscriber_id(),
            EndUserMutationOperation::SubscriptionDiscountClaim,
        )
        .await?;

        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(provider_free_transaction_error)?;
        let outcome = match crate::discounts::claim_subscription_discount_on_connection(
            &mut transaction,
            self.offers.as_ref(),
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
        transaction
            .commit()
            .await
            .map_err(provider_free_transaction_error)?;
        Ok(outcome)
    }

    /// Clears the saved discount claim for one exact subscriber aggregate.
    ///
    /// Admission happens before database work. The command has no provider
    /// identity and this operation performs neither gateway resolution nor
    /// provider I/O.
    pub async fn clear_discount(
        &self,
        command: ClearSubscriptionDiscount,
    ) -> Result<SubscriptionDiscountClearOutcome, SubscriptionBillingServiceError> {
        self.admit_subscriber_mutation(
            command.billing_scope_id(),
            command.subscriber_id(),
            EndUserMutationOperation::SubscriptionDiscountClear,
        )
        .await?;

        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(provider_free_transaction_error)?;
        let outcome = match crate::discounts::clear_subscription_discount_on_connection(
            &mut transaction,
            command.billing_scope_id(),
            command.subscriber_id(),
            command.plan_key(),
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(error) => {
                let _ = transaction.rollback().await;
                return Err(error.into());
            }
        };
        transaction
            .commit()
            .await
            .map_err(provider_free_transaction_error)?;
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests;
