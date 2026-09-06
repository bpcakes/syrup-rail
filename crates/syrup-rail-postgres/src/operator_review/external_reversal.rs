use super::*;

/// Value-redacted failure returned by the host's reversal target store.
#[derive(Debug)]
pub struct ExternalReversalHostStoreError {
    source: RedactedHostErrorSource,
}

impl ExternalReversalHostStoreError {
    /// Wraps a host error without exposing its value through ordinary error
    /// formatting or the standard error-source chain.
    pub fn new(source: impl Error + Send + Sync + 'static) -> Self {
        Self {
            source: RedactedHostErrorSource::new(source),
        }
    }

    /// Returns the host error for explicit application-level inspection.
    pub fn into_source(self) -> BoxError {
        self.source.into_inner()
    }
}

impl fmt::Display for ExternalReversalHostStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("external reversal host target transition failed")
    }
}

impl Error for ExternalReversalHostStoreError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExternalReversalHostTransitionOutcome {
    Changed,
    Unchanged,
}

#[async_trait]
pub trait ExternalReversalHostStore: Send + Sync {
    async fn release(
        &self,
        connection: &mut PgConnection,
        release: ExternalReversalHostChargeRelease,
    ) -> Result<ExternalReversalHostTransitionOutcome, ExternalReversalHostStoreError>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExternalReversalAttestationOutcome {
    Attested {
        attempt: PaymentAttempt,
        attestation: ExternalReversalAttestation,
    },
    Replayed {
        attempt: PaymentAttempt,
        attestation: ExternalReversalAttestation,
    },
    NotFound,
    Ineligible,
    ReplayConflict,
}

#[derive(Clone, Debug)]
pub(super) struct ChargeLocator {
    pub(super) charge_id: ProcessorChargeId,
    pub(super) attempt_id: PaymentAttemptId,
    pub(super) billing_scope_id: BillingScopeId,
    pub(super) subscriber_id: SubscriberId,
    pub(super) kind: PaymentAttemptKind,
    pub(super) plan_key: Option<PlanKey>,
    pub(super) host_target_id: Option<HostChargeTargetId>,
}

pub async fn attest_external_reversal(
    pool: &PgPool,
    host: &dyn ExternalReversalHostStore,
    processor_charge_id: ProcessorChargeId,
    actor_id: ActorId,
    kind: ExternalReversalKind,
    expected_transaction_id: &GatewayTransactionId,
    reason: &ExternalReversalReason,
) -> Result<ExternalReversalAttestationOutcome, OperatorReviewError> {
    let mut transaction = pool.begin().await?;
    set_enrollment_timeouts(&mut transaction).await?;
    let Some(locator) = charge_locator(&mut transaction, processor_charge_id).await? else {
        transaction.commit().await?;
        return Ok(ExternalReversalAttestationOutcome::NotFound);
    };
    if let Some(plan_key) = &locator.plan_key {
        lock_subscription_aggregate(&mut transaction, locator.subscriber_id, plan_key).await?;
    }
    let Some(attempt) = lock_payment_attempt_by_id_on_connection(
        &mut transaction,
        locator.billing_scope_id,
        locator.attempt_id,
    )
    .await
    .map_err(|error| match error {
        crate::PaymentAttemptStoreError::Sql(error) => OperatorReviewError::Sql(error),
        crate::PaymentAttemptStoreError::InvalidState(_) => {
            OperatorReviewError::InvalidState("locked operator review attempt is invalid")
        }
    })?
    else {
        transaction.commit().await?;
        return Ok(ExternalReversalAttestationOutcome::NotFound);
    };
    let Some(charge) = lock_processor_charge(&mut transaction, processor_charge_id).await? else {
        return Err(OperatorReviewError::InvalidState(
            "operator review processor charge disappeared while locked",
        ));
    };
    if !locator_matches(&locator, &attempt, &charge) {
        return Err(OperatorReviewError::InvalidState(
            "operator review locator changed while locking",
        ));
    }

    if let Some(existing) =
        attestation_by_charge(&mut transaction, processor_charge_id.into_uuid()).await?
    {
        let matches = existing.actor_id() == actor_id
            && existing.kind() == kind
            && existing.reason() == reason
            && existing.gateway_transaction_id() == expected_transaction_id
            && charge.progression() == ProcessorChargeProgression::ExternallyReversed
            && attestation_matches_source(&existing, &attempt, &charge);
        if matches && can_release_host_target(&attempt, &charge) {
            release_host_target(host, &mut transaction, &attempt).await?;
        }
        transaction.commit().await?;
        return Ok(if matches {
            ExternalReversalAttestationOutcome::Replayed {
                attempt,
                attestation: existing,
            }
        } else {
            ExternalReversalAttestationOutcome::ReplayConflict
        });
    }

    if !processor_charge_can_attest_external_reversal(&charge)
        || charge.evidence().transaction_id() != Some(expected_transaction_id)
    {
        transaction.commit().await?;
        return Ok(ExternalReversalAttestationOutcome::Ineligible);
    }

    persist_external_reversal(
        &mut transaction,
        host,
        &attempt,
        &charge,
        actor_id,
        kind,
        reason,
    )
    .await?;
    let attempt = load_attempt(
        &mut transaction,
        locator.billing_scope_id,
        locator.attempt_id,
    )
    .await?;
    let attestation = attestation_by_charge(&mut transaction, processor_charge_id.into_uuid())
        .await?
        .ok_or(OperatorReviewError::InvalidState(
            "external reversal attestation disappeared while locked",
        ))?;
    transaction.commit().await?;
    Ok(ExternalReversalAttestationOutcome::Attested {
        attempt,
        attestation,
    })
}

async fn persist_external_reversal(
    transaction: &mut Transaction<'_, Postgres>,
    host: &dyn ExternalReversalHostStore,
    attempt: &PaymentAttempt,
    charge: &ProcessorCharge,
    actor_id: ActorId,
    kind: ExternalReversalKind,
    reason: &ExternalReversalReason,
) -> Result<(), OperatorReviewError> {
    let resolution = expected_reversal_resolution(attempt, charge, kind);
    let attested_at: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut **transaction)
        .await?;
    insert_attestation(
        transaction,
        attempt,
        charge,
        actor_id,
        reason,
        resolution,
        attested_at,
    )
    .await?;
    let updated = sqlx::query(
        r#"
        UPDATE billing_processor_charges
        SET progression_state = 'externally_reversed',
            state_code = $2,
            externally_reversed_at = COALESCE(externally_reversed_at, clock_timestamp()),
            updated_at = clock_timestamp()
        WHERE id = $1 AND progression_state = 'external_reversal_required'
        "#,
    )
    .bind(charge.id().as_uuid())
    .bind(resolution.prior_resolution_code())
    .execute(&mut **transaction)
    .await?;
    if updated.rows_affected() != 1 {
        return Err(OperatorReviewError::InvalidState(
            "eligible processor charge did not accept external reversal",
        ));
    }
    if charge.role() == ProcessorChargeRole::Primary && !attempt.status().is_terminal() {
        let updated = sqlx::query(
            r#"
            UPDATE billing_payment_attempts
            SET status = 'failed', resolution_code = $2,
                resolved_at = $3, updated_at = $3
            WHERE id = $1 AND status = $4
            "#,
        )
        .bind(attempt.identity().attempt_id().as_uuid())
        .bind(resolution.final_resolution_code().as_str())
        .bind(attested_at)
        .bind(attempt.status().as_str())
        .execute(&mut **transaction)
        .await?;
        if updated.rows_affected() != 1 {
            return Err(OperatorReviewError::InvalidState(
                "eligible attempt did not accept external reversal",
            ));
        }
    }
    if can_release_host_target(attempt, charge) {
        release_host_target(host, transaction, attempt).await?;
    }
    Ok(())
}

pub(super) async fn charge_locator(
    transaction: &mut Transaction<'_, Postgres>,
    charge_id: ProcessorChargeId,
) -> Result<Option<ChargeLocator>, OperatorReviewError> {
    let row = sqlx::query(
        r#"
        SELECT charges.id, charges.attempt_id,
            attempts.billing_scope_id, attempts.subscriber_id,
            attempts.attempt_kind, attempts.plan_key, attempts.host_charge_target_id
        FROM billing_processor_charges charges
        INNER JOIN billing_payment_attempts attempts ON attempts.id = charges.attempt_id
        WHERE charges.id = $1
        "#,
    )
    .bind(charge_id.as_uuid())
    .fetch_optional(&mut **transaction)
    .await?;
    row.map(|row| {
        Ok(ChargeLocator {
            charge_id: ProcessorChargeId::new(row.try_get("id")?),
            attempt_id: PaymentAttemptId::new(row.try_get("attempt_id")?),
            billing_scope_id: BillingScopeId::new(row.try_get("billing_scope_id")?),
            subscriber_id: SubscriberId::new(row.try_get("subscriber_id")?),
            kind: row
                .try_get::<String, _>("attempt_kind")?
                .parse()
                .map_err(|_| {
                    OperatorReviewError::InvalidState("operator locator attempt kind is invalid")
                })?,
            plan_key: row
                .try_get::<Option<String>, _>("plan_key")?
                .map(PlanKey::new)
                .transpose()
                .map_err(|_| {
                    OperatorReviewError::InvalidState("operator locator plan key is invalid")
                })?,
            host_target_id: row
                .try_get::<Option<Uuid>, _>("host_charge_target_id")?
                .map(HostChargeTargetId::new),
        })
    })
    .transpose()
}

pub(super) async fn lock_processor_charge(
    transaction: &mut Transaction<'_, Postgres>,
    charge_id: ProcessorChargeId,
) -> Result<Option<ProcessorCharge>, OperatorReviewError> {
    let row = sqlx::query(
        r#"
        SELECT id, attempt_id, billing_scope_id, gateway_account_id,
            gateway_order_id, attempt_kind, amount_cents, currency,
            charge_role, progression_state, state_code,
            gateway_transaction_id, gateway_payment_method_reference,
            gateway_approval_evidence, gateway_response, gateway_response_code, gateway_response_text,
            gateway_condition, payment_type, card_brand, card_last4,
            card_exp_month, card_exp_year, observed_at
        FROM billing_processor_charges WHERE id = $1 FOR UPDATE
        "#,
    )
    .bind(charge_id.as_uuid())
    .fetch_optional(&mut **transaction)
    .await?;
    Ok(row.as_ref().map(processor_charge_from_row).transpose()?)
}

#[allow(clippy::too_many_arguments)]
async fn insert_attestation(
    transaction: &mut Transaction<'_, Postgres>,
    attempt: &PaymentAttempt,
    charge: &ProcessorCharge,
    actor_id: ActorId,
    reason: &ExternalReversalReason,
    resolution: ExternalReversalResolution,
    attested_at: DateTime<Utc>,
) -> Result<(), OperatorReviewError> {
    let identity = attempt.identity();
    let evidence = charge.evidence();
    let descriptor = evidence.descriptor();
    let transaction_id = evidence
        .transaction_id()
        .ok_or(OperatorReviewError::InvalidState(
            "eligible charge is missing transaction identity",
        ))?;
    sqlx::query(
        r#"
        INSERT INTO billing_external_reversal_attestations (
            processor_charge_id, attempt_id, actor_id, reversal_kind, reason,
            prior_resolution_code, final_resolution_code,
            gateway_account_id, gateway_configuration_id, gateway_order_id,
            amount_cents, currency, gateway_transaction_id,
            gateway_payment_method_reference, gateway_response, gateway_response_code,
            gateway_response_text, gateway_condition, payment_type, card_brand,
            card_last4, card_exp_month, card_exp_year, attested_at, gateway_approval_evidence
        ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12,
            $13, $14, $15, $16, $17, $18, $19, $20, $21, $22, $23, $24, $25
        )
        "#,
    )
    .bind(charge.id().as_uuid())
    .bind(identity.attempt_id().as_uuid())
    .bind(actor_id.as_uuid())
    .bind(resolution.kind().as_str())
    .bind(reason.expose())
    .bind(resolution.prior_resolution_code())
    .bind(resolution.final_resolution_code().as_str())
    .bind(identity.gateway_account_id().as_uuid())
    .bind(identity.gateway_configuration_id().as_uuid())
    .bind(charge.gateway_order_id().expose())
    .bind(charge.amount().cents())
    .bind(charge.amount().currency().as_str())
    .bind(transaction_id.expose())
    .bind(
        evidence
            .payment_method_reference()
            .map(|value| value.expose()),
    )
    .bind(evidence.response().map(|value| value.expose()))
    .bind(evidence.response_code().map(|value| value.expose()))
    .bind(evidence.response_text().map(|value| value.expose()))
    .bind(evidence.condition().map(|value| value.expose()))
    .bind(descriptor.payment_type().map(|value| value.expose()))
    .bind(descriptor.card_brand().map(|value| value.expose()))
    .bind(descriptor.card_last_four().map(|value| value.expose()))
    .bind(descriptor.card_exp_month())
    .bind(descriptor.card_exp_year())
    .bind(attested_at)
    .bind(evidence.approval_evidence().as_str())
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

fn locator_matches(
    locator: &ChargeLocator,
    attempt: &PaymentAttempt,
    charge: &ProcessorCharge,
) -> bool {
    let identity = attempt.identity();
    locator.charge_id == charge.id()
        && locator.attempt_id == identity.attempt_id()
        && locator.billing_scope_id == identity.billing_scope_id()
        && locator.subscriber_id == identity.subscriber_id()
        && locator.kind == attempt.kind()
        && locator.plan_key.as_ref() == attempt.request().target().plan_key()
        && locator.host_target_id == attempt.request().target().host_charge_target_id()
        && charge.attempt_id() == identity.attempt_id()
        && charge.billing_scope_id() == identity.billing_scope_id()
        && charge.gateway_account_id() == identity.gateway_account_id()
        && charge.attempt_kind() == attempt.kind()
        && charge.gateway_order_id() == attempt.request().gateway_order_id()
        && charge.amount() == attempt.request().amount()
}

fn processor_charge_can_attest_external_reversal(charge: &ProcessorCharge) -> bool {
    charge.progression() == ProcessorChargeProgression::ExternalReversalRequired
        && charge.evidence().transaction_id().is_some()
        && matches!(
            charge.attempt_kind(),
            PaymentAttemptKind::HostCharge
                | PaymentAttemptKind::SubscriptionInitial
                | PaymentAttemptKind::SubscriptionRenewal
                | PaymentAttemptKind::SubscriptionRecovery
        )
}

fn can_release_host_target(attempt: &PaymentAttempt, charge: &ProcessorCharge) -> bool {
    attempt.kind() == PaymentAttemptKind::HostCharge
        && (matches!(
            attempt.status(),
            PaymentAttemptStatus::Declined | PaymentAttemptStatus::Failed
        ) || (charge.role() == ProcessorChargeRole::Primary && !attempt.status().is_terminal()))
}

async fn release_host_target(
    host: &dyn ExternalReversalHostStore,
    transaction: &mut Transaction<'_, Postgres>,
    attempt: &PaymentAttempt,
) -> Result<(), OperatorReviewError> {
    let target_id = attempt.request().target().host_charge_target_id().ok_or(
        OperatorReviewError::InvalidState("host charge is missing exact target"),
    )?;
    let identity = attempt.identity();
    let _ = host
        .release(
            &mut *transaction,
            ExternalReversalHostChargeRelease::new(
                identity.billing_scope_id(),
                identity.subscriber_id(),
                target_id,
            ),
        )
        .await?;
    Ok(())
}

async fn load_attempt(
    transaction: &mut Transaction<'_, Postgres>,
    scope: BillingScopeId,
    attempt_id: PaymentAttemptId,
) -> Result<PaymentAttempt, OperatorReviewError> {
    let query = format!(
        "{} WHERE billing_scope_id = $1 AND id = $2",
        crate::attempts::PAYMENT_ATTEMPT_SELECT
    );
    let row = sqlx::query(&query)
        .bind(scope.as_uuid())
        .bind(attempt_id.as_uuid())
        .fetch_optional(&mut **transaction)
        .await?
        .ok_or(OperatorReviewError::InvalidState(
            "operator review attempt disappeared",
        ))?;
    payment_attempt_from_row(&row).map_err(|_| {
        OperatorReviewError::InvalidState("post-attestation payment attempt is invalid")
    })
}
