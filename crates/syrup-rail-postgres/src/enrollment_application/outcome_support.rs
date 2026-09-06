use super::*;

pub(crate) async fn commit_rate_limit_cooldown(
    pool: &PgPool,
    reservation: OutcomeReservation<'_>,
    cooldown: RateLimitCooldown,
) -> Result<(), SubscriptionEnrollmentApplicationError> {
    // Provider/account backoff protects work beyond this one attempt. Commit it
    // independently before outcome resolution so a concurrent canonical winner,
    // later projection failure, or crash cannot erase an observed throttle. The
    // fail-safe side is conservative pacing; same-key replay can still finish
    // any unresolved application.
    match commit_rate_limit_cooldown_for_operation(
        pool,
        reservation.identity(),
        reservation.provider_key(),
        cooldown,
        RateLimitCooldownOperation::Subscription,
    )
    .await
    {
        Ok(()) => Ok(()),
        Err(RateLimitCooldownCommitError::Sql(error)) => Err(error.into()),
        Err(RateLimitCooldownCommitError::MissingProviderCooldown) => Err(
            SubscriptionEnrollmentApplicationError::InvalidState(INVALID_APPLICATION_STATE),
        ),
    }
}

pub(crate) async fn commit_rate_limit_cooldown_for_identity(
    pool: &PgPool,
    identity: PaymentAttemptIdentity,
    provider_key: &GatewayProviderKey,
    cooldown: RateLimitCooldown,
) -> Result<RateLimitCooldownCommitDisposition, sqlx::Error> {
    let mut transaction = pool.begin().await?;
    set_application_timeouts(&mut transaction).await?;
    if !rate_limit_cooldown_identity_is_durable(&mut transaction, identity).await? {
        transaction.rollback().await?;
        return Ok(RateLimitCooldownCommitDisposition::IdentityNotDurable);
    }
    match persist_rate_limit_cooldown(&mut transaction, identity, provider_key, cooldown).await? {
        RateLimitCooldownPersistence::Applied => {
            transaction.commit().await?;
            Ok(RateLimitCooldownCommitDisposition::Applied)
        }
        RateLimitCooldownPersistence::IdentityChanged => {
            transaction.rollback().await?;
            Ok(RateLimitCooldownCommitDisposition::IdentityChanged)
        }
        RateLimitCooldownPersistence::MissingProviderCooldown => {
            transaction.rollback().await?;
            Ok(RateLimitCooldownCommitDisposition::MissingProviderCooldown)
        }
    }
}

pub(crate) async fn commit_rate_limit_cooldown_for_operation(
    pool: &PgPool,
    identity: PaymentAttemptIdentity,
    provider_key: &GatewayProviderKey,
    cooldown: RateLimitCooldown,
    operation: RateLimitCooldownOperation,
) -> Result<(), RateLimitCooldownCommitError> {
    match commit_rate_limit_cooldown_for_identity(pool, identity, provider_key, cooldown).await? {
        RateLimitCooldownCommitDisposition::Applied => Ok(()),
        RateLimitCooldownCommitDisposition::IdentityNotDurable => {
            tracing::warn!(
                target: "syrup_rail::gateway_cooldown",
                billing_scope_id = %identity.billing_scope_id().as_uuid(),
                gateway_account_id = %identity.gateway_account_id().as_uuid(),
                provider_key = provider_key.as_str(),
                ?cooldown,
                "{}",
                operation.identity_not_durable_message()
            );
            Ok(())
        }
        RateLimitCooldownCommitDisposition::IdentityChanged => {
            tracing::warn!(
                target: "syrup_rail::gateway_cooldown",
                billing_scope_id = %identity.billing_scope_id().as_uuid(),
                gateway_account_id = %identity.gateway_account_id().as_uuid(),
                provider_key = provider_key.as_str(),
                ?cooldown,
                "{}",
                operation.identity_changed_message()
            );
            Ok(())
        }
        RateLimitCooldownCommitDisposition::MissingProviderCooldown => {
            tracing::error!(
                target: "syrup_rail::gateway_cooldown",
                billing_scope_id = %identity.billing_scope_id().as_uuid(),
                gateway_account_id = %identity.gateway_account_id().as_uuid(),
                provider_key = provider_key.as_str(),
                ?cooldown,
                "{}",
                operation.missing_provider_message()
            );
            Err(RateLimitCooldownCommitError::MissingProviderCooldown)
        }
    }
}

pub(crate) async fn rate_limit_cooldown_identity_is_durable(
    connection: &mut PgConnection,
    identity: PaymentAttemptIdentity,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM billing_payment_attempts
            WHERE id = $1
                AND billing_scope_id = $2
                AND subscriber_id = $3
                AND gateway_account_id = $4
                AND gateway_configuration_id = $5
                AND required_gateway_account_mode = $6
        )
        "#,
    )
    .bind(identity.attempt_id().as_uuid())
    .bind(identity.billing_scope_id().as_uuid())
    .bind(identity.subscriber_id().as_uuid())
    .bind(identity.gateway_account_id().as_uuid())
    .bind(identity.gateway_configuration_id().as_uuid())
    .bind(identity.required_gateway_account_mode().as_str())
    .fetch_one(&mut *connection)
    .await
}

pub(crate) async fn persist_rate_limit_cooldown(
    connection: &mut PgConnection,
    identity: PaymentAttemptIdentity,
    provider_key: &GatewayProviderKey,
    cooldown: RateLimitCooldown,
) -> Result<RateLimitCooldownPersistence, sqlx::Error> {
    if cooldown == RateLimitCooldown::Provider {
        return persist_bound_provider_rate_limit_cooldown(
            connection,
            identity.billing_scope_id(),
            identity.gateway_account_id(),
            provider_key,
        )
        .await;
    }
    let result = sqlx::query(
        r#"
        UPDATE billing_gateway_accounts
        SET mutation_rate_limited_until = GREATEST(
                COALESCE(mutation_rate_limited_until, '-infinity'::timestamptz),
                clock_timestamp() + make_interval(secs => $4)
            )
        WHERE id = $1 AND billing_scope_id = $2
            AND gateway_configuration_id = $3
        "#,
    )
    .bind(identity.gateway_account_id().as_uuid())
    .bind(identity.billing_scope_id().as_uuid())
    .bind(identity.gateway_configuration_id().as_uuid())
    .bind(syrup_rail::GATEWAY_MUTATION_RATE_LIMIT_RETRY_AFTER_SECONDS)
    .execute(&mut *connection)
    .await?;
    Ok(if result.rows_affected() == 1 {
        RateLimitCooldownPersistence::Applied
    } else {
        RateLimitCooldownPersistence::IdentityChanged
    })
}

pub(crate) async fn persist_bound_provider_rate_limit_cooldown(
    connection: &mut PgConnection,
    billing_scope_id: syrup_rail::BillingScopeId,
    gateway_account_id: syrup_rail::GatewayAccountId,
    provider_key: &GatewayProviderKey,
) -> Result<RateLimitCooldownPersistence, sqlx::Error> {
    let current_provider = sqlx::query_scalar::<_, String>(
        r#"
        SELECT provider_key
        FROM billing_gateway_accounts
        WHERE id = $1 AND billing_scope_id = $2
        FOR UPDATE
        "#,
    )
    .bind(gateway_account_id.as_uuid())
    .bind(billing_scope_id.as_uuid())
    .fetch_optional(&mut *connection)
    .await?;
    if current_provider.as_deref() != Some(provider_key.as_str()) {
        return Ok(RateLimitCooldownPersistence::IdentityChanged);
    }
    let result = sqlx::query(
        r#"
        UPDATE billing_gateway_provider_rate_limits AS provider_limits
        SET rate_limited_until = GREATEST(
            provider_limits.rate_limited_until,
            clock_timestamp() + make_interval(secs => $4)
        )
        FROM billing_gateway_accounts AS accounts
        WHERE provider_limits.provider_key = $1
            AND accounts.provider_key = provider_limits.provider_key
            AND accounts.id = $2
            AND accounts.billing_scope_id = $3
        "#,
    )
    .bind(provider_key.as_str())
    .bind(gateway_account_id.as_uuid())
    .bind(billing_scope_id.as_uuid())
    .bind(syrup_rail::GATEWAY_MUTATION_RATE_LIMIT_RETRY_AFTER_SECONDS)
    .execute(&mut *connection)
    .await?;
    Ok(if result.rows_affected() == 1 {
        RateLimitCooldownPersistence::Applied
    } else {
        RateLimitCooldownPersistence::MissingProviderCooldown
    })
}

/// Complete durable policy for a mutation that provably stopped before the
/// provider accepted it.
///
/// Keeping resolution, retry safety, cooldown, and claimed-target release in
/// one exhaustive transform prevents a new error variant from acquiring only
/// part of its required persistence behavior.
///
/// Every provably not-submitted transient outage restores a resumable prepared
/// attempt. Automatic renewal cannot consume that retry classification because
/// it has no prepared-attempt replay path.
#[derive(Clone, Copy)]
pub(crate) struct GatewayNotSubmittedPolicy {
    resolution_code: PaymentResolutionCode,
    retry_safety: GatewayNotSubmittedRetrySafety,
    cooldown: Option<RateLimitCooldown>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GatewayNotSubmittedRetrySafety {
    Terminal,
    RestorePreparedWhenSupported,
}

impl GatewayNotSubmittedPolicy {
    pub(crate) const fn for_error(error: &GatewayNotSubmittedError) -> Self {
        match error {
            GatewayNotSubmittedError::RequestRejected(_) => {
                Self::terminal(PaymentResolutionCode::GatewayRequestRejectedBeforeSubmission)
            }
            GatewayNotSubmittedError::Malformed(_) => {
                Self::terminal(PaymentResolutionCode::GatewayMalformedBeforeSubmission)
            }
            GatewayNotSubmittedError::Configuration(_) => {
                Self::terminal(PaymentResolutionCode::GatewayConfigurationBeforeSubmission)
            }
            GatewayNotSubmittedError::NotTransmitted(_) => {
                Self::retryable(PaymentResolutionCode::GatewayUnavailableBeforeSubmission)
            }
            // An in-band mutation response proves this resolved merchant
            // account was throttled before submission; another account may
            // still submit. NMI's distinct HTTP 429 transport signal is
            // documented as system-wide and indeterminate, so its provider
            // cooldown is handled separately by RateLimitedIndeterminate.
            GatewayNotSubmittedError::RateLimited(_) => Self::throttled(
                PaymentResolutionCode::GatewayAccountRateLimitedBeforeSubmission,
                RateLimitCooldown::Account,
            ),
            GatewayNotSubmittedError::AccountModeMismatch { required, .. } => {
                Self::for_account_mode_mismatch(*required)
            }
            GatewayNotSubmittedError::AccountModeVerification(error) => {
                Self::for_readiness_error(error)
            }
        }
    }

    pub(crate) const fn for_readiness_error(error: &GatewayError) -> Self {
        match error {
            GatewayError::RequestRejected(_) => {
                Self::terminal(PaymentResolutionCode::GatewayRequestRejectedBeforeSubmission)
            }
            GatewayError::Malformed(_) => {
                Self::terminal(PaymentResolutionCode::GatewayMalformedBeforeSubmission)
            }
            GatewayError::Configuration(_) => {
                Self::terminal(PaymentResolutionCode::GatewayConfigurationBeforeSubmission)
            }
            GatewayError::Unavailable(_) => {
                Self::retryable(PaymentResolutionCode::GatewayUnavailableBeforeSubmission)
            }
            // Readiness adapters expose one coarse RateLimited category. NMI's
            // query path collapses its system-wide HTTP 429 and any in-band
            // throttle into that category, so provider scope is the
            // conservative safe policy when the original signal is unavailable.
            GatewayError::RateLimited(_) => Self::throttled(
                PaymentResolutionCode::GatewayProviderRateLimitedBeforeSubmission,
                RateLimitCooldown::Provider,
            ),
        }
    }

    pub(crate) const fn for_account_mode_mismatch(required: GatewayAccountMode) -> Self {
        Self::terminal(match required {
            GatewayAccountMode::Live => {
                PaymentResolutionCode::GatewayLiveReadinessFailedBeforeSubmission
            }
            GatewayAccountMode::Test => {
                PaymentResolutionCode::GatewayTestReadinessFailedBeforeSubmission
            }
        })
    }

    const fn terminal(resolution_code: PaymentResolutionCode) -> Self {
        Self {
            resolution_code,
            retry_safety: GatewayNotSubmittedRetrySafety::Terminal,
            cooldown: None,
        }
    }

    const fn throttled(
        resolution_code: PaymentResolutionCode,
        cooldown: RateLimitCooldown,
    ) -> Self {
        Self {
            resolution_code,
            retry_safety: GatewayNotSubmittedRetrySafety::Terminal,
            cooldown: Some(cooldown),
        }
    }

    const fn retryable(resolution_code: PaymentResolutionCode) -> Self {
        Self {
            resolution_code,
            retry_safety: GatewayNotSubmittedRetrySafety::RestorePreparedWhenSupported,
            cooldown: None,
        }
    }

    pub(crate) const fn resolution_code(self) -> PaymentResolutionCode {
        self.resolution_code
    }

    pub(crate) const fn cooldown(self) -> Option<RateLimitCooldown> {
        self.cooldown
    }

    /// Whether a flow with a durable prepared replay path should undo its
    /// admission after proving that the provider mutation was not contacted.
    /// Automatic renewal has no such replay path and retains terminal handling.
    pub(crate) const fn restores_prepared_attempt_when_supported(self) -> bool {
        matches!(
            self.retry_safety,
            GatewayNotSubmittedRetrySafety::RestorePreparedWhenSupported
        )
    }
}
