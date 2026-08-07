use std::fmt;

use chrono::{DateTime, Utc};
use sqlx::{Postgres, Row, Transaction, postgres::PgRow};
use syrup_rail::{
    BillingContactSnapshot, BillingPeriod, BillingScopeId, ChargeAmount, CumulativeRefundCents,
    CurrencyCode, DiscountClaimId, DiscountCodeId, GatewayAccountId, GatewayConfigurationId,
    GatewayDiagnostic, GatewayLifecycleState, GatewayOrderId, GatewayPaymentDescriptor,
    GatewayPaymentMethodReference, GatewayTransactionId, HostChargeTargetId, IdempotencyKey,
    LimitedDiscountMonths, Money, PaymentAttempt, PaymentAttemptFingerprint, PaymentAttemptId,
    PaymentAttemptIdentity, PaymentAttemptKind, PaymentAttemptLifecycle, PaymentAttemptRequest,
    PaymentAttemptState, PaymentAttemptStatus, PaymentAttemptTarget, PaymentAttemptTimestamps,
    PaymentMethodId, PaymentMethodUpdateSnapshot, PaymentResolutionCode, PercentOffBasisPoints,
    PlanKey, PositiveDiscountCents, ProcessorEvidence, SubscriberId, SubscriptionDiscountCode,
    SubscriptionDiscountDuration, SubscriptionDiscountKind, SubscriptionDiscountSnapshot,
    SubscriptionEnrollmentDiscountSnapshot, SubscriptionId, SubscriptionInitialApplication,
    SubscriptionPaymentStateSnapshot, SubscriptionStatus,
};
use thiserror::Error;
use uuid::Uuid;

const INVALID_ATTEMPT_STATE: &str = "canonical payment attempt state is invalid";

const PAYMENT_ATTEMPT_SELECT: &str = r#"
    SELECT id, billing_scope_id, subscriber_id, plan_key,
        host_charge_target_id, subscription_id, payment_method_id,
        attempt_kind, status, idempotency_key, request_fingerprint,
        amount_cents, currency, billing_period_start_at,
        billing_period_end_at, gateway_account_id,
        gateway_configuration_id, gateway_order_id,
        gateway_transaction_id, gateway_payment_method_reference,
        gateway_response, gateway_response_code, gateway_response_text,
        gateway_condition, payment_type, card_brand, card_last4,
        card_exp_month, card_exp_year, submitted_at, resolved_at,
        created_at, updated_at, gateway_lifecycle_status,
        gateway_lifecycle_action, gateway_lifecycle_at,
        gateway_lifecycle_reconciled_at, refunded_amount_cents,
        billing_name, billing_email, resolution_code, review_required_at,
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
        subscription_initial_discount_discounted_amount_cents
    FROM billing_payment_attempts
"#;

#[derive(Error)]
pub enum PaymentAttemptStoreError {
    #[error("payment attempt storage operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("{0}")]
    InvalidState(&'static str),
}

impl fmt::Debug for PaymentAttemptStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sql(_) => formatter.write_str("PaymentAttemptStoreError::Sql"),
            Self::InvalidState(detail) => formatter
                .debug_tuple("PaymentAttemptStoreError::InvalidState")
                .field(detail)
                .finish(),
        }
    }
}

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

/// Locks the exact owner-scoped idempotency row for replay or mutation.
pub async fn lock_payment_attempt_by_idempotency_in_transaction(
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

fn payment_attempt_from_row(row: &PgRow) -> Result<PaymentAttempt, PaymentAttemptStoreError> {
    let attempt_id = PaymentAttemptId::new(row.try_get("id")?);
    let identity = PaymentAttemptIdentity::new(
        attempt_id,
        BillingScopeId::new(row.try_get("billing_scope_id")?),
        SubscriberId::new(row.try_get("subscriber_id")?),
        GatewayAccountId::new(row.try_get("gateway_account_id")?),
        GatewayConfigurationId::new(row.try_get("gateway_configuration_id")?),
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
    let request = PaymentAttemptRequest::new(
        target,
        IdempotencyKey::new(row.try_get::<String, _>("idempotency_key")?)
            .map_err(|_| invalid_state())?,
        PaymentAttemptFingerprint::new(row.try_get::<String, _>("request_fingerprint")?)
            .map_err(|_| invalid_state())?,
        amount,
        gateway_order_id,
        BillingContactSnapshot::new(row.try_get("billing_name")?, row.try_get("billing_email")?),
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

    match kind {
        PaymentAttemptKind::HostCharge
            if plan_key.is_none()
                && subscription_id.is_none()
                && payment_method_id.is_none()
                && period.is_none()
                && method_update_snapshot.is_none()
                && subscription_snapshot.is_none()
                && discount.is_none() =>
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
            Ok(PaymentAttemptTarget::SubscriptionInitial {
                plan_key: plan_key.ok_or_else(invalid_state)?,
                discount,
                application,
            })
        }
        PaymentAttemptKind::SubscriptionRenewal
            if host_target.is_none() && method_update_snapshot.is_none() && discount.is_none() =>
        {
            Ok(PaymentAttemptTarget::SubscriptionRenewal {
                plan_key: plan_key.ok_or_else(invalid_state)?,
                payment_method_id: payment_method_id.ok_or_else(invalid_state)?,
                period: period.ok_or_else(invalid_state)?,
                expected_state: subscription_snapshot.ok_or_else(invalid_state)?,
            })
        }
        PaymentAttemptKind::SubscriptionRecovery
            if host_target.is_none() && method_update_snapshot.is_none() && discount.is_none() =>
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
                && discount.is_none() =>
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

fn processor_evidence_from_row(row: &PgRow) -> Result<ProcessorEvidence, PaymentAttemptStoreError> {
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

const fn invalid_state() -> PaymentAttemptStoreError {
    PaymentAttemptStoreError::InvalidState(INVALID_ATTEMPT_STATE)
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use chrono::Duration;

    use super::*;
    use crate::test_support::{TestDatabase, create_gateway_account};

    #[tokio::test]
    async fn loaders_preserve_exact_scope_and_redact_durable_values() -> Result<(), Box<dyn Error>>
    {
        let database = TestDatabase::start("attempt_owner").await?;
        let account = create_gateway_account(&database.pool, "nmi").await?;
        let attempt_id = Uuid::now_v7();
        let subscriber_id = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO billing_payment_attempts (
                id, billing_scope_id, subscriber_id, host_charge_target_id,
                attempt_kind, status, idempotency_key, request_fingerprint,
                amount_cents, currency, gateway_account_id,
                gateway_configuration_id, gateway_order_id,
                gateway_transaction_id, gateway_payment_method_reference,
                gateway_response, gateway_response_code, gateway_response_text,
                gateway_condition, billing_name, billing_email
            ) VALUES (
                $1, $2, $3, $4, 'host_charge', 'pending', $5, $6,
                1000, 'USD', $7, $8, $9, $10, $11, $12, $13, $14,
                $15, $16, $17
            )
            "#,
        )
        .bind(attempt_id)
        .bind(account.billing_scope_id)
        .bind(subscriber_id)
        .bind(Uuid::now_v7())
        .bind("idempotency-secret")
        .bind("fingerprint-secret")
        .bind(account.gateway_account_id)
        .bind(account.gateway_configuration_id)
        .bind("order-secret")
        .bind("transaction-secret")
        .bind("method-secret")
        .bind("response-secret")
        .bind("code-secret")
        .bind("text-secret")
        .bind("condition-secret")
        .bind("Sensitive Name")
        .bind("secret@example.test")
        .execute(&database.pool)
        .await?;

        let mut transaction = database.pool.begin().await?;
        assert!(
            find_payment_attempt_by_id_in_transaction(
                &mut transaction,
                BillingScopeId::new(Uuid::now_v7()),
                PaymentAttemptId::new(attempt_id),
            )
            .await?
            .is_none()
        );
        let attempt = lock_payment_attempt_by_idempotency_in_transaction(
            &mut transaction,
            BillingScopeId::new(account.billing_scope_id),
            SubscriberId::new(subscriber_id),
            &IdempotencyKey::new("idempotency-secret")?,
        )
        .await?
        .expect("exact owner row should load");
        assert_eq!(attempt.identity().attempt_id().as_uuid(), &attempt_id);
        assert_eq!(attempt.kind(), PaymentAttemptKind::HostCharge);
        assert_eq!(attempt.request().amount().cents(), 1_000);
        assert_eq!(
            attempt
                .state()
                .processor_evidence()
                .transaction_id()
                .expect("transaction ID")
                .expose(),
            "transaction-secret"
        );
        let debug = format!("{attempt:?}");
        for secret in [
            "idempotency-secret",
            "fingerprint-secret",
            "order-secret",
            "transaction-secret",
            "method-secret",
            "response-secret",
            "code-secret",
            "text-secret",
            "condition-secret",
            "Sensitive Name",
            "secret@example.test",
        ] {
            assert!(!debug.contains(secret), "debug leaked {secret}");
        }
        transaction.rollback().await?;
        database.cleanup().await
    }

    #[tokio::test]
    async fn recovery_keeps_related_and_expected_payment_methods_distinct()
    -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("attempt_recovery").await?;
        let account = create_gateway_account(&database.pool, "nmi").await?;
        let subscriber_id = Uuid::now_v7();
        let expected_method_id = Uuid::now_v7();
        let related_method_id = Uuid::now_v7();
        for (method_id, reference) in [
            (expected_method_id, "vault-expected"),
            (related_method_id, "vault-related"),
        ] {
            sqlx::query(
                r#"
                INSERT INTO billing_payment_methods (
                    id, billing_scope_id, subscriber_id, gateway_account_id,
                    gateway_payment_method_reference, status
                ) VALUES ($1, $2, $3, $4, $5, 'active')
                "#,
            )
            .bind(method_id)
            .bind(account.billing_scope_id)
            .bind(subscriber_id)
            .bind(account.gateway_account_id)
            .bind(reference)
            .execute(&database.pool)
            .await?;
        }
        let subscription_id = Uuid::now_v7();
        let period_start = Utc::now();
        let period_end = period_start + Duration::days(30);
        sqlx::query(
            r#"
            INSERT INTO billing_subscriptions (
                id, billing_scope_id, subscriber_id, plan_key, status,
                gateway_account_id, payment_method_id, amount_cents, currency,
                current_period_start_at, current_period_end_at, next_renewal_at,
                initial_transaction_id
            ) VALUES (
                $1, $2, $3, 'premium', 'active', $4, $5, 1000, 'USD',
                $6, $7, $7, 'txn-initial'
            )
            "#,
        )
        .bind(subscription_id)
        .bind(account.billing_scope_id)
        .bind(subscriber_id)
        .bind(account.gateway_account_id)
        .bind(related_method_id)
        .bind(period_start)
        .bind(period_end)
        .execute(&database.pool)
        .await?;

        let attempt_id = Uuid::now_v7();
        let charge_start = period_end;
        let charge_end = charge_start + Duration::days(30);
        let order_id = format!("sr_recovery_{}", attempt_id.simple());
        sqlx::query(
            r#"
            INSERT INTO billing_payment_attempts (
                id, billing_scope_id, subscriber_id, plan_key,
                subscription_id, payment_method_id, attempt_kind, status,
                idempotency_key, request_fingerprint, amount_cents, currency,
                billing_period_start_at, billing_period_end_at,
                gateway_account_id, gateway_configuration_id, gateway_order_id,
                gateway_transaction_id, submitted_at, resolved_at,
                subscription_expected_payment_method_id,
                subscription_expected_initial_transaction_id,
                subscription_expected_status
            ) VALUES (
                $1, $2, $3, 'premium', $4, $5,
                'subscription_recovery', 'approved', $6, $7, 1000, 'USD',
                $8, $9, $10, $11, $12, 'txn-recovery', now(), now(),
                $13, 'txn-initial', 'past_due'
            )
            "#,
        )
        .bind(attempt_id)
        .bind(account.billing_scope_id)
        .bind(subscriber_id)
        .bind(subscription_id)
        .bind(related_method_id)
        .bind("recovery-key")
        .bind("recovery-fingerprint")
        .bind(charge_start)
        .bind(charge_end)
        .bind(account.gateway_account_id)
        .bind(account.gateway_configuration_id)
        .bind(order_id)
        .bind(expected_method_id)
        .execute(&database.pool)
        .await?;

        let mut transaction = database.pool.begin().await?;
        let attempt = find_payment_attempt_by_id_in_transaction(
            &mut transaction,
            BillingScopeId::new(account.billing_scope_id),
            PaymentAttemptId::new(attempt_id),
        )
        .await?
        .expect("recovery row should load");
        let target = attempt.request().target();
        assert_eq!(
            target.payment_method_id().unwrap().as_uuid(),
            &related_method_id
        );
        assert_eq!(
            target.subscription_id().unwrap().as_uuid(),
            &subscription_id
        );
        assert_eq!(
            target
                .subscription_payment_state_snapshot()
                .expect("expected state")
                .payment_method_id()
                .as_uuid(),
            &expected_method_id
        );
        assert_eq!(
            target
                .subscription_payment_state_snapshot()
                .expect("expected state")
                .status(),
            SubscriptionStatus::PastDue
        );
        transaction.rollback().await?;
        database.cleanup().await
    }
}
