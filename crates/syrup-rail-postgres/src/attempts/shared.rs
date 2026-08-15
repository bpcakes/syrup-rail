use super::*;

pub(super) const BILLING_ROW_LOCK_TIMEOUT: &str = "250ms";
pub(super) const BILLING_OPERATION_TIMEOUT: &str = "5s";
pub(crate) const INITIAL_PREPARED_STALE_AFTER_SECONDS: i64 = 30 * 60;
pub(super) const INITIAL_PREPARED_EXPIRED_TEXT: &str =
    "Prepared checkout expired before processor submission.";
pub(super) const INITIAL_BILLING_STATE_CHANGED_TEXT: &str =
    "Checkout was canceled before submission because billing state changed.";
pub(super) const INITIAL_TERMS_CHANGED_TEXT: &str =
    "Checkout was canceled before submission because enrollment terms changed.";
pub(super) const INITIAL_CONFIGURATION_CHANGED_TEXT: &str =
    "Checkout was canceled before submission because payment configuration changed.";
pub(super) const RECOVERY_STATE_CHANGED_TEXT: &str =
    "Subscription recovery was canceled before submission because billing state changed.";
pub(super) const RECOVERY_CONFIGURATION_CHANGED_TEXT: &str =
    "Subscription recovery was canceled before submission because payment configuration changed.";
pub(super) const RENEWAL_STATE_CHANGED_TEXT: &str =
    "Subscription renewal was canceled before submission because billing state changed.";
pub(super) const RENEWAL_CONFIGURATION_CHANGED_TEXT: &str =
    "Subscription renewal was canceled before submission because payment configuration changed.";
pub(super) const PAYMENT_METHOD_REPLACEMENT_STATE_CHANGED_TEXT: &str =
    "Payment method replacement was canceled before submission because billing state changed.";
pub(super) const PAYMENT_METHOD_REPLACEMENT_CONFIGURATION_CHANGED_TEXT: &str = "Payment method replacement was canceled before submission because payment configuration changed.";
pub(crate) const PAYMENT_METHOD_UPDATE_UNSUBMITTED_STALE_AFTER_SECONDS: i64 = 3 * 60;
pub(crate) const SUBSCRIPTION_CHARGE_UNSUBMITTED_STALE_AFTER_SECONDS: i64 = 30 * 60;
pub(crate) const STALE_UNSUBMITTED_RENEWAL_TEXT: &str =
    "Subscription renewal was abandoned before gateway submission.";
pub(crate) const STALE_UNSUBMITTED_RECOVERY_TEXT: &str =
    "Subscription recovery was abandoned before gateway submission.";

/// The only two replay phases exposed by a durable payment attempt.
///
/// Mutable billing context is relevant only while a prepared attempt could
/// still cause provider I/O. Once an attempt was submitted or terminalized,
/// its immutable request and durable result are the canonical idempotency
/// response even if the surrounding subscription later changes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AttemptReplayPhase {
    ResumePrepared,
    ReturnCanonical,
}

pub(crate) fn attempt_replay_phase(attempt: &PaymentAttempt) -> AttemptReplayPhase {
    if attempt.status() == PaymentAttemptStatus::Pending
        && attempt.state().timestamps().submitted_at().is_none()
    {
        AttemptReplayPhase::ResumePrepared
    } else {
        AttemptReplayPhase::ReturnCanonical
    }
}

/// The exact gateway identity a locked database row must still expose before
/// an operation can reserve or submit a provider mutation.
///
/// Database provider keys remain raw values at this boundary: an unexpected
/// persisted value fails closed as a mismatch rather than becoming a new parse
/// error contract.
#[derive(Clone, Copy)]
pub(super) struct ExpectedGatewayIdentity<'a> {
    pub(super) billing_scope_id: BillingScopeId,
    pub(super) gateway_account_id: GatewayAccountId,
    pub(super) gateway_configuration_id: GatewayConfigurationId,
    pub(super) provider_key: &'a GatewayProviderKey,
}

impl<'a> ExpectedGatewayIdentity<'a> {
    pub(super) fn for_gateway(
        billing_scope_id: BillingScopeId,
        expected_gateway_configuration_id: GatewayConfigurationId,
        gateway: &'a syrup_rail::ResolvedGateway,
    ) -> Self {
        Self {
            billing_scope_id,
            gateway_account_id: gateway.gateway_account_id(),
            gateway_configuration_id: expected_gateway_configuration_id,
            provider_key: gateway.provider_key(),
        }
    }

    pub(super) fn from_reservation(
        identity: PaymentAttemptIdentity,
        provider_key: &'a GatewayProviderKey,
    ) -> Self {
        Self {
            billing_scope_id: identity.billing_scope_id(),
            gateway_account_id: identity.gateway_account_id(),
            gateway_configuration_id: identity.gateway_configuration_id(),
            provider_key,
        }
    }

    pub(super) const fn billing_scope_id(&self) -> BillingScopeId {
        self.billing_scope_id
    }

    pub(super) const fn gateway_account_id(&self) -> GatewayAccountId {
        self.gateway_account_id
    }

    pub(super) fn matches_row(
        &self,
        account_id: Uuid,
        configuration_id: Uuid,
        provider_key: &str,
    ) -> bool {
        account_id == self.gateway_account_id.into_uuid()
            && configuration_id == self.gateway_configuration_id.into_uuid()
            && provider_key == self.provider_key.as_str()
    }
}

pub(crate) async fn set_enrollment_timeouts(
    connection: &mut PgConnection,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "SELECT set_config('lock_timeout', $1, true), set_config('statement_timeout', $2, true)",
    )
    .bind(BILLING_ROW_LOCK_TIMEOUT)
    .bind(BILLING_OPERATION_TIMEOUT)
    .execute(&mut *connection)
    .await?;
    Ok(())
}

pub(crate) async fn lock_subscription_aggregate(
    connection: &mut PgConnection,
    subscriber_id: SubscriberId,
    plan_key: &PlanKey,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::uuid::text || ':' || $2, 0))")
        .bind(subscriber_id.as_uuid())
        .bind(plan_key.as_str())
        .execute(&mut *connection)
        .await?;
    Ok(())
}

pub(crate) async fn try_lock_subscription_aggregate(
    transaction: &mut Transaction<'_, Postgres>,
    subscriber_id: SubscriberId,
    plan_key: &PlanKey,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT pg_try_advisory_xact_lock(hashtextextended($1::uuid::text || ':' || $2, 0))",
    )
    .bind(subscriber_id.as_uuid())
    .bind(plan_key.as_str())
    .fetch_one(&mut **transaction)
    .await
}

pub(super) async fn payment_attempt_by_idempotency(
    transaction: &mut Transaction<'_, Postgres>,
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    idempotency_key: &IdempotencyKey,
    for_update: bool,
) -> Result<Option<PaymentAttempt>, PaymentAttemptStoreError> {
    let lock = if for_update { "FOR UPDATE" } else { "" };
    let query = format!(
        "{PAYMENT_ATTEMPT_SELECT} \
         WHERE billing_scope_id = $1 AND subscriber_id = $2 AND idempotency_key = $3 \
         {lock}"
    );
    let row = sqlx::query(&query)
        .bind(billing_scope_id.as_uuid())
        .bind(subscriber_id.as_uuid())
        .bind(idempotency_key.expose())
        .fetch_optional(&mut **transaction)
        .await?;
    row.as_ref().map(payment_attempt_from_row).transpose()
}

pub(crate) async fn lock_initial_attempt_rows(
    transaction: &mut Transaction<'_, Postgres>,
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    plan_key: &PlanKey,
) -> Result<(), sqlx::Error> {
    sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT id FROM billing_payment_attempts
        WHERE billing_scope_id = $1 AND subscriber_id = $2 AND plan_key = $3
            AND attempt_kind = 'subscription_initial'
        ORDER BY created_at, id FOR UPDATE
        "#,
    )
    .bind(billing_scope_id.as_uuid())
    .bind(subscriber_id.as_uuid())
    .bind(plan_key.as_str())
    .fetch_all(&mut **transaction)
    .await?;
    Ok(())
}

pub(crate) async fn lock_initial_charge_rows(
    transaction: &mut Transaction<'_, Postgres>,
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    plan_key: &PlanKey,
) -> Result<(), sqlx::Error> {
    sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT charges.id
        FROM billing_processor_charges charges
        INNER JOIN billing_payment_attempts attempts ON attempts.id = charges.attempt_id
        WHERE attempts.billing_scope_id = $1
            AND attempts.subscriber_id = $2
            AND attempts.plan_key = $3
            AND attempts.attempt_kind = 'subscription_initial'
        ORDER BY charges.observed_at, charges.id
        FOR UPDATE OF charges
        "#,
    )
    .bind(billing_scope_id.as_uuid())
    .bind(subscriber_id.as_uuid())
    .bind(plan_key.as_str())
    .fetch_all(&mut **transaction)
    .await?;
    Ok(())
}

pub(crate) async fn expire_stale_initial_attempts(
    transaction: &mut Transaction<'_, Postgres>,
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    plan_key: &PlanKey,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        r#"
        UPDATE billing_payment_attempts
        SET status = 'failed',
            gateway_response_text = COALESCE(gateway_response_text, $4),
            gateway_condition = COALESCE(gateway_condition, 'failed'),
            resolution_code = COALESCE(
                resolution_code,
                'subscription_initial_prepared_attempt_expired'
            ),
            resolved_at = clock_timestamp(), updated_at = clock_timestamp()
        WHERE billing_scope_id = $1 AND subscriber_id = $2 AND plan_key = $3
            AND attempt_kind = 'subscription_initial'
            AND status IN ('pending', 'review_required')
            AND submitted_at IS NULL
            AND created_at <= clock_timestamp()
                - ($5::bigint * interval '1 second')
        "#,
    )
    .bind(billing_scope_id.as_uuid())
    .bind(subscriber_id.as_uuid())
    .bind(plan_key.as_str())
    .bind(INITIAL_PREPARED_EXPIRED_TEXT)
    .bind(INITIAL_PREPARED_STALE_AFTER_SECONDS)
    .execute(&mut **transaction)
    .await?;
    Ok(result.rows_affected())
}

pub(super) async fn gateway_identity_matches_account(
    transaction: &mut Transaction<'_, Postgres>,
    expected: &ExpectedGatewayIdentity<'_>,
) -> Result<bool, sqlx::Error> {
    let row = sqlx::query_as::<_, (Uuid, Uuid, String)>(
        r#"
        SELECT id, gateway_configuration_id, provider_key
        FROM billing_gateway_accounts
        WHERE billing_scope_id = $1 AND id = $2
        FOR SHARE
        "#,
    )
    .bind(expected.billing_scope_id().as_uuid())
    .bind(expected.gateway_account_id().as_uuid())
    .fetch_optional(&mut **transaction)
    .await?;
    Ok(
        row.is_some_and(|(account_id, configuration_id, provider_key)| {
            expected.matches_row(account_id, configuration_id, &provider_key)
        }),
    )
}

pub(super) fn locked_subscription_payment_state(
    subscription_id: SubscriptionId,
    payment_method_id: PaymentMethodId,
    initial_transaction_id: GatewayTransactionId,
    status: SubscriptionStatus,
) -> Result<SubscriptionPaymentStateSnapshot, PaymentAttemptStoreError> {
    SubscriptionPaymentStateSnapshot::new(
        subscription_id,
        payment_method_id,
        initial_transaction_id,
        status,
    )
    .map_err(|_| invalid_state())
}

pub(super) async fn fail_stale_unsubmitted_payment_method_updates(
    transaction: &mut Transaction<'_, Postgres>,
    subscription_id: SubscriptionId,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        UPDATE billing_payment_attempts
        SET status = 'failed',
            gateway_response_text = COALESCE(
                gateway_response_text,
                'Payment method update was abandoned before gateway submission.'
            ),
            resolved_at = COALESCE(resolved_at, clock_timestamp()),
            updated_at = clock_timestamp()
        WHERE attempt_kind = 'subscription_payment_method_update'
            AND subscription_id = $1
            AND status = 'pending' AND submitted_at IS NULL
            AND created_at <= clock_timestamp()
                - ($2::bigint * interval '1 second')
        "#,
    )
    .bind(subscription_id.as_uuid())
    .bind(PAYMENT_METHOD_UPDATE_UNSUBMITTED_STALE_AFTER_SECONDS)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

pub(crate) async fn fail_stale_unsubmitted_subscription_charges(
    connection: &mut PgConnection,
    subscription_id: SubscriptionId,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        r#"
        UPDATE billing_payment_attempts
        SET status = 'failed',
            gateway_response_text = CASE attempt_kind
                WHEN 'subscription_renewal'
                THEN $3
                WHEN 'subscription_recovery'
                THEN $4
            END,
            gateway_condition = COALESCE(gateway_condition, 'failed'),
            resolved_at = COALESCE(resolved_at, clock_timestamp()),
            updated_at = clock_timestamp()
        WHERE attempt_kind IN ('subscription_renewal', 'subscription_recovery')
            AND subscription_id = $1
            AND status IN ('pending', 'review_required')
            AND submitted_at IS NULL
            AND created_at <= clock_timestamp()
                - ($2::bigint * interval '1 second')
        "#,
    )
    .bind(subscription_id.as_uuid())
    .bind(SUBSCRIPTION_CHARGE_UNSUBMITTED_STALE_AFTER_SECONDS)
    .bind(STALE_UNSUBMITTED_RENEWAL_TEXT)
    .bind(STALE_UNSUBMITTED_RECOVERY_TEXT)
    .execute(connection)
    .await?;
    Ok(result.rows_affected())
}

pub(super) async fn blocking_subscription_charge_attempt_exists(
    transaction: &mut Transaction<'_, Postgres>,
    subscription_id: SubscriptionId,
) -> Result<bool, sqlx::Error> {
    blocking_subscription_charge_attempt_exists_except(
        transaction,
        subscription_id,
        PaymentAttemptId::new(Uuid::nil()),
    )
    .await
}

pub(super) async fn blocking_subscription_charge_attempt_exists_except(
    transaction: &mut Transaction<'_, Postgres>,
    subscription_id: SubscriptionId,
    excluded_attempt_id: PaymentAttemptId,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM billing_payment_attempts AS attempts
            INNER JOIN billing_subscriptions AS subscriptions
                ON subscriptions.id = attempts.subscription_id
            WHERE attempts.subscription_id = $1
                AND attempts.id <> $2
                AND attempts.attempt_kind IN ('subscription_renewal', 'subscription_recovery')
                AND (
                    attempts.status IN ('pending', 'unknown', 'review_required')
                    OR (
                        attempts.status = 'approved'
                        AND attempts.billing_period_start_at = subscriptions.next_renewal_at
                    )
                )
        )
        "#,
    )
    .bind(subscription_id.as_uuid())
    .bind(excluded_attempt_id.as_uuid())
    .fetch_one(&mut **transaction)
    .await
}

pub(super) async fn blocking_payment_method_update_exists(
    transaction: &mut Transaction<'_, Postgres>,
    subscription_id: SubscriptionId,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        r#"
        SELECT EXISTS (
            SELECT 1 FROM billing_payment_attempts
            WHERE subscription_id = $1
                AND attempt_kind = 'subscription_payment_method_update'
                AND status IN ('pending', 'unknown', 'review_required')
        )
        "#,
    )
    .bind(subscription_id.as_uuid())
    .fetch_one(&mut **transaction)
    .await
}
