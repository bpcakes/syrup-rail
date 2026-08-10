use super::*;

#[derive(Debug)]
pub struct ManualAttemptFailureHostStoreError {
    source: BoxError,
}

impl ManualAttemptFailureHostStoreError {
    pub fn new(source: impl Error + Send + Sync + 'static) -> Self {
        Self {
            source: Box::new(source),
        }
    }

    pub fn into_source(self) -> BoxError {
        self.source
    }
}

impl fmt::Display for ManualAttemptFailureHostStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("manual attempt failure host target transition failed")
    }
}

impl Error for ManualAttemptFailureHostStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.source.as_ref())
    }
}

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
    let Some(preloaded) = payment_attempt_by_id(pool, attempt_id).await? else {
        return Ok(ManualAttemptFailureOutcome::NotFound);
    };
    if !review_required_attempt_can_be_manually_failed(&preloaded) {
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
        } else if let Some(target_id) = target.host_charge_target_id() {
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
        if !review_required_attempt_can_be_manually_failed(&current)
            || (current.kind() != PaymentAttemptKind::SubscriptionPaymentMethodUpdate
                && unresolved_processor_charge_exists(connection, attempt_id).await?)
        {
            return Ok((ManualAttemptFailureOutcome::KeptOpen(current), Vec::new()));
        }

        let evidence = review_required_manual_failure_evidence(&current);
        let updated = update_attempt_for_manual_failure(connection, &current, &evidence).await?;
        if updated != 1 {
            return Err(OperatorReviewError::InvalidState(INVALID_OPERATOR_STATE));
        }

        if let Some(target_id) = current.request().target().host_charge_target_id() {
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

async fn unresolved_processor_charge_exists(
    connection: &mut PgConnection,
    attempt_id: PaymentAttemptId,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM billing_processor_charges
            WHERE attempt_id = $1
                AND progression_state IN (
                    'pending', 'reconciliation_required', 'external_reversal_required'
                )
        )
        "#,
    )
    .bind(attempt_id.as_uuid())
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
