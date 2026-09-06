use super::*;

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
            charges.gateway_approval_evidence AS review_charge_gateway_approval_evidence,
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

fn processor_charge_from_review_row(row: &PgRow) -> Result<ProcessorCharge, OperatorReviewError> {
    let attempt_id = PaymentAttemptId::new(row.try_get("review_charge_attempt_id")?);
    let currency = CurrencyCode::new(&row.try_get::<String, _>("review_charge_currency")?)
        .map_err(|_| OperatorReviewError::InvalidState(INVALID_OPERATOR_STATE))?;
    let amount = Money::new(row.try_get("review_charge_amount_cents")?, currency)
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
        row.try_get::<String, _>("review_charge_gateway_approval_evidence")?
            .parse()
            .map_err(|_| OperatorReviewError::InvalidState(INVALID_OPERATOR_STATE))?,
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
