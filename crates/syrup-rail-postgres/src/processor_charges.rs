use std::time::Duration;

use sqlx::{PgConnection, PgPool, Row};
use syrup_rail::{
    GatewayDiagnostic, GatewayOrderId, GatewayTransactionId, PaymentAttempt, PaymentAttemptId,
    PaymentAttemptKind, PaymentAttemptStatus, PaymentResolutionCode, PlanKey, ProcessorCharge,
    ProcessorChargeId, ProcessorChargeProgression, ProcessorChargeRole, ProcessorChargeStateCode,
    ProcessorEvidence,
};
use thiserror::Error;
use uuid::Uuid;

use crate::attempts::{
    PAYMENT_ATTEMPT_SELECT, find_payment_attempt_by_id_on_connection,
    lock_payment_attempt_by_id_on_connection, lock_subscription_aggregate,
    payment_attempt_from_row, set_enrollment_timeouts,
};
use crate::operator_review::{
    attestation_by_charge, attestation_matches_source, processor_charge_from_row,
};
use crate::{OperatorReviewError, PaymentAttemptStoreError};

const INVALID_CHARGE_STATE: &str = "canonical processor charge state is invalid";
const STORE_MAX_ATTEMPTS: usize = 3;
const STORE_RETRY_DELAY: Duration = Duration::from_millis(50);

#[derive(Debug, Error)]
pub enum ProcessorChargeStoreError {
    #[error("processor charge storage operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("payment attempt storage operation failed")]
    Attempt(#[from] PaymentAttemptStoreError),
    #[error("{0}")]
    InvalidState(&'static str),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompensatingProcessorChargeOutcome {
    Observed,
    ExactReplay,
    OwnedByOtherAttempt,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProcessorChargeObservationOutcome {
    Observed(ProcessorCharge),
    ExactReplay(ProcessorCharge),
    OwnedByOtherAttempt,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ChargeRecord {
    pub(crate) id: Uuid,
    pub(crate) role: ProcessorChargeRole,
    exact_replay: bool,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum ObservedCharge {
    Owned(ChargeRecord),
    OwnedByOtherAttempt,
}

pub async fn store_compensating_processor_charge(
    pool: &PgPool,
    attempt_id: PaymentAttemptId,
    gateway_order_id: &GatewayOrderId,
    evidence: &ProcessorEvidence,
) -> Result<CompensatingProcessorChargeOutcome, ProcessorChargeStoreError> {
    let mut last_transient_error = None;
    for store_attempt in 1..=STORE_MAX_ATTEMPTS {
        match store_once(pool, attempt_id, gateway_order_id, evidence).await {
            Ok(outcome) => return Ok(outcome),
            Err(error) if is_transient(&error) && store_attempt < STORE_MAX_ATTEMPTS => {
                last_transient_error = Some(error);
                tokio::time::sleep(STORE_RETRY_DELAY).await;
            }
            Err(error) => return Err(error),
        }
    }
    Err(
        last_transient_error.unwrap_or(ProcessorChargeStoreError::InvalidState(
            "compensating processor charge retry loop exhausted",
        )),
    )
}

async fn store_once(
    pool: &PgPool,
    attempt_id: PaymentAttemptId,
    gateway_order_id: &GatewayOrderId,
    evidence: &ProcessorEvidence,
) -> Result<CompensatingProcessorChargeOutcome, ProcessorChargeStoreError> {
    let mut transaction = pool.begin().await?;
    set_enrollment_timeouts(&mut transaction).await?;
    let query = format!("{PAYMENT_ATTEMPT_SELECT} WHERE id = $1");
    let row = sqlx::query(&query)
        .bind(attempt_id.as_uuid())
        .fetch_optional(&mut *transaction)
        .await?;
    let preloaded_attempt = row
        .as_ref()
        .map(payment_attempt_from_row)
        .transpose()?
        .ok_or(ProcessorChargeStoreError::InvalidState(
            "compensating processor charge attempt was not found",
        ))?;
    if preloaded_attempt.kind() == PaymentAttemptKind::SubscriptionInitial {
        let plan_key = preloaded_attempt.request().target().plan_key().ok_or(
            ProcessorChargeStoreError::InvalidState(INVALID_CHARGE_STATE),
        )?;
        lock_subscription_aggregate(
            &mut transaction,
            preloaded_attempt.identity().subscriber_id(),
            plan_key,
        )
        .await?;
    }
    let attempt = if preloaded_attempt.kind() == PaymentAttemptKind::SubscriptionPaymentMethodUpdate
    {
        lock_payment_attempt_by_id_on_connection(
            &mut transaction,
            preloaded_attempt.identity().billing_scope_id(),
            attempt_id,
        )
        .await?
        .ok_or(ProcessorChargeStoreError::InvalidState(
            "compensating processor charge attempt was not found",
        ))?
    } else {
        let reloaded = find_payment_attempt_by_id_on_connection(
            &mut transaction,
            preloaded_attempt.identity().billing_scope_id(),
            attempt_id,
        )
        .await?
        .ok_or(ProcessorChargeStoreError::InvalidState(
            "compensating processor charge attempt was not found",
        ))?;
        if reloaded != preloaded_attempt {
            return Err(ProcessorChargeStoreError::InvalidState(
                "compensating processor charge attempt changed while its aggregate was locked",
            ));
        }
        reloaded
    };
    if attempt.request().gateway_order_id() != gateway_order_id {
        return Err(ProcessorChargeStoreError::InvalidState(
            "compensating processor charge order does not match its attempt",
        ));
    }
    let initial_progression = compensating_progression(&attempt, evidence);
    let observation =
        observe_processor_charge(&mut transaction, &attempt, evidence, initial_progression).await?;
    let outcome = match observation {
        ObservedCharge::OwnedByOtherAttempt => {
            CompensatingProcessorChargeOutcome::OwnedByOtherAttempt
        }
        ObservedCharge::Owned(charge) => {
            let mut persisted = charge_by_id(&mut transaction, charge.id).await?;
            if !charge.exact_replay {
                let state_code = initial_charge_state_code(
                    persisted.role(),
                    persisted.progression(),
                    evidence.transaction_id().is_some(),
                );
                if persisted.state_code() != state_code {
                    persisted = transition_processor_charge(
                        &mut transaction,
                        persisted.id(),
                        &[persisted.progression()],
                        persisted.progression(),
                        state_code,
                        false,
                    )
                    .await?;
                }
            }
            if let Some(attestation) = attestation_by_charge(&mut transaction, charge.id)
                .await
                .map_err(map_operator_error)?
                && (!charge.exact_replay
                    || !attestation_matches_source(&attestation, &attempt, &persisted))
            {
                return Err(ProcessorChargeStoreError::InvalidState(
                    "attested processor charge was reobserved with different evidence",
                ));
            }
            if charge.exact_replay {
                CompensatingProcessorChargeOutcome::ExactReplay
            } else {
                CompensatingProcessorChargeOutcome::Observed
            }
        }
    };
    transaction.commit().await?;
    Ok(outcome)
}

pub async fn observe_processor_charge_in_transaction(
    connection: &mut PgConnection,
    attempt_id: PaymentAttemptId,
    gateway_order_id: &GatewayOrderId,
    evidence: &ProcessorEvidence,
    initial_progression: ProcessorChargeProgression,
) -> Result<ProcessorChargeObservationOutcome, ProcessorChargeStoreError> {
    let query = format!("{PAYMENT_ATTEMPT_SELECT} WHERE id = $1");
    let row = sqlx::query(&query)
        .bind(attempt_id.as_uuid())
        .fetch_optional(&mut *connection)
        .await?;
    let attempt = row
        .as_ref()
        .map(payment_attempt_from_row)
        .transpose()?
        .ok_or(ProcessorChargeStoreError::InvalidState(
            "processor charge attempt was not found",
        ))?;
    if attempt.request().gateway_order_id() != gateway_order_id {
        return Err(ProcessorChargeStoreError::InvalidState(
            "processor charge order does not match its attempt",
        ));
    }
    match observe_processor_charge(connection, &attempt, evidence, initial_progression).await? {
        ObservedCharge::OwnedByOtherAttempt => {
            Ok(ProcessorChargeObservationOutcome::OwnedByOtherAttempt)
        }
        ObservedCharge::Owned(charge) => {
            if charge.exact_replay {
                Ok(ProcessorChargeObservationOutcome::ExactReplay(
                    charge_by_id(connection, charge.id).await?,
                ))
            } else {
                let mut persisted = charge_by_id(connection, charge.id).await?;
                let state_code = initial_charge_state_code(
                    persisted.role(),
                    persisted.progression(),
                    evidence.transaction_id().is_some(),
                );
                if persisted.state_code() != state_code {
                    persisted = transition_processor_charge(
                        connection,
                        persisted.id(),
                        &[persisted.progression()],
                        persisted.progression(),
                        state_code,
                        false,
                    )
                    .await?;
                }
                Ok(ProcessorChargeObservationOutcome::Observed(persisted))
            }
        }
    }
}

pub async fn transition_processor_charge_in_transaction(
    connection: &mut PgConnection,
    charge_id: ProcessorChargeId,
    expected_progressions: &[ProcessorChargeProgression],
    progression: ProcessorChargeProgression,
    state_code: Option<ProcessorChargeStateCode>,
) -> Result<ProcessorCharge, ProcessorChargeStoreError> {
    transition_processor_charge(
        connection,
        charge_id,
        expected_progressions,
        progression,
        state_code,
        false,
    )
    .await
}

pub(crate) async fn observe_processor_charge(
    connection: &mut PgConnection,
    attempt: &PaymentAttempt,
    evidence: &ProcessorEvidence,
    initial_progression: ProcessorChargeProgression,
) -> Result<ObservedCharge, ProcessorChargeStoreError> {
    let identity = attempt.identity();
    let transaction_id = evidence.transaction_id().map(GatewayTransactionId::expose);
    let has_existing_charge: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM billing_processor_charges WHERE attempt_id = $1)",
    )
    .bind(identity.attempt_id().as_uuid())
    .fetch_one(&mut *connection)
    .await?;
    if let Some(transaction_id) = transaction_id {
        if owned_by_other_attempt(connection, attempt, transaction_id).await? {
            return Ok(ObservedCharge::OwnedByOtherAttempt);
        }
        if let Some(charge) = identify_transactionless(
            connection,
            attempt,
            evidence,
            transaction_id,
            initial_progression,
        )
        .await?
        {
            return Ok(ObservedCharge::Owned(charge));
        }
    }
    let role = if has_existing_charge {
        ProcessorChargeRole::Additional
    } else {
        ProcessorChargeRole::Primary
    };
    let identified = transaction_id.is_some();
    let progression = initial_charge_progression(
        role,
        attempt.request().amount().cents(),
        identified,
        initial_progression,
    );
    let descriptor = evidence.descriptor();
    let conflict_clause = if identified {
        "ON CONFLICT (gateway_account_id, gateway_transaction_id) \
         WHERE billing_canonical_gateway_transaction_id(gateway_transaction_id) IS NOT NULL \
         DO NOTHING RETURNING id"
    } else {
        "ON CONFLICT (attempt_id) \
         WHERE billing_canonical_gateway_transaction_id(gateway_transaction_id) IS NULL \
         DO NOTHING RETURNING id"
    };
    let insert = format!(
        r#"
        INSERT INTO billing_processor_charges (
            id, attempt_id, billing_scope_id, gateway_account_id, gateway_order_id,
            gateway_transaction_id, gateway_payment_method_reference,
            gateway_response, gateway_response_code, gateway_response_text,
            gateway_condition, payment_type, card_brand, card_last4,
            card_exp_month, card_exp_year, charge_role, progression_state, state_code,
            reconciliation_required_at, external_reversal_required_at, applied_at,
            attempt_kind, plan_key, host_charge_target_id, amount_cents, currency
        ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
            $14, $15, $16, $17, $18, $19,
            CASE WHEN $18 = 'reconciliation_required' THEN clock_timestamp() END,
            CASE WHEN $18 = 'external_reversal_required' THEN clock_timestamp() END,
            CASE WHEN $18 = 'applied' THEN clock_timestamp() END,
            $20, $21, $22, $23, $24
        )
        {conflict_clause}
        "#
    );
    let inserted = sqlx::query_scalar::<_, Uuid>(&insert)
        .bind(Uuid::now_v7())
        .bind(identity.attempt_id().as_uuid())
        .bind(identity.billing_scope_id().as_uuid())
        .bind(identity.gateway_account_id().as_uuid())
        .bind(attempt.request().gateway_order_id().expose())
        .bind(transaction_id)
        .bind(
            evidence
                .payment_method_reference()
                .map(|value| value.expose()),
        )
        .bind(evidence.response().map(GatewayDiagnostic::expose))
        .bind(evidence.response_code().map(GatewayDiagnostic::expose))
        .bind(evidence.response_text().map(GatewayDiagnostic::expose))
        .bind(evidence.condition().map(GatewayDiagnostic::expose))
        .bind(descriptor.payment_type().map(GatewayDiagnostic::expose))
        .bind(descriptor.card_brand().map(GatewayDiagnostic::expose))
        .bind(descriptor.card_last_four().map(|value| value.expose()))
        .bind(descriptor.card_exp_month())
        .bind(descriptor.card_exp_year())
        .bind(role.as_str())
        .bind(progression.as_str())
        .bind(Option::<&str>::None)
        .bind(attempt.kind().as_str())
        .bind(attempt.request().target().plan_key().map(PlanKey::as_str))
        .bind(
            attempt
                .request()
                .target()
                .host_charge_target_id()
                .map(|value| *value.as_uuid()),
        )
        .bind(attempt.request().amount().cents())
        .bind(attempt.request().amount().currency().as_str())
        .fetch_optional(&mut *connection)
        .await?;
    if let Some(id) = inserted {
        return Ok(ObservedCharge::Owned(ChargeRecord {
            id,
            role,
            exact_replay: false,
        }));
    }
    if let Some(row) = matching_charge(connection, attempt, evidence, transaction_id).await? {
        if !row.try_get::<bool, _>("evidence_matches")? {
            return Err(ProcessorChargeStoreError::InvalidState(
                "processor charge replay evidence changed",
            ));
        }
        return Ok(ObservedCharge::Owned(ChargeRecord {
            id: row.try_get("id")?,
            role: parse_role(&row.try_get::<String, _>("charge_role")?)?,
            exact_replay: true,
        }));
    }
    if let Some(transaction_id) = transaction_id
        && owned_by_other_attempt(connection, attempt, transaction_id).await?
    {
        return Ok(ObservedCharge::OwnedByOtherAttempt);
    }
    Err(ProcessorChargeStoreError::InvalidState(
        INVALID_CHARGE_STATE,
    ))
}

pub(crate) async fn transition_charge(
    connection: &mut PgConnection,
    charge_id: Uuid,
    progression: ProcessorChargeProgression,
    resolution_code: Option<PaymentResolutionCode>,
) -> Result<(), ProcessorChargeStoreError> {
    const EXPECTED: &[ProcessorChargeProgression] = &[
        ProcessorChargeProgression::Pending,
        ProcessorChargeProgression::ReconciliationRequired,
        ProcessorChargeProgression::ExternalReversalRequired,
        ProcessorChargeProgression::Applied,
    ];
    transition_processor_charge(
        connection,
        ProcessorChargeId::new(charge_id),
        EXPECTED,
        progression,
        resolution_code.map(ProcessorChargeStateCode::PaymentResolution),
        true,
    )
    .await?;
    Ok(())
}

async fn transition_processor_charge(
    connection: &mut PgConnection,
    charge_id: ProcessorChargeId,
    expected_progressions: &[ProcessorChargeProgression],
    progression: ProcessorChargeProgression,
    state_code: Option<ProcessorChargeStateCode>,
    preserve_existing_state_code: bool,
) -> Result<ProcessorCharge, ProcessorChargeStoreError> {
    if expected_progressions.is_empty() {
        return Err(ProcessorChargeStoreError::InvalidState(
            "processor charge transition requires an expected state",
        ));
    }
    let expected_progressions = expected_progressions
        .iter()
        .map(|progression| progression.as_str())
        .collect::<Vec<_>>();
    let row = sqlx::query(
        r#"
        UPDATE billing_processor_charges
        SET progression_state = $3,
            state_code = CASE WHEN $5 THEN COALESCE(state_code, $4) ELSE $4 END,
            reconciliation_required_at = CASE WHEN $3 = 'reconciliation_required'
                THEN COALESCE(reconciliation_required_at, clock_timestamp())
                ELSE NULL END,
            external_reversal_required_at = CASE WHEN $3 = 'external_reversal_required'
                THEN COALESCE(external_reversal_required_at, clock_timestamp())
                ELSE NULL END,
            applied_at = CASE WHEN $3 = 'applied'
                THEN COALESCE(applied_at, clock_timestamp()) ELSE NULL END,
            externally_reversed_at = CASE WHEN $3 = 'externally_reversed'
                THEN COALESCE(externally_reversed_at, clock_timestamp()) ELSE NULL END,
            updated_at = clock_timestamp()
        WHERE id = $1 AND progression_state = ANY($2::text[])
            AND (
                $3 <> ALL(ARRAY['external_reversal_required'::text, 'externally_reversed'::text])
                OR (
                    billing_canonical_gateway_transaction_id(gateway_transaction_id) IS NOT NULL
                    AND amount_cents > 0
                    AND attempt_kind <> 'subscription_payment_method_update'
                )
            )
            AND (
                $3 <> 'applied'
                OR (
                    charge_role = 'primary'
                    AND billing_canonical_gateway_transaction_id(gateway_transaction_id) IS NOT NULL
                )
            )
        RETURNING id, attempt_id, billing_scope_id, gateway_account_id,
            gateway_order_id, attempt_kind, amount_cents, currency,
            charge_role, progression_state, state_code,
            gateway_transaction_id, gateway_payment_method_reference,
            gateway_response, gateway_response_code, gateway_response_text,
            gateway_condition, payment_type, card_brand, card_last4,
            card_exp_month, card_exp_year, observed_at
        "#,
    )
    .bind(charge_id.as_uuid())
    .bind(expected_progressions)
    .bind(progression.as_str())
    .bind(state_code.map(ProcessorChargeStateCode::as_str))
    .bind(preserve_existing_state_code)
    .fetch_optional(&mut *connection)
    .await?
    .ok_or(ProcessorChargeStoreError::InvalidState(
        "processor charge transition did not match its expected state or eligibility",
    ))?;
    processor_charge_from_row(&row).map_err(map_operator_error)
}

async fn matching_charge(
    connection: &mut PgConnection,
    attempt: &PaymentAttempt,
    evidence: &ProcessorEvidence,
    transaction_id: Option<&str>,
) -> Result<Option<sqlx::postgres::PgRow>, sqlx::Error> {
    let descriptor = evidence.descriptor();
    sqlx::query(
        r#"
        SELECT id, charge_role,
            gateway_payment_method_reference IS NOT DISTINCT FROM $3
                AND gateway_response IS NOT DISTINCT FROM $4
                AND gateway_response_code IS NOT DISTINCT FROM $5
                AND gateway_response_text IS NOT DISTINCT FROM $6
                AND gateway_condition IS NOT DISTINCT FROM $7
                AND payment_type IS NOT DISTINCT FROM $8
                AND card_brand IS NOT DISTINCT FROM $9
                AND card_last4 IS NOT DISTINCT FROM $10
                AND card_exp_month IS NOT DISTINCT FROM $11
                AND card_exp_year IS NOT DISTINCT FROM $12 AS evidence_matches
        FROM billing_processor_charges
        WHERE attempt_id = $1 AND gateway_transaction_id IS NOT DISTINCT FROM $2
        FOR UPDATE
        "#,
    )
    .bind(attempt.identity().attempt_id().as_uuid())
    .bind(transaction_id)
    .bind(
        evidence
            .payment_method_reference()
            .map(|value| value.expose()),
    )
    .bind(evidence.response().map(GatewayDiagnostic::expose))
    .bind(evidence.response_code().map(GatewayDiagnostic::expose))
    .bind(evidence.response_text().map(GatewayDiagnostic::expose))
    .bind(evidence.condition().map(GatewayDiagnostic::expose))
    .bind(descriptor.payment_type().map(GatewayDiagnostic::expose))
    .bind(descriptor.card_brand().map(GatewayDiagnostic::expose))
    .bind(descriptor.card_last_four().map(|value| value.expose()))
    .bind(descriptor.card_exp_month())
    .bind(descriptor.card_exp_year())
    .fetch_optional(connection)
    .await
}

async fn owned_by_other_attempt(
    connection: &mut PgConnection,
    attempt: &PaymentAttempt,
    transaction_id: &str,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        r#"
        SELECT EXISTS (
            SELECT 1 FROM billing_processor_charges
            WHERE gateway_account_id = $1 AND gateway_transaction_id = $2
                AND attempt_id <> $3
        )
        "#,
    )
    .bind(attempt.identity().gateway_account_id().as_uuid())
    .bind(transaction_id)
    .bind(attempt.identity().attempt_id().as_uuid())
    .fetch_one(connection)
    .await
}

async fn identify_transactionless(
    connection: &mut PgConnection,
    attempt: &PaymentAttempt,
    evidence: &ProcessorEvidence,
    transaction_id: &str,
    requested_progression: ProcessorChargeProgression,
) -> Result<Option<ChargeRecord>, ProcessorChargeStoreError> {
    let descriptor = evidence.descriptor();
    let row = sqlx::query(
        r#"
        SELECT id, charge_role,
            gateway_payment_method_reference IS NOT DISTINCT FROM $2
                AND gateway_response IS NOT DISTINCT FROM $3
                AND gateway_response_code IS NOT DISTINCT FROM $4
                AND gateway_response_text IS NOT DISTINCT FROM $5
                AND gateway_condition IS NOT DISTINCT FROM $6
                AND payment_type IS NOT DISTINCT FROM $7
                AND card_brand IS NOT DISTINCT FROM $8
                AND card_last4 IS NOT DISTINCT FROM $9
                AND card_exp_month IS NOT DISTINCT FROM $10
                AND card_exp_year IS NOT DISTINCT FROM $11 AS evidence_matches
        FROM billing_processor_charges
        WHERE attempt_id = $1
            AND billing_canonical_gateway_transaction_id(gateway_transaction_id) IS NULL
        FOR UPDATE
        "#,
    )
    .bind(attempt.identity().attempt_id().as_uuid())
    .bind(
        evidence
            .payment_method_reference()
            .map(|value| value.expose()),
    )
    .bind(evidence.response().map(GatewayDiagnostic::expose))
    .bind(evidence.response_code().map(GatewayDiagnostic::expose))
    .bind(evidence.response_text().map(GatewayDiagnostic::expose))
    .bind(evidence.condition().map(GatewayDiagnostic::expose))
    .bind(descriptor.payment_type().map(GatewayDiagnostic::expose))
    .bind(descriptor.card_brand().map(GatewayDiagnostic::expose))
    .bind(descriptor.card_last_four().map(|value| value.expose()))
    .bind(descriptor.card_exp_month())
    .bind(descriptor.card_exp_year())
    .fetch_optional(&mut *connection)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    if !row.try_get::<bool, _>("evidence_matches")? {
        return Ok(None);
    }
    let id = row.try_get("id")?;
    let role = parse_role(&row.try_get::<String, _>("charge_role")?)?;
    sqlx::query(
        "UPDATE billing_processor_charges SET gateway_transaction_id = $2, updated_at = clock_timestamp() WHERE id = $1",
    )
    .bind(id)
    .bind(transaction_id)
    .execute(&mut *connection)
    .await?;
    let progression = initial_charge_progression(
        role,
        attempt.request().amount().cents(),
        true,
        requested_progression,
    );
    sqlx::query(
        r#"
        UPDATE billing_processor_charges
        SET progression_state = $2, state_code = $3,
            reconciliation_required_at = CASE WHEN $2 = 'reconciliation_required'
                THEN COALESCE(reconciliation_required_at, clock_timestamp())
                ELSE NULL END,
            external_reversal_required_at = CASE WHEN $2 = 'external_reversal_required'
                THEN COALESCE(external_reversal_required_at, clock_timestamp())
                ELSE NULL END,
            applied_at = CASE WHEN $2 = 'applied'
                THEN COALESCE(applied_at, clock_timestamp()) ELSE NULL END,
            updated_at = clock_timestamp()
        WHERE id = $1
        "#,
    )
    .bind(id)
    .bind(progression.as_str())
    .bind(Option::<&str>::None)
    .execute(&mut *connection)
    .await?;
    Ok(Some(ChargeRecord {
        id,
        role,
        exact_replay: false,
    }))
}

async fn charge_by_id(
    connection: &mut PgConnection,
    charge_id: Uuid,
) -> Result<ProcessorCharge, ProcessorChargeStoreError> {
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
    .bind(charge_id)
    .fetch_one(connection)
    .await?;
    processor_charge_from_row(&row).map_err(map_operator_error)
}

fn compensating_progression(
    attempt: &PaymentAttempt,
    evidence: &ProcessorEvidence,
) -> ProcessorChargeProgression {
    if attempt.status() == PaymentAttemptStatus::Approved {
        ProcessorChargeProgression::Applied
    } else if attempt.request().amount().cents() > 0
        && evidence.transaction_id().is_some()
        && (matches!(
            attempt.status(),
            PaymentAttemptStatus::Declined | PaymentAttemptStatus::Failed
        ) || attempt.state().resolution_code()
            == Some(PaymentResolutionCode::SubscriptionInitialCurrentGrantConflict)
            || is_external_reversal_terminal_attempt(attempt))
    {
        ProcessorChargeProgression::ExternalReversalRequired
    } else if matches!(
        attempt.status(),
        PaymentAttemptStatus::Declined
            | PaymentAttemptStatus::Failed
            | PaymentAttemptStatus::ReviewRequired
    ) {
        ProcessorChargeProgression::ReconciliationRequired
    } else {
        ProcessorChargeProgression::Pending
    }
}

fn is_external_reversal_terminal_attempt(attempt: &PaymentAttempt) -> bool {
    attempt.status() == PaymentAttemptStatus::Failed
        && matches!(
            attempt.state().resolution_code(),
            Some(
                PaymentResolutionCode::SubscriptionInitialExternallyRefunded
                    | PaymentResolutionCode::SubscriptionInitialExternallyVoided
                    | PaymentResolutionCode::ProcessorChargeExternallyRefunded
                    | PaymentResolutionCode::ProcessorChargeExternallyVoided
            )
        )
}

fn initial_charge_progression(
    role: ProcessorChargeRole,
    amount_cents: i32,
    identified: bool,
    requested: ProcessorChargeProgression,
) -> ProcessorChargeProgression {
    if !identified {
        return ProcessorChargeProgression::ReconciliationRequired;
    }
    match role {
        ProcessorChargeRole::Primary => requested,
        ProcessorChargeRole::Additional if amount_cents > 0 => {
            ProcessorChargeProgression::ExternalReversalRequired
        }
        ProcessorChargeRole::Additional => ProcessorChargeProgression::ReconciliationRequired,
    }
}

fn initial_charge_state_code(
    role: ProcessorChargeRole,
    progression: ProcessorChargeProgression,
    identified: bool,
) -> Option<ProcessorChargeStateCode> {
    match (role, progression, identified) {
        (
            ProcessorChargeRole::Additional,
            ProcessorChargeProgression::ExternalReversalRequired,
            true,
        ) => Some(ProcessorChargeStateCode::AdditionalApprovedChargeIdentified),
        (_, ProcessorChargeProgression::ReconciliationRequired, false) => {
            Some(ProcessorChargeStateCode::TransactionIdentityRequired)
        }
        (
            ProcessorChargeRole::Primary,
            ProcessorChargeProgression::ReconciliationRequired,
            true,
        ) => Some(ProcessorChargeStateCode::ApprovedChargeWaitingForApplication),
        (
            ProcessorChargeRole::Additional,
            ProcessorChargeProgression::ReconciliationRequired,
            true,
        ) => Some(ProcessorChargeStateCode::ZeroAmountAdditionalApprovedCharge),
        _ => None,
    }
}

fn parse_role(value: &str) -> Result<ProcessorChargeRole, ProcessorChargeStoreError> {
    match value {
        "primary" => Ok(ProcessorChargeRole::Primary),
        "additional" => Ok(ProcessorChargeRole::Additional),
        _ => Err(ProcessorChargeStoreError::InvalidState(
            INVALID_CHARGE_STATE,
        )),
    }
}

fn is_transient(error: &ProcessorChargeStoreError) -> bool {
    let database_error = match error {
        ProcessorChargeStoreError::Sql(sqlx::Error::Database(error))
        | ProcessorChargeStoreError::Attempt(PaymentAttemptStoreError::Sql(
            sqlx::Error::Database(error),
        )) => error,
        _ => return false,
    };
    matches!(
        database_error.code().as_deref(),
        Some("40001" | "40P01" | "55P03" | "57014")
    )
}

fn map_operator_error(error: OperatorReviewError) -> ProcessorChargeStoreError {
    match error {
        OperatorReviewError::Sql(error) => ProcessorChargeStoreError::Sql(error),
        OperatorReviewError::InvalidState(message) => {
            ProcessorChargeStoreError::InvalidState(message)
        }
        OperatorReviewError::Host(_)
        | OperatorReviewError::ManualFailureHost(_)
        | OperatorReviewError::BillingTransaction(_)
        | OperatorReviewError::BillingEvent(_) => {
            ProcessorChargeStoreError::InvalidState(INVALID_CHARGE_STATE)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use syrup_rail::{
        GatewayDiagnostic, GatewayPaymentDescriptor, GatewayTransactionId, PaymentAttemptId,
    };

    use super::*;
    use crate::test_support::{GatewayAccountFixture, TestDatabase, create_gateway_account};

    async fn insert_host_charge_attempt(
        pool: &PgPool,
        gateway: GatewayAccountFixture,
        gateway_order_id: &str,
    ) -> Result<PaymentAttemptId, sqlx::Error> {
        let attempt_id = PaymentAttemptId::new(Uuid::now_v7());
        let host_charge_target_id = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO billing_payment_attempts (
                id, billing_scope_id, subscriber_id, host_charge_target_id,
                attempt_kind, status, idempotency_key, request_fingerprint,
                amount_cents, currency, gateway_account_id,
                gateway_configuration_id, gateway_order_id
            ) VALUES (
                $1, $2, $3, $4, 'host_charge', 'pending', $5, $6,
                100, 'USD', $7, $8, $9
            )
            "#,
        )
        .bind(attempt_id.as_uuid())
        .bind(gateway.billing_scope_id)
        .bind(Uuid::now_v7())
        .bind(host_charge_target_id)
        .bind(format!("charge-test-{attempt_id}"))
        .bind(format!("host_charge:{host_charge_target_id}:100:USD"))
        .bind(gateway.gateway_account_id)
        .bind(gateway.gateway_configuration_id)
        .bind(gateway_order_id)
        .execute(pool)
        .await?;
        Ok(attempt_id)
    }

    fn approved_evidence(transaction_id: &str) -> ProcessorEvidence {
        ProcessorEvidence::new(
            Some(GatewayTransactionId::new(transaction_id).unwrap()),
            None,
            Some(GatewayDiagnostic::new("1")),
            Some(GatewayDiagnostic::new("100")),
            Some(GatewayDiagnostic::new("Approved")),
            Some(GatewayDiagnostic::new("complete")),
            GatewayPaymentDescriptor::default(),
        )
    }

    #[tokio::test]
    async fn compensating_store_retries_transient_database_failures() -> Result<(), Box<dyn Error>>
    {
        let database = TestDatabase::start("rail_chg_retry").await?;
        let result = async {
            let gateway = create_gateway_account(&database.pool, "test_gateway").await?;
            let attempt_id =
                insert_host_charge_attempt(&database.pool, gateway, "retry-order").await?;
            sqlx::raw_sql(
                r#"
                CREATE SEQUENCE processor_charge_retry_sequence;
                CREATE FUNCTION fail_first_processor_charge_writes()
                RETURNS trigger LANGUAGE plpgsql AS $$
                BEGIN
                    IF nextval('processor_charge_retry_sequence') <= 2 THEN
                        RAISE EXCEPTION 'transient test failure' USING ERRCODE = '40001';
                    END IF;
                    RETURN NEW;
                END;
                $$;
                CREATE TRIGGER fail_first_processor_charge_writes
                BEFORE INSERT ON billing_processor_charges
                FOR EACH ROW EXECUTE FUNCTION fail_first_processor_charge_writes();
                "#,
            )
            .execute(&database.pool)
            .await?;

            let order = GatewayOrderId::from_correlation("retry-order")?;
            let outcome = store_compensating_processor_charge(
                &database.pool,
                attempt_id,
                &order,
                &approved_evidence("txn_retry"),
            )
            .await?;
            assert_eq!(outcome, CompensatingProcessorChargeOutcome::Observed);
            let attempts: i64 = sqlx::query_scalar(
                "SELECT last_value::bigint FROM processor_charge_retry_sequence",
            )
            .fetch_one(&database.pool)
            .await?;
            assert_eq!(attempts, 3);
            let count: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM billing_processor_charges WHERE attempt_id = $1",
            )
            .bind(attempt_id.as_uuid())
            .fetch_one(&database.pool)
            .await?;
            assert_eq!(count, 1);
            Ok::<_, Box<dyn Error>>(())
        }
        .await;
        let cleanup = database.cleanup().await;
        result?;
        cleanup
    }

    #[tokio::test]
    async fn transaction_observation_preserves_exact_replay_and_rejects_drift()
    -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("rail_charge_tx").await?;
        let result = async {
            let gateway = create_gateway_account(&database.pool, "test_gateway").await?;
            let attempt_id =
                insert_host_charge_attempt(&database.pool, gateway, "tx-order").await?;
            let order = GatewayOrderId::from_correlation("tx-order")?;
            let evidence = approved_evidence("txn_transactional");
            let mut transaction = database.pool.begin().await?;
            let observed = observe_processor_charge_in_transaction(
                &mut transaction,
                attempt_id,
                &order,
                &evidence,
                ProcessorChargeProgression::ReconciliationRequired,
            )
            .await?;
            assert!(matches!(
                observed,
                ProcessorChargeObservationOutcome::Observed(_)
            ));
            let replay = observe_processor_charge_in_transaction(
                &mut transaction,
                attempt_id,
                &order,
                &evidence,
                ProcessorChargeProgression::ReconciliationRequired,
            )
            .await?;
            assert!(matches!(
                replay,
                ProcessorChargeObservationOutcome::ExactReplay(_)
            ));
            let mut changed = approved_evidence("txn_transactional");
            changed = ProcessorEvidence::new(
                changed.transaction_id().cloned(),
                changed.payment_method_reference().cloned(),
                changed.response().cloned(),
                changed.response_code().cloned(),
                Some(GatewayDiagnostic::new("Changed")),
                changed.condition().cloned(),
                changed.descriptor().clone(),
            );
            let drift = observe_processor_charge_in_transaction(
                &mut transaction,
                attempt_id,
                &order,
                &changed,
                ProcessorChargeProgression::ReconciliationRequired,
            )
            .await;
            assert!(matches!(
                drift,
                Err(ProcessorChargeStoreError::InvalidState(
                    "processor charge replay evidence changed"
                ))
            ));
            transaction.rollback().await?;
            Ok::<_, Box<dyn Error>>(())
        }
        .await;
        let cleanup = database.cleanup().await;
        result?;
        cleanup
    }
}
