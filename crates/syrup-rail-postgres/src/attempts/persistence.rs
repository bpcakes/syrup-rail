use super::*;

pub(crate) const PAYMENT_ATTEMPT_SELECT: &str = r#"
    SELECT id, billing_scope_id, subscriber_id, plan_key,
        host_charge_target_id, subscription_id, payment_method_id,
        attempt_kind, status, idempotency_key, request_fingerprint,
        amount_cents, currency, billing_period_start_at,
        billing_period_end_at, gateway_account_id,
        gateway_configuration_id, required_gateway_account_mode,
        gateway_order_id,
        gateway_transaction_id, gateway_payment_method_reference,
        gateway_response, gateway_response_code, gateway_response_text,
        gateway_condition, payment_type, card_brand, card_last4,
        card_exp_month, card_exp_year, submitted_at, resolved_at,
        created_at, updated_at, gateway_lifecycle_status,
        gateway_lifecycle_action, gateway_lifecycle_at,
        gateway_lifecycle_reconciled_at, refunded_amount_cents,
        billing_first_name, billing_email, resolution_code, review_required_at,
        payment_method_update_expected_payment_method_id,
        payment_method_update_expected_initial_transaction_id,
        subscription_expected_payment_method_id,
        subscription_expected_initial_transaction_id,
        subscription_expected_status,
        subscription_initial_discount_claim_id,
        subscription_initial_discount_code_id,
        subscription_initial_discount_code_snapshot,
        subscription_initial_discount_label_snapshot,
        subscription_initial_discount_kind,
        subscription_initial_discount_amount_off_cents,
        subscription_initial_discount_percent_off_bps,
        subscription_initial_discount_currency,
        subscription_initial_discount_duration,
        subscription_initial_discount_duration_months,
        subscription_initial_discount_base_amount_cents,
        subscription_initial_discount_discounted_amount_cents,
        subscription_initial_terms_version,
        subscription_initial_start_kind,
        subscription_initial_trial_amount_cents,
        subscription_initial_trial_period_kind,
        subscription_initial_trial_period_count,
        subscription_initial_recurring_base_amount_cents,
        subscription_initial_recurring_period_kind,
        subscription_initial_recurring_period_count,
        subscription_initial_dunning_retry_delays_seconds,
        subscription_initial_dunning_exhaustion,
        subscription_initial_past_due_access, billing_last_name
    FROM billing_payment_attempts
"#;

/// Loads an attempt by its exact scope and durable identity without locking it.
pub async fn find_payment_attempt_by_id_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    billing_scope_id: BillingScopeId,
    attempt_id: PaymentAttemptId,
) -> Result<Option<PaymentAttempt>, PaymentAttemptStoreError> {
    let query = format!("{PAYMENT_ATTEMPT_SELECT} WHERE billing_scope_id = $1 AND id = $2");
    let row = sqlx::query(&query)
        .bind(billing_scope_id.as_uuid())
        .bind(attempt_id.as_uuid())
        .fetch_optional(&mut **transaction)
        .await?;
    row.as_ref().map(payment_attempt_from_row).transpose()
}

pub(super) async fn find_payment_attempt_by_idempotency(
    transaction: &mut Transaction<'_, Postgres>,
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    idempotency_key: &IdempotencyKey,
) -> Result<Option<PaymentAttempt>, PaymentAttemptStoreError> {
    let query = format!(
        "{PAYMENT_ATTEMPT_SELECT} \
         WHERE billing_scope_id = $1 AND subscriber_id = $2 AND idempotency_key = $3"
    );
    let row = sqlx::query(&query)
        .bind(billing_scope_id.as_uuid())
        .bind(subscriber_id.as_uuid())
        .bind(idempotency_key.expose())
        .fetch_optional(&mut **transaction)
        .await?;
    row.as_ref().map(payment_attempt_from_row).transpose()
}

pub(super) async fn lock_payment_attempt_by_idempotency(
    transaction: &mut Transaction<'_, Postgres>,
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    idempotency_key: &IdempotencyKey,
) -> Result<Option<PaymentAttempt>, PaymentAttemptStoreError> {
    let query = format!(
        "{PAYMENT_ATTEMPT_SELECT} \
         WHERE billing_scope_id = $1 AND subscriber_id = $2 AND idempotency_key = $3 \
         FOR UPDATE"
    );
    let row = sqlx::query(&query)
        .bind(billing_scope_id.as_uuid())
        .bind(subscriber_id.as_uuid())
        .bind(idempotency_key.expose())
        .fetch_optional(&mut **transaction)
        .await?;
    row.as_ref().map(payment_attempt_from_row).transpose()
}

pub(super) async fn insert_subscription_charge_attempt(
    transaction: &mut Transaction<'_, Postgres>,
    identity: PaymentAttemptIdentity,
    request: &PaymentAttemptRequest,
) -> Result<bool, sqlx::Error> {
    let (kind, plan_key, payment_method_id, period, expected_state) = match request.target() {
        PaymentAttemptTarget::SubscriptionRenewal {
            plan_key,
            payment_method_id,
            period,
            expected_state,
        } => (
            PaymentAttemptKind::SubscriptionRenewal,
            plan_key,
            payment_method_id,
            period,
            expected_state,
        ),
        PaymentAttemptTarget::SubscriptionRecovery {
            plan_key,
            payment_method_id,
            period,
            expected_state,
        } => (
            PaymentAttemptKind::SubscriptionRecovery,
            plan_key,
            payment_method_id,
            period,
            expected_state,
        ),
        _ => {
            return Err(sqlx::Error::Protocol(
                "subscription charge attempt target is invalid".to_owned(),
            ));
        }
    };
    let contact = request.billing_contact();
    let result = sqlx::query(
        r#"
        INSERT INTO billing_payment_attempts (
            id, billing_scope_id, subscriber_id, plan_key, subscription_id,
            payment_method_id, attempt_kind, status, idempotency_key,
            request_fingerprint, amount_cents, currency,
            billing_period_start_at, billing_period_end_at,
            gateway_account_id, gateway_configuration_id,
            required_gateway_account_mode, gateway_order_id,
            billing_first_name, billing_last_name, billing_email,
            subscription_expected_payment_method_id,
            subscription_expected_initial_transaction_id,
            subscription_expected_status
        ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, 'pending', $8, $9, $10, $11,
            $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22, $23
        )
        ON CONFLICT DO NOTHING
        "#,
    )
    .bind(identity.attempt_id().as_uuid())
    .bind(identity.billing_scope_id().as_uuid())
    .bind(identity.subscriber_id().as_uuid())
    .bind(plan_key.as_str())
    .bind(expected_state.subscription_id().as_uuid())
    .bind(payment_method_id.as_uuid())
    .bind(kind.as_str())
    .bind(request.idempotency_key().expose())
    .bind(request.fingerprint().expose())
    .bind(request.amount().cents())
    .bind(request.amount().currency().as_str())
    .bind(period.start_at())
    .bind(period.end_at())
    .bind(identity.gateway_account_id().as_uuid())
    .bind(identity.gateway_configuration_id().as_uuid())
    .bind(identity.required_gateway_account_mode().as_str())
    .bind(request.gateway_order_id().expose())
    .bind(contact.first_name())
    .bind(contact.last_name())
    .bind(contact.email())
    .bind(expected_state.payment_method_id().as_uuid())
    .bind(expected_state.initial_transaction_id().expose())
    .bind(expected_state.status().as_str())
    .execute(&mut **transaction)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Observes an owner-scoped idempotency row without changing lock order.
///
/// Callers that will mutate the result must lock and revalidate it after any
/// host-owned target lock has been acquired.
pub(crate) async fn find_payment_attempt_by_idempotency_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    idempotency_key: &IdempotencyKey,
) -> Result<Option<PaymentAttempt>, PaymentAttemptStoreError> {
    find_payment_attempt_by_idempotency(
        transaction,
        billing_scope_id,
        subscriber_id,
        idempotency_key,
    )
    .await
}

/// Locks the exact owner-scoped idempotency row for replay or mutation.
pub async fn lock_payment_attempt_by_idempotency_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    idempotency_key: &IdempotencyKey,
) -> Result<Option<PaymentAttempt>, PaymentAttemptStoreError> {
    lock_payment_attempt_by_idempotency(
        transaction,
        billing_scope_id,
        subscriber_id,
        idempotency_key,
    )
    .await
}

pub(crate) async fn lock_payment_attempt_by_id_on_connection(
    connection: &mut PgConnection,
    billing_scope_id: BillingScopeId,
    attempt_id: PaymentAttemptId,
) -> Result<Option<PaymentAttempt>, PaymentAttemptStoreError> {
    let query =
        format!("{PAYMENT_ATTEMPT_SELECT} WHERE billing_scope_id = $1 AND id = $2 FOR UPDATE");
    let row = sqlx::query(&query)
        .bind(billing_scope_id.as_uuid())
        .bind(attempt_id.as_uuid())
        .fetch_optional(&mut *connection)
        .await?;
    row.as_ref().map(payment_attempt_from_row).transpose()
}

pub(crate) async fn find_payment_attempt_by_id_on_connection(
    connection: &mut PgConnection,
    billing_scope_id: BillingScopeId,
    attempt_id: PaymentAttemptId,
) -> Result<Option<PaymentAttempt>, PaymentAttemptStoreError> {
    let query = format!("{PAYMENT_ATTEMPT_SELECT} WHERE billing_scope_id = $1 AND id = $2");
    let row = sqlx::query(&query)
        .bind(billing_scope_id.as_uuid())
        .bind(attempt_id.as_uuid())
        .fetch_optional(&mut *connection)
        .await?;
    row.as_ref().map(payment_attempt_from_row).transpose()
}

pub(crate) fn payment_attempt_from_row(
    row: &PgRow,
) -> Result<PaymentAttempt, PaymentAttemptStoreError> {
    let attempt_id = PaymentAttemptId::new(row.try_get("id")?);
    let required_gateway_account_mode = row
        .try_get::<String, _>("required_gateway_account_mode")?
        .parse::<GatewayAccountMode>()
        .map_err(|_| invalid_state())?;
    let identity = PaymentAttemptIdentity::new(
        attempt_id,
        BillingScopeId::new(row.try_get("billing_scope_id")?),
        SubscriberId::new(row.try_get("subscriber_id")?),
        GatewayAccountId::new(row.try_get("gateway_account_id")?),
        GatewayConfigurationId::new(row.try_get("gateway_configuration_id")?),
        required_gateway_account_mode,
    );
    let kind = row
        .try_get::<String, _>("attempt_kind")?
        .parse::<PaymentAttemptKind>()
        .map_err(|_| invalid_state())?;
    let status = row
        .try_get::<String, _>("status")?
        .parse::<PaymentAttemptStatus>()
        .map_err(|_| invalid_state())?;
    let target = payment_attempt_target_from_row(row, kind, status)?;
    let currency =
        CurrencyCode::new(&row.try_get::<String, _>("currency")?).map_err(|_| invalid_state())?;
    let amount = Money::new(row.try_get("amount_cents")?, currency).map_err(|_| invalid_state())?;
    let order_value = row.try_get::<String, _>("gateway_order_id")?;
    let gateway_order_id = GatewayOrderId::from_generated_attempt(&order_value, attempt_id)
        .or_else(|_| GatewayOrderId::from_correlation(&order_value))
        .map_err(|_| invalid_state())?;
    let request = PaymentAttemptRequest::from_persisted_parts(
        target,
        IdempotencyKey::new(row.try_get::<String, _>("idempotency_key")?)
            .map_err(|_| invalid_state())?,
        PaymentAttemptFingerprint::new(row.try_get::<String, _>("request_fingerprint")?)
            .map_err(|_| invalid_state())?,
        amount,
        gateway_order_id,
        BillingContactSnapshot::from_parts(
            row.try_get("billing_first_name")?,
            row.try_get("billing_last_name")?,
            row.try_get("billing_email")?,
        ),
    );
    let state = PaymentAttemptState::new(
        status,
        row.try_get::<Option<String>, _>("resolution_code")?
            .as_deref()
            .map(PaymentResolutionCode::try_from)
            .transpose()
            .map_err(|_| invalid_state())?,
        processor_evidence_from_row(row)?,
        lifecycle_from_row(row)?,
        PaymentAttemptTimestamps::new(
            row.try_get("submitted_at")?,
            row.try_get("resolved_at")?,
            row.try_get("review_required_at")?,
            row.try_get("created_at")?,
            row.try_get("updated_at")?,
        ),
    );
    PaymentAttempt::new(identity, request, state).map_err(|_| invalid_state())
}

fn payment_attempt_target_from_row(
    row: &PgRow,
    kind: PaymentAttemptKind,
    status: PaymentAttemptStatus,
) -> Result<PaymentAttemptTarget, PaymentAttemptStoreError> {
    let plan_key = row
        .try_get::<Option<String>, _>("plan_key")?
        .map(PlanKey::new)
        .transpose()
        .map_err(|_| invalid_state())?;
    let host_target = row
        .try_get::<Option<Uuid>, _>("host_charge_target_id")?
        .map(HostChargeTargetId::new);
    let subscription_id = row
        .try_get::<Option<Uuid>, _>("subscription_id")?
        .map(SubscriptionId::new);
    let payment_method_id = row
        .try_get::<Option<Uuid>, _>("payment_method_id")?
        .map(PaymentMethodId::new);
    let period = period_from_row(row)?;
    let method_update_snapshot = payment_method_update_snapshot_from_row(row, subscription_id)?;
    let subscription_snapshot = subscription_snapshot_from_row(row, subscription_id)?;
    let discount = enrollment_discount_from_row(row)?;
    let initial_terms = enrollment_terms_from_row(row, plan_key.as_ref())?;

    match kind {
        PaymentAttemptKind::HostCharge
            if plan_key.is_none()
                && subscription_id.is_none()
                && payment_method_id.is_none()
                && period.is_none()
                && method_update_snapshot.is_none()
                && subscription_snapshot.is_none()
                && discount.is_none()
                && initial_terms.is_none() =>
        {
            Ok(PaymentAttemptTarget::HostCharge {
                target_id: host_target.ok_or_else(invalid_state)?,
            })
        }
        PaymentAttemptKind::SubscriptionInitial
            if host_target.is_none()
                && period.is_none()
                && method_update_snapshot.is_none()
                && subscription_snapshot.is_none() =>
        {
            let application = match (subscription_id, payment_method_id) {
                (subscription_id, Some(payment_method_id)) => Some(
                    SubscriptionInitialApplication::new(subscription_id, payment_method_id),
                ),
                (None, None) => None,
                (Some(_), None) => return Err(invalid_state()),
            };
            if matches!(
                status,
                PaymentAttemptStatus::Pending | PaymentAttemptStatus::Unknown
            ) && application.is_some()
            {
                return Err(invalid_state());
            }
            let (terms_version, offer) = initial_terms.ok_or_else(invalid_state)?;
            Ok(PaymentAttemptTarget::SubscriptionInitial {
                terms_version,
                offer,
                discount,
                application,
            })
        }
        PaymentAttemptKind::SubscriptionRenewal
            if host_target.is_none()
                && method_update_snapshot.is_none()
                && discount.is_none()
                && initial_terms.is_none() =>
        {
            Ok(PaymentAttemptTarget::SubscriptionRenewal {
                plan_key: plan_key.ok_or_else(invalid_state)?,
                payment_method_id: payment_method_id.ok_or_else(invalid_state)?,
                period: period.ok_or_else(invalid_state)?,
                expected_state: subscription_snapshot.ok_or_else(invalid_state)?,
            })
        }
        PaymentAttemptKind::SubscriptionRecovery
            if host_target.is_none()
                && method_update_snapshot.is_none()
                && discount.is_none()
                && initial_terms.is_none() =>
        {
            Ok(PaymentAttemptTarget::SubscriptionRecovery {
                plan_key: plan_key.ok_or_else(invalid_state)?,
                payment_method_id: payment_method_id.ok_or_else(invalid_state)?,
                period: period.ok_or_else(invalid_state)?,
                expected_state: subscription_snapshot.ok_or_else(invalid_state)?,
            })
        }
        PaymentAttemptKind::SubscriptionPaymentMethodUpdate
            if host_target.is_none()
                && period.is_none()
                && subscription_snapshot.is_none()
                && discount.is_none()
                && initial_terms.is_none() =>
        {
            Ok(PaymentAttemptTarget::SubscriptionPaymentMethodUpdate {
                plan_key: plan_key.ok_or_else(invalid_state)?,
                payment_method_id: payment_method_id.ok_or_else(invalid_state)?,
                expected_state: method_update_snapshot.ok_or_else(invalid_state)?,
            })
        }
        _ => Err(invalid_state()),
    }
}

fn period_from_row(row: &PgRow) -> Result<Option<BillingPeriod>, PaymentAttemptStoreError> {
    let start = row.try_get::<Option<DateTime<Utc>>, _>("billing_period_start_at")?;
    let end = row.try_get::<Option<DateTime<Utc>>, _>("billing_period_end_at")?;
    match (start, end) {
        (None, None) => Ok(None),
        (Some(start), Some(end)) => BillingPeriod::new(start, end)
            .map(Some)
            .map_err(|_| invalid_state()),
        _ => Err(invalid_state()),
    }
}

fn payment_method_update_snapshot_from_row(
    row: &PgRow,
    subscription_id: Option<SubscriptionId>,
) -> Result<Option<PaymentMethodUpdateSnapshot>, PaymentAttemptStoreError> {
    let expected_method = row
        .try_get::<Option<Uuid>, _>("payment_method_update_expected_payment_method_id")?
        .map(PaymentMethodId::new);
    let expected_transaction =
        row.try_get::<Option<String>, _>("payment_method_update_expected_initial_transaction_id")?;
    match (subscription_id, expected_method, expected_transaction) {
        (Some(subscription_id), Some(payment_method_id), Some(transaction)) => {
            Ok(Some(PaymentMethodUpdateSnapshot::new(
                subscription_id,
                payment_method_id,
                GatewayTransactionId::new(transaction).map_err(|_| invalid_state())?,
            )))
        }
        (_, None, None) => Ok(None),
        _ => Err(invalid_state()),
    }
}

fn subscription_snapshot_from_row(
    row: &PgRow,
    subscription_id: Option<SubscriptionId>,
) -> Result<Option<SubscriptionPaymentStateSnapshot>, PaymentAttemptStoreError> {
    let expected_method = row
        .try_get::<Option<Uuid>, _>("subscription_expected_payment_method_id")?
        .map(PaymentMethodId::new);
    let expected_transaction =
        row.try_get::<Option<String>, _>("subscription_expected_initial_transaction_id")?;
    let expected_status = row.try_get::<Option<String>, _>("subscription_expected_status")?;
    match (
        subscription_id,
        expected_method,
        expected_transaction,
        expected_status,
    ) {
        (Some(subscription_id), Some(payment_method_id), Some(transaction), Some(status)) => {
            Ok(Some(
                SubscriptionPaymentStateSnapshot::new(
                    subscription_id,
                    payment_method_id,
                    GatewayTransactionId::new(transaction).map_err(|_| invalid_state())?,
                    status
                        .parse::<SubscriptionStatus>()
                        .map_err(|_| invalid_state())?,
                )
                .map_err(|_| invalid_state())?,
            ))
        }
        (_, None, None, None) => Ok(None),
        _ => Err(invalid_state()),
    }
}

fn enrollment_discount_from_row(
    row: &PgRow,
) -> Result<Option<SubscriptionEnrollmentDiscountSnapshot>, PaymentAttemptStoreError> {
    let claim_id = row
        .try_get::<Option<Uuid>, _>("subscription_initial_discount_claim_id")?
        .map(DiscountClaimId::new);
    let code_id = row
        .try_get::<Option<Uuid>, _>("subscription_initial_discount_code_id")?
        .map(DiscountCodeId::new);
    let code = row.try_get::<Option<String>, _>("subscription_initial_discount_code_snapshot")?;
    let label = row.try_get::<Option<String>, _>("subscription_initial_discount_label_snapshot")?;
    let kind = row.try_get::<Option<String>, _>("subscription_initial_discount_kind")?;
    let amount_off =
        row.try_get::<Option<i32>, _>("subscription_initial_discount_amount_off_cents")?;
    let percent_off =
        row.try_get::<Option<i32>, _>("subscription_initial_discount_percent_off_bps")?;
    let currency = row.try_get::<Option<String>, _>("subscription_initial_discount_currency")?;
    let duration = row.try_get::<Option<String>, _>("subscription_initial_discount_duration")?;
    let duration_months =
        row.try_get::<Option<i32>, _>("subscription_initial_discount_duration_months")?;
    let base_amount =
        row.try_get::<Option<i32>, _>("subscription_initial_discount_base_amount_cents")?;
    let discounted_amount =
        row.try_get::<Option<i32>, _>("subscription_initial_discount_discounted_amount_cents")?;

    if claim_id.is_none()
        && code_id.is_none()
        && code.is_none()
        && label.is_none()
        && kind.is_none()
        && amount_off.is_none()
        && percent_off.is_none()
        && currency.is_none()
        && duration.is_none()
        && duration_months.is_none()
        && base_amount.is_none()
        && discounted_amount.is_none()
    {
        return Ok(None);
    }

    let kind = match (kind.as_deref(), amount_off, percent_off) {
        (Some("amount_off"), Some(value), None) => SubscriptionDiscountKind::AmountOffCents(
            PositiveDiscountCents::new(value).map_err(|_| invalid_state())?,
        ),
        (Some("percent_off"), None, Some(value)) => {
            SubscriptionDiscountKind::PercentOffBasisPoints(
                PercentOffBasisPoints::new(u16::try_from(value).map_err(|_| invalid_state())?)
                    .map_err(|_| invalid_state())?,
            )
        }
        _ => return Err(invalid_state()),
    };
    let duration = match (duration.as_deref(), duration_months) {
        (Some("indefinite"), None) => SubscriptionDiscountDuration::Indefinite,
        (Some("limited_months"), Some(value)) => SubscriptionDiscountDuration::LimitedMonths(
            LimitedDiscountMonths::new(u8::try_from(value).map_err(|_| invalid_state())?)
                .map_err(|_| invalid_state())?,
        ),
        _ => return Err(invalid_state()),
    };
    let currency = CurrencyCode::new(currency.as_deref().ok_or_else(invalid_state)?)
        .map_err(|_| invalid_state())?;
    let snapshot = SubscriptionDiscountSnapshot::new(
        SubscriptionDiscountCode::new(code.as_deref().ok_or_else(invalid_state)?)
            .map_err(|_| invalid_state())?,
        label,
        kind,
        duration,
        ChargeAmount::new(base_amount.ok_or_else(invalid_state)?, currency)
            .map_err(|_| invalid_state())?,
        ChargeAmount::new(discounted_amount.ok_or_else(invalid_state)?, currency)
            .map_err(|_| invalid_state())?,
    )
    .map_err(|_| invalid_state())?;
    Ok(Some(SubscriptionEnrollmentDiscountSnapshot::new(
        claim_id.ok_or_else(invalid_state)?,
        code_id.ok_or_else(invalid_state)?,
        snapshot,
    )))
}

fn enrollment_terms_from_row(
    row: &PgRow,
    plan_key: Option<&PlanKey>,
) -> Result<Option<(SubscriptionEnrollmentTermsVersion, SubscriptionOffer)>, PaymentAttemptStoreError>
{
    let version = row.try_get::<Option<i16>, _>("subscription_initial_terms_version")?;
    let start_kind = row.try_get::<Option<String>, _>("subscription_initial_start_kind")?;
    let trial_amount = row.try_get::<Option<i32>, _>("subscription_initial_trial_amount_cents")?;
    let trial_period_kind =
        row.try_get::<Option<String>, _>("subscription_initial_trial_period_kind")?;
    let trial_period_count =
        row.try_get::<Option<i32>, _>("subscription_initial_trial_period_count")?;
    let recurring_base =
        row.try_get::<Option<i32>, _>("subscription_initial_recurring_base_amount_cents")?;
    let recurring_period_kind =
        row.try_get::<Option<String>, _>("subscription_initial_recurring_period_kind")?;
    let recurring_period_count =
        row.try_get::<Option<i32>, _>("subscription_initial_recurring_period_count")?;
    let retry_delays =
        row.try_get::<Option<Vec<i64>>, _>("subscription_initial_dunning_retry_delays_seconds")?;
    let exhaustion = row.try_get::<Option<String>, _>("subscription_initial_dunning_exhaustion")?;
    let past_due_access =
        row.try_get::<Option<String>, _>("subscription_initial_past_due_access")?;

    if version.is_none()
        && start_kind.is_none()
        && trial_amount.is_none()
        && trial_period_kind.is_none()
        && trial_period_count.is_none()
        && recurring_base.is_none()
        && recurring_period_kind.is_none()
        && recurring_period_count.is_none()
        && retry_delays.is_none()
        && exhaustion.is_none()
        && past_due_access.is_none()
    {
        return Ok(None);
    }

    let terms_version = SubscriptionEnrollmentTermsVersion::try_from(
        u16::try_from(version.ok_or_else(invalid_state)?).map_err(|_| invalid_state())?,
    )
    .map_err(|_| invalid_state())?;
    let currency =
        CurrencyCode::new(&row.try_get::<String, _>("currency")?).map_err(|_| invalid_state())?;
    let recurring_period =
        subscription_period_rule_from_scalars(SubscriptionPeriodRuleScalars::new(
            recurring_period_kind.as_deref().ok_or_else(invalid_state)?,
            recurring_period_count.ok_or_else(invalid_state)?,
        ))
        .map_err(map_subscription_persistence_error)?;
    let recurring = RecurringSubscriptionTerms::new(
        ChargeAmount::new(recurring_base.ok_or_else(invalid_state)?, currency)
            .map_err(|_| invalid_state())?,
        recurring_period,
    );
    let start = match (
        start_kind.as_deref(),
        trial_amount,
        trial_period_kind.as_deref(),
        trial_period_count,
    ) {
        (Some("recurring_immediately"), None, None, None) => {
            SubscriptionStart::RecurringImmediately
        }
        (Some("paid_trial"), Some(amount), Some(kind), Some(count)) => {
            SubscriptionStart::PaidTrial(PaidTrialTerms::new(
                ChargeAmount::new(amount, currency).map_err(|_| invalid_state())?,
                subscription_period_rule_from_scalars(SubscriptionPeriodRuleScalars::new(
                    kind, count,
                ))
                .map_err(map_subscription_persistence_error)?,
            ))
        }
        _ => return Err(invalid_state()),
    };
    let renewal_failure = renewal_failure_policy_from_scalars(RenewalFailurePolicyScalars::new(
        retry_delays.ok_or_else(invalid_state)?,
        exhaustion.as_deref().ok_or_else(invalid_state)?,
        past_due_access.as_deref().ok_or_else(invalid_state)?,
    ))
    .map_err(map_subscription_persistence_error)?;
    let offer = SubscriptionOffer::new(
        plan_key.ok_or_else(invalid_state)?.clone(),
        recurring,
        start,
        renewal_failure,
    )
    .map_err(|_| invalid_state())?;
    Ok(Some((terms_version, offer)))
}

pub(crate) fn processor_evidence_from_row(
    row: &PgRow,
) -> Result<ProcessorEvidence, PaymentAttemptStoreError> {
    let card_last_four = row.try_get::<Option<String>, _>("card_last4")?;
    let card_exp_month = row.try_get::<Option<i16>, _>("card_exp_month")?;
    let card_exp_year = row.try_get::<Option<i16>, _>("card_exp_year")?;
    let descriptor = GatewayPaymentDescriptor::from_provider_parts(
        diagnostic(row, "payment_type")?,
        diagnostic(row, "card_brand")?,
        card_last_four.as_deref(),
        card_exp_month,
        card_exp_year,
    );
    if descriptor.card_last_four().is_some() != card_last_four.is_some()
        || descriptor.card_exp_month() != card_exp_month
        || descriptor.card_exp_year() != card_exp_year
    {
        return Err(invalid_state());
    }
    Ok(ProcessorEvidence::new(
        row.try_get::<Option<String>, _>("gateway_transaction_id")?
            .map(GatewayTransactionId::new)
            .transpose()
            .map_err(|_| invalid_state())?,
        row.try_get::<Option<String>, _>("gateway_payment_method_reference")?
            .map(GatewayPaymentMethodReference::new)
            .transpose()
            .map_err(|_| invalid_state())?,
        diagnostic(row, "gateway_response")?,
        diagnostic(row, "gateway_response_code")?,
        diagnostic(row, "gateway_response_text")?,
        diagnostic(row, "gateway_condition")?,
        descriptor,
    ))
}

fn diagnostic(row: &PgRow, column: &'static str) -> Result<Option<GatewayDiagnostic>, sqlx::Error> {
    row.try_get::<Option<String>, _>(column)
        .map(|value| value.map(|value| GatewayDiagnostic::new(&value)))
}

fn lifecycle_from_row(row: &PgRow) -> Result<PaymentAttemptLifecycle, PaymentAttemptStoreError> {
    let refunded = row.try_get::<i32, _>("refunded_amount_cents")?;
    let state = match row
        .try_get::<String, _>("gateway_lifecycle_status")?
        .as_str()
    {
        "unknown" if refunded == 0 => GatewayLifecycleState::Unknown,
        "pending_settlement" if refunded == 0 => GatewayLifecycleState::PendingSettlement,
        "voided" if refunded == 0 => GatewayLifecycleState::Voided,
        "settled" if refunded == 0 => GatewayLifecycleState::Settled {
            cumulative_refunded_cents: None,
        },
        "settled" if refunded > 0 => GatewayLifecycleState::Settled {
            cumulative_refunded_cents: Some(
                CumulativeRefundCents::new(refunded).map_err(|_| invalid_state())?,
            ),
        },
        "refunded" => GatewayLifecycleState::Refunded {
            cumulative_refunded_cents: CumulativeRefundCents::new(refunded)
                .map_err(|_| invalid_state())?,
        },
        "chargeback" if refunded == 0 => GatewayLifecycleState::Chargeback {
            cumulative_refunded_cents: None,
        },
        "chargeback" if refunded > 0 => GatewayLifecycleState::Chargeback {
            cumulative_refunded_cents: Some(
                CumulativeRefundCents::new(refunded).map_err(|_| invalid_state())?,
            ),
        },
        _ => return Err(invalid_state()),
    };
    Ok(PaymentAttemptLifecycle::new(
        state,
        diagnostic(row, "gateway_lifecycle_action")?,
        row.try_get("gateway_lifecycle_at")?,
        row.try_get("gateway_lifecycle_reconciled_at")?,
    ))
}

pub(super) fn map_subscription_persistence_error(
    error: SubscriptionPersistenceCodecError,
) -> PaymentAttemptStoreError {
    match error {
        SubscriptionPersistenceCodecError::RowRead(error) => PaymentAttemptStoreError::Sql(error),
        SubscriptionPersistenceCodecError::InvalidState => invalid_state(),
    }
}
