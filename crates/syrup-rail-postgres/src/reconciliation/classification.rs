use super::*;

pub(super) async fn count_pending_processor_charges(
    pool: &PgPool,
    gateway_account_id: GatewayAccountId,
) -> Result<u64, sqlx::Error> {
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM billing_processor_charges WHERE gateway_account_id = $1 AND progression_state = 'pending'",
    )
    .bind(gateway_account_id.as_uuid())
    .fetch_one(pool)
    .await?;
    u64::try_from(count).map_err(|_| invalid_reconciliation_state())
}

pub(super) async fn attempt_locator(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    attempt_id: Uuid,
) -> Result<Option<AttemptLocator>, sqlx::Error> {
    let row = sqlx::query(
        r#"
        SELECT id, billing_scope_id, subscriber_id, plan_key,
            gateway_account_id, attempt_kind
        FROM billing_payment_attempts
        WHERE id = $1
        "#,
    )
    .bind(attempt_id)
    .fetch_optional(&mut **transaction)
    .await?;
    row.as_ref().map(attempt_locator_from_row).transpose()
}

pub(super) fn attempt_locator_from_row(
    row: &sqlx::postgres::PgRow,
) -> Result<AttemptLocator, sqlx::Error> {
    let kind = row
        .try_get::<String, _>("attempt_kind")?
        .parse::<PaymentAttemptKind>()
        .map_err(|_| invalid_reconciliation_state())?;
    let plan_key = row
        .try_get::<Option<String>, _>("plan_key")?
        .map(PlanKey::new)
        .transpose()
        .map_err(|_| invalid_reconciliation_state())?;
    if (kind == PaymentAttemptKind::HostCharge) != plan_key.is_none() {
        return Err(invalid_reconciliation_state());
    }
    Ok(AttemptLocator {
        id: row.try_get("id")?,
        billing_scope_id: BillingScopeId::new(row.try_get("billing_scope_id")?),
        subscriber_id: SubscriberId::new(row.try_get("subscriber_id")?),
        plan_key,
        gateway_account_id: GatewayAccountId::new(row.try_get("gateway_account_id")?),
        kind,
    })
}

pub(super) async fn lock_attempt_for_classification(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    locator: AttemptLocator,
) -> Result<Option<LockedAttempt>, sqlx::Error> {
    let row = sqlx::query(
        r#"
        SELECT id, billing_scope_id, subscriber_id, plan_key,
            gateway_account_id, attempt_kind, status, resolution_code,
            amount_cents,
            billing_canonical_gateway_transaction_id(
                gateway_transaction_id
            ) AS transaction_id
        FROM billing_payment_attempts
        WHERE id = $1
        FOR UPDATE SKIP LOCKED
        "#,
    )
    .bind(locator.id)
    .fetch_optional(&mut **transaction)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let locked_locator = attempt_locator_from_row(&row)?;
    if locked_locator != locator {
        return Err(invalid_reconciliation_state());
    }
    let status = row
        .try_get::<String, _>("status")?
        .parse::<PaymentAttemptStatus>()
        .map_err(|_| invalid_reconciliation_state())?;
    let resolution_code = row
        .try_get::<Option<String>, _>("resolution_code")?
        .as_deref()
        .map(PaymentResolutionCode::try_from)
        .transpose()
        .map_err(|_| invalid_reconciliation_state())?;
    Ok(Some(LockedAttempt {
        locator,
        status,
        resolution_code,
        amount_cents: row.try_get("amount_cents")?,
        transaction_id: row.try_get("transaction_id")?,
    }))
}

pub(super) async fn lock_pending_charge_for_classification(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    charge_id: Uuid,
    attempt_id: Uuid,
) -> Result<Option<(ChargeRole, Option<String>, bool, bool)>, sqlx::Error> {
    let row = sqlx::query(
        r#"
        SELECT charges.charge_role,
            billing_canonical_gateway_transaction_id(
                charges.gateway_transaction_id
            ) AS transaction_id,
            CASE
                WHEN billing_canonical_gateway_transaction_id(
                    attempts.gateway_transaction_id
                ) IS NOT NULL
                    AND billing_canonical_gateway_transaction_id(
                        charges.gateway_transaction_id
                    ) IS NOT NULL
                THEN billing_canonical_gateway_transaction_id(
                    attempts.gateway_transaction_id
                ) = billing_canonical_gateway_transaction_id(
                    charges.gateway_transaction_id
                )
                WHEN billing_canonical_gateway_transaction_id(
                    attempts.gateway_transaction_id
                ) IS NULL
                    AND billing_canonical_gateway_transaction_id(
                        charges.gateway_transaction_id
                    ) IS NULL
                THEN attempts.gateway_order_id = charges.gateway_order_id
                    AND attempts.gateway_payment_method_reference
                        IS NOT DISTINCT FROM charges.gateway_payment_method_reference
                    AND attempts.gateway_response
                        IS NOT DISTINCT FROM charges.gateway_response
                    AND attempts.gateway_response_code
                        IS NOT DISTINCT FROM charges.gateway_response_code
                    AND attempts.gateway_response_text
                        IS NOT DISTINCT FROM charges.gateway_response_text
                    AND attempts.gateway_condition
                        IS NOT DISTINCT FROM charges.gateway_condition
                    AND attempts.payment_type IS NOT DISTINCT FROM charges.payment_type
                    AND attempts.card_brand IS NOT DISTINCT FROM charges.card_brand
                    AND attempts.card_last4 IS NOT DISTINCT FROM charges.card_last4
                    AND attempts.card_exp_month
                        IS NOT DISTINCT FROM charges.card_exp_month
                    AND attempts.card_exp_year
                        IS NOT DISTINCT FROM charges.card_exp_year
                ELSE false
            END AS same_charge,
            charges.attempt_id = attempts.id
                AND charges.billing_scope_id = attempts.billing_scope_id
                AND charges.gateway_account_id = attempts.gateway_account_id
                AND charges.gateway_order_id = attempts.gateway_order_id
                AND charges.attempt_kind = attempts.attempt_kind
                AND charges.plan_key IS NOT DISTINCT FROM attempts.plan_key
                AND charges.host_charge_target_id
                    IS NOT DISTINCT FROM attempts.host_charge_target_id
                AND charges.amount_cents = attempts.amount_cents
                AND charges.currency = attempts.currency AS dimensions_match
        FROM billing_processor_charges charges
        INNER JOIN billing_payment_attempts attempts
            ON attempts.id = charges.attempt_id
        WHERE charges.id = $1 AND attempts.id = $2
            AND charges.progression_state = 'pending'
        FOR UPDATE OF charges SKIP LOCKED
        "#,
    )
    .bind(charge_id)
    .bind(attempt_id)
    .fetch_optional(&mut **transaction)
    .await?;
    row.map(|row| {
        let role = match row.try_get::<String, _>("charge_role")?.as_str() {
            "primary" => ChargeRole::Primary,
            "additional" => ChargeRole::Additional,
            _ => return Err(invalid_reconciliation_state()),
        };
        Ok((
            role,
            row.try_get("transaction_id")?,
            row.try_get("same_charge")?,
            row.try_get("dimensions_match")?,
        ))
    })
    .transpose()
}

pub(super) async fn classify_pending_charge(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    attempt: &LockedAttempt,
    charge_id: Uuid,
    role: ChargeRole,
    transaction_id: Option<&str>,
    same_charge: bool,
) -> Result<(ChargeProgression, Option<String>), sqlx::Error> {
    let Some(transaction_id) = transaction_id else {
        return Ok((
            ChargeProgression::ReconciliationRequired,
            Some("processor_charge_transaction_identity_required".to_owned()),
        ));
    };
    let attestation = sqlx::query_as::<_, (Uuid, String)>(
        r#"
        SELECT processor_charge_id, final_resolution_code
        FROM billing_external_reversal_attestations
        WHERE attempt_id = $1 AND gateway_transaction_id = $2
        FOR UPDATE
        "#,
    )
    .bind(attempt.locator.id)
    .bind(transaction_id)
    .fetch_optional(&mut **transaction)
    .await?;
    if let Some((attested_charge_id, final_resolution_code)) = attestation {
        if attested_charge_id != charge_id {
            return Err(invalid_reconciliation_state());
        }
        let final_resolution_code = PaymentResolutionCode::try_from(final_resolution_code.as_str())
            .map_err(|_| invalid_reconciliation_state())?;
        return Ok((
            ChargeProgression::ExternallyReversed,
            Some(final_resolution_code.as_str().to_owned()),
        ));
    }

    let terminal_external_reversal = attempt.status == PaymentAttemptStatus::Failed
        && matches!(
            attempt.resolution_code,
            Some(
                PaymentResolutionCode::SubscriptionInitialExternallyRefunded
                    | PaymentResolutionCode::SubscriptionInitialExternallyVoided
                    | PaymentResolutionCode::ProcessorChargeExternallyRefunded
                    | PaymentResolutionCode::ProcessorChargeExternallyVoided
            )
        );
    let initial_grant_conflict = attempt.locator.kind == PaymentAttemptKind::SubscriptionInitial
        && attempt.resolution_code
            == Some(PaymentResolutionCode::SubscriptionInitialCurrentGrantConflict);
    if attempt.amount_cents > 0
        && (role == ChargeRole::Additional
            || terminal_external_reversal
            || initial_grant_conflict
            || (attempt.transaction_id.is_some() && !same_charge))
    {
        return Ok((
            ChargeProgression::ExternalReversalRequired,
            Some(
                if role == ChargeRole::Additional {
                    "additional_approved_charge_identified"
                } else {
                    "processor_charge_external_reversal_required"
                }
                .to_owned(),
            ),
        ));
    }
    if same_charge && attempt.status == PaymentAttemptStatus::Approved {
        return Ok((ChargeProgression::Applied, None));
    }
    Ok((
        ChargeProgression::ReconciliationRequired,
        Some(
            if role == ChargeRole::Additional {
                "zero_amount_additional_approved_charge"
            } else {
                "approved_charge_waiting_for_application"
            }
            .to_owned(),
        ),
    ))
}

pub(super) async fn transition_pending_charge(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    charge_id: Uuid,
    progression: ChargeProgression,
    state_code: Option<&str>,
) -> Result<(), sqlx::Error> {
    let result = sqlx::query(
        r#"
        UPDATE billing_processor_charges
        SET progression_state = $2,
            state_code = $3,
            reconciliation_required_at = CASE
                WHEN $2 = 'reconciliation_required'
                THEN COALESCE(reconciliation_required_at, clock_timestamp())
            END,
            external_reversal_required_at = CASE
                WHEN $2 = 'external_reversal_required'
                THEN COALESCE(external_reversal_required_at, clock_timestamp())
            END,
            applied_at = CASE WHEN $2 = 'applied'
                THEN COALESCE(applied_at, clock_timestamp()) END,
            externally_reversed_at = CASE WHEN $2 = 'externally_reversed'
                THEN COALESCE(externally_reversed_at, clock_timestamp()) END,
            updated_at = clock_timestamp()
        WHERE id = $1 AND progression_state = 'pending'
            AND (
                $2 NOT IN ('external_reversal_required', 'externally_reversed')
                OR (
                    billing_canonical_gateway_transaction_id(
                        gateway_transaction_id
                    ) IS NOT NULL
                    AND amount_cents > 0
                    AND attempt_kind <> 'subscription_payment_method_update'
                )
            )
            AND (
                $2 <> 'applied'
                OR (
                    charge_role = 'primary'
                    AND billing_canonical_gateway_transaction_id(
                        gateway_transaction_id
                    ) IS NOT NULL
                )
            )
        "#,
    )
    .bind(charge_id)
    .bind(progression.as_str())
    .bind(state_code)
    .execute(&mut **transaction)
    .await?;
    if result.rows_affected() != 1 {
        return Err(invalid_reconciliation_state());
    }
    Ok(())
}

pub(super) fn invalid_reconciliation_state() -> sqlx::Error {
    sqlx::Error::Protocol("canonical reconciliation state is invalid".to_owned())
}
