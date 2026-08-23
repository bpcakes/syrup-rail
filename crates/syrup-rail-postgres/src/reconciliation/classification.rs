use super::*;

pub(super) async fn count_pending_processor_charges(
    pool: &PgPool,
    gateway_account_id: GatewayAccountId,
) -> Result<u64, sqlx::Error> {
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM billing_processor_charges WHERE gateway_account_id = $1 AND progression_state = 'pending'",
    )
    .bind(gateway_account_id.as_uuid())
    .fetch_one(pool)
    .await?;
    u64::try_from(count).map_err(|_| invalid_reconciliation_state())
}

pub(super) async fn attempt_locator(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    attempt_id: Uuid,
) -> Result<Option<AttemptLocator>, sqlx::Error> {
    let row = sqlx::query(
        r#"
        SELECT id, billing_scope_id, subscriber_id, plan_key,
            gateway_account_id, attempt_kind
        FROM billing_payment_attempts
        WHERE id = $1
        "#,
    )
    .bind(attempt_id)
    .fetch_optional(&mut **transaction)
    .await?;
    row.as_ref().map(attempt_locator_from_row).transpose()
}

pub(super) fn attempt_locator_from_row(
    row: &sqlx::postgres::PgRow,
) -> Result<AttemptLocator, sqlx::Error> {
    let kind = row
        .try_get::<String, _>("attempt_kind")?
        .parse::<PaymentAttemptKind>()
        .map_err(|_| invalid_reconciliation_state())?;
    let plan_key = row
        .try_get::<Option<String>, _>("plan_key")?
        .map(PlanKey::new)
        .transpose()
        .map_err(|_| invalid_reconciliation_state())?;
    if (kind == PaymentAttemptKind::HostCharge) != plan_key.is_none() {
        return Err(invalid_reconciliation_state());
    }
    Ok(AttemptLocator {
        id: row.try_get("id")?,
        billing_scope_id: BillingScopeId::new(row.try_get("billing_scope_id")?),
        subscriber_id: SubscriberId::new(row.try_get("subscriber_id")?),
        plan_key,
        gateway_account_id: GatewayAccountId::new(row.try_get("gateway_account_id")?),
        kind,
    })
}

pub(super) async fn lock_attempt_for_classification(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    locator: AttemptLocator,
) -> Result<Option<LockedAttempt>, sqlx::Error> {
    let row = sqlx::query(
        r#"
        SELECT id, billing_scope_id, subscriber_id, plan_key,
            gateway_account_id, attempt_kind, status, resolution_code,
            amount_cents,
            billing_canonical_gateway_transaction_id(
                gateway_transaction_id
            ) AS transaction_id
        FROM billing_payment_attempts
        WHERE id = $1
        FOR UPDATE SKIP LOCKED
        "#,
    )
    .bind(locator.id)
    .fetch_optional(&mut **transaction)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let locked_locator = attempt_locator_from_row(&row)?;
    if locked_locator != locator {
        return Err(invalid_reconciliation_state());
    }
    let status = row
        .try_get::<String, _>("status")?
        .parse::<PaymentAttemptStatus>()
        .map_err(|_| invalid_reconciliation_state())?;
    let resolution_code = row
        .try_get::<Option<String>, _>("resolution_code")?
        .as_deref()
        .map(PaymentResolutionCode::try_from)
        .transpose()
        .map_err(|_| invalid_reconciliation_state())?;
    Ok(Some(LockedAttempt {
        locator,
        status,
        resolution_code,
        amount_cents: row.try_get("amount_cents")?,
        transaction_id: row.try_get("transaction_id")?,
    }))
}

pub(super) async fn lock_pending_charge_for_classification(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    charge_id: Uuid,
    attempt_id: Uuid,
) -> Result<Option<LockedPendingCharge>, sqlx::Error> {
    let row = sqlx::query(
        r#"
        SELECT charges.charge_role,
            billing_canonical_gateway_transaction_id(
                charges.gateway_transaction_id
            ) AS transaction_id,
            CASE
                WHEN billing_canonical_gateway_transaction_id(
                    attempts.gateway_transaction_id
                ) IS NOT NULL
                    AND billing_canonical_gateway_transaction_id(
                        charges.gateway_transaction_id
                    ) IS NOT NULL
                THEN billing_canonical_gateway_transaction_id(
                    attempts.gateway_transaction_id
                ) = billing_canonical_gateway_transaction_id(
                    charges.gateway_transaction_id
                )
                WHEN billing_canonical_gateway_transaction_id(
                    attempts.gateway_transaction_id
                ) IS NULL
                    AND billing_canonical_gateway_transaction_id(
                        charges.gateway_transaction_id
                    ) IS NULL
                THEN attempts.gateway_order_id = charges.gateway_order_id
                    AND attempts.gateway_payment_method_reference
                        IS NOT DISTINCT FROM charges.gateway_payment_method_reference
                    AND attempts.gateway_response
                        IS NOT DISTINCT FROM charges.gateway_response
                    AND attempts.gateway_response_code
                        IS NOT DISTINCT FROM charges.gateway_response_code
                    AND attempts.gateway_response_text
                        IS NOT DISTINCT FROM charges.gateway_response_text
                    AND attempts.gateway_condition
                        IS NOT DISTINCT FROM charges.gateway_condition
                    AND attempts.payment_type IS NOT DISTINCT FROM charges.payment_type
                    AND attempts.card_brand IS NOT DISTINCT FROM charges.card_brand
                    AND attempts.card_last4 IS NOT DISTINCT FROM charges.card_last4
                    AND attempts.card_exp_month
                        IS NOT DISTINCT FROM charges.card_exp_month
                    AND attempts.card_exp_year
                        IS NOT DISTINCT FROM charges.card_exp_year
                ELSE false
            END AS same_charge,
            charges.attempt_id = attempts.id
                AND charges.billing_scope_id = attempts.billing_scope_id
                AND charges.gateway_account_id = attempts.gateway_account_id
                AND charges.gateway_order_id = attempts.gateway_order_id
                AND charges.attempt_kind = attempts.attempt_kind
                AND charges.plan_key IS NOT DISTINCT FROM attempts.plan_key
                AND charges.host_charge_target_id
                    IS NOT DISTINCT FROM attempts.host_charge_target_id
                AND charges.amount_cents = attempts.amount_cents
                AND charges.currency = attempts.currency AS dimensions_match
        FROM billing_processor_charges charges
        INNER JOIN billing_payment_attempts attempts
            ON attempts.id = charges.attempt_id
        WHERE charges.id = $1 AND attempts.id = $2
            AND charges.progression_state = 'pending'
        FOR UPDATE OF charges SKIP LOCKED
        "#,
    )
    .bind(charge_id)
    .bind(attempt_id)
    .fetch_optional(&mut **transaction)
    .await?;
    row.map(|row| {
        let role = match row.try_get::<String, _>("charge_role")?.as_str() {
            "primary" => ProcessorChargeRole::Primary,
            "additional" => ProcessorChargeRole::Additional,
            _ => return Err(invalid_reconciliation_state()),
        };
        Ok(LockedPendingCharge {
            id: charge_id,
            role,
            transaction_id: row.try_get("transaction_id")?,
            matches_attempt_evidence: row.try_get("same_charge")?,
            dimensions_match: row.try_get("dimensions_match")?,
        })
    })
    .transpose()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct LockedExternalReversalAttestation {
    processor_charge_id: Uuid,
    final_resolution_code: PaymentResolutionCode,
}

pub(super) async fn lock_external_reversal_attestation(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    attempt: &LockedAttempt,
    charge: &LockedPendingCharge,
) -> Result<Option<LockedExternalReversalAttestation>, sqlx::Error> {
    let Some(transaction_id) = charge.transaction_id.as_deref() else {
        return Ok(None);
    };
    let attestation = sqlx::query_as::<_, (Uuid, String)>(
        r#"
        SELECT processor_charge_id, final_resolution_code
        FROM billing_external_reversal_attestations
        WHERE attempt_id = $1 AND gateway_transaction_id = $2
        FOR UPDATE
        "#,
    )
    .bind(attempt.locator.id)
    .bind(transaction_id)
    .fetch_optional(&mut **transaction)
    .await?;
    attestation
        .map(|(processor_charge_id, final_resolution_code)| {
            Ok(LockedExternalReversalAttestation {
                processor_charge_id,
                final_resolution_code: PaymentResolutionCode::try_from(
                    final_resolution_code.as_str(),
                )
                .map_err(|_| invalid_reconciliation_state())?,
            })
        })
        .transpose()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PendingChargeTransition {
    ReconciliationRequired(ProcessorChargeStateCode),
    ExternalReversalRequired(ProcessorChargeStateCode),
    Applied,
    ExternallyReversed(PaymentResolutionCode),
}

impl PendingChargeTransition {
    const fn progression(self) -> ProcessorChargeProgression {
        match self {
            Self::ReconciliationRequired(_) => ProcessorChargeProgression::ReconciliationRequired,
            Self::ExternalReversalRequired(_) => {
                ProcessorChargeProgression::ExternalReversalRequired
            }
            Self::Applied => ProcessorChargeProgression::Applied,
            Self::ExternallyReversed(_) => ProcessorChargeProgression::ExternallyReversed,
        }
    }

    const fn state_code(self) -> Option<ProcessorChargeStateCode> {
        match self {
            Self::ReconciliationRequired(code) | Self::ExternalReversalRequired(code) => Some(code),
            Self::Applied => None,
            Self::ExternallyReversed(code) => {
                Some(ProcessorChargeStateCode::PaymentResolution(code))
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct PendingChargeClassificationError;

pub(super) fn classify_pending_charge(
    attempt: &LockedAttempt,
    charge: &LockedPendingCharge,
    attestation: Option<&LockedExternalReversalAttestation>,
) -> Result<PendingChargeTransition, PendingChargeClassificationError> {
    if charge.transaction_id.is_none() {
        return Ok(PendingChargeTransition::ReconciliationRequired(
            ProcessorChargeStateCode::TransactionIdentityRequired,
        ));
    }
    if let Some(attestation) = attestation {
        if attestation.processor_charge_id != charge.id {
            return Err(PendingChargeClassificationError);
        }
        return Ok(PendingChargeTransition::ExternallyReversed(
            attestation.final_resolution_code,
        ));
    }

    let terminal_external_reversal = attempt.status == PaymentAttemptStatus::Failed
        && matches!(
            attempt.resolution_code,
            Some(
                PaymentResolutionCode::SubscriptionInitialExternallyRefunded
                    | PaymentResolutionCode::SubscriptionInitialExternallyVoided
                    | PaymentResolutionCode::ProcessorChargeExternallyRefunded
                    | PaymentResolutionCode::ProcessorChargeExternallyVoided
            )
        );
    let initial_grant_conflict = attempt.locator.kind == PaymentAttemptKind::SubscriptionInitial
        && attempt.resolution_code
            == Some(PaymentResolutionCode::SubscriptionInitialCurrentGrantConflict);
    if attempt.amount_cents > 0
        && (charge.role == ProcessorChargeRole::Additional
            || terminal_external_reversal
            || initial_grant_conflict
            || (attempt.transaction_id.is_some() && !charge.matches_attempt_evidence))
    {
        let code = if charge.role == ProcessorChargeRole::Additional {
            ProcessorChargeStateCode::AdditionalApprovedChargeIdentified
        } else {
            ProcessorChargeStateCode::ExternalReversalRequired
        };
        return Ok(PendingChargeTransition::ExternalReversalRequired(code));
    }
    if charge.matches_attempt_evidence && attempt.status == PaymentAttemptStatus::Approved {
        return Ok(PendingChargeTransition::Applied);
    }
    let code = if charge.role == ProcessorChargeRole::Additional {
        ProcessorChargeStateCode::ZeroAmountAdditionalApprovedCharge
    } else {
        ProcessorChargeStateCode::ApprovedChargeWaitingForApplication
    };
    Ok(PendingChargeTransition::ReconciliationRequired(code))
}

pub(super) async fn transition_pending_charge(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    charge_id: Uuid,
    transition: PendingChargeTransition,
) -> Result<(), sqlx::Error> {
    let progression = transition.progression();
    let state_code = transition.state_code();
    let result = sqlx::query(
        r#"
        UPDATE billing_processor_charges
        SET progression_state = $2,
            state_code = $3,
            reconciliation_required_at = CASE
                WHEN $2 = 'reconciliation_required'
                THEN COALESCE(reconciliation_required_at, clock_timestamp())
            END,
            external_reversal_required_at = CASE
                WHEN $2 = 'external_reversal_required'
                THEN COALESCE(external_reversal_required_at, clock_timestamp())
            END,
            applied_at = CASE WHEN $2 = 'applied'
                THEN COALESCE(applied_at, clock_timestamp()) END,
            externally_reversed_at = CASE WHEN $2 = 'externally_reversed'
                THEN COALESCE(externally_reversed_at, clock_timestamp()) END,
            updated_at = clock_timestamp()
        WHERE id = $1 AND progression_state = 'pending'
            AND (
                $2 NOT IN ('external_reversal_required', 'externally_reversed')
                OR (
                    billing_canonical_gateway_transaction_id(
                        gateway_transaction_id
                    ) IS NOT NULL
                    AND amount_cents > 0
                    AND attempt_kind <> 'subscription_payment_method_update'
                )
            )
            AND (
                $2 <> 'applied'
                OR (
                    charge_role = 'primary'
                    AND billing_canonical_gateway_transaction_id(
                        gateway_transaction_id
                    ) IS NOT NULL
                )
            )
        "#,
    )
    .bind(charge_id)
    .bind(progression.as_str())
    .bind(state_code.map(ProcessorChargeStateCode::as_str))
    .execute(&mut **transaction)
    .await?;
    if result.rows_affected() != 1 {
        return Err(invalid_reconciliation_state());
    }
    Ok(())
}

pub(super) fn invalid_reconciliation_state() -> sqlx::Error {
    sqlx::Error::Protocol("canonical reconciliation state is invalid".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn locked_attempt(
        kind: PaymentAttemptKind,
        status: PaymentAttemptStatus,
        resolution_code: Option<PaymentResolutionCode>,
        amount_cents: i32,
        transaction_id: Option<&str>,
    ) -> LockedAttempt {
        LockedAttempt {
            locator: AttemptLocator {
                id: Uuid::from_u128(1),
                billing_scope_id: BillingScopeId::new(Uuid::from_u128(2)),
                subscriber_id: SubscriberId::new(Uuid::from_u128(3)),
                plan_key: Some(PlanKey::new("plan").expect("valid plan key")),
                gateway_account_id: GatewayAccountId::new(Uuid::from_u128(4)),
                kind,
            },
            status,
            resolution_code,
            amount_cents,
            transaction_id: transaction_id.map(str::to_owned),
        }
    }

    fn locked_charge(
        role: ProcessorChargeRole,
        transaction_id: Option<&str>,
        matches_attempt_evidence: bool,
    ) -> LockedPendingCharge {
        LockedPendingCharge {
            id: Uuid::from_u128(5),
            role,
            transaction_id: transaction_id.map(str::to_owned),
            matches_attempt_evidence,
            dimensions_match: true,
        }
    }

    #[test]
    fn pending_charge_classification_covers_the_full_decision_table() {
        let ordinary_pending = locked_attempt(
            PaymentAttemptKind::SubscriptionRenewal,
            PaymentAttemptStatus::Pending,
            None,
            100,
            None,
        );
        let cases = [
            (
                "missing transaction identity",
                ordinary_pending.clone(),
                locked_charge(ProcessorChargeRole::Primary, None, false),
                None,
                Ok(PendingChargeTransition::ReconciliationRequired(
                    ProcessorChargeStateCode::TransactionIdentityRequired,
                )),
            ),
            (
                "matching attestation",
                ordinary_pending.clone(),
                locked_charge(ProcessorChargeRole::Primary, Some("transaction"), false),
                Some(LockedExternalReversalAttestation {
                    processor_charge_id: Uuid::from_u128(5),
                    final_resolution_code: PaymentResolutionCode::ProcessorChargeExternallyRefunded,
                }),
                Ok(PendingChargeTransition::ExternallyReversed(
                    PaymentResolutionCode::ProcessorChargeExternallyRefunded,
                )),
            ),
            (
                "positive additional charge",
                ordinary_pending.clone(),
                locked_charge(ProcessorChargeRole::Additional, Some("transaction"), false),
                None,
                Ok(PendingChargeTransition::ExternalReversalRequired(
                    ProcessorChargeStateCode::AdditionalApprovedChargeIdentified,
                )),
            ),
            (
                "zero amount additional charge",
                locked_attempt(
                    PaymentAttemptKind::SubscriptionPaymentMethodUpdate,
                    PaymentAttemptStatus::Pending,
                    None,
                    0,
                    None,
                ),
                locked_charge(ProcessorChargeRole::Additional, Some("transaction"), false),
                None,
                Ok(PendingChargeTransition::ReconciliationRequired(
                    ProcessorChargeStateCode::ZeroAmountAdditionalApprovedCharge,
                )),
            ),
            (
                "terminal external reversal",
                locked_attempt(
                    PaymentAttemptKind::SubscriptionRenewal,
                    PaymentAttemptStatus::Failed,
                    Some(PaymentResolutionCode::ProcessorChargeExternallyVoided),
                    100,
                    None,
                ),
                locked_charge(ProcessorChargeRole::Primary, Some("transaction"), false),
                None,
                Ok(PendingChargeTransition::ExternalReversalRequired(
                    ProcessorChargeStateCode::ExternalReversalRequired,
                )),
            ),
            (
                "initial grant conflict",
                locked_attempt(
                    PaymentAttemptKind::SubscriptionInitial,
                    PaymentAttemptStatus::ReviewRequired,
                    Some(PaymentResolutionCode::SubscriptionInitialCurrentGrantConflict),
                    100,
                    None,
                ),
                locked_charge(ProcessorChargeRole::Primary, Some("transaction"), false),
                None,
                Ok(PendingChargeTransition::ExternalReversalRequired(
                    ProcessorChargeStateCode::ExternalReversalRequired,
                )),
            ),
            (
                "different attempt transaction",
                locked_attempt(
                    PaymentAttemptKind::SubscriptionRenewal,
                    PaymentAttemptStatus::Approved,
                    None,
                    100,
                    Some("other-transaction"),
                ),
                locked_charge(ProcessorChargeRole::Primary, Some("transaction"), false),
                None,
                Ok(PendingChargeTransition::ExternalReversalRequired(
                    ProcessorChargeStateCode::ExternalReversalRequired,
                )),
            ),
            (
                "approved matching charge",
                locked_attempt(
                    PaymentAttemptKind::SubscriptionRenewal,
                    PaymentAttemptStatus::Approved,
                    None,
                    100,
                    Some("transaction"),
                ),
                locked_charge(ProcessorChargeRole::Primary, Some("transaction"), true),
                None,
                Ok(PendingChargeTransition::Applied),
            ),
            (
                "primary charge waiting for application",
                ordinary_pending,
                locked_charge(ProcessorChargeRole::Primary, Some("transaction"), false),
                None,
                Ok(PendingChargeTransition::ReconciliationRequired(
                    ProcessorChargeStateCode::ApprovedChargeWaitingForApplication,
                )),
            ),
            (
                "attestation for another charge",
                locked_attempt(
                    PaymentAttemptKind::SubscriptionRenewal,
                    PaymentAttemptStatus::Pending,
                    None,
                    100,
                    None,
                ),
                locked_charge(ProcessorChargeRole::Primary, Some("transaction"), false),
                Some(LockedExternalReversalAttestation {
                    processor_charge_id: Uuid::from_u128(6),
                    final_resolution_code: PaymentResolutionCode::ProcessorChargeExternallyRefunded,
                }),
                Err(PendingChargeClassificationError),
            ),
        ];

        for (name, attempt, charge, attestation, expected) in cases {
            assert_eq!(
                classify_pending_charge(&attempt, &charge, attestation.as_ref()),
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn pending_charge_transitions_project_only_valid_progression_and_code_pairs() {
        let cases = [
            (
                PendingChargeTransition::ReconciliationRequired(
                    ProcessorChargeStateCode::ApprovedChargeWaitingForApplication,
                ),
                ProcessorChargeProgression::ReconciliationRequired,
                Some(ProcessorChargeStateCode::ApprovedChargeWaitingForApplication),
            ),
            (
                PendingChargeTransition::ExternalReversalRequired(
                    ProcessorChargeStateCode::AdditionalApprovedChargeIdentified,
                ),
                ProcessorChargeProgression::ExternalReversalRequired,
                Some(ProcessorChargeStateCode::AdditionalApprovedChargeIdentified),
            ),
            (
                PendingChargeTransition::Applied,
                ProcessorChargeProgression::Applied,
                None,
            ),
            (
                PendingChargeTransition::ExternallyReversed(
                    PaymentResolutionCode::ProcessorChargeExternallyVoided,
                ),
                ProcessorChargeProgression::ExternallyReversed,
                Some(ProcessorChargeStateCode::PaymentResolution(
                    PaymentResolutionCode::ProcessorChargeExternallyVoided,
                )),
            ),
        ];

        for (transition, progression, state_code) in cases {
            assert_eq!(transition.progression(), progression);
            assert_eq!(transition.state_code(), state_code);
        }
    }
}
