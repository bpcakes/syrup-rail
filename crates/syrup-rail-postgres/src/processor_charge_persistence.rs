use sqlx::{PgConnection, Row, postgres::PgRow};
use syrup_rail::{
    ActorId, BillingScopeId, ChargeAmount, CurrencyCode, ExternalReversalAttestation,
    ExternalReversalKind, ExternalReversalReason, GatewayAccountId, GatewayConfigurationId,
    GatewayOrderId, GatewayTransactionId, Money, PaymentAttempt, PaymentAttemptId,
    PaymentAttemptKind, PaymentResolutionCode, ProcessorCharge, ProcessorChargeId,
    ProcessorChargeProgression, ProcessorChargeRole, ProcessorChargeStateCode,
};
use uuid::Uuid;

use crate::{
    attempts::processor_evidence_from_row,
    operator_review::{INVALID_OPERATOR_STATE, OperatorReviewError},
};

pub(crate) fn processor_charge_from_row(
    row: &PgRow,
) -> Result<ProcessorCharge, OperatorReviewError> {
    let attempt_id = PaymentAttemptId::new(row.try_get("attempt_id")?);
    let currency = CurrencyCode::new(&row.try_get::<String, _>("currency")?)
        .map_err(|_| OperatorReviewError::InvalidState(INVALID_OPERATOR_STATE))?;
    let amount = Money::new(row.try_get("amount_cents")?, currency)
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
        && attestation.amount().money() == charge.amount()
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

pub(crate) fn expected_prior_resolution_code(
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

pub(crate) fn expected_final_resolution_code(
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

pub(crate) fn parse_kind(value: &str) -> Result<PaymentAttemptKind, OperatorReviewError> {
    value
        .parse()
        .map_err(|_| OperatorReviewError::InvalidState(INVALID_OPERATOR_STATE))
}

pub(crate) fn parse_role(value: &str) -> Result<ProcessorChargeRole, OperatorReviewError> {
    match value {
        "primary" => Ok(ProcessorChargeRole::Primary),
        "additional" => Ok(ProcessorChargeRole::Additional),
        _ => Err(OperatorReviewError::InvalidState(INVALID_OPERATOR_STATE)),
    }
}

pub(crate) fn parse_progression(
    value: &str,
) -> Result<ProcessorChargeProgression, OperatorReviewError> {
    match value {
        "pending" => Ok(ProcessorChargeProgression::Pending),
        "reconciliation_required" => Ok(ProcessorChargeProgression::ReconciliationRequired),
        "external_reversal_required" => Ok(ProcessorChargeProgression::ExternalReversalRequired),
        "applied" => Ok(ProcessorChargeProgression::Applied),
        "externally_reversed" => Ok(ProcessorChargeProgression::ExternallyReversed),
        _ => Err(OperatorReviewError::InvalidState(INVALID_OPERATOR_STATE)),
    }
}

pub(crate) fn parse_reversal_kind(
    value: &str,
) -> Result<ExternalReversalKind, OperatorReviewError> {
    match value {
        "refund" => Ok(ExternalReversalKind::Refund),
        "void" => Ok(ExternalReversalKind::Void),
        _ => Err(OperatorReviewError::InvalidState(INVALID_OPERATOR_STATE)),
    }
}

pub(crate) fn parse_charge_state_code(
    value: &str,
) -> Result<ProcessorChargeStateCode, OperatorReviewError> {
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
