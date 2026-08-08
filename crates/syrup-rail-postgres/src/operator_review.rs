use std::{error::Error, fmt};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool, Postgres, Row, Transaction, postgres::PgRow};
use syrup_rail::{
    ActorId, BillingScopeId, ChargeAmount, CurrencyCode, ExternalReversalAttestation,
    ExternalReversalHostChargeRelease, ExternalReversalKind, ExternalReversalReason,
    GatewayAccountId, GatewayConfigurationId, GatewayOrderId, GatewayTransactionId,
    HostChargeTargetId, PaymentAttempt, PaymentAttemptId, PaymentAttemptKind, PaymentAttemptStatus,
    PaymentResolutionCode, PlanKey, ProcessorCharge, ProcessorChargeId, ProcessorChargeProgression,
    ProcessorChargeRole, ProcessorChargeStateCode, SubscriberId,
};
use thiserror::Error;
use uuid::Uuid;

use crate::attempts::{
    lock_payment_attempt_by_id_on_connection, lock_subscription_aggregate,
    payment_attempt_from_row, processor_evidence_from_row, set_enrollment_timeouts,
};

const INVALID_OPERATOR_STATE: &str = "canonical operator review state is invalid";
type BoxError = Box<dyn Error + Send + Sync + 'static>;

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

    if let Some(existing) = attestation_by_charge(&mut transaction, processor_charge_id).await? {
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
            state_code = COALESCE(state_code, $2),
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
    let attestation = attestation_by_charge(&mut transaction, processor_charge_id)
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

fn processor_charge_from_row(row: &PgRow) -> Result<ProcessorCharge, OperatorReviewError> {
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

async fn attestation_by_charge(
    transaction: &mut Transaction<'_, Postgres>,
    charge_id: ProcessorChargeId,
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
    .bind(charge_id.as_uuid())
    .fetch_optional(&mut **transaction)
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

fn attestation_matches_source(
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
    if value == "processor_charge_external_reversal_required" {
        return Ok(ProcessorChargeStateCode::ExternalReversalRequired);
    }
    PaymentResolutionCode::try_from(value)
        .map(ProcessorChargeStateCode::PaymentResolution)
        .map_err(|_| OperatorReviewError::InvalidState(INVALID_OPERATOR_STATE))
}

#[cfg(test)]
mod tests {
    use std::{
        error::Error,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;
    use crate::test_support::{TestDatabase, create_gateway_account};

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
}
