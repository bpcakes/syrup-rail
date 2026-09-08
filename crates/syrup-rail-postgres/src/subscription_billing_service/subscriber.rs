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
        let gateway = self
            .resolver
            .resolve_identity(account.identity().clone())
            .await?;
        if account.identity() != gateway.identity() {
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
            identity: GatewayAccountIdentity::new(
                billing_scope_id,
                GatewayAccountId::new(row.0),
                provider_key,
                gateway_configuration_id,
            ),
        })
    }

    pub(super) async fn active_cooldown(
        &self,
        account: &GatewayAccountSnapshot,
    ) -> Result<Option<GatewayMutationCooldownScope>, SubscriptionBillingServiceError> {
        let row = crate::gateway_accounts::load_gateway_cooldown(
            &self.pool,
            account.account_id(),
            account.provider_key(),
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
        let cooldown = policy.cooldown();
        let cooldown_error_scope = policy.cooldown_error_scope();
        let detail = failure.into_detail();
        let condition = (code != PaymentResolutionCode::GatewayProviderRateLimitedBeforeSubmission)
            .then(|| GatewayDiagnostic::new("failed"));
        let evidence = ProcessorEvidence::new(
            syrup_rail::ProcessorApprovalEvidence::Absent,
            None,
            None,
            None,
            None,
            Some(detail),
            condition,
            GatewayPaymentDescriptor::default(),
        );
        let payment = reservation
            .resolve_non_approved(&self.pool, &evidence, code, cooldown, boundary)
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
