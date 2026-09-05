#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExternalReversalAttestation {
    processor_charge_id: ProcessorChargeId,
    attempt_id: PaymentAttemptId,
    actor_id: ActorId,
    reason: ExternalReversalReason,
    resolution: ExternalReversalResolution,
    gateway_account_id: GatewayAccountId,
    gateway_configuration_id: GatewayConfigurationId,
    gateway_order_id: GatewayOrderId,
    amount: ChargeAmount,
    gateway_transaction_id: GatewayTransactionId,
    processor_evidence: ProcessorEvidence,
    attested_at: DateTime<Utc>,
}

impl ExternalReversalAttestation {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        processor_charge_id: ProcessorChargeId,
        attempt_id: PaymentAttemptId,
        actor_id: ActorId,
        reason: ExternalReversalReason,
        resolution: ExternalReversalResolution,
        gateway_account_id: GatewayAccountId,
        gateway_configuration_id: GatewayConfigurationId,
        gateway_order_id: GatewayOrderId,
        amount: ChargeAmount,
        gateway_transaction_id: GatewayTransactionId,
        processor_evidence: ProcessorEvidence,
        attested_at: DateTime<Utc>,
    ) -> Self {
        Self {
            processor_charge_id,
            attempt_id,
            actor_id,
            reason,
            resolution,
            gateway_account_id,
            gateway_configuration_id,
            gateway_order_id,
            amount,
            gateway_transaction_id,
            processor_evidence,
            attested_at,
        }
    }

    /// Validates the historical raw resolution fields before constructing the typed form.
    #[allow(clippy::too_many_arguments)]
    pub fn from_legacy_parts(
        processor_charge_id: ProcessorChargeId,
        attempt_id: PaymentAttemptId,
        actor_id: ActorId,
        kind: ExternalReversalKind,
        reason: ExternalReversalReason,
        prior_resolution_code: impl AsRef<str>,
        final_resolution_code: PaymentResolutionCode,
        gateway_account_id: GatewayAccountId,
        gateway_configuration_id: GatewayConfigurationId,
        gateway_order_id: GatewayOrderId,
        amount: ChargeAmount,
        gateway_transaction_id: GatewayTransactionId,
        processor_evidence: ProcessorEvidence,
        attested_at: DateTime<Utc>,
    ) -> Result<Self, ExternalReversalResolutionError> {
        let prior = ExternalReversalPriorClassification::from_resolution_code(
            prior_resolution_code.as_ref(),
        )?;
        let outcome = ExternalReversalOutcome::from_kind_and_final_resolution_code(
            kind,
            final_resolution_code,
        )?;
        let resolution = ExternalReversalResolution::new(prior, outcome)?;
        Ok(Self::new(
            processor_charge_id,
            attempt_id,
            actor_id,
            reason,
            resolution,
            gateway_account_id,
            gateway_configuration_id,
            gateway_order_id,
            amount,
            gateway_transaction_id,
            processor_evidence,
            attested_at,
        ))
    }

    pub const fn processor_charge_id(&self) -> ProcessorChargeId {
        self.processor_charge_id
    }
    pub const fn attempt_id(&self) -> PaymentAttemptId {
        self.attempt_id
    }
    pub const fn actor_id(&self) -> ActorId {
        self.actor_id
    }
    pub const fn kind(&self) -> ExternalReversalKind {
        self.resolution.kind()
    }
    pub const fn reason(&self) -> &ExternalReversalReason {
        &self.reason
    }
    pub const fn resolution(&self) -> ExternalReversalResolution {
        self.resolution
    }
    pub const fn prior_classification(&self) -> ExternalReversalPriorClassification {
        self.resolution.prior()
    }
    pub const fn outcome(&self) -> ExternalReversalOutcome {
        self.resolution.outcome()
    }
    pub const fn prior_resolution_code(&self) -> &'static str {
        self.resolution.prior_resolution_code()
    }
    pub const fn final_resolution_code(&self) -> PaymentResolutionCode {
        self.resolution.final_resolution_code()
    }
    pub const fn gateway_account_id(&self) -> GatewayAccountId {
        self.gateway_account_id
    }
    pub const fn gateway_configuration_id(&self) -> GatewayConfigurationId {
        self.gateway_configuration_id
    }
    pub const fn gateway_order_id(&self) -> &GatewayOrderId {
        &self.gateway_order_id
    }
    pub const fn amount(&self) -> ChargeAmount {
        self.amount
    }
    pub const fn gateway_transaction_id(&self) -> &GatewayTransactionId {
        &self.gateway_transaction_id
    }
    pub const fn processor_evidence(&self) -> &ProcessorEvidence {
        &self.processor_evidence
    }
    pub const fn attested_at(&self) -> DateTime<Utc> {
        self.attested_at
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExternalReversalHostChargeRelease {
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    target_id: HostChargeTargetId,
}

impl ExternalReversalHostChargeRelease {
    pub const fn new(
        billing_scope_id: BillingScopeId,
        subscriber_id: SubscriberId,
        target_id: HostChargeTargetId,
    ) -> Self {
        Self {
            billing_scope_id,
            subscriber_id,
            target_id,
        }
    }
    pub const fn billing_scope_id(self) -> BillingScopeId {
        self.billing_scope_id
    }
    pub const fn subscriber_id(self) -> SubscriberId {
        self.subscriber_id
    }
    pub const fn target_id(self) -> HostChargeTargetId {
        self.target_id
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
