//! Named charge application transitions. SQL compare-and-set predicates remain
//! authoritative for concurrent writers; callers select an operation, not a
//! source/destination pair.
use super::*;

pub(super) enum ChargeStateCodeUpdate {
    Replace(Option<ProcessorChargeStateCode>),
    PreserveExisting(Option<ProcessorChargeStateCode>),
}

/// A named application decision for an already observed processor charge.
/// Persistence revalidates source state, ownership, and economic eligibility.
#[derive(Clone, Copy, Debug)]
pub enum ChargeTransition {
    /// Link an identified primary charge to its already-approved owning attempt.
    MarkApplied,
    /// Retain unresolved evidence for later exact reconciliation.
    RequireReconciliation(Option<PaymentResolutionCode>),
    /// Send an identified positive charge to external-reversal review.
    RequireExternalReversal(Option<PaymentResolutionCode>),
    /// Retain conflicting approval evidence while its attempt remains unresolved.
    /// This may reclassify reversal review, but never an expired-period approval.
    ReconcileConflictingApproval,
    /// Permanently park an expired-period approval with its canonical reason.
    ParkExpiredPeriod,
}

impl ChargeTransition {
    fn parts(self) -> (ProcessorChargeProgression, ChargeStateCodeUpdate) {
        use ProcessorChargeProgression::*;
        match self {
            Self::ReconcileConflictingApproval => (
                ReconciliationRequired,
                ChargeStateCodeUpdate::PreserveExisting(None),
            ),
            Self::MarkApplied => (Applied, ChargeStateCodeUpdate::PreserveExisting(None)),
            Self::RequireReconciliation(code) => (
                ReconciliationRequired,
                ChargeStateCodeUpdate::PreserveExisting(
                    code.map(ProcessorChargeStateCode::PaymentResolution),
                ),
            ),
            Self::RequireExternalReversal(code) => (
                ExternalReversalRequired,
                ChargeStateCodeUpdate::PreserveExisting(
                    code.map(ProcessorChargeStateCode::PaymentResolution),
                ),
            ),
            Self::ParkExpiredPeriod => (
                ExternalReversalRequired,
                ChargeStateCodeUpdate::Replace(Some(ProcessorChargeStateCode::PaymentResolution(
                    PaymentResolutionCode::SubscriptionApprovedPeriodExpired,
                ))),
            ),
        }
    }

    fn sources(self) -> &'static [ProcessorChargeProgression] {
        use ProcessorChargeProgression::*;
        match self {
            Self::ReconcileConflictingApproval => {
                &[Pending, ReconciliationRequired, ExternalReversalRequired]
            }
            Self::MarkApplied => &[Pending, ReconciliationRequired, Applied],
            Self::RequireReconciliation(_) => &[Pending, ReconciliationRequired],
            Self::RequireExternalReversal(_) | Self::ParkExpiredPeriod => {
                &[Pending, ReconciliationRequired, ExternalReversalRequired]
            }
        }
    }
}

pub(crate) async fn transition_charge(
    connection: &mut PgConnection,
    charge_id: Uuid,
    transition: ChargeTransition,
) -> Result<(), ProcessorChargeStoreError> {
    apply_processor_charge_transition_in_transaction(
        connection,
        ProcessorChargeId::new(charge_id),
        transition,
    )
    .await?;
    Ok(())
}

/// Applies a named charge decision on the caller's transaction connection.
/// The caller must retain the owning aggregate/attempt locks and commit any
/// application state and host events atomically. This does not perform provider I/O.
pub async fn apply_processor_charge_transition_in_transaction(
    connection: &mut PgConnection,
    charge_id: ProcessorChargeId,
    transition: ChargeTransition,
) -> Result<ProcessorCharge, ProcessorChargeStoreError> {
    let (progression, code) = transition.parts();
    transition_processor_charge(
        connection,
        charge_id,
        transition.sources(),
        progression,
        code,
    )
    .await
}

pub(crate) async fn apply_pending_charge_decision(
    connection: &mut PgConnection,
    charge_id: Uuid,
    decision: super::policy::PendingChargeDecision,
) -> Result<(), ProcessorChargeStoreError> {
    let (progression, code) = decision.parts();
    update_processor_charge(
        connection,
        ProcessorChargeId::new(charge_id),
        &[ProcessorChargeProgression::Pending],
        progression,
        ChargeStateCodeUpdate::Replace(code),
    )
    .await?;
    Ok(())
}

pub(super) async fn transition_processor_charge(
    connection: &mut PgConnection,
    charge_id: ProcessorChargeId,
    expected_progressions: &[ProcessorChargeProgression],
    progression: ProcessorChargeProgression,
    state_code_update: ChargeStateCodeUpdate,
) -> Result<ProcessorCharge, ProcessorChargeStoreError> {
    let row = update_processor_charge(
        connection,
        charge_id,
        expected_progressions,
        progression,
        state_code_update,
    )
    .await?;
    processor_charge_from_row(&row).map_err(map_operator_error)
}

async fn update_processor_charge(
    connection: &mut PgConnection,
    charge_id: ProcessorChargeId,
    expected_progressions: &[ProcessorChargeProgression],
    progression: ProcessorChargeProgression,
    state_code_update: ChargeStateCodeUpdate,
) -> Result<sqlx::postgres::PgRow, ProcessorChargeStoreError> {
    let (state_code, preserve_existing_state_code) = match state_code_update {
        ChargeStateCodeUpdate::Replace(code) => (code, false),
        ChargeStateCodeUpdate::PreserveExisting(code) => (code, true),
    };
    if expected_progressions.is_empty() {
        return Err(ProcessorChargeStoreError::InvalidState(
            "processor charge transition requires an expected state",
        ));
    }
    let expected_progressions = expected_progressions
        .iter()
        .filter(|source| transition_allowed(**source, progression))
        .map(|progression| progression.as_str())
        .collect::<Vec<_>>();
    let row = sqlx::query(
        r#"
        UPDATE billing_processor_charges AS charges
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
            AND (charges.state_code IS DISTINCT FROM 'subscription_approved_period_expired'
                OR $3 = 'externally_reversed'
                OR ($3 = 'external_reversal_required'
                    AND (CASE WHEN $5 THEN COALESCE(charges.state_code, $4) ELSE $4 END)
                        = 'subscription_approved_period_expired'))
            AND (charges.progression_state <> 'external_reversal_required'
                OR $3 <> 'reconciliation_required' OR (
                    charges.state_code IS DISTINCT FROM 'subscription_approved_period_expired'
                    AND EXISTS (SELECT 1 FROM billing_payment_attempts attempts
                        WHERE attempts.id = charges.attempt_id
                            AND attempts.status IN ('pending', 'unknown', 'review_required'))
                ))
            AND ($3 <> 'applied' OR EXISTS (
                SELECT 1 FROM billing_payment_attempts attempts
                WHERE attempts.id = charges.attempt_id AND attempts.status = 'approved'
                    AND billing_canonical_gateway_transaction_id(attempts.gateway_transaction_id)
                        = billing_canonical_gateway_transaction_id(charges.gateway_transaction_id)
            ))
            AND ($3 <> 'externally_reversed' OR EXISTS (
                SELECT 1 FROM billing_external_reversal_attestations attestations
                WHERE attestations.processor_charge_id = charges.id
                    AND attestations.attempt_id = charges.attempt_id
                    AND attestations.gateway_transaction_id = charges.gateway_transaction_id
                    AND (CASE WHEN $5 THEN COALESCE(charges.state_code, $4) ELSE $4 END)
                        IN (attestations.prior_resolution_code, attestations.final_resolution_code)
            ))
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
            gateway_response, gateway_response_code, gateway_response_text,
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
    Ok(row)
}

/// Compatibility callers may narrow source states, but cannot authorize an
/// edge absent from the charge machine. Reversal review can return to
/// reconciliation only while its owning attempt is unresolved (enforced in SQL).
fn transition_allowed(from: ProcessorChargeProgression, to: ProcessorChargeProgression) -> bool {
    use ProcessorChargeProgression::*;
    match from {
        Pending => matches!(
            to,
            Pending
                | ReconciliationRequired
                | ExternalReversalRequired
                | Applied
                | ExternallyReversed
        ),
        ReconciliationRequired => matches!(
            to,
            ReconciliationRequired | ExternalReversalRequired | Applied
        ),
        ExternalReversalRequired => matches!(
            to,
            ReconciliationRequired | ExternalReversalRequired | ExternallyReversed
        ),
        Applied => matches!(to, Applied),
        ExternallyReversed => matches!(to, ExternallyReversed),
    }
}

/// The operator path retains the original review timestamp as audit history.
/// Its reason comes from the immutable attestation, never from a caller's label.
pub(crate) async fn complete_attested_reversal(
    connection: &mut PgConnection,
    charge_id: ProcessorChargeId,
) -> Result<(), crate::OperatorReviewError> {
    let updated = sqlx::query(
        r#"
        UPDATE billing_processor_charges AS charges
        SET progression_state = 'externally_reversed',
            state_code = attestations.prior_resolution_code,
            externally_reversed_at = COALESCE(charges.externally_reversed_at, clock_timestamp()),
            updated_at = clock_timestamp()
        FROM billing_external_reversal_attestations attestations
        WHERE charges.id = $1 AND charges.progression_state = 'external_reversal_required'
            AND attestations.processor_charge_id = charges.id
            AND attestations.attempt_id = charges.attempt_id
            AND attestations.gateway_transaction_id = charges.gateway_transaction_id
        "#,
    )
    .bind(charge_id.as_uuid())
    .execute(connection)
    .await?;
    if updated.rows_affected() != 1 {
        return Err(crate::OperatorReviewError::InvalidState(
            "eligible processor charge did not accept external reversal",
        ));
    }
    Ok(())
}
