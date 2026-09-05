use std::time::Duration;

use sqlx::{PgConnection, PgPool, Row};
use syrup_rail::{
    CurrencyCode, GatewayDiagnostic, GatewayOrderId, GatewayTransactionId, PaymentAttempt,
    PaymentAttemptId, PaymentAttemptIdentity, PaymentAttemptKind, PaymentAttemptStatus,
    PaymentResolutionCode, PlanKey, ProcessorCharge, ProcessorChargeId, ProcessorChargeProgression,
    ProcessorChargeRole, ProcessorChargeStateCode, ProcessorEvidence,
    SubscriptionEnrollmentReservation, SubscriptionPaymentMethodReplacement,
    SubscriptionRecoveryReservation,
};
use thiserror::Error;
use uuid::Uuid;

use crate::PaymentAttemptStoreError;
use crate::attempts::{
    PAYMENT_ATTEMPT_SELECT, find_payment_attempt_by_id_on_connection,
    lock_payment_attempt_by_id_on_connection, lock_subscription_aggregate,
    payment_attempt_from_row, set_enrollment_timeouts,
};
use crate::processor_charge_persistence::{
    ProcessorChargePersistenceError, attestation_by_charge, attestation_matches_source,
    processor_charge_from_row,
};

use storage::{
    charge_by_id, compensating_progression, identify_transactionless, initial_charge_progression,
    initial_charge_state_code, is_transient, matching_charge, owned_by_other_attempt, parse_role,
};

mod storage;

const INVALID_CHARGE_STATE: &str = "canonical processor charge state is invalid";
const STORE_MAX_ATTEMPTS: usize = 3;
const STORE_RETRY_DELAY: Duration = Duration::from_millis(50);

#[derive(Debug, Error)]
pub enum ProcessorChargeStoreError {
    #[error("processor charge storage operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("payment attempt storage operation failed")]
    Attempt(#[from] PaymentAttemptStoreError),
    #[error("{0}")]
    InvalidState(&'static str),
}

impl From<ProcessorChargePersistenceError> for ProcessorChargeStoreError {
    fn from(error: ProcessorChargePersistenceError) -> Self {
        match error {
            ProcessorChargePersistenceError::Sql(error) => Self::Sql(error),
            ProcessorChargePersistenceError::InvalidState(message) => Self::InvalidState(message),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompensatingProcessorChargeOutcome {
    Observed,
    ExactReplay,
    OwnedByOtherAttempt,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProcessorChargeObservationOutcome {
    Observed(ProcessorCharge),
    ExactReplay(ProcessorCharge),
    OwnedByOtherAttempt,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ChargeRecord {
    pub(crate) id: Uuid,
    pub(crate) role: ProcessorChargeRole,
    exact_replay: bool,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum ObservedCharge {
    Owned(ChargeRecord),
    OwnedByOtherAttempt,
}

/// Promotes a replayed conflicting charge into the operator reversal queue
/// without reopening a charge whose reversal was already attested.
pub(crate) async fn promote_conflicting_charge_to_external_reversal(
    connection: &mut PgConnection,
    observation: ObservedCharge,
) -> Result<(), ProcessorChargeStoreError> {
    let ObservedCharge::Owned(charge) = observation else {
        return Ok(());
    };
    match charge_by_id(connection, charge.id).await?.progression() {
        ProcessorChargeProgression::Pending
        | ProcessorChargeProgression::ReconciliationRequired => {
            transition_charge(
                connection,
                charge.id,
                ProcessorChargeProgression::ExternalReversalRequired,
                None,
            )
            .await
        }
        ProcessorChargeProgression::ExternalReversalRequired
        | ProcessorChargeProgression::ExternallyReversed => Ok(()),
        ProcessorChargeProgression::Applied => Err(ProcessorChargeStoreError::InvalidState(
            "an applied processor charge cannot become a conflicting external reversal",
        )),
    }
}

/// Frozen subscription charge dimensions used to retain approved processor
/// evidence when the owning payment-attempt row cannot be locked in time.
///
/// The operation-specific constructors keep the kind and amount shape tied to
/// the validated reservation rather than deriving them from an unlocked row.
#[derive(Clone, Copy)]
pub(crate) struct LockFreeApprovedEvidenceTerms<'a> {
    identity: PaymentAttemptIdentity,
    attempt_kind: PaymentAttemptKind,
    plan_key: &'a PlanKey,
    gateway_order_id: &'a GatewayOrderId,
    amount_cents: i32,
    currency: CurrencyCode,
    host_charge_target_id: Option<Uuid>,
}

impl<'a> LockFreeApprovedEvidenceTerms<'a> {
    pub(crate) fn initial(reservation: &'a SubscriptionEnrollmentReservation) -> Self {
        let charge = reservation.expected_terms().initial_charge();
        Self::subscription(
            reservation.identity(),
            PaymentAttemptKind::SubscriptionInitial,
            reservation.plan_key(),
            reservation.gateway_order_id(),
            charge.cents(),
            charge.currency(),
        )
    }

    pub(crate) fn recovery(reservation: &'a SubscriptionRecoveryReservation) -> Self {
        let request = reservation.request();
        let amount = request.amount();
        Self::subscription(
            reservation.identity(),
            PaymentAttemptKind::SubscriptionRecovery,
            reservation.plan_key(),
            request.gateway_order_id(),
            amount.cents(),
            amount.currency(),
        )
    }

    pub(crate) fn payment_method_replacement(
        reservation: &'a SubscriptionPaymentMethodReplacement,
    ) -> Self {
        let request = reservation.request();
        debug_assert_eq!(request.amount().cents(), 0);
        Self::subscription(
            reservation.identity(),
            PaymentAttemptKind::SubscriptionPaymentMethodUpdate,
            reservation.plan_key(),
            request.gateway_order_id(),
            0,
            request.amount().currency(),
        )
    }

    fn subscription(
        identity: PaymentAttemptIdentity,
        attempt_kind: PaymentAttemptKind,
        plan_key: &'a PlanKey,
        gateway_order_id: &'a GatewayOrderId,
        amount_cents: i32,
        currency: CurrencyCode,
    ) -> Self {
        debug_assert!(matches!(
            attempt_kind,
            PaymentAttemptKind::SubscriptionInitial
                | PaymentAttemptKind::SubscriptionRecovery
                | PaymentAttemptKind::SubscriptionPaymentMethodUpdate
        ));
        Self {
            identity,
            attempt_kind,
            plan_key,
            gateway_order_id,
            amount_cents,
            currency,
            host_charge_target_id: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LockFreeApprovedEvidenceOutcome {
    Persisted,
    ExactReplay,
    OwnedByOtherAttempt,
    NotDurable,
}

/// Last-resort immutable evidence write used when another transaction keeps
/// the attempt row locked beyond the bounded application window.
///
/// Database foreign keys and uniqueness constraints remain the authority, so
/// this path can retain processor evidence without mutating or locking the
/// attempt itself.
pub(crate) async fn persist_approved_evidence_without_attempt_lock(
    pool: &PgPool,
    terms: LockFreeApprovedEvidenceTerms<'_>,
    evidence: &ProcessorEvidence,
) -> Result<LockFreeApprovedEvidenceOutcome, sqlx::Error> {
    let identity = terms.identity;
    let descriptor = evidence.descriptor();
    let transaction_id = evidence.transaction_id().map(GatewayTransactionId::expose);
    let mut transaction = pool.begin().await?;
    set_enrollment_timeouts(&mut transaction).await?;

    for _ in 0..2 {
        let has_existing_charge: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM billing_processor_charges WHERE attempt_id = $1)",
        )
        .bind(identity.attempt_id().as_uuid())
        .fetch_one(&mut *transaction)
        .await?;
        let role = if has_existing_charge {
            "additional"
        } else {
            "primary"
        };
        let inserted = sqlx::query_scalar::<_, Uuid>(
            r#"
            INSERT INTO billing_processor_charges (
                id, attempt_id, billing_scope_id, gateway_account_id, gateway_order_id,
                gateway_transaction_id, gateway_payment_method_reference,
                gateway_response, gateway_response_code, gateway_response_text,
                gateway_condition, payment_type, card_brand, card_last4,
                card_exp_month, card_exp_year, charge_role, progression_state,
                attempt_kind, plan_key, host_charge_target_id, amount_cents, currency, gateway_approval_evidence
            ) VALUES (
                $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
                $14, $15, $16, $17, 'pending', $18, $19, $20, $21, $22, $23
            )
            ON CONFLICT DO NOTHING
            RETURNING id
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(identity.attempt_id().as_uuid())
        .bind(identity.billing_scope_id().as_uuid())
        .bind(identity.gateway_account_id().as_uuid())
        .bind(terms.gateway_order_id.expose())
        .bind(transaction_id)
        .bind(
            evidence
                .payment_method_reference()
                .map(|value| value.expose()),
        )
        .bind(evidence.response().map(GatewayDiagnostic::expose))
        .bind(evidence.response_code().map(GatewayDiagnostic::expose))
        .bind(evidence.response_text().map(GatewayDiagnostic::expose))
        .bind(evidence.condition().map(GatewayDiagnostic::expose))
        .bind(descriptor.payment_type().map(GatewayDiagnostic::expose))
        .bind(descriptor.card_brand().map(GatewayDiagnostic::expose))
        .bind(descriptor.card_last_four().map(|value| value.expose()))
        .bind(descriptor.card_exp_month())
        .bind(descriptor.card_exp_year())
        .bind(role)
        .bind(terms.attempt_kind.as_str())
        .bind(terms.plan_key.as_str())
        .bind(terms.host_charge_target_id)
        .bind(terms.amount_cents)
        .bind(terms.currency.as_str())
        .bind(evidence.approval_evidence().as_str())
        .fetch_optional(&mut *transaction)
        .await?;
        if inserted.is_some() {
            transaction.commit().await?;
            return Ok(LockFreeApprovedEvidenceOutcome::Persisted);
        }
    }

    let evidence_matches = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT gateway_approval_evidence IS NOT DISTINCT FROM $3
            AND gateway_payment_method_reference IS NOT DISTINCT FROM $4
            AND gateway_response IS NOT DISTINCT FROM $5
            AND gateway_response_code IS NOT DISTINCT FROM $6
            AND gateway_response_text IS NOT DISTINCT FROM $7
            AND gateway_condition IS NOT DISTINCT FROM $8
            AND payment_type IS NOT DISTINCT FROM $9
            AND card_brand IS NOT DISTINCT FROM $10
            AND card_last4 IS NOT DISTINCT FROM $11
            AND card_exp_month IS NOT DISTINCT FROM $12
            AND card_exp_year IS NOT DISTINCT FROM $13
        FROM billing_processor_charges
        WHERE attempt_id = $1 AND gateway_transaction_id IS NOT DISTINCT FROM $2
        "#,
    )
    .bind(identity.attempt_id().as_uuid())
    .bind(transaction_id)
    .bind(evidence.approval_evidence().as_str())
    .bind(
        evidence
            .payment_method_reference()
            .map(|value| value.expose()),
    )
    .bind(evidence.response().map(GatewayDiagnostic::expose))
    .bind(evidence.response_code().map(GatewayDiagnostic::expose))
    .bind(evidence.response_text().map(GatewayDiagnostic::expose))
    .bind(evidence.condition().map(GatewayDiagnostic::expose))
    .bind(descriptor.payment_type().map(GatewayDiagnostic::expose))
    .bind(descriptor.card_brand().map(GatewayDiagnostic::expose))
    .bind(descriptor.card_last_four().map(|value| value.expose()))
    .bind(descriptor.card_exp_month())
    .bind(descriptor.card_exp_year())
    .fetch_optional(&mut *transaction)
    .await?;
    if evidence_matches == Some(true) {
        transaction.commit().await?;
        return Ok(LockFreeApprovedEvidenceOutcome::ExactReplay);
    }
    if let Some(transaction_id) = transaction_id {
        let owned_elsewhere: bool = sqlx::query_scalar(
            r#"
            SELECT EXISTS (
                SELECT 1 FROM billing_processor_charges
                WHERE gateway_account_id = $1 AND gateway_transaction_id = $2
                    AND attempt_id <> $3
            )
            "#,
        )
        .bind(identity.gateway_account_id().as_uuid())
        .bind(transaction_id)
        .bind(identity.attempt_id().as_uuid())
        .fetch_one(&mut *transaction)
        .await?;
        if owned_elsewhere {
            transaction.commit().await?;
            return Ok(LockFreeApprovedEvidenceOutcome::OwnedByOtherAttempt);
        }
    }
    Ok(LockFreeApprovedEvidenceOutcome::NotDurable)
}

pub async fn store_compensating_processor_charge(
    pool: &PgPool,
    attempt_id: PaymentAttemptId,
    gateway_order_id: &GatewayOrderId,
    evidence: &ProcessorEvidence,
) -> Result<CompensatingProcessorChargeOutcome, ProcessorChargeStoreError> {
    let mut last_transient_error = None;
    for store_attempt in 1..=STORE_MAX_ATTEMPTS {
        match store_once(pool, attempt_id, gateway_order_id, evidence).await {
            Ok(outcome) => return Ok(outcome),
            Err(error) if is_transient(&error) && store_attempt < STORE_MAX_ATTEMPTS => {
                last_transient_error = Some(error);
                tokio::time::sleep(STORE_RETRY_DELAY).await;
            }
            Err(error) => return Err(error),
        }
    }
    Err(
        last_transient_error.unwrap_or(ProcessorChargeStoreError::InvalidState(
            "compensating processor charge retry loop exhausted",
        )),
    )
}

async fn store_once(
    pool: &PgPool,
    attempt_id: PaymentAttemptId,
    gateway_order_id: &GatewayOrderId,
    evidence: &ProcessorEvidence,
) -> Result<CompensatingProcessorChargeOutcome, ProcessorChargeStoreError> {
    let mut transaction = pool.begin().await?;
    set_enrollment_timeouts(&mut transaction).await?;
    let query = format!("{PAYMENT_ATTEMPT_SELECT} WHERE id = $1");
    let row = sqlx::query(&query)
        .bind(attempt_id.as_uuid())
        .fetch_optional(&mut *transaction)
        .await?;
    let preloaded_attempt = row
        .as_ref()
        .map(payment_attempt_from_row)
        .transpose()?
        .ok_or(ProcessorChargeStoreError::InvalidState(
            "compensating processor charge attempt was not found",
        ))?;
    if preloaded_attempt.kind() == PaymentAttemptKind::SubscriptionInitial {
        let plan_key = preloaded_attempt.request().target().plan_key().ok_or(
            ProcessorChargeStoreError::InvalidState(INVALID_CHARGE_STATE),
        )?;
        lock_subscription_aggregate(
            &mut transaction,
            preloaded_attempt.identity().subscriber_id(),
            plan_key,
        )
        .await?;
    }
    let attempt = if preloaded_attempt.kind() == PaymentAttemptKind::SubscriptionPaymentMethodUpdate
    {
        lock_payment_attempt_by_id_on_connection(
            &mut transaction,
            preloaded_attempt.identity().billing_scope_id(),
            attempt_id,
        )
        .await?
        .ok_or(ProcessorChargeStoreError::InvalidState(
            "compensating processor charge attempt was not found",
        ))?
    } else {
        let reloaded = find_payment_attempt_by_id_on_connection(
            &mut transaction,
            preloaded_attempt.identity().billing_scope_id(),
            attempt_id,
        )
        .await?
        .ok_or(ProcessorChargeStoreError::InvalidState(
            "compensating processor charge attempt was not found",
        ))?;
        if reloaded != preloaded_attempt {
            return Err(ProcessorChargeStoreError::InvalidState(
                "compensating processor charge attempt changed while its aggregate was locked",
            ));
        }
        reloaded
    };
    if attempt.request().gateway_order_id() != gateway_order_id {
        return Err(ProcessorChargeStoreError::InvalidState(
            "compensating processor charge order does not match its attempt",
        ));
    }
    let initial_progression = compensating_progression(&attempt, evidence);
    let observation =
        observe_processor_charge(&mut transaction, &attempt, evidence, initial_progression).await?;
    let outcome = match observation {
        ObservedCharge::OwnedByOtherAttempt => {
            CompensatingProcessorChargeOutcome::OwnedByOtherAttempt
        }
        ObservedCharge::Owned(charge) => {
            let mut persisted = charge_by_id(&mut transaction, charge.id).await?;
            if !charge.exact_replay {
                let state_code = initial_charge_state_code(
                    persisted.role(),
                    persisted.progression(),
                    evidence.transaction_id().is_some(),
                );
                if persisted.state_code() != state_code {
                    persisted = transition_processor_charge(
                        &mut transaction,
                        persisted.id(),
                        &[persisted.progression()],
                        persisted.progression(),
                        state_code,
                        false,
                    )
                    .await?;
                }
            }
            if let Some(attestation) = attestation_by_charge(&mut transaction, charge.id)
                .await
                .map_err(ProcessorChargeStoreError::from)?
                && (!charge.exact_replay
                    || !attestation_matches_source(&attestation, &attempt, &persisted))
            {
                return Err(ProcessorChargeStoreError::InvalidState(
                    "attested processor charge was reobserved with different evidence",
                ));
            }
            if charge.exact_replay {
                CompensatingProcessorChargeOutcome::ExactReplay
            } else {
                CompensatingProcessorChargeOutcome::Observed
            }
        }
    };
    transaction.commit().await?;
    Ok(outcome)
}

pub async fn observe_processor_charge_in_transaction(
    connection: &mut PgConnection,
    attempt_id: PaymentAttemptId,
    gateway_order_id: &GatewayOrderId,
    evidence: &ProcessorEvidence,
    initial_progression: ProcessorChargeProgression,
) -> Result<ProcessorChargeObservationOutcome, ProcessorChargeStoreError> {
    let query = format!("{PAYMENT_ATTEMPT_SELECT} WHERE id = $1");
    let row = sqlx::query(&query)
        .bind(attempt_id.as_uuid())
        .fetch_optional(&mut *connection)
        .await?;
    let attempt = row
        .as_ref()
        .map(payment_attempt_from_row)
        .transpose()?
        .ok_or(ProcessorChargeStoreError::InvalidState(
            "processor charge attempt was not found",
        ))?;
    if attempt.request().gateway_order_id() != gateway_order_id {
        return Err(ProcessorChargeStoreError::InvalidState(
            "processor charge order does not match its attempt",
        ));
    }
    match observe_processor_charge(connection, &attempt, evidence, initial_progression).await? {
        ObservedCharge::OwnedByOtherAttempt => {
            Ok(ProcessorChargeObservationOutcome::OwnedByOtherAttempt)
        }
        ObservedCharge::Owned(charge) => {
            if charge.exact_replay {
                Ok(ProcessorChargeObservationOutcome::ExactReplay(
                    charge_by_id(connection, charge.id).await?,
                ))
            } else {
                let mut persisted = charge_by_id(connection, charge.id).await?;
                let state_code = initial_charge_state_code(
                    persisted.role(),
                    persisted.progression(),
                    evidence.transaction_id().is_some(),
                );
                if persisted.state_code() != state_code {
                    persisted = transition_processor_charge(
                        connection,
                        persisted.id(),
                        &[persisted.progression()],
                        persisted.progression(),
                        state_code,
                        false,
                    )
                    .await?;
                }
                Ok(ProcessorChargeObservationOutcome::Observed(persisted))
            }
        }
    }
}

pub async fn transition_processor_charge_in_transaction(
    connection: &mut PgConnection,
    charge_id: ProcessorChargeId,
    expected_progressions: &[ProcessorChargeProgression],
    progression: ProcessorChargeProgression,
    state_code: Option<ProcessorChargeStateCode>,
) -> Result<ProcessorCharge, ProcessorChargeStoreError> {
    transition_processor_charge(
        connection,
        charge_id,
        expected_progressions,
        progression,
        state_code,
        false,
    )
    .await
}

pub(crate) async fn observe_processor_charge(
    connection: &mut PgConnection,
    attempt: &PaymentAttempt,
    evidence: &ProcessorEvidence,
    initial_progression: ProcessorChargeProgression,
) -> Result<ObservedCharge, ProcessorChargeStoreError> {
    let identity = attempt.identity();
    let transaction_id = evidence.transaction_id().map(GatewayTransactionId::expose);
    let has_existing_charge: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM billing_processor_charges WHERE attempt_id = $1)",
    )
    .bind(identity.attempt_id().as_uuid())
    .fetch_one(&mut *connection)
    .await?;
    if let Some(transaction_id) = transaction_id {
        if owned_by_other_attempt(connection, attempt, transaction_id).await? {
            return Ok(ObservedCharge::OwnedByOtherAttempt);
        }
        if let Some(charge) = identify_transactionless(
            connection,
            attempt,
            evidence,
            transaction_id,
            initial_progression,
        )
        .await?
        {
            return Ok(ObservedCharge::Owned(charge));
        }
    }
    let role = if has_existing_charge {
        ProcessorChargeRole::Additional
    } else {
        ProcessorChargeRole::Primary
    };
    let identified = transaction_id.is_some();
    let progression = initial_charge_progression(
        role,
        attempt.request().amount().cents(),
        identified,
        initial_progression,
    );
    let descriptor = evidence.descriptor();
    let conflict_clause = if identified {
        "ON CONFLICT (gateway_account_id, gateway_transaction_id) \
         WHERE billing_canonical_gateway_transaction_id(gateway_transaction_id) IS NOT NULL \
         DO NOTHING RETURNING id"
    } else {
        "ON CONFLICT (attempt_id) \
         WHERE billing_canonical_gateway_transaction_id(gateway_transaction_id) IS NULL \
         DO NOTHING RETURNING id"
    };
    let insert = format!(
        r#"
        INSERT INTO billing_processor_charges (
            id, attempt_id, billing_scope_id, gateway_account_id, gateway_order_id,
            gateway_transaction_id, gateway_payment_method_reference,
            gateway_response, gateway_response_code, gateway_response_text,
            gateway_condition, payment_type, card_brand, card_last4,
            card_exp_month, card_exp_year, charge_role, progression_state, state_code,
            reconciliation_required_at, external_reversal_required_at, applied_at,
            attempt_kind, plan_key, host_charge_target_id, amount_cents, currency, gateway_approval_evidence
        ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
            $14, $15, $16, $17, $18, $19,
            CASE WHEN $18 = 'reconciliation_required' THEN clock_timestamp() END,
            CASE WHEN $18 = 'external_reversal_required' THEN clock_timestamp() END,
            CASE WHEN $18 = 'applied' THEN clock_timestamp() END,
            $20, $21, $22, $23, $24, $25
        )
        {conflict_clause}
        "#
    );
    let inserted = sqlx::query_scalar::<_, Uuid>(&insert)
        .bind(Uuid::now_v7())
        .bind(identity.attempt_id().as_uuid())
        .bind(identity.billing_scope_id().as_uuid())
        .bind(identity.gateway_account_id().as_uuid())
        .bind(attempt.request().gateway_order_id().expose())
        .bind(transaction_id)
        .bind(
            evidence
                .payment_method_reference()
                .map(|value| value.expose()),
        )
        .bind(evidence.response().map(GatewayDiagnostic::expose))
        .bind(evidence.response_code().map(GatewayDiagnostic::expose))
        .bind(evidence.response_text().map(GatewayDiagnostic::expose))
        .bind(evidence.condition().map(GatewayDiagnostic::expose))
        .bind(descriptor.payment_type().map(GatewayDiagnostic::expose))
        .bind(descriptor.card_brand().map(GatewayDiagnostic::expose))
        .bind(descriptor.card_last_four().map(|value| value.expose()))
        .bind(descriptor.card_exp_month())
        .bind(descriptor.card_exp_year())
        .bind(role.as_str())
        .bind(progression.as_str())
        .bind(Option::<&str>::None)
        .bind(attempt.kind().as_str())
        .bind(attempt.request().target().plan_key().map(PlanKey::as_str))
        .bind(
            attempt
                .request()
                .target()
                .host_charge_target_id()
                .map(|value| *value.as_uuid()),
        )
        .bind(attempt.request().amount().cents())
        .bind(attempt.request().amount().currency().as_str())
        .bind(evidence.approval_evidence().as_str())
        .fetch_optional(&mut *connection)
        .await?;
    if let Some(id) = inserted {
        return Ok(ObservedCharge::Owned(ChargeRecord {
            id,
            role,
            exact_replay: false,
        }));
    }
    if let Some(row) = matching_charge(connection, attempt, evidence, transaction_id).await? {
        if !row.try_get::<bool, _>("evidence_matches")? {
            return Err(ProcessorChargeStoreError::InvalidState(
                "processor charge replay evidence changed",
            ));
        }
        return Ok(ObservedCharge::Owned(ChargeRecord {
            id: row.try_get("id")?,
            role: parse_role(&row.try_get::<String, _>("charge_role")?)?,
            exact_replay: true,
        }));
    }
    if let Some(transaction_id) = transaction_id
        && owned_by_other_attempt(connection, attempt, transaction_id).await?
    {
        return Ok(ObservedCharge::OwnedByOtherAttempt);
    }
    Err(ProcessorChargeStoreError::InvalidState(
        INVALID_CHARGE_STATE,
    ))
}

pub(crate) async fn transition_charge(
    connection: &mut PgConnection,
    charge_id: Uuid,
    progression: ProcessorChargeProgression,
    resolution_code: Option<PaymentResolutionCode>,
) -> Result<(), ProcessorChargeStoreError> {
    const EXPECTED: &[ProcessorChargeProgression] = &[
        ProcessorChargeProgression::Pending,
        ProcessorChargeProgression::ReconciliationRequired,
        ProcessorChargeProgression::ExternalReversalRequired,
        ProcessorChargeProgression::Applied,
    ];
    transition_processor_charge(
        connection,
        ProcessorChargeId::new(charge_id),
        EXPECTED,
        progression,
        resolution_code.map(ProcessorChargeStateCode::PaymentResolution),
        true,
    )
    .await?;
    Ok(())
}

async fn transition_processor_charge(
    connection: &mut PgConnection,
    charge_id: ProcessorChargeId,
    expected_progressions: &[ProcessorChargeProgression],
    progression: ProcessorChargeProgression,
    state_code: Option<ProcessorChargeStateCode>,
    preserve_existing_state_code: bool,
) -> Result<ProcessorCharge, ProcessorChargeStoreError> {
    if expected_progressions.is_empty() {
        return Err(ProcessorChargeStoreError::InvalidState(
            "processor charge transition requires an expected state",
        ));
    }
    let expected_progressions = expected_progressions
        .iter()
        .map(|progression| progression.as_str())
        .collect::<Vec<_>>();
    let row = sqlx::query(
        r#"
        UPDATE billing_processor_charges
        SET progression_state = $3,
            state_code = CASE WHEN $5 THEN COALESCE(state_code, $4) ELSE $4 END,
            reconciliation_required_at = CASE WHEN $3 = 'reconciliation_required'
                THEN COALESCE(reconciliation_required_at, clock_timestamp())
                ELSE NULL END,
            external_reversal_required_at = CASE WHEN $3 = 'external_reversal_required'
                THEN COALESCE(external_reversal_required_at, clock_timestamp())
                ELSE NULL END,
            applied_at = CASE WHEN $3 = 'applied'
                THEN COALESCE(applied_at, clock_timestamp()) ELSE NULL END,
            externally_reversed_at = CASE WHEN $3 = 'externally_reversed'
                THEN COALESCE(externally_reversed_at, clock_timestamp()) ELSE NULL END,
            updated_at = clock_timestamp()
        WHERE id = $1 AND progression_state = ANY($2::text[])
            AND (
                $3 <> ALL(ARRAY['external_reversal_required'::text, 'externally_reversed'::text])
                OR (
                    billing_canonical_gateway_transaction_id(gateway_transaction_id) IS NOT NULL
                    AND amount_cents > 0
                    AND attempt_kind <> 'subscription_payment_method_update'
                )
            )
            AND (
                $3 <> 'applied'
                OR (
                    charge_role = 'primary'
                    AND billing_canonical_gateway_transaction_id(gateway_transaction_id) IS NOT NULL
                )
            )
        RETURNING id, attempt_id, billing_scope_id, gateway_account_id,
            gateway_order_id, attempt_kind, amount_cents, currency,
            charge_role, progression_state, state_code,
            gateway_transaction_id, gateway_payment_method_reference,
            gateway_approval_evidence, gateway_response, gateway_response_code, gateway_response_text,
            gateway_condition, payment_type, card_brand, card_last4,
            card_exp_month, card_exp_year, observed_at
        "#,
    )
    .bind(charge_id.as_uuid())
    .bind(expected_progressions)
    .bind(progression.as_str())
    .bind(state_code.map(ProcessorChargeStateCode::as_str))
    .bind(preserve_existing_state_code)
    .fetch_optional(&mut *connection)
    .await?
    .ok_or(ProcessorChargeStoreError::InvalidState(
        "processor charge transition did not match its expected state or eligibility",
    ))?;
    processor_charge_from_row(&row).map_err(ProcessorChargeStoreError::from)
}

#[cfg(test)]
mod tests;
