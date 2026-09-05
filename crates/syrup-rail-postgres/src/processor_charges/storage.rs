use super::*;

pub(super) async fn matching_charge(
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

pub(super) async fn owned_by_other_attempt(
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

pub(super) async fn identify_transactionless(
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

pub(super) async fn charge_by_id(
    connection: &mut PgConnection,
    charge_id: Uuid,
) -> Result<ProcessorCharge, ProcessorChargeStoreError> {
    let row = sqlx::query(
        r#"
        SELECT id, attempt_id, billing_scope_id, gateway_account_id,
            gateway_order_id, attempt_kind, amount_cents, currency,
            charge_role, progression_state, state_code,
            gateway_transaction_id, gateway_payment_method_reference,
            gateway_approval_evidence, gateway_response, gateway_response_code, gateway_response_text,
            gateway_condition, payment_type, card_brand, card_last4,
            card_exp_month, card_exp_year, observed_at
        FROM billing_processor_charges WHERE id = $1 FOR UPDATE
        "#,
    )
    .bind(charge_id)
    .fetch_one(connection)
    .await?;
    processor_charge_from_row(&row).map_err(ProcessorChargeStoreError::from)
}

pub(super) fn compensating_progression(
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

pub(super) fn is_external_reversal_terminal_attempt(attempt: &PaymentAttempt) -> bool {
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

pub(super) fn initial_charge_progression(
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

pub(super) fn initial_charge_state_code(
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

pub(super) fn parse_role(value: &str) -> Result<ProcessorChargeRole, ProcessorChargeStoreError> {
    crate::processor_charge_persistence::decode_processor_charge_role(value).ok_or(
        ProcessorChargeStoreError::InvalidState(INVALID_CHARGE_STATE),
    )
}

pub(super) fn is_transient(error: &ProcessorChargeStoreError) -> bool {
    let database_error = match error {
        ProcessorChargeStoreError::Sql(sqlx::Error::Database(error))
        | ProcessorChargeStoreError::Attempt(PaymentAttemptStoreError::Sql(
            sqlx::Error::Database(error),
        )) => error,
        _ => return false,
    };
    database_error
        .code()
        .as_deref()
        .is_some_and(crate::transaction_support::is_transient_sqlstate)
}
