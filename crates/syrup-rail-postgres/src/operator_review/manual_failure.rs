use super::*;
use syrup_rail::{BillingEvent, RenewalFailureDisposition, SubscriptionPaymentFailureDisposition};

/// Value-redacted failure returned by the host's manual-failure store.
#[derive(Debug)]
pub struct ManualAttemptFailureHostStoreError {
    source: RedactedHostErrorSource,
}

impl ManualAttemptFailureHostStoreError {
    /// Wraps a host error without exposing its value through ordinary error
    /// formatting or the standard error-source chain.
    pub fn new(source: impl Error + Send + Sync + 'static) -> Self {
        Self {
            source: RedactedHostErrorSource::new(source),
        }
    }

    /// Returns the host error for explicit application-level inspection.
    pub fn into_source(self) -> BoxError {
        self.source.into_inner()
    }
}

impl fmt::Display for ManualAttemptFailureHostStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("manual attempt failure host target transition failed")
    }
}

impl Error for ManualAttemptFailureHostStoreError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManualAttemptFailureHostTransitionOutcome {
    Changed,
    Unchanged,
}

#[async_trait]
pub trait ManualAttemptFailureHostStore: Send + Sync {
    async fn lock_payment_failure_target(
        &self,
        connection: &mut PgConnection,
        charge: ManualFailureHostCharge,
    ) -> Result<(), ManualAttemptFailureHostStoreError>;

    async fn mark_payment_failed(
        &self,
        connection: &mut PgConnection,
        charge: ManualFailureHostCharge,
    ) -> Result<ManualAttemptFailureHostTransitionOutcome, ManualAttemptFailureHostStoreError>;
}

pub async fn fail_review_required_attempt(
    pool: &PgPool,
    coordinator: &dyn BillingTransactionCoordinator,
    host: &dyn ManualAttemptFailureHostStore,
    attempt_id: PaymentAttemptId,
) -> Result<ManualAttemptFailureOutcome, OperatorReviewError> {
    fail_review_required(
        pool,
        coordinator,
        ManualFailureMode::Ordinary(host),
        attempt_id,
    )
    .await
}

/// Closes an operator-reviewed, submitted renewal and makes its next permitted
/// automatic retry due immediately, in the same transaction as its failure event.
///
/// The host must authorize this exact attempt and freshly reconcile its exact
/// provider identity to confirm that no processor transaction exists before
/// calling this function. This operation performs no provider query or charge.
/// It preserves ordinary dunning history, access policy, and exhaustion; only a
/// newly scheduled retry is accelerated. Canceled, unpaid, and already exhausted
/// subscriptions are not reopened. Hosts must retain their cancellation fences
/// when dispatching the resulting renewal through the ordinary renewal service.
///
/// Replays and concurrent calls cannot accelerate another attempt's retry. An
/// uncertain commit is recovered by rereading the attempt and normal due-renewal
/// discovery, never by submitting a provider charge directly.
pub async fn fail_review_required_renewal_for_retry(
    pool: &PgPool,
    coordinator: &dyn BillingTransactionCoordinator,
    attempt_id: PaymentAttemptId,
) -> Result<ManualAttemptFailureOutcome, OperatorReviewError> {
    fail_review_required(
        pool,
        coordinator,
        ManualFailureMode::RetryRenewal,
        attempt_id,
    )
    .await
}

#[derive(Clone, Copy)]
enum ManualFailureMode<'a> {
    Ordinary(&'a dyn ManualAttemptFailureHostStore),
    RetryRenewal,
}

impl ManualFailureMode<'_> {
    fn accepts(self, attempt: &PaymentAttempt) -> bool {
        review_required_attempt_can_be_manually_failed(attempt)
            && (matches!(self, Self::Ordinary(_))
                || (attempt.kind() == PaymentAttemptKind::SubscriptionRenewal
                    && attempt.state().timestamps().submitted_at().is_some()
                    && attempt.state().resolution_code().is_none()))
    }
}

async fn fail_review_required(
    pool: &PgPool,
    coordinator: &dyn BillingTransactionCoordinator,
    mode: ManualFailureMode<'_>,
    attempt_id: PaymentAttemptId,
) -> Result<ManualAttemptFailureOutcome, OperatorReviewError> {
    let Some(preloaded) = payment_attempt_by_id(pool, attempt_id).await? else {
        return Ok(ManualAttemptFailureOutcome::NotFound);
    };
    if !mode.accepts(&preloaded) {
        return Ok(ManualAttemptFailureOutcome::KeptOpen(preloaded));
    }

    let identity = preloaded.identity();
    let subject = BillingEventSubject::new(identity.billing_scope_id(), identity.subscriber_id());
    let mut transaction = coordinator
        .begin(subject, std::time::Duration::from_millis(250))
        .await?;
    let result = async {
        let connection = transaction.connection();
        set_enrollment_timeouts(connection).await?;
        let target = preloaded.request().target();
        if let Some(plan_key) = target.plan_key() {
            lock_subscription_aggregate(connection, identity.subscriber_id(), plan_key).await?;
        } else if let (Some(target_id), ManualFailureMode::Ordinary(host)) =
            (target.host_charge_target_id(), mode)
        {
            host.lock_payment_failure_target(
                connection,
                ManualFailureHostCharge::new(
                    identity.billing_scope_id(),
                    identity.subscriber_id(),
                    target_id,
                ),
            )
            .await?;
        }

        let Some(current) = lock_payment_attempt_by_id_on_connection(
            connection,
            identity.billing_scope_id(),
            attempt_id,
        )
        .await?
        else {
            return Ok((ManualAttemptFailureOutcome::NotFound, Vec::new()));
        };
        if current.identity() != preloaded.identity() || current.request() != preloaded.request() {
            return Err(OperatorReviewError::InvalidState(INVALID_OPERATOR_STATE));
        }
        if !mode.accepts(&current)
            || (current.kind() != PaymentAttemptKind::SubscriptionPaymentMethodUpdate
                && processor_charge_blocks_failure(connection, attempt_id, mode).await?)
        {
            return Ok((ManualAttemptFailureOutcome::KeptOpen(current), Vec::new()));
        }

        if matches!(mode, ManualFailureMode::RetryRenewal)
            && !lock_retryable_subscription(connection, &current).await?
        {
            return Ok((ManualAttemptFailureOutcome::KeptOpen(current), Vec::new()));
        }

        let evidence = review_required_manual_failure_evidence(&current);
        let updated = update_attempt_for_manual_failure(connection, &current, &evidence).await?;
        if updated != 1 {
            return Err(OperatorReviewError::InvalidState(INVALID_OPERATOR_STATE));
        }

        if let (Some(target_id), ManualFailureMode::Ordinary(host)) =
            (current.request().target().host_charge_target_id(), mode)
        {
            host.mark_payment_failed(
                connection,
                ManualFailureHostCharge::new(
                    identity.billing_scope_id(),
                    identity.subscriber_id(),
                    target_id,
                ),
            )
            .await?;
        }
        let attempt = lock_payment_attempt_by_id_on_connection(
            connection,
            identity.billing_scope_id(),
            attempt_id,
        )
        .await?
        .ok_or(OperatorReviewError::InvalidState(INVALID_OPERATOR_STATE))?;
        let events = if attempt.kind() == PaymentAttemptKind::SubscriptionRenewal
            && attempt.state().timestamps().submitted_at().is_some()
            && attempt.state().resolution_code().is_none()
        {
            match apply_resolved_automatic_renewal_failure(connection, &attempt).await? {
                RenewalFailureApplication::Applied {
                    disposition: RenewalFailureDisposition::RetryScheduled { retry_at },
                    mut events,
                } if matches!(mode, ManualFailureMode::RetryRenewal) => {
                    expedite_retry(connection, &attempt, retry_at, &mut events).await?;
                    events
                }
                RenewalFailureApplication::Applied { events, .. } => events,
                RenewalFailureApplication::Noop => Vec::new(),
            }
        } else {
            Vec::new()
        };
        Ok((ManualAttemptFailureOutcome::Failed(attempt), events))
    }
    .await;

    let (outcome, events) = match result {
        Ok(value) => value,
        Err(error) => {
            let _ = transaction.rollback().await;
            return Err(error);
        }
    };
    for event in &events {
        if let Err(error) = transaction.append_event(event).await {
            let _ = transaction.rollback().await;
            return Err(error.into());
        }
    }
    transaction.commit().await?;
    Ok(outcome)
}

async fn lock_retryable_subscription(
    connection: &mut PgConnection,
    attempt: &PaymentAttempt,
) -> Result<bool, OperatorReviewError> {
    let target = attempt.request().target();
    let subscription_id = target
        .subscription_id()
        .ok_or(OperatorReviewError::InvalidState(INVALID_OPERATOR_STATE))?;
    let period = target
        .period()
        .ok_or(OperatorReviewError::InvalidState(INVALID_OPERATOR_STATE))?;
    Ok(sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT id FROM billing_subscriptions
        WHERE id = $1 AND billing_scope_id = $2 AND subscriber_id = $3
            AND status IN ('active', 'past_due') AND unpaid_at IS NULL
            AND next_renewal_at = $4 AND next_renewal_at <= clock_timestamp()
            AND next_payment_attempt_at IS NOT NULL
        FOR NO KEY UPDATE
        "#,
    )
    .bind(subscription_id.as_uuid())
    .bind(attempt.identity().billing_scope_id().as_uuid())
    .bind(attempt.identity().subscriber_id().as_uuid())
    .bind(period.start_at())
    .fetch_optional(connection)
    .await?
    .is_some())
}

async fn expedite_retry(
    connection: &mut PgConnection,
    attempt: &PaymentAttempt,
    scheduled_at: DateTime<Utc>,
    events: &mut [BillingEvent],
) -> Result<(), OperatorReviewError> {
    let target = attempt.request().target();
    let subscription_id = target
        .subscription_id()
        .ok_or(OperatorReviewError::InvalidState(INVALID_OPERATOR_STATE))?;
    let period = target
        .period()
        .ok_or(OperatorReviewError::InvalidState(INVALID_OPERATOR_STATE))?;
    let retry_at = sqlx::query_scalar::<_, DateTime<Utc>>(
        r#"
        UPDATE billing_subscriptions
        SET next_payment_attempt_at = LEAST(next_payment_attempt_at, clock_timestamp()),
            updated_at = clock_timestamp()
        WHERE id = $1 AND billing_scope_id = $2 AND subscriber_id = $3
            AND status = 'past_due' AND unpaid_at IS NULL
            AND next_renewal_at = $4 AND next_payment_attempt_at = $5
        RETURNING next_payment_attempt_at
        "#,
    )
    .bind(subscription_id.as_uuid())
    .bind(attempt.identity().billing_scope_id().as_uuid())
    .bind(attempt.identity().subscriber_id().as_uuid())
    .bind(period.start_at())
    .bind(scheduled_at)
    .fetch_optional(connection)
    .await?
    .ok_or(OperatorReviewError::InvalidState(INVALID_OPERATOR_STATE))?;
    let [
        BillingEvent::SubscriptionPaymentFailed {
            attempt_id,
            disposition,
            ..
        },
    ] = events
    else {
        return Err(OperatorReviewError::InvalidState(INVALID_OPERATOR_STATE));
    };
    if *attempt_id != attempt.identity().attempt_id()
        || *disposition
            != (SubscriptionPaymentFailureDisposition::RetryScheduled {
                retry_at: scheduled_at,
            })
    {
        return Err(OperatorReviewError::InvalidState(INVALID_OPERATOR_STATE));
    }
    *disposition = SubscriptionPaymentFailureDisposition::RetryScheduled { retry_at };
    Ok(())
}

async fn payment_attempt_by_id(
    pool: &PgPool,
    attempt_id: PaymentAttemptId,
) -> Result<Option<PaymentAttempt>, OperatorReviewError> {
    let query = format!("{} WHERE id = $1", crate::attempts::PAYMENT_ATTEMPT_SELECT);
    let row = sqlx::query(&query)
        .bind(attempt_id.as_uuid())
        .fetch_optional(pool)
        .await?;
    row.as_ref()
        .map(payment_attempt_from_row)
        .transpose()
        .map_err(Into::into)
}

async fn processor_charge_blocks_failure(
    connection: &mut PgConnection,
    attempt_id: PaymentAttemptId,
    mode: ManualFailureMode<'_>,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM billing_processor_charges
            WHERE attempt_id = $1
                AND ($2 OR progression_state IN (
                    'pending', 'reconciliation_required', 'external_reversal_required'
                ))
        )
        "#,
    )
    .bind(attempt_id.as_uuid())
    .bind(matches!(mode, ManualFailureMode::RetryRenewal))
    .fetch_one(&mut *connection)
    .await
}

async fn update_attempt_for_manual_failure(
    connection: &mut PgConnection,
    attempt: &PaymentAttempt,
    evidence: &ProcessorEvidence,
) -> Result<u64, sqlx::Error> {
    let descriptor = evidence.descriptor();
    let result = sqlx::query(
        r#"
        UPDATE billing_payment_attempts
        SET status = 'failed',
            gateway_transaction_id = $2,
            gateway_payment_method_reference = $3,
            gateway_response = $4,
            gateway_response_code = $5,
            gateway_response_text = $6,
            gateway_condition = $7,
            payment_type = $8,
            card_brand = $9,
            card_last4 = $10,
            card_exp_month = $11,
            card_exp_year = $12,
            resolved_at = clock_timestamp(),
            updated_at = clock_timestamp()
        WHERE id = $1 AND status = 'review_required'
        "#,
    )
    .bind(attempt.identity().attempt_id().as_uuid())
    .bind(evidence.transaction_id().map(GatewayTransactionId::expose))
    .bind(
        evidence
            .payment_method_reference()
            .map(GatewayPaymentMethodReference::expose),
    )
    .bind(evidence.response().map(GatewayDiagnostic::expose))
    .bind(evidence.response_code().map(GatewayDiagnostic::expose))
    .bind(evidence.response_text().map(GatewayDiagnostic::expose))
    .bind(evidence.condition().map(GatewayDiagnostic::expose))
    .bind(descriptor.payment_type().map(GatewayDiagnostic::expose))
    .bind(descriptor.card_brand().map(GatewayDiagnostic::expose))
    .bind(
        descriptor
            .card_last_four()
            .map(syrup_rail::CardLastFour::expose),
    )
    .bind(descriptor.card_exp_month())
    .bind(descriptor.card_exp_year())
    .execute(&mut *connection)
    .await?;
    Ok(result.rows_affected())
}
