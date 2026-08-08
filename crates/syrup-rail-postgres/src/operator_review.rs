use std::{error::Error, fmt};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool, Postgres, Row, Transaction, postgres::PgRow};
use syrup_rail::{
    ActorId, AttemptReviewCursor, AttemptReviewPage, BillingEvent, BillingEventSubject,
    BillingScopeId, ChargeAmount, CurrencyCode, ExternalReversalAttestation,
    ExternalReversalHostChargeRelease, ExternalReversalKind, ExternalReversalReason,
    GatewayAccountId, GatewayConfigurationId, GatewayDiagnostic, GatewayOrderId,
    GatewayPaymentDescriptor, GatewayPaymentMethodReference, GatewayTransactionId,
    HostChargeTargetId, ManualAttemptFailureOutcome, ManualFailureHostCharge,
    OperatorReviewPageLimit, PaymentAttempt, PaymentAttemptId, PaymentAttemptKind,
    PaymentAttemptStatus, PaymentResolutionCode, PlanKey, ProcessorCharge, ProcessorChargeId,
    ProcessorChargeProgression, ProcessorChargeReviewCursor, ProcessorChargeReviewItem,
    ProcessorChargeReviewPage, ProcessorChargeRole, ProcessorChargeStateCode, ProcessorEvidence,
    SubscriberId, review_required_attempt_can_be_manually_failed,
    review_required_manual_failure_evidence,
};
use thiserror::Error;
use uuid::Uuid;

use crate::attempts::{
    lock_payment_attempt_by_id_on_connection, lock_subscription_aggregate,
    payment_attempt_from_row, processor_evidence_from_row, set_enrollment_timeouts,
};
use crate::transactions::{
    BillingEventWriteError, BillingTransactionCoordinator, BillingTransactionError,
};

const INVALID_OPERATOR_STATE: &str = "canonical operator review state is invalid";
type BoxError = Box<dyn Error + Send + Sync + 'static>;

pub async fn attempt_review_page(
    pool: &PgPool,
    limit: OperatorReviewPageLimit,
    cursor: Option<AttemptReviewCursor>,
) -> Result<AttemptReviewPage, OperatorReviewError> {
    let query = format!(
        r#"
        {}
        WHERE status = 'review_required'
            AND review_required_at IS NOT NULL
            AND NOT EXISTS (
                SELECT 1 FROM billing_processor_charges charges
                WHERE charges.attempt_id = billing_payment_attempts.id
                    AND charges.progression_state = 'external_reversal_required'
            )
            AND (
                $1::timestamptz IS NULL
                OR (review_required_at, id) > ($1::timestamptz, $2::uuid)
            )
        ORDER BY review_required_at, id
        LIMIT $3
        "#,
        crate::attempts::PAYMENT_ATTEMPT_SELECT
    );
    let rows = sqlx::query(&query)
        .bind(cursor.map(AttemptReviewCursor::reviewed_at))
        .bind(cursor.map(|value| value.attempt_id().into_uuid()))
        .bind(limit.get() + 1)
        .fetch_all(pool)
        .await?;
    let mut items = rows
        .iter()
        .map(payment_attempt_from_row)
        .collect::<Result<Vec<_>, _>>()
        .map_err(OperatorReviewError::from)?;
    let has_more = items.len() > limit.get() as usize;
    if has_more {
        items.pop();
    }
    let next_cursor = if has_more {
        items.last().map(|attempt| {
            AttemptReviewCursor::new(
                attempt
                    .state()
                    .timestamps()
                    .review_required_at()
                    .expect("review page query requires review timestamp"),
                attempt.identity().attempt_id(),
            )
        })
    } else {
        None
    };
    Ok(AttemptReviewPage::new(items, next_cursor))
}

pub async fn processor_charge_review_page(
    pool: &PgPool,
    limit: OperatorReviewPageLimit,
    cursor: Option<ProcessorChargeReviewCursor>,
) -> Result<ProcessorChargeReviewPage, OperatorReviewError> {
    let query = format!(
        r#"
        SELECT attempts.*,
            charges.id AS review_charge_id,
            charges.attempt_id AS review_charge_attempt_id,
            charges.billing_scope_id AS review_charge_billing_scope_id,
            charges.gateway_account_id AS review_charge_gateway_account_id,
            charges.gateway_order_id AS review_charge_gateway_order_id,
            charges.attempt_kind AS review_charge_attempt_kind,
            charges.amount_cents AS review_charge_amount_cents,
            charges.currency AS review_charge_currency,
            charges.charge_role AS review_charge_role,
            charges.progression_state AS review_charge_progression_state,
            charges.state_code AS review_charge_state_code,
            charges.gateway_transaction_id AS review_charge_gateway_transaction_id,
            charges.gateway_payment_method_reference AS review_charge_gateway_payment_method_reference,
            charges.gateway_response AS review_charge_gateway_response,
            charges.gateway_response_code AS review_charge_gateway_response_code,
            charges.gateway_response_text AS review_charge_gateway_response_text,
            charges.gateway_condition AS review_charge_gateway_condition,
            charges.payment_type AS review_charge_payment_type,
            charges.card_brand AS review_charge_card_brand,
            charges.card_last4 AS review_charge_card_last4,
            charges.card_exp_month AS review_charge_card_exp_month,
            charges.card_exp_year AS review_charge_card_exp_year,
            charges.observed_at AS review_charge_observed_at,
            charges.external_reversal_required_at AS review_charge_required_at
        FROM billing_processor_charges charges
        INNER JOIN ({}) attempts ON attempts.id = charges.attempt_id
        WHERE charges.progression_state = 'external_reversal_required'
            AND charges.external_reversal_required_at IS NOT NULL
            AND charges.amount_cents > 0
            AND public.billing_canonical_gateway_transaction_id(
                charges.gateway_transaction_id
            ) IS NOT NULL
            AND charges.attempt_kind IN (
                'host_charge', 'subscription_initial',
                'subscription_renewal', 'subscription_recovery'
            )
            AND (
                $1::timestamptz IS NULL
                OR (charges.external_reversal_required_at, charges.id)
                    > ($1::timestamptz, $2::uuid)
            )
        ORDER BY charges.external_reversal_required_at, charges.id
        LIMIT $3
        "#,
        crate::attempts::PAYMENT_ATTEMPT_SELECT
    );
    let rows = sqlx::query(&query)
        .bind(cursor.map(ProcessorChargeReviewCursor::reviewed_at))
        .bind(cursor.map(|value| value.processor_charge_id().into_uuid()))
        .bind(limit.get() + 1)
        .fetch_all(pool)
        .await?;
    let mut items = rows
        .iter()
        .map(|row| {
            Ok(ProcessorChargeReviewItem::new(
                payment_attempt_from_row(row).map_err(OperatorReviewError::from)?,
                processor_charge_from_review_row(row)?,
                row.try_get("review_charge_required_at")?,
            ))
        })
        .collect::<Result<Vec<_>, OperatorReviewError>>()?;
    let has_more = items.len() > limit.get() as usize;
    if has_more {
        items.pop();
    }
    let next_cursor = if has_more {
        items.last().map(|item| {
            ProcessorChargeReviewCursor::new(
                item.external_reversal_required_at(),
                item.charge().id(),
            )
        })
    } else {
        None
    };
    Ok(ProcessorChargeReviewPage::new(items, next_cursor))
}

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

#[derive(Debug)]
pub struct ExternalReversalHostStoreError {
    source: BoxError,
}

impl ExternalReversalHostStoreError {
    pub fn new(source: impl Error + Send + Sync + 'static) -> Self {
        Self {
            source: Box::new(source),
        }
    }
    pub fn into_source(self) -> BoxError {
        self.source
    }
}

impl fmt::Display for ExternalReversalHostStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("external reversal host target transition failed")
    }
}

impl Error for ExternalReversalHostStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.source.as_ref())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExternalReversalHostTransitionOutcome {
    Changed,
    Unchanged,
}

#[async_trait]
pub trait ExternalReversalHostStore: Send + Sync {
    async fn release(
        &self,
        connection: &mut PgConnection,
        release: ExternalReversalHostChargeRelease,
    ) -> Result<ExternalReversalHostTransitionOutcome, ExternalReversalHostStoreError>;
}

#[derive(Debug, Error)]
pub enum OperatorReviewError {
    #[error("operator review storage operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("{0}")]
    InvalidState(&'static str),
    #[error(transparent)]
    Host(#[from] ExternalReversalHostStoreError),
    #[error(transparent)]
    ManualFailureHost(#[from] ManualAttemptFailureHostStoreError),
    #[error(transparent)]
    BillingTransaction(#[from] BillingTransactionError),
    #[error(transparent)]
    BillingEvent(#[from] BillingEventWriteError),
}

impl From<crate::PaymentAttemptStoreError> for OperatorReviewError {
    fn from(error: crate::PaymentAttemptStoreError) -> Self {
        match error {
            crate::PaymentAttemptStoreError::Sql(error) => Self::Sql(error),
            crate::PaymentAttemptStoreError::InvalidState(_) => {
                Self::InvalidState(INVALID_OPERATOR_STATE)
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExternalReversalAttestationOutcome {
    Attested {
        attempt: PaymentAttempt,
        attestation: ExternalReversalAttestation,
    },
    Replayed {
        attempt: PaymentAttempt,
        attestation: ExternalReversalAttestation,
    },
    NotFound,
    Ineligible,
    ReplayConflict,
}

#[derive(Clone, Debug)]
struct ChargeLocator {
    charge_id: ProcessorChargeId,
    attempt_id: PaymentAttemptId,
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    kind: PaymentAttemptKind,
    plan_key: Option<PlanKey>,
    host_target_id: Option<HostChargeTargetId>,
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
            return Ok((ManualAttemptFailureOutcome::NotFound, None));
        };
        if current.identity() != preloaded.identity() || current.request() != preloaded.request() {
            return Err(OperatorReviewError::InvalidState(INVALID_OPERATOR_STATE));
        }
        if !review_required_attempt_can_be_manually_failed(&current)
            || (current.kind() != PaymentAttemptKind::SubscriptionPaymentMethodUpdate
                && unresolved_processor_charge_exists(connection, attempt_id).await?)
        {
            return Ok((ManualAttemptFailureOutcome::KeptOpen(current), None));
        }

        let evidence = review_required_manual_failure_evidence(&current);
        let updated = update_attempt_for_manual_failure(connection, &current, &evidence).await?;
        if updated != 1 {
            return Err(OperatorReviewError::InvalidState(INVALID_OPERATOR_STATE));
        }

        let event = if matches!(
            current.kind(),
            PaymentAttemptKind::SubscriptionRenewal | PaymentAttemptKind::SubscriptionRecovery
        ) {
            mark_subscription_past_due_for_manual_failure(connection, &current).await?
        } else {
            None
        };
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
        Ok((ManualAttemptFailureOutcome::Failed(attempt), event))
    }
    .await;

    let (outcome, event) = match result {
        Ok(value) => value,
        Err(error) => {
            let _ = transaction.rollback().await;
            return Err(error);
        }
    };
    if let Some(event) = event.as_ref()
        && let Err(error) = transaction.append_event(event).await
    {
        let _ = transaction.rollback().await;
        return Err(error.into());
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

async fn mark_subscription_past_due_for_manual_failure(
    connection: &mut PgConnection,
    attempt: &PaymentAttempt,
) -> Result<Option<BillingEvent>, OperatorReviewError> {
    let identity = attempt.identity();
    let target = attempt.request().target();
    let subscription_id = target
        .subscription_id()
        .ok_or(OperatorReviewError::InvalidState(INVALID_OPERATOR_STATE))?;
    let plan_key = target
        .plan_key()
        .ok_or(OperatorReviewError::InvalidState(INVALID_OPERATOR_STATE))?;
    let retry_at = sqlx::query_scalar::<_, DateTime<Utc>>(
        r#"
        UPDATE billing_subscriptions
        SET status = 'past_due', updated_at = clock_timestamp()
        WHERE id = $1
            AND billing_scope_id = $2
            AND subscriber_id = $3
            AND plan_key = $4
            AND status = 'active'
        RETURNING next_renewal_at
        "#,
    )
    .bind(subscription_id.as_uuid())
    .bind(identity.billing_scope_id().as_uuid())
    .bind(identity.subscriber_id().as_uuid())
    .bind(plan_key.as_str())
    .fetch_optional(&mut *connection)
    .await?;
    Ok(
        retry_at.map(|retry_at| BillingEvent::SubscriptionPaymentFailed {
            attempt_id: identity.attempt_id(),
            subscription_id,
            plan_key: plan_key.clone(),
            retry_at,
        }),
    )
}

pub async fn attest_external_reversal(
    pool: &PgPool,
    host: &dyn ExternalReversalHostStore,
    processor_charge_id: ProcessorChargeId,
    actor_id: ActorId,
    kind: ExternalReversalKind,
    expected_transaction_id: &GatewayTransactionId,
    reason: &ExternalReversalReason,
) -> Result<ExternalReversalAttestationOutcome, OperatorReviewError> {
    let mut transaction = pool.begin().await?;
    set_enrollment_timeouts(&mut transaction).await?;
    let Some(locator) = charge_locator(&mut transaction, processor_charge_id).await? else {
        transaction.commit().await?;
        return Ok(ExternalReversalAttestationOutcome::NotFound);
    };
    if let Some(plan_key) = &locator.plan_key {
        lock_subscription_aggregate(&mut transaction, locator.subscriber_id, plan_key).await?;
    }
    let Some(attempt) = lock_payment_attempt_by_id_on_connection(
        &mut transaction,
        locator.billing_scope_id,
        locator.attempt_id,
    )
    .await
    .map_err(|error| match error {
        crate::PaymentAttemptStoreError::Sql(error) => OperatorReviewError::Sql(error),
        crate::PaymentAttemptStoreError::InvalidState(_) => {
            OperatorReviewError::InvalidState("locked operator review attempt is invalid")
        }
    })?
    else {
        transaction.commit().await?;
        return Ok(ExternalReversalAttestationOutcome::NotFound);
    };
    let Some(charge) = lock_processor_charge(&mut transaction, processor_charge_id).await? else {
        return Err(OperatorReviewError::InvalidState(
            "operator review processor charge disappeared while locked",
        ));
    };
    if !locator_matches(&locator, &attempt, &charge) {
        return Err(OperatorReviewError::InvalidState(
            "operator review locator changed while locking",
        ));
    }

    if let Some(existing) =
        attestation_by_charge(&mut transaction, processor_charge_id.into_uuid()).await?
    {
        let matches = existing.actor_id() == actor_id
            && existing.kind() == kind
            && existing.reason() == reason
            && existing.gateway_transaction_id() == expected_transaction_id
            && charge.progression() == ProcessorChargeProgression::ExternallyReversed
            && attestation_matches_source(&existing, &attempt, &charge);
        if matches && can_release_host_target(&attempt, &charge) {
            release_host_target(host, &mut transaction, &attempt).await?;
        }
        transaction.commit().await?;
        return Ok(if matches {
            ExternalReversalAttestationOutcome::Replayed {
                attempt,
                attestation: existing,
            }
        } else {
            ExternalReversalAttestationOutcome::ReplayConflict
        });
    }

    if !processor_charge_can_attest_external_reversal(&charge)
        || charge.evidence().transaction_id() != Some(expected_transaction_id)
    {
        transaction.commit().await?;
        return Ok(ExternalReversalAttestationOutcome::Ineligible);
    }

    let prior = expected_prior_resolution_code(&attempt, &charge).to_owned();
    let final_code = expected_final_resolution_code(attempt.kind(), kind);
    let attested_at: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *transaction)
        .await?;
    insert_attestation(
        &mut transaction,
        &attempt,
        &charge,
        actor_id,
        kind,
        reason,
        &prior,
        final_code,
        attested_at,
    )
    .await?;
    let updated = sqlx::query(
        r#"
        UPDATE billing_processor_charges
        SET progression_state = 'externally_reversed',
            state_code = $2,
            externally_reversed_at = COALESCE(externally_reversed_at, clock_timestamp()),
            updated_at = clock_timestamp()
        WHERE id = $1 AND progression_state = 'external_reversal_required'
        "#,
    )
    .bind(processor_charge_id.as_uuid())
    .bind(&prior)
    .execute(&mut *transaction)
    .await?;
    if updated.rows_affected() != 1 {
        return Err(OperatorReviewError::InvalidState(
            "eligible processor charge did not accept external reversal",
        ));
    }
    if charge.role() == ProcessorChargeRole::Primary && !attempt.status().is_terminal() {
        let updated = sqlx::query(
            r#"
            UPDATE billing_payment_attempts
            SET status = 'failed', resolution_code = $2,
                resolved_at = $3, updated_at = $3
            WHERE id = $1 AND status = $4
            "#,
        )
        .bind(attempt.identity().attempt_id().as_uuid())
        .bind(final_code.as_str())
        .bind(attested_at)
        .bind(attempt.status().as_str())
        .execute(&mut *transaction)
        .await?;
        if updated.rows_affected() != 1 {
            return Err(OperatorReviewError::InvalidState(
                "eligible attempt did not accept external reversal",
            ));
        }
    }
    if can_release_host_target(&attempt, &charge) {
        release_host_target(host, &mut transaction, &attempt).await?;
    }
    let attempt = load_attempt(
        &mut transaction,
        locator.billing_scope_id,
        locator.attempt_id,
    )
    .await?;
    let attestation = attestation_by_charge(&mut transaction, processor_charge_id.into_uuid())
        .await?
        .ok_or(OperatorReviewError::InvalidState(
            "external reversal attestation disappeared while locked",
        ))?;
    transaction.commit().await?;
    Ok(ExternalReversalAttestationOutcome::Attested {
        attempt,
        attestation,
    })
}

async fn charge_locator(
    transaction: &mut Transaction<'_, Postgres>,
    charge_id: ProcessorChargeId,
) -> Result<Option<ChargeLocator>, OperatorReviewError> {
    let row = sqlx::query(
        r#"
        SELECT charges.id, charges.attempt_id,
            attempts.billing_scope_id, attempts.subscriber_id,
            attempts.attempt_kind, attempts.plan_key, attempts.host_charge_target_id
        FROM billing_processor_charges charges
        INNER JOIN billing_payment_attempts attempts ON attempts.id = charges.attempt_id
        WHERE charges.id = $1
        "#,
    )
    .bind(charge_id.as_uuid())
    .fetch_optional(&mut **transaction)
    .await?;
    row.map(|row| {
        Ok(ChargeLocator {
            charge_id: ProcessorChargeId::new(row.try_get("id")?),
            attempt_id: PaymentAttemptId::new(row.try_get("attempt_id")?),
            billing_scope_id: BillingScopeId::new(row.try_get("billing_scope_id")?),
            subscriber_id: SubscriberId::new(row.try_get("subscriber_id")?),
            kind: row
                .try_get::<String, _>("attempt_kind")?
                .parse()
                .map_err(|_| {
                    OperatorReviewError::InvalidState("operator locator attempt kind is invalid")
                })?,
            plan_key: row
                .try_get::<Option<String>, _>("plan_key")?
                .map(PlanKey::new)
                .transpose()
                .map_err(|_| {
                    OperatorReviewError::InvalidState("operator locator plan key is invalid")
                })?,
            host_target_id: row
                .try_get::<Option<Uuid>, _>("host_charge_target_id")?
                .map(HostChargeTargetId::new),
        })
    })
    .transpose()
}

async fn lock_processor_charge(
    transaction: &mut Transaction<'_, Postgres>,
    charge_id: ProcessorChargeId,
) -> Result<Option<ProcessorCharge>, OperatorReviewError> {
    let row = sqlx::query(
        r#"
        SELECT id, attempt_id, billing_scope_id, gateway_account_id,
            gateway_order_id, attempt_kind, amount_cents, currency,
            charge_role, progression_state, state_code,
            gateway_transaction_id, gateway_payment_method_reference,
            gateway_response, gateway_response_code, gateway_response_text,
            gateway_condition, payment_type, card_brand, card_last4,
            card_exp_month, card_exp_year, observed_at
        FROM billing_processor_charges WHERE id = $1 FOR UPDATE
        "#,
    )
    .bind(charge_id.as_uuid())
    .fetch_optional(&mut **transaction)
    .await?;
    row.as_ref().map(processor_charge_from_row).transpose()
}

pub(crate) fn processor_charge_from_row(
    row: &PgRow,
) -> Result<ProcessorCharge, OperatorReviewError> {
    let attempt_id = PaymentAttemptId::new(row.try_get("attempt_id")?);
    let currency = CurrencyCode::new(&row.try_get::<String, _>("currency")?)
        .map_err(|_| OperatorReviewError::InvalidState(INVALID_OPERATOR_STATE))?;
    let amount = ChargeAmount::new(row.try_get("amount_cents")?, currency)
        .map_err(|_| OperatorReviewError::InvalidState(INVALID_OPERATOR_STATE))?;
    let order = row.try_get::<String, _>("gateway_order_id")?;
    let gateway_order_id = GatewayOrderId::from_generated_attempt(&order, attempt_id)
        .or_else(|_| GatewayOrderId::from_correlation(&order))
        .map_err(|_| OperatorReviewError::InvalidState(INVALID_OPERATOR_STATE))?;
    Ok(ProcessorCharge::new(
        ProcessorChargeId::new(row.try_get("id")?),
        attempt_id,
        BillingScopeId::new(row.try_get("billing_scope_id")?),
        GatewayAccountId::new(row.try_get("gateway_account_id")?),
        gateway_order_id,
        parse_kind(&row.try_get::<String, _>("attempt_kind")?)?,
        amount,
        parse_role(&row.try_get::<String, _>("charge_role")?)?,
        parse_progression(&row.try_get::<String, _>("progression_state")?)?,
        row.try_get::<Option<String>, _>("state_code")?
            .as_deref()
            .map(parse_charge_state_code)
            .transpose()?,
        processor_evidence_from_row(row).map_err(|_| {
            OperatorReviewError::InvalidState("operator charge evidence is invalid")
        })?,
        row.try_get("observed_at")?,
    ))
}

fn processor_charge_from_review_row(row: &PgRow) -> Result<ProcessorCharge, OperatorReviewError> {
    let attempt_id = PaymentAttemptId::new(row.try_get("review_charge_attempt_id")?);
    let currency = CurrencyCode::new(&row.try_get::<String, _>("review_charge_currency")?)
        .map_err(|_| OperatorReviewError::InvalidState(INVALID_OPERATOR_STATE))?;
    let amount = ChargeAmount::new(row.try_get("review_charge_amount_cents")?, currency)
        .map_err(|_| OperatorReviewError::InvalidState(INVALID_OPERATOR_STATE))?;
    let order = row.try_get::<String, _>("review_charge_gateway_order_id")?;
    let gateway_order_id = GatewayOrderId::from_generated_attempt(&order, attempt_id)
        .or_else(|_| GatewayOrderId::from_correlation(&order))
        .map_err(|_| OperatorReviewError::InvalidState(INVALID_OPERATOR_STATE))?;
    let card_last_four = row.try_get::<Option<String>, _>("review_charge_card_last4")?;
    let card_exp_month = row.try_get::<Option<i16>, _>("review_charge_card_exp_month")?;
    let card_exp_year = row.try_get::<Option<i16>, _>("review_charge_card_exp_year")?;
    let descriptor = GatewayPaymentDescriptor::from_provider_parts(
        review_diagnostic(row, "review_charge_payment_type")?,
        review_diagnostic(row, "review_charge_card_brand")?,
        card_last_four.as_deref(),
        card_exp_month,
        card_exp_year,
    );
    if descriptor.card_last_four().is_some() != card_last_four.is_some()
        || descriptor.card_exp_month() != card_exp_month
        || descriptor.card_exp_year() != card_exp_year
    {
        return Err(OperatorReviewError::InvalidState(INVALID_OPERATOR_STATE));
    }
    let evidence = ProcessorEvidence::new(
        row.try_get::<Option<String>, _>("review_charge_gateway_transaction_id")?
            .map(GatewayTransactionId::new)
            .transpose()
            .map_err(|_| OperatorReviewError::InvalidState(INVALID_OPERATOR_STATE))?,
        row.try_get::<Option<String>, _>("review_charge_gateway_payment_method_reference")?
            .map(GatewayPaymentMethodReference::new)
            .transpose()
            .map_err(|_| OperatorReviewError::InvalidState(INVALID_OPERATOR_STATE))?,
        review_diagnostic(row, "review_charge_gateway_response")?,
        review_diagnostic(row, "review_charge_gateway_response_code")?,
        review_diagnostic(row, "review_charge_gateway_response_text")?,
        review_diagnostic(row, "review_charge_gateway_condition")?,
        descriptor,
    );
    Ok(ProcessorCharge::new(
        ProcessorChargeId::new(row.try_get("review_charge_id")?),
        attempt_id,
        BillingScopeId::new(row.try_get("review_charge_billing_scope_id")?),
        GatewayAccountId::new(row.try_get("review_charge_gateway_account_id")?),
        gateway_order_id,
        parse_kind(&row.try_get::<String, _>("review_charge_attempt_kind")?)?,
        amount,
        parse_role(&row.try_get::<String, _>("review_charge_role")?)?,
        parse_progression(&row.try_get::<String, _>("review_charge_progression_state")?)?,
        row.try_get::<Option<String>, _>("review_charge_state_code")?
            .as_deref()
            .map(parse_charge_state_code)
            .transpose()?,
        evidence,
        row.try_get("review_charge_observed_at")?,
    ))
}

fn review_diagnostic(
    row: &PgRow,
    column: &'static str,
) -> Result<Option<GatewayDiagnostic>, sqlx::Error> {
    row.try_get::<Option<String>, _>(column)
        .map(|value| value.map(|value| GatewayDiagnostic::new(&value)))
}

#[allow(clippy::too_many_arguments)]
async fn insert_attestation(
    transaction: &mut Transaction<'_, Postgres>,
    attempt: &PaymentAttempt,
    charge: &ProcessorCharge,
    actor_id: ActorId,
    kind: ExternalReversalKind,
    reason: &ExternalReversalReason,
    prior: &str,
    final_code: PaymentResolutionCode,
    attested_at: DateTime<Utc>,
) -> Result<(), OperatorReviewError> {
    let identity = attempt.identity();
    let evidence = charge.evidence();
    let descriptor = evidence.descriptor();
    let transaction_id = evidence
        .transaction_id()
        .ok_or(OperatorReviewError::InvalidState(
            "eligible charge is missing transaction identity",
        ))?;
    sqlx::query(
        r#"
        INSERT INTO billing_external_reversal_attestations (
            processor_charge_id, attempt_id, actor_id, reversal_kind, reason,
            prior_resolution_code, final_resolution_code,
            gateway_account_id, gateway_configuration_id, gateway_order_id,
            amount_cents, currency, gateway_transaction_id,
            gateway_payment_method_reference, gateway_response, gateway_response_code,
            gateway_response_text, gateway_condition, payment_type, card_brand,
            card_last4, card_exp_month, card_exp_year, attested_at
        ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12,
            $13, $14, $15, $16, $17, $18, $19, $20, $21, $22, $23, $24
        )
        "#,
    )
    .bind(charge.id().as_uuid())
    .bind(identity.attempt_id().as_uuid())
    .bind(actor_id.as_uuid())
    .bind(kind.as_str())
    .bind(reason.expose())
    .bind(prior)
    .bind(final_code.as_str())
    .bind(identity.gateway_account_id().as_uuid())
    .bind(identity.gateway_configuration_id().as_uuid())
    .bind(charge.gateway_order_id().expose())
    .bind(charge.amount().cents())
    .bind(charge.amount().currency().as_str())
    .bind(transaction_id.expose())
    .bind(
        evidence
            .payment_method_reference()
            .map(|value| value.expose()),
    )
    .bind(evidence.response().map(|value| value.expose()))
    .bind(evidence.response_code().map(|value| value.expose()))
    .bind(evidence.response_text().map(|value| value.expose()))
    .bind(evidence.condition().map(|value| value.expose()))
    .bind(descriptor.payment_type().map(|value| value.expose()))
    .bind(descriptor.card_brand().map(|value| value.expose()))
    .bind(descriptor.card_last_four().map(|value| value.expose()))
    .bind(descriptor.card_exp_month())
    .bind(descriptor.card_exp_year())
    .bind(attested_at)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

pub(crate) async fn attestation_by_charge(
    connection: &mut PgConnection,
    charge_id: Uuid,
) -> Result<Option<ExternalReversalAttestation>, OperatorReviewError> {
    let row = sqlx::query(
        r#"
        SELECT processor_charge_id, attempt_id, actor_id, reversal_kind, reason,
            prior_resolution_code, final_resolution_code,
            gateway_account_id, gateway_configuration_id, gateway_order_id,
            amount_cents, currency, gateway_transaction_id,
            gateway_payment_method_reference, gateway_response, gateway_response_code,
            gateway_response_text, gateway_condition, payment_type, card_brand,
            card_last4, card_exp_month, card_exp_year, attested_at
        FROM billing_external_reversal_attestations
        WHERE processor_charge_id = $1 FOR UPDATE
        "#,
    )
    .bind(charge_id)
    .fetch_optional(connection)
    .await?;
    row.as_ref().map(attestation_from_row).transpose()
}

fn attestation_from_row(row: &PgRow) -> Result<ExternalReversalAttestation, OperatorReviewError> {
    let attempt_id = PaymentAttemptId::new(row.try_get("attempt_id")?);
    let order = row.try_get::<String, _>("gateway_order_id")?;
    let order = GatewayOrderId::from_generated_attempt(&order, attempt_id)
        .or_else(|_| GatewayOrderId::from_correlation(&order))
        .map_err(|_| {
            OperatorReviewError::InvalidState("operator attestation order identity is invalid")
        })?;
    let currency = CurrencyCode::new(&row.try_get::<String, _>("currency")?).map_err(|_| {
        OperatorReviewError::InvalidState("operator attestation currency is invalid")
    })?;
    Ok(ExternalReversalAttestation::new(
        ProcessorChargeId::new(row.try_get("processor_charge_id")?),
        attempt_id,
        ActorId::new(row.try_get("actor_id")?),
        parse_reversal_kind(&row.try_get::<String, _>("reversal_kind")?).map_err(|_| {
            OperatorReviewError::InvalidState("operator attestation reversal kind is invalid")
        })?,
        ExternalReversalReason::new(row.try_get::<String, _>("reason")?).map_err(|_| {
            OperatorReviewError::InvalidState("operator attestation reason is invalid")
        })?,
        row.try_get("prior_resolution_code")?,
        PaymentResolutionCode::try_from(
            row.try_get::<String, _>("final_resolution_code")?.as_str(),
        )
        .map_err(|_| {
            OperatorReviewError::InvalidState("operator attestation final resolution is invalid")
        })?,
        GatewayAccountId::new(row.try_get("gateway_account_id")?),
        GatewayConfigurationId::new(row.try_get("gateway_configuration_id")?),
        order,
        ChargeAmount::new(row.try_get("amount_cents")?, currency).map_err(|_| {
            OperatorReviewError::InvalidState("operator attestation amount is invalid")
        })?,
        GatewayTransactionId::new(row.try_get::<String, _>("gateway_transaction_id")?).map_err(
            |_| {
                OperatorReviewError::InvalidState(
                    "operator attestation transaction identity is invalid",
                )
            },
        )?,
        processor_evidence_from_row(row).map_err(|_| {
            OperatorReviewError::InvalidState("operator attestation evidence is invalid")
        })?,
        row.try_get("attested_at")?,
    ))
}

fn locator_matches(
    locator: &ChargeLocator,
    attempt: &PaymentAttempt,
    charge: &ProcessorCharge,
) -> bool {
    let identity = attempt.identity();
    locator.charge_id == charge.id()
        && locator.attempt_id == identity.attempt_id()
        && locator.billing_scope_id == identity.billing_scope_id()
        && locator.subscriber_id == identity.subscriber_id()
        && locator.kind == attempt.kind()
        && locator.plan_key.as_ref() == attempt.request().target().plan_key()
        && locator.host_target_id == attempt.request().target().host_charge_target_id()
        && charge.attempt_id() == identity.attempt_id()
        && charge.billing_scope_id() == identity.billing_scope_id()
        && charge.gateway_account_id() == identity.gateway_account_id()
        && charge.attempt_kind() == attempt.kind()
        && charge.gateway_order_id() == attempt.request().gateway_order_id()
        && charge.amount().money() == attempt.request().amount()
}

fn processor_charge_can_attest_external_reversal(charge: &ProcessorCharge) -> bool {
    charge.progression() == ProcessorChargeProgression::ExternalReversalRequired
        && charge.evidence().transaction_id().is_some()
        && matches!(
            charge.attempt_kind(),
            PaymentAttemptKind::HostCharge
                | PaymentAttemptKind::SubscriptionInitial
                | PaymentAttemptKind::SubscriptionRenewal
                | PaymentAttemptKind::SubscriptionRecovery
        )
}

fn can_release_host_target(attempt: &PaymentAttempt, charge: &ProcessorCharge) -> bool {
    attempt.kind() == PaymentAttemptKind::HostCharge
        && (matches!(
            attempt.status(),
            PaymentAttemptStatus::Declined | PaymentAttemptStatus::Failed
        ) || (charge.role() == ProcessorChargeRole::Primary && !attempt.status().is_terminal()))
}

async fn release_host_target(
    host: &dyn ExternalReversalHostStore,
    transaction: &mut Transaction<'_, Postgres>,
    attempt: &PaymentAttempt,
) -> Result<(), OperatorReviewError> {
    let target_id = attempt.request().target().host_charge_target_id().ok_or(
        OperatorReviewError::InvalidState("host charge is missing exact target"),
    )?;
    let identity = attempt.identity();
    let _ = host
        .release(
            &mut *transaction,
            ExternalReversalHostChargeRelease::new(
                identity.billing_scope_id(),
                identity.subscriber_id(),
                target_id,
            ),
        )
        .await?;
    Ok(())
}

pub(crate) fn attestation_matches_source(
    attestation: &ExternalReversalAttestation,
    attempt: &PaymentAttempt,
    charge: &ProcessorCharge,
) -> bool {
    attestation.processor_charge_id() == charge.id()
        && attestation.attempt_id() == attempt.identity().attempt_id()
        && attestation.gateway_account_id() == attempt.identity().gateway_account_id()
        && attestation.gateway_configuration_id() == attempt.identity().gateway_configuration_id()
        && attestation.gateway_order_id() == attempt.request().gateway_order_id()
        && attestation.amount() == charge.amount()
        && attestation.gateway_transaction_id()
            == charge
                .evidence()
                .transaction_id()
                .expect("eligible charge has transaction identity")
        && attestation.processor_evidence() == charge.evidence()
        && attestation.prior_resolution_code() == expected_prior_resolution_code(attempt, charge)
        && attestation.final_resolution_code()
            == expected_final_resolution_code(attempt.kind(), attestation.kind())
}

fn expected_prior_resolution_code(
    attempt: &PaymentAttempt,
    charge: &ProcessorCharge,
) -> &'static str {
    if charge.role() == ProcessorChargeRole::Primary
        && charge.attempt_kind() == PaymentAttemptKind::SubscriptionInitial
        && (attempt.state().resolution_code()
            == Some(PaymentResolutionCode::SubscriptionInitialCurrentGrantConflict)
            || charge.state_code()
                == Some(ProcessorChargeStateCode::PaymentResolution(
                    PaymentResolutionCode::SubscriptionInitialCurrentGrantConflict,
                )))
    {
        PaymentResolutionCode::SubscriptionInitialCurrentGrantConflict.as_str()
    } else {
        "processor_charge_external_reversal_required"
    }
}

fn expected_final_resolution_code(
    attempt_kind: PaymentAttemptKind,
    kind: ExternalReversalKind,
) -> PaymentResolutionCode {
    match (attempt_kind, kind) {
        (PaymentAttemptKind::SubscriptionInitial, ExternalReversalKind::Refund) => {
            PaymentResolutionCode::SubscriptionInitialExternallyRefunded
        }
        (PaymentAttemptKind::SubscriptionInitial, ExternalReversalKind::Void) => {
            PaymentResolutionCode::SubscriptionInitialExternallyVoided
        }
        (_, ExternalReversalKind::Refund) => {
            PaymentResolutionCode::ProcessorChargeExternallyRefunded
        }
        (_, ExternalReversalKind::Void) => PaymentResolutionCode::ProcessorChargeExternallyVoided,
    }
}

async fn load_attempt(
    transaction: &mut Transaction<'_, Postgres>,
    scope: BillingScopeId,
    attempt_id: PaymentAttemptId,
) -> Result<PaymentAttempt, OperatorReviewError> {
    let query = format!(
        "{} WHERE billing_scope_id = $1 AND id = $2",
        crate::attempts::PAYMENT_ATTEMPT_SELECT
    );
    let row = sqlx::query(&query)
        .bind(scope.as_uuid())
        .bind(attempt_id.as_uuid())
        .fetch_optional(&mut **transaction)
        .await?
        .ok_or(OperatorReviewError::InvalidState(
            "operator review attempt disappeared",
        ))?;
    payment_attempt_from_row(&row).map_err(|_| {
        OperatorReviewError::InvalidState("post-attestation payment attempt is invalid")
    })
}

fn parse_kind(value: &str) -> Result<PaymentAttemptKind, OperatorReviewError> {
    value
        .parse()
        .map_err(|_| OperatorReviewError::InvalidState(INVALID_OPERATOR_STATE))
}
fn parse_role(value: &str) -> Result<ProcessorChargeRole, OperatorReviewError> {
    match value {
        "primary" => Ok(ProcessorChargeRole::Primary),
        "additional" => Ok(ProcessorChargeRole::Additional),
        _ => Err(OperatorReviewError::InvalidState(INVALID_OPERATOR_STATE)),
    }
}
fn parse_progression(value: &str) -> Result<ProcessorChargeProgression, OperatorReviewError> {
    match value {
        "pending" => Ok(ProcessorChargeProgression::Pending),
        "reconciliation_required" => Ok(ProcessorChargeProgression::ReconciliationRequired),
        "external_reversal_required" => Ok(ProcessorChargeProgression::ExternalReversalRequired),
        "applied" => Ok(ProcessorChargeProgression::Applied),
        "externally_reversed" => Ok(ProcessorChargeProgression::ExternallyReversed),
        _ => Err(OperatorReviewError::InvalidState(INVALID_OPERATOR_STATE)),
    }
}
fn parse_reversal_kind(value: &str) -> Result<ExternalReversalKind, OperatorReviewError> {
    match value {
        "refund" => Ok(ExternalReversalKind::Refund),
        "void" => Ok(ExternalReversalKind::Void),
        _ => Err(OperatorReviewError::InvalidState(INVALID_OPERATOR_STATE)),
    }
}

fn parse_charge_state_code(value: &str) -> Result<ProcessorChargeStateCode, OperatorReviewError> {
    match value {
        "processor_charge_external_reversal_required" => {
            return Ok(ProcessorChargeStateCode::ExternalReversalRequired);
        }
        "additional_approved_charge_identified" => {
            return Ok(ProcessorChargeStateCode::AdditionalApprovedChargeIdentified);
        }
        "processor_charge_transaction_identity_required" => {
            return Ok(ProcessorChargeStateCode::TransactionIdentityRequired);
        }
        "approved_charge_waiting_for_application" => {
            return Ok(ProcessorChargeStateCode::ApprovedChargeWaitingForApplication);
        }
        "zero_amount_additional_approved_charge" => {
            return Ok(ProcessorChargeStateCode::ZeroAmountAdditionalApprovedCharge);
        }
        _ => {}
    }
    PaymentResolutionCode::try_from(value)
        .map(ProcessorChargeStateCode::PaymentResolution)
        .map_err(|_| OperatorReviewError::InvalidState(INVALID_OPERATOR_STATE))
}

#[cfg(test)]
mod tests {
    use std::{
        error::Error,
        fmt,
        sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        },
        time::Duration,
    };

    use super::*;
    use crate::test_support::{TestDatabase, create_gateway_account};
    use crate::transactions::{BillingTransaction, BillingTransactionSubjectState};
    use tokio::sync::Mutex;

    #[derive(Debug)]
    struct InjectedTestError;

    impl fmt::Display for InjectedTestError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("injected test error")
        }
    }

    impl Error for InjectedTestError {}

    #[derive(Clone)]
    struct TestCoordinator {
        pool: PgPool,
        events: Arc<Mutex<Vec<BillingEvent>>>,
        fail_event: bool,
    }

    #[async_trait]
    impl BillingTransactionCoordinator for TestCoordinator {
        async fn begin(
            &self,
            _subject: BillingEventSubject,
            _lock_timeout: Duration,
        ) -> Result<Box<dyn BillingTransaction>, BillingTransactionError> {
            Ok(Box::new(TestTransaction {
                transaction: Some(
                    self.pool
                        .begin()
                        .await
                        .map_err(BillingTransactionError::new)?,
                ),
                events: Arc::clone(&self.events),
                fail_event: self.fail_event,
            }))
        }
    }

    struct TestTransaction {
        transaction: Option<Transaction<'static, Postgres>>,
        events: Arc<Mutex<Vec<BillingEvent>>>,
        fail_event: bool,
    }

    #[async_trait]
    impl BillingTransaction for TestTransaction {
        fn connection(&mut self) -> &mut PgConnection {
            &mut *self.transaction.as_mut().expect("active test transaction")
        }

        fn subject_state(&self) -> BillingTransactionSubjectState {
            BillingTransactionSubjectState::LiveRecipient
        }

        async fn append_event(
            &mut self,
            event: &BillingEvent,
        ) -> Result<(), BillingEventWriteError> {
            if self.fail_event {
                return Err(BillingEventWriteError::new(InjectedTestError));
            }
            self.events.lock().await.push(event.clone());
            Ok(())
        }

        async fn commit(mut self: Box<Self>) -> Result<(), BillingTransactionError> {
            self.transaction
                .take()
                .expect("active test transaction")
                .commit()
                .await
                .map_err(BillingTransactionError::new)
        }

        async fn rollback(mut self: Box<Self>) -> Result<(), BillingTransactionError> {
            self.transaction
                .take()
                .expect("active test transaction")
                .rollback()
                .await
                .map_err(BillingTransactionError::new)
        }
    }

    struct ExactManualFailureHost;

    #[async_trait]
    impl ManualAttemptFailureHostStore for ExactManualFailureHost {
        async fn lock_payment_failure_target(
            &self,
            connection: &mut PgConnection,
            charge: ManualFailureHostCharge,
        ) -> Result<(), ManualAttemptFailureHostStoreError> {
            sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM manual_failure_host_targets WHERE id = $1 AND billing_scope_id = $2 AND subscriber_id = $3 FOR UPDATE",
            )
            .bind(charge.target_id().as_uuid())
            .bind(charge.billing_scope_id().as_uuid())
            .bind(charge.subscriber_id().as_uuid())
            .fetch_optional(connection)
            .await
            .map_err(ManualAttemptFailureHostStoreError::new)?;
            Ok(())
        }

        async fn mark_payment_failed(
            &self,
            connection: &mut PgConnection,
            charge: ManualFailureHostCharge,
        ) -> Result<ManualAttemptFailureHostTransitionOutcome, ManualAttemptFailureHostStoreError>
        {
            let result = sqlx::query(
                "UPDATE manual_failure_host_targets SET status = 'payment_failed' WHERE id = $1 AND billing_scope_id = $2 AND subscriber_id = $3 AND status = 'pending'",
            )
            .bind(charge.target_id().as_uuid())
            .bind(charge.billing_scope_id().as_uuid())
            .bind(charge.subscriber_id().as_uuid())
            .execute(connection)
            .await
            .map_err(ManualAttemptFailureHostStoreError::new)?;
            Ok(if result.rows_affected() == 1 {
                ManualAttemptFailureHostTransitionOutcome::Changed
            } else {
                ManualAttemptFailureHostTransitionOutcome::Unchanged
            })
        }
    }

    async fn insert_review_renewal(
        database: &TestDatabase,
        account: &crate::test_support::GatewayAccountFixture,
        subscriber_id: Uuid,
        suffix: &str,
    ) -> Result<(Uuid, Uuid), Box<dyn Error>> {
        let payment_method_id = Uuid::now_v7();
        let subscription_id = Uuid::now_v7();
        let attempt_id = Uuid::now_v7();
        let initial_transaction_id = format!("txn-initial-{suffix}");
        sqlx::query(
            r#"
            INSERT INTO billing_payment_methods (
                id, billing_scope_id, subscriber_id, gateway_account_id,
                gateway_payment_method_reference, status
            ) VALUES ($1, $2, $3, $4, $5, 'active')
            "#,
        )
        .bind(payment_method_id)
        .bind(account.billing_scope_id)
        .bind(subscriber_id)
        .bind(account.gateway_account_id)
        .bind(format!("method-{suffix}"))
        .execute(&database.pool)
        .await?;
        sqlx::query(
            r#"
            WITH clock AS MATERIALIZED (SELECT clock_timestamp() AS observed_at)
            INSERT INTO billing_subscriptions (
                id, billing_scope_id, subscriber_id, plan_key, status,
                gateway_account_id, payment_method_id, amount_cents, currency,
                current_period_start_at, current_period_end_at, next_renewal_at,
                initial_transaction_id
            ) SELECT
                $1, $2, $3, 'test_plan', 'active', $4, $5, 500, 'USD',
                observed_at - interval '1 month', observed_at, observed_at,
                $6
            FROM clock
            "#,
        )
        .bind(subscription_id)
        .bind(account.billing_scope_id)
        .bind(subscriber_id)
        .bind(account.gateway_account_id)
        .bind(payment_method_id)
        .bind(&initial_transaction_id)
        .execute(&database.pool)
        .await?;
        sqlx::query(
            r#"
            WITH clock AS MATERIALIZED (SELECT clock_timestamp() AS observed_at)
            INSERT INTO billing_payment_attempts (
                id, billing_scope_id, subscriber_id, plan_key,
                subscription_id, payment_method_id, attempt_kind, status,
                idempotency_key, request_fingerprint, amount_cents, currency,
                billing_period_start_at, billing_period_end_at,
                gateway_account_id, gateway_configuration_id, gateway_order_id,
                submitted_at, review_required_at,
                subscription_expected_payment_method_id,
                subscription_expected_initial_transaction_id,
                subscription_expected_status
            ) SELECT
                $1, $2, $3, 'test_plan', $4, $5, 'subscription_renewal',
                'review_required', $6, $7, 500, 'USD', observed_at,
                observed_at + interval '1 month', $8, $9, $10,
                observed_at, observed_at, $5, $11, 'active'
            FROM clock
            "#,
        )
        .bind(attempt_id)
        .bind(account.billing_scope_id)
        .bind(subscriber_id)
        .bind(subscription_id)
        .bind(payment_method_id)
        .bind(format!("idem-{suffix}"))
        .bind(format!("fingerprint-{suffix}"))
        .bind(account.gateway_account_id)
        .bind(account.gateway_configuration_id)
        .bind(format!("order-{suffix}"))
        .bind(initial_transaction_id)
        .execute(&database.pool)
        .await?;
        Ok((subscription_id, attempt_id))
    }

    #[tokio::test]
    async fn manual_failure_is_policy_safe_atomic_eventful_and_host_exact()
    -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("rail_manual").await?;
        let account = create_gateway_account(&database.pool, "nmi").await?;
        sqlx::query(
            r#"
            CREATE TABLE manual_failure_host_targets (
                id uuid PRIMARY KEY,
                billing_scope_id uuid NOT NULL,
                subscriber_id uuid NOT NULL,
                status text NOT NULL
            )
            "#,
        )
        .execute(&database.pool)
        .await?;
        let events = Arc::new(Mutex::new(Vec::new()));
        let coordinator = TestCoordinator {
            pool: database.pool.clone(),
            events: Arc::clone(&events),
            fail_event: false,
        };
        let host = ExactManualFailureHost;

        let subscriber_id = Uuid::now_v7();
        let (subscription_id, renewal_attempt_id) =
            insert_review_renewal(&database, &account, subscriber_id, "success").await?;
        let outcome = fail_review_required_attempt(
            &database.pool,
            &coordinator,
            &host,
            PaymentAttemptId::new(renewal_attempt_id),
        )
        .await?;
        assert!(matches!(outcome, ManualAttemptFailureOutcome::Failed(_)));
        let (attempt_status, response_text, condition): (String, Option<String>, Option<String>) =
            sqlx::query_as(
                "SELECT status, gateway_response_text, gateway_condition FROM billing_payment_attempts WHERE id = $1",
            )
            .bind(renewal_attempt_id)
            .fetch_one(&database.pool)
            .await?;
        assert_eq!(attempt_status, "failed");
        assert_eq!(
            response_text.as_deref(),
            Some(syrup_rail::MANUAL_ATTEMPT_FAILURE_NOTE)
        );
        assert_eq!(condition.as_deref(), Some("failed"));
        let subscription_status: String =
            sqlx::query_scalar("SELECT status FROM billing_subscriptions WHERE id = $1")
                .bind(subscription_id)
                .fetch_one(&database.pool)
                .await?;
        assert_eq!(subscription_status, "past_due");
        let recorded_events = events.lock().await;
        assert_eq!(recorded_events.len(), 1);
        assert!(matches!(
            &recorded_events[0],
            BillingEvent::SubscriptionPaymentFailed { attempt_id, .. }
                if *attempt_id == PaymentAttemptId::new(renewal_attempt_id)
        ));
        drop(recorded_events);
        assert!(matches!(
            fail_review_required_attempt(
                &database.pool,
                &coordinator,
                &host,
                PaymentAttemptId::new(renewal_attempt_id),
            )
            .await?,
            ManualAttemptFailureOutcome::KeptOpen(_)
        ));
        assert_eq!(events.lock().await.len(), 1);

        let blocked_subscriber_id = Uuid::now_v7();
        let (blocked_subscription_id, blocked_attempt_id) =
            insert_review_renewal(&database, &account, blocked_subscriber_id, "blocked").await?;
        let failing_coordinator = TestCoordinator {
            pool: database.pool.clone(),
            events: Arc::new(Mutex::new(Vec::new())),
            fail_event: true,
        };
        assert!(
            fail_review_required_attempt(
                &database.pool,
                &failing_coordinator,
                &host,
                PaymentAttemptId::new(blocked_attempt_id),
            )
            .await
            .is_err()
        );
        let rolled_back: (String, String) = sqlx::query_as(
            "SELECT attempts.status, subscriptions.status FROM billing_payment_attempts attempts INNER JOIN billing_subscriptions subscriptions ON subscriptions.id = attempts.subscription_id WHERE attempts.id = $1",
        )
        .bind(blocked_attempt_id)
        .fetch_one(&database.pool)
        .await?;
        assert_eq!(
            rolled_back,
            ("review_required".to_owned(), "active".to_owned())
        );
        sqlx::query(
            r#"
            INSERT INTO billing_processor_charges (
                id, attempt_id, billing_scope_id, gateway_account_id,
                gateway_order_id, gateway_transaction_id, charge_role,
                progression_state, observed_at, attempt_kind, plan_key,
                amount_cents, currency
            ) SELECT
                $2, id, billing_scope_id, gateway_account_id, gateway_order_id,
                'txn-blocked', 'primary', 'pending', clock_timestamp(),
                attempt_kind, plan_key, amount_cents, currency
            FROM billing_payment_attempts WHERE id = $1
            "#,
        )
        .bind(blocked_attempt_id)
        .bind(Uuid::now_v7())
        .execute(&database.pool)
        .await?;
        assert!(matches!(
            fail_review_required_attempt(
                &database.pool,
                &coordinator,
                &host,
                PaymentAttemptId::new(blocked_attempt_id),
            )
            .await?,
            ManualAttemptFailureOutcome::KeptOpen(_)
        ));
        let blocked_subscription_status: String =
            sqlx::query_scalar("SELECT status FROM billing_subscriptions WHERE id = $1")
                .bind(blocked_subscription_id)
                .fetch_one(&database.pool)
                .await?;
        assert_eq!(blocked_subscription_status, "active");

        let host_subscriber_id = Uuid::now_v7();
        let host_target_id = Uuid::now_v7();
        let host_attempt_id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO manual_failure_host_targets (id, billing_scope_id, subscriber_id, status) VALUES ($1, $2, $3, 'pending')",
        )
        .bind(host_target_id)
        .bind(account.billing_scope_id)
        .bind(host_subscriber_id)
        .execute(&database.pool)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO billing_payment_attempts (
                id, billing_scope_id, subscriber_id, host_charge_target_id,
                attempt_kind, status, idempotency_key, request_fingerprint,
                amount_cents, currency, gateway_account_id,
                gateway_configuration_id, gateway_order_id, review_required_at
            ) VALUES (
                $1, $2, $3, $4, 'host_charge', 'review_required', $5, $6,
                500, 'USD', $7, $8, $9, clock_timestamp()
            )
            "#,
        )
        .bind(host_attempt_id)
        .bind(account.billing_scope_id)
        .bind(host_subscriber_id)
        .bind(host_target_id)
        .bind(format!("idem-{host_attempt_id}"))
        .bind(format!("fingerprint-{host_attempt_id}"))
        .bind(account.gateway_account_id)
        .bind(account.gateway_configuration_id)
        .bind(format!("host-order-{host_attempt_id}"))
        .execute(&database.pool)
        .await?;
        assert!(matches!(
            fail_review_required_attempt(
                &database.pool,
                &coordinator,
                &host,
                PaymentAttemptId::new(host_attempt_id),
            )
            .await?,
            ManualAttemptFailureOutcome::Failed(_)
        ));
        let host_state: (String, String) = sqlx::query_as(
            "SELECT attempts.status, targets.status FROM billing_payment_attempts attempts INNER JOIN manual_failure_host_targets targets ON targets.id = attempts.host_charge_target_id WHERE attempts.id = $1",
        )
        .bind(host_attempt_id)
        .fetch_one(&database.pool)
        .await?;
        assert_eq!(
            host_state,
            ("failed".to_owned(), "payment_failed".to_owned())
        );
        Ok(())
    }

    #[derive(Default)]
    struct ExactHostRelease {
        calls: AtomicU64,
    }

    #[async_trait]
    impl ExternalReversalHostStore for ExactHostRelease {
        async fn release(
            &self,
            connection: &mut PgConnection,
            release: ExternalReversalHostChargeRelease,
        ) -> Result<ExternalReversalHostTransitionOutcome, ExternalReversalHostStoreError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let changed = sqlx::query(
                r#"
                UPDATE host_targets SET released = true
                WHERE id = $1 AND billing_scope_id = $2 AND subscriber_id = $3
                    AND released = false
                "#,
            )
            .bind(release.target_id().as_uuid())
            .bind(release.billing_scope_id().as_uuid())
            .bind(release.subscriber_id().as_uuid())
            .execute(connection)
            .await
            .map_err(ExternalReversalHostStoreError::new)?
            .rows_affected()
                == 1;
            Ok(if changed {
                ExternalReversalHostTransitionOutcome::Changed
            } else {
                ExternalReversalHostTransitionOutcome::Unchanged
            })
        }
    }

    #[tokio::test]
    async fn external_reversal_is_exact_atomic_replayable_and_conflict_safe()
    -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("rail_operator").await?;
        let account = create_gateway_account(&database.pool, "nmi").await?;
        let attempt_id = Uuid::now_v7();
        let charge_id = Uuid::now_v7();
        let additional_charge_id = Uuid::now_v7();
        let subscriber_id = Uuid::now_v7();
        let target_id = Uuid::now_v7();
        let order_id = format!("ck_{}", attempt_id.simple());
        sqlx::query(
            r#"
            CREATE TABLE host_targets (
                id uuid PRIMARY KEY, billing_scope_id uuid NOT NULL,
                subscriber_id uuid NOT NULL, released boolean NOT NULL DEFAULT false
            )
            "#,
        )
        .execute(&database.pool)
        .await?;
        sqlx::query(
            "INSERT INTO host_targets (id, billing_scope_id, subscriber_id) VALUES ($1, $2, $3)",
        )
        .bind(target_id)
        .bind(account.billing_scope_id)
        .bind(subscriber_id)
        .execute(&database.pool)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO billing_payment_attempts (
                id, billing_scope_id, subscriber_id, host_charge_target_id,
                attempt_kind, status, idempotency_key, request_fingerprint,
                amount_cents, currency, gateway_account_id,
                gateway_configuration_id, gateway_order_id, review_required_at
            ) VALUES (
                $1, $2, $3, $4, 'host_charge', 'review_required', $5, $6,
                500, 'USD', $7, $8, $9, clock_timestamp()
            )
            "#,
        )
        .bind(attempt_id)
        .bind(account.billing_scope_id)
        .bind(subscriber_id)
        .bind(target_id)
        .bind(format!("idem-{attempt_id}"))
        .bind(format!("fingerprint-{attempt_id}"))
        .bind(account.gateway_account_id)
        .bind(account.gateway_configuration_id)
        .bind(&order_id)
        .execute(&database.pool)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO billing_processor_charges (
                id, attempt_id, billing_scope_id, gateway_account_id,
                gateway_order_id, gateway_transaction_id, gateway_response,
                gateway_response_code, gateway_response_text, gateway_condition,
                charge_role, progression_state, observed_at, attempt_kind,
                host_charge_target_id, amount_cents, currency,
                external_reversal_required_at
            ) VALUES (
                $1, $2, $3, $4, $5, 'txn-operator', '1', '100', 'Approved',
                'complete', 'primary', 'external_reversal_required',
                clock_timestamp(), 'host_charge', $6, 500, 'USD', clock_timestamp()
            )
            "#,
        )
        .bind(charge_id)
        .bind(attempt_id)
        .bind(account.billing_scope_id)
        .bind(account.gateway_account_id)
        .bind(&order_id)
        .bind(target_id)
        .execute(&database.pool)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO billing_processor_charges (
                id, attempt_id, billing_scope_id, gateway_account_id,
                gateway_order_id, gateway_transaction_id, gateway_response,
                gateway_response_code, gateway_response_text, gateway_condition,
                charge_role, progression_state, observed_at, attempt_kind,
                host_charge_target_id, amount_cents, currency,
                external_reversal_required_at
            ) VALUES (
                $1, $2, $3, $4, $5, 'txn-operator-additional', '1', '100',
                'Approved additional charge', 'complete', 'additional',
                'external_reversal_required', clock_timestamp(), 'host_charge',
                $6, 500, 'USD', clock_timestamp()
            )
            "#,
        )
        .bind(additional_charge_id)
        .bind(attempt_id)
        .bind(account.billing_scope_id)
        .bind(account.gateway_account_id)
        .bind(&order_id)
        .bind(target_id)
        .execute(&database.pool)
        .await?;

        let page_limit = OperatorReviewPageLimit::new(1)?;
        let attempt_page = attempt_review_page(&database.pool, page_limit, None).await?;
        assert!(attempt_page.into_items().is_empty());
        let first_charge_page =
            processor_charge_review_page(&database.pool, page_limit, None).await?;
        let next_cursor = first_charge_page.next_cursor().expect("second charge page");
        let first_charge_items = first_charge_page.into_items();
        assert_eq!(first_charge_items.len(), 1);
        let second_charge_page =
            processor_charge_review_page(&database.pool, page_limit, Some(next_cursor)).await?;
        assert!(second_charge_page.next_cursor().is_none());
        let second_charge_items = second_charge_page.into_items();
        assert_eq!(second_charge_items.len(), 1);
        let returned_charge_ids = [
            first_charge_items[0].charge().id(),
            second_charge_items[0].charge().id(),
        ];
        assert!(returned_charge_ids.contains(&ProcessorChargeId::new(charge_id)));
        assert!(returned_charge_ids.contains(&ProcessorChargeId::new(additional_charge_id)));
        assert!(
            first_charge_items
                .iter()
                .chain(&second_charge_items)
                .all(|item| item.attempt().identity().attempt_id()
                    == PaymentAttemptId::new(attempt_id))
        );

        let mut preflight = database.pool.begin().await?;
        let locator = charge_locator(&mut preflight, ProcessorChargeId::new(charge_id))
            .await?
            .expect("charge locator");
        lock_payment_attempt_by_id_on_connection(
            &mut preflight,
            locator.billing_scope_id,
            locator.attempt_id,
        )
        .await
        .expect("attempt parser")
        .expect("attempt exists");
        lock_processor_charge(&mut preflight, ProcessorChargeId::new(charge_id))
            .await
            .expect("charge parser")
            .expect("charge parser");
        preflight.rollback().await?;

        let host = ExactHostRelease::default();
        let actor = ActorId::new(Uuid::now_v7());
        let reason = ExternalReversalReason::new("processor refund verified")?;
        let mismatch = GatewayTransactionId::new("txn-other")?;
        assert_eq!(
            attest_external_reversal(
                &database.pool,
                &host,
                ProcessorChargeId::new(charge_id),
                actor,
                ExternalReversalKind::Refund,
                &mismatch,
                &reason,
            )
            .await?,
            ExternalReversalAttestationOutcome::Ineligible
        );
        let transaction_id = GatewayTransactionId::new("txn-operator")?;
        let attested = attest_external_reversal(
            &database.pool,
            &host,
            ProcessorChargeId::new(charge_id),
            actor,
            ExternalReversalKind::Refund,
            &transaction_id,
            &reason,
        )
        .await?;
        let ExternalReversalAttestationOutcome::Attested {
            attempt,
            attestation,
        } = attested
        else {
            panic!("expected attestation");
        };
        assert_eq!(attempt.status(), PaymentAttemptStatus::Failed);
        assert_eq!(attestation.actor_id(), actor);
        assert_eq!(host.calls.load(Ordering::SeqCst), 1);
        assert!(
            sqlx::query_scalar::<_, bool>("SELECT released FROM host_targets WHERE id = $1")
                .bind(target_id)
                .fetch_one(&database.pool)
                .await?
        );

        assert!(matches!(
            attest_external_reversal(
                &database.pool,
                &host,
                ProcessorChargeId::new(charge_id),
                actor,
                ExternalReversalKind::Refund,
                &transaction_id,
                &reason,
            )
            .await?,
            ExternalReversalAttestationOutcome::Replayed { .. }
        ));
        assert_eq!(host.calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            attest_external_reversal(
                &database.pool,
                &host,
                ProcessorChargeId::new(charge_id),
                ActorId::new(Uuid::now_v7()),
                ExternalReversalKind::Refund,
                &transaction_id,
                &reason,
            )
            .await?,
            ExternalReversalAttestationOutcome::ReplayConflict
        );
        let counts: (i64, String) = sqlx::query_as(
            "SELECT COUNT(*)::bigint, MIN(progression_state) FROM billing_processor_charges WHERE id = $1",
        ).bind(charge_id).fetch_one(&database.pool).await?;
        assert_eq!(counts, (1, "externally_reversed".to_owned()));

        database.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn grant_conflict_replay_uses_the_persisted_prior_charge_classification()
    -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("rail_op_grant").await?;
        let account = create_gateway_account(&database.pool, "nmi").await?;
        let attempt_id = Uuid::now_v7();
        let charge_id = Uuid::now_v7();
        let subscriber_id = Uuid::now_v7();
        let order_id = format!("subscription_{}", attempt_id.simple());
        sqlx::query(
            r#"
            INSERT INTO billing_payment_attempts (
                id, billing_scope_id, subscriber_id, plan_key,
                attempt_kind, status, idempotency_key, request_fingerprint,
                amount_cents, currency, gateway_account_id,
                gateway_configuration_id, gateway_order_id,
                gateway_transaction_id, gateway_response, gateway_response_code,
                gateway_response_text, gateway_condition, resolution_code,
                submitted_at, review_required_at
            ) VALUES (
                $1, $2, $3, 'base', 'subscription_initial', 'review_required',
                $4, $5, 500, 'USD', $6, $7, $8, 'txn-grant-conflict',
                '1', '100', 'Approved', 'complete',
                'subscription_initial_current_grant_conflict',
                clock_timestamp(), clock_timestamp()
            )
            "#,
        )
        .bind(attempt_id)
        .bind(account.billing_scope_id)
        .bind(subscriber_id)
        .bind(format!("idem-{attempt_id}"))
        .bind(format!("fingerprint-{attempt_id}"))
        .bind(account.gateway_account_id)
        .bind(account.gateway_configuration_id)
        .bind(&order_id)
        .execute(&database.pool)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO billing_processor_charges (
                id, attempt_id, billing_scope_id, gateway_account_id,
                gateway_order_id, gateway_transaction_id, gateway_response,
                gateway_response_code, gateway_response_text, gateway_condition,
                charge_role, progression_state, state_code, observed_at,
                attempt_kind, plan_key, amount_cents, currency,
                external_reversal_required_at
            ) VALUES (
                $1, $2, $3, $4, $5, 'txn-grant-conflict', '1', '100',
                'Approved', 'complete', 'primary', 'external_reversal_required',
                'processor_charge_external_reversal_required', clock_timestamp(),
                'subscription_initial', 'base', 500, 'USD', clock_timestamp()
            )
            "#,
        )
        .bind(charge_id)
        .bind(attempt_id)
        .bind(account.billing_scope_id)
        .bind(account.gateway_account_id)
        .bind(&order_id)
        .execute(&database.pool)
        .await?;

        let host = ExactHostRelease::default();
        let actor = ActorId::new(Uuid::now_v7());
        let reason = ExternalReversalReason::new("processor refund verified")?;
        let transaction_id = GatewayTransactionId::new("txn-grant-conflict")?;
        let attested = attest_external_reversal(
            &database.pool,
            &host,
            ProcessorChargeId::new(charge_id),
            actor,
            ExternalReversalKind::Refund,
            &transaction_id,
            &reason,
        )
        .await?;
        assert!(matches!(
            attested,
            ExternalReversalAttestationOutcome::Attested { .. }
        ));
        let state_code: String =
            sqlx::query_scalar("SELECT state_code FROM billing_processor_charges WHERE id = $1")
                .bind(charge_id)
                .fetch_one(&database.pool)
                .await?;
        assert_eq!(
            state_code,
            PaymentResolutionCode::SubscriptionInitialCurrentGrantConflict.as_str()
        );
        assert!(matches!(
            attest_external_reversal(
                &database.pool,
                &host,
                ProcessorChargeId::new(charge_id),
                actor,
                ExternalReversalKind::Refund,
                &transaction_id,
                &reason,
            )
            .await?,
            ExternalReversalAttestationOutcome::Replayed { .. }
        ));

        database.cleanup().await?;
        Ok(())
    }
}
