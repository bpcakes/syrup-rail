use super::*;
use std::sync::LazyLock;

pub(super) const BILLING_ROW_LOCK_TIMEOUT: &str = "250ms";
pub(super) const BILLING_OPERATION_TIMEOUT: &str = "5s";
const STANDARD_LOCAL_ATTEMPT_STALE_AFTER_SECONDS: i64 = 30 * 60;
const PAYMENT_METHOD_UPDATE_LOCAL_ATTEMPT_STALE_AFTER_SECONDS: i64 = 3 * 60;
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
pub(crate) const STALE_UNSUBMITTED_RENEWAL_TEXT: &str =
    "Subscription renewal was abandoned before gateway submission.";
pub(crate) const STALE_UNSUBMITTED_RECOVERY_TEXT: &str =
    "Subscription recovery was abandoned before gateway submission.";

/// The replay action for a durable payment attempt.
///
/// Provider submission and storage status are separate dimensions. A prepared
/// attempt can resume, while a `review_required` attempt that never crossed the
/// provider boundary must first be checked for local expiry. Once an attempt
/// was submitted or terminalized, its immutable request and durable result are
/// the canonical idempotency response even if the surrounding subscription
/// later changes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AttemptReplayDisposition {
    ResumePrepared,
    RepairUnsubmittedReview,
    ReturnCanonical,
}

/// The database policy for attempts that are still wholly local.
///
/// The status classification is global because provider submission and replay
/// disposition do not vary by attempt kind. Only the stale window is
/// kind-specific. Keeping those dimensions separate prevents callers from
/// selecting an arbitrary kind merely to obtain the shared status vocabulary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LocalAttemptPolicy {
    kind: PaymentAttemptKind,
}

static LOCAL_ATTEMPT_EXPIRABLE_STATUS_VALUES: LazyLock<Vec<String>> = LazyLock::new(|| {
    PaymentAttemptStatus::ALL
        .into_iter()
        .filter(|status| LocalAttemptPolicy::should_expire(*status, false, true))
        .map(|status| status.as_str().to_owned())
        .collect()
});

impl LocalAttemptPolicy {
    pub(crate) const fn for_kind(kind: PaymentAttemptKind) -> Self {
        Self { kind }
    }

    pub(crate) const fn stale_after_seconds(self) -> i64 {
        match self.kind {
            PaymentAttemptKind::SubscriptionPaymentMethodUpdate => {
                PAYMENT_METHOD_UPDATE_LOCAL_ATTEMPT_STALE_AFTER_SECONDS
            }
            PaymentAttemptKind::HostCharge
            | PaymentAttemptKind::SubscriptionInitial
            | PaymentAttemptKind::SubscriptionRenewal
            | PaymentAttemptKind::SubscriptionRecovery => {
                STANDARD_LOCAL_ATTEMPT_STALE_AFTER_SECONDS
            }
        }
    }

    pub(crate) fn expirable_status_values() -> &'static [String] {
        LOCAL_ATTEMPT_EXPIRABLE_STATUS_VALUES.as_slice()
    }

    pub(crate) const fn should_expire(
        status: PaymentAttemptStatus,
        was_submitted: bool,
        is_stale: bool,
    ) -> bool {
        is_stale
            && matches!(
                attempt_replay_disposition_for(status, was_submitted),
                AttemptReplayDisposition::ResumePrepared
                    | AttemptReplayDisposition::RepairUnsubmittedReview
            )
    }
}

/// The immutable first-stage decision for subscriber-initiated preflight.
///
/// Only attempts that may resume or need local repair reach the aggregate-lock
/// stage. A matching canonical replay is complete without mutable subscription
/// context and must not inherit its lock availability.
pub(super) enum ExistingAttemptPreflight {
    Continue,
    IdempotencyConflict,
    ReplayCanonical(Box<PaymentAttempt>),
    RequiresLockedContext,
}

pub(crate) fn attempt_replay_disposition(attempt: &PaymentAttempt) -> AttemptReplayDisposition {
    attempt_replay_disposition_for(
        attempt.status(),
        attempt.state().timestamps().submitted_at().is_some(),
    )
}

const fn attempt_replay_disposition_for(
    status: PaymentAttemptStatus,
    was_submitted: bool,
) -> AttemptReplayDisposition {
    match (status, was_submitted) {
        (PaymentAttemptStatus::Pending, false) => AttemptReplayDisposition::ResumePrepared,
        (PaymentAttemptStatus::ReviewRequired, false) => {
            AttemptReplayDisposition::RepairUnsubmittedReview
        }
        _ => AttemptReplayDisposition::ReturnCanonical,
    }
}

pub(super) async fn preflight_existing_attempt(
    transaction: &mut Transaction<'_, Postgres>,
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    idempotency_key: &IdempotencyKey,
    matches_command: impl FnOnce(&PaymentAttempt) -> bool,
) -> Result<ExistingAttemptPreflight, PaymentAttemptStoreError> {
    let Some(existing) = find_payment_attempt_by_idempotency(
        transaction,
        billing_scope_id,
        subscriber_id,
        idempotency_key,
    )
    .await?
    else {
        return Ok(ExistingAttemptPreflight::Continue);
    };
    if !matches_command(&existing) {
        return Ok(ExistingAttemptPreflight::IdempotencyConflict);
    }
    Ok(match attempt_replay_disposition(&existing) {
        AttemptReplayDisposition::ReturnCanonical => {
            ExistingAttemptPreflight::ReplayCanonical(Box::new(existing))
        }
        AttemptReplayDisposition::ResumePrepared
        | AttemptReplayDisposition::RepairUnsubmittedReview => {
            ExistingAttemptPreflight::RequiresLockedContext
        }
    })
}

/// The exact gateway identity a locked database row must still expose before
/// an operation can reserve or submit a provider mutation.
///
/// Database provider keys remain raw values at this boundary: an unexpected
/// persisted value fails closed as a mismatch rather than becoming a new parse
/// error contract.
#[derive(Clone)]
pub(super) struct ExpectedGatewayIdentity {
    pub(super) identity: syrup_rail::GatewayAccountIdentity,
}

impl ExpectedGatewayIdentity {
    pub(super) fn for_gateway(
        billing_scope_id: BillingScopeId,
        expected_gateway_configuration_id: GatewayConfigurationId,
        gateway: &syrup_rail::ResolvedGateway,
    ) -> Self {
        Self {
            identity: syrup_rail::GatewayAccountIdentity::new(
                billing_scope_id,
                gateway.gateway_account_id(),
                gateway.provider_key().clone(),
                expected_gateway_configuration_id,
            ),
        }
    }

    pub(super) fn from_reservation(
        identity: PaymentAttemptIdentity,
        provider_key: &GatewayProviderKey,
    ) -> Self {
        Self {
            identity: syrup_rail::GatewayAccountIdentity::new(
                identity.billing_scope_id(),
                identity.gateway_account_id(),
                provider_key.clone(),
                identity.gateway_configuration_id(),
            ),
        }
    }

    pub(super) const fn billing_scope_id(&self) -> BillingScopeId {
        self.identity.billing_scope_id()
    }

    pub(super) const fn gateway_account_id(&self) -> GatewayAccountId {
        self.identity.gateway_account_id()
    }

    pub(super) fn matches_row(
        &self,
        account_id: Uuid,
        configuration_id: Uuid,
        provider_key: &str,
    ) -> bool {
        account_id == self.identity.gateway_account_id().into_uuid()
            && configuration_id == self.identity.gateway_configuration_id().into_uuid()
            && provider_key == self.identity.provider_key().as_str()
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
    let policy = LocalAttemptPolicy::for_kind(PaymentAttemptKind::SubscriptionInitial);
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
            AND status = ANY($5::text[])
            AND submitted_at IS NULL
            AND created_at <= clock_timestamp()
                - ($6::bigint * interval '1 second')
        "#,
    )
    .bind(billing_scope_id.as_uuid())
    .bind(subscriber_id.as_uuid())
    .bind(plan_key.as_str())
    .bind(INITIAL_PREPARED_EXPIRED_TEXT)
    .bind(LocalAttemptPolicy::expirable_status_values())
    .bind(policy.stale_after_seconds())
    .execute(&mut **transaction)
    .await?;
    Ok(result.rows_affected())
}

pub(super) async fn gateway_identity_matches_account(
    transaction: &mut Transaction<'_, Postgres>,
    expected: &ExpectedGatewayIdentity,
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

pub(crate) async fn fail_stale_unsubmitted_payment_method_updates(
    connection: &mut PgConnection,
    subscription_id: SubscriptionId,
) -> Result<(), sqlx::Error> {
    let policy = LocalAttemptPolicy::for_kind(PaymentAttemptKind::SubscriptionPaymentMethodUpdate);
    sqlx::query(
        r#"
        UPDATE billing_payment_attempts
        SET status = 'failed',
            gateway_response_text = COALESCE(
                gateway_response_text,
                'Payment method update was abandoned before gateway submission.'
            ),
            gateway_condition = COALESCE(gateway_condition, 'failed'),
            resolved_at = COALESCE(resolved_at, clock_timestamp()),
            updated_at = clock_timestamp()
        WHERE attempt_kind = 'subscription_payment_method_update'
            AND subscription_id = $1
            AND status = ANY($2::text[])
            AND submitted_at IS NULL
            AND created_at <= clock_timestamp()
                - ($3::bigint * interval '1 second')
        "#,
    )
    .bind(subscription_id.as_uuid())
    .bind(LocalAttemptPolicy::expirable_status_values())
    .bind(policy.stale_after_seconds())
    .execute(connection)
    .await?;
    Ok(())
}

pub(crate) async fn fail_stale_unsubmitted_subscription_charges(
    connection: &mut PgConnection,
    subscription_id: SubscriptionId,
) -> Result<u64, sqlx::Error> {
    let policy = LocalAttemptPolicy::for_kind(PaymentAttemptKind::SubscriptionRenewal);
    let result = sqlx::query(
        r#"
        WITH stale_attempts AS (
            SELECT id
            FROM billing_payment_attempts
            WHERE attempt_kind IN ('subscription_renewal', 'subscription_recovery')
                AND subscription_id = $1
                AND status = ANY($2::text[])
                AND submitted_at IS NULL
                AND created_at <= clock_timestamp()
                    - ($3::bigint * interval '1 second')
            FOR UPDATE SKIP LOCKED
        )
        UPDATE billing_payment_attempts AS attempts
        SET status = 'failed',
            gateway_response_text = CASE attempts.attempt_kind
                WHEN 'subscription_renewal'
                THEN $4
                WHEN 'subscription_recovery'
                THEN $5
            END,
            gateway_condition = COALESCE(gateway_condition, 'failed'),
            resolved_at = COALESCE(resolved_at, clock_timestamp()),
            updated_at = clock_timestamp()
        FROM stale_attempts
        WHERE attempts.id = stale_attempts.id
        "#,
    )
    .bind(subscription_id.as_uuid())
    .bind(LocalAttemptPolicy::expirable_status_values())
    .bind(policy.stale_after_seconds())
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

pub(crate) async fn blocking_payment_method_update_exists(
    connection: &mut PgConnection,
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
    .fetch_one(connection)
    .await
}

#[cfg(test)]
mod replay_disposition_tests {
    use super::*;

    #[test]
    fn replay_disposition_keeps_local_review_distinct_from_prepared_and_canonical() {
        assert_eq!(
            attempt_replay_disposition_for(PaymentAttemptStatus::Pending, false),
            AttemptReplayDisposition::ResumePrepared,
        );
        assert_eq!(
            attempt_replay_disposition_for(PaymentAttemptStatus::ReviewRequired, false),
            AttemptReplayDisposition::RepairUnsubmittedReview,
        );

        for status in PaymentAttemptStatus::ALL {
            assert_eq!(
                attempt_replay_disposition_for(status, true),
                AttemptReplayDisposition::ReturnCanonical,
            );
        }
        for status in [
            PaymentAttemptStatus::Approved,
            PaymentAttemptStatus::Declined,
            PaymentAttemptStatus::Unknown,
            PaymentAttemptStatus::Failed,
        ] {
            assert_eq!(
                attempt_replay_disposition_for(status, false),
                AttemptReplayDisposition::ReturnCanonical,
            );
        }
    }

    #[test]
    fn local_attempt_state_policy_covers_the_complete_state_matrix() {
        for status in PaymentAttemptStatus::ALL {
            for was_submitted in [false, true] {
                for is_stale in [false, true] {
                    let expected = is_stale
                        && !was_submitted
                        && matches!(
                            status,
                            PaymentAttemptStatus::Pending | PaymentAttemptStatus::ReviewRequired
                        );
                    assert_eq!(
                        LocalAttemptPolicy::should_expire(status, was_submitted, is_stale),
                        expected,
                        "status={status:?} submitted={was_submitted} stale={is_stale}",
                    );
                }
            }
        }

        assert_eq!(
            LocalAttemptPolicy::expirable_status_values(),
            &["pending", "review_required"],
        );
    }

    #[test]
    fn local_attempt_policy_defines_each_kind_stale_window() {
        for kind in PaymentAttemptKind::ALL {
            let expected = if kind == PaymentAttemptKind::SubscriptionPaymentMethodUpdate {
                3 * 60
            } else {
                30 * 60
            };
            assert_eq!(
                LocalAttemptPolicy::for_kind(kind).stale_after_seconds(),
                expected,
                "kind={kind:?}",
            );
        }
    }
}
