use super::*;

impl SubscriptionBillingService {
    pub(super) async fn admit_subscriber_mutation(
        &self,
        billing_scope_id: BillingScopeId,
        subscriber_id: syrup_rail::SubscriberId,
        operation: EndUserMutationOperation,
    ) -> Result<(), SubscriptionBillingServiceError> {
        let result = self
            .admission
            .admit(EndUserMutationCommand::new(
                billing_scope_id,
                subscriber_id,
                operation,
            ))
            .await;
        map_subscriber_mutation_admission(result)
    }

    /// Resolves the canonical account after the caller's operation-specific
    /// preflight and admission phases. The three subscriber-initiated paths
    /// share the same cooldown and exact resolver-identity contract.
    pub(super) async fn resolve_active_gateway(
        &self,
        billing_scope_id: BillingScopeId,
        gateway_configuration_id: syrup_rail::GatewayConfigurationId,
    ) -> Result<
        (GatewayAccountSnapshot, syrup_rail::ResolvedGateway),
        SubscriptionBillingServiceError,
    > {
        let account = self
            .gateway_account(billing_scope_id, gateway_configuration_id)
            .await?;
        if let Some(scope) = self.active_cooldown(&account).await? {
            return Err(SubscriptionBillingServiceError::GatewayMutationCooldown { scope });
        }
        let expected = ExpectedGatewayIdentity::for_account(
            billing_scope_id,
            gateway_configuration_id,
            &account,
        );
        let gateway = self
            .resolver
            .resolve(
                expected.billing_scope_id,
                expected.gateway_account_id,
                expected.gateway_configuration_id,
                expected.provider_key.clone(),
            )
            .await?;
        if !expected.matches(&gateway) {
            return Err(SubscriptionBillingServiceError::ResolvedGatewayIdentityMismatch);
        }
        Ok((account, gateway))
    }

    pub(super) async fn gateway_account(
        &self,
        billing_scope_id: BillingScopeId,
        gateway_configuration_id: syrup_rail::GatewayConfigurationId,
    ) -> Result<GatewayAccountSnapshot, SubscriptionBillingServiceError> {
        let row = sqlx::query_as::<_, (uuid::Uuid, String)>(
            r#"
            SELECT id, provider_key
            FROM billing_gateway_accounts
            WHERE billing_scope_id = $1 AND gateway_configuration_id = $2
            "#,
        )
        .bind(billing_scope_id.as_uuid())
        .bind(gateway_configuration_id.as_uuid())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(SubscriptionBillingServiceError::GatewayConfigurationChanged)?;
        let provider_key = GatewayProviderKey::new(&row.1)
            .map_err(|_| SubscriptionBillingServiceError::InvalidState(INVALID_SERVICE_STATE))?;
        Ok(GatewayAccountSnapshot {
            account_id: GatewayAccountId::new(row.0),
            provider_key,
        })
    }

    pub(super) async fn active_cooldown(
        &self,
        account: &GatewayAccountSnapshot,
    ) -> Result<Option<GatewayMutationCooldownScope>, SubscriptionBillingServiceError> {
        let row = crate::gateway_accounts::load_gateway_cooldown(
            &self.pool,
            account.account_id,
            &account.provider_key,
        )
        .await?
        .ok_or(SubscriptionBillingServiceError::GatewayConfigurationChanged)?;
        Ok(GatewayMutationCooldownScope::from_active_flags(
            row.0, row.1,
        ))
    }

    pub(super) async fn resolve_subscriber_readiness_failure(
        &self,
        reservation: SubscriberInitiatedReservation<'_>,
        failure: SubscriberReadinessFailure,
        boundary: OutcomeResolutionBoundary,
    ) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionBillingServiceError> {
        let gateway_error = failure.gateway_error();
        if boundary == OutcomeResolutionBoundary::Prepared
            && let Some(error) = gateway_error.as_ref()
            && GatewayNotSubmittedPolicy::for_readiness_error(error)
                .restores_prepared_attempt_when_supported()
        {
            return Err(SubscriptionBillingServiceError::GatewayReadiness(
                clone_gateway_error(error),
            ));
        }
        let policy = failure.policy();
        let code = policy.resolution_code();
        let cooldown_error_scope = policy.cooldown_error_scope();
        let payment = record_readiness_failure(&self.pool, reservation, failure, boundary)
            .await
            .map_err(SubscriptionBillingServiceError::from)?;
        if let Some(scope) = cooldown_error_scope
            && payment.attempt().state().resolution_code() == Some(code)
        {
            return Err(SubscriptionBillingServiceError::GatewayMutationCooldown { scope });
        }
        if let Some(error) = gateway_error
            && payment.attempt().state().resolution_code() == Some(code)
        {
            return Err(SubscriptionBillingServiceError::GatewayReadiness(error));
        }
        Ok(payment)
    }
}

/// Records one readiness failure on its prepared or admitted reservation: the
/// failure's resolution code, the cooldown it implies, and its diagnostic.
async fn record_readiness_failure(
    pool: &PgPool,
    reservation: SubscriberInitiatedReservation<'_>,
    failure: SubscriberReadinessFailure,
    boundary: OutcomeResolutionBoundary,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    let policy = failure.policy();
    let evidence = ProcessorEvidence::new(
        None,
        None,
        None,
        None,
        Some(failure.into_detail()),
        Some(GatewayDiagnostic::new("failed")),
        GatewayPaymentDescriptor::default(),
    );
    reservation
        .resolve_non_approved(
            pool,
            &evidence,
            policy.resolution_code(),
            policy.cooldown(),
            boundary,
        )
        .await
}

/// What became of a prepared subscription recovery whose gateway readiness
/// check failed before admission.
#[derive(Debug)]
pub enum SubscriptionRecoveryReadinessResolution {
    /// A transient provider outage: nothing was recorded, and the prepared
    /// attempt stays resumable by an exact replay.
    Retained,
    /// The attempt resolved before submission with the resolution code
    /// [`SubscriptionBillingService::recover`] records for the same failure.
    /// A rate-limited readiness query also recorded the provider-scoped
    /// cooldown that fences every subscriber mutation on that provider.
    Resolved(Box<SubscriptionEnrollmentPaymentResult>),
}

/// Applies [`SubscriptionBillingService::recover`]'s readiness policy to a
/// host-composed recovery whose [`crate::verify_gateway_account_mode`] failed
/// before [`crate::admit_subscription_recovery_submission_with_transaction`].
///
/// A host that reserves and admits a recovery itself must not discard that
/// failure: a transient outage keeps the prepared attempt, a rate limit
/// records the provider cooldown and resolves the attempt, and any other
/// failure, including an account-mode mismatch, resolves it terminally. An
/// attempt that is no longer prepared is left as it is and reported as its
/// current payment.
pub async fn resolve_subscription_recovery_readiness_failure(
    pool: &PgPool,
    reservation: &SubscriptionRecoveryReservation,
    error: GatewayAccountModeVerificationError,
) -> Result<SubscriptionRecoveryReadinessResolution, SubscriptionEnrollmentApplicationError> {
    let failure = match error {
        GatewayAccountModeVerificationError::AccountModeMismatch { required, .. } => {
            SubscriberReadinessFailure::AccountMode(required)
        }
        GatewayAccountModeVerificationError::Gateway(error) => {
            if GatewayNotSubmittedPolicy::for_readiness_error(&error)
                .restores_prepared_attempt_when_supported()
            {
                return Ok(SubscriptionRecoveryReadinessResolution::Retained);
            }
            SubscriberReadinessFailure::Gateway(error)
        }
    };
    record_readiness_failure(
        pool,
        SubscriberInitiatedReservation::Recovery(reservation),
        failure,
        OutcomeResolutionBoundary::Prepared,
    )
    .await
    .map(|payment| SubscriptionRecoveryReadinessResolution::Resolved(Box::new(payment)))
}
