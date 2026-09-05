/// Stops a conflicting approval before it can replace the durable identity or
/// mutate subscription state. The newly observed charge remains traceable for
/// operator reconciliation while the canonical attempt stays unresolved.
pub(crate) async fn stop_conflicting_subscription_approval(
    connection: &mut PgConnection,
    attempt: &PaymentAttempt,
    observed: &ProcessorEvidence,
) -> Result<
    (
        Option<SubscriptionEnrollmentPaymentResult>,
        Vec<GatewayPaymentDiagnostic>,
    ),
    SubscriptionEnrollmentApplicationError,
> {
    let diagnostics = processor_identity_conflict_diagnostics(attempt, observed);
    if diagnostics.is_empty() {
        return Ok((None, diagnostics));
    }
    if !attempt.status().is_resolvable() && attempt.status() != PaymentAttemptStatus::Approved {
        return Ok((None, diagnostics));
    }
    if attempt.status() == PaymentAttemptStatus::Approved
        && same_processor_transaction(attempt, observed)
    {
        let payment = payment_result_for_attempt(connection, attempt.clone()).await?;
        return Ok((
            Some(payment.with_observation_diagnostics(diagnostics)),
            Vec::new(),
        ));
    }
    let progression = if attempt.status().is_resolvable() || attempt.request().amount().cents() == 0
    {
        ProcessorChargeProgression::ReconciliationRequired
    } else {
        ProcessorChargeProgression::ExternalReversalRequired
    };
    let observation = observe_processor_charge(connection, attempt, observed, progression).await?;
    if progression == ProcessorChargeProgression::ExternalReversalRequired
        && observed.transaction_id().is_some()
    {
        promote_conflicting_charge_to_external_reversal(connection, observation).await?;
    }
    let payment = payment_result_for_attempt(connection, attempt.clone()).await?;
    Ok((
        Some(payment.with_observation_diagnostics(diagnostics)),
        Vec::new(),
    ))
}

/// Builds durable non-approved evidence without treating fields from separate
/// processor observations as one approving response.
///
/// A concrete identity conflict preserves the established forensic bundle and
/// forces callers to leave the attempt unresolved. Otherwise the current
/// observation owns the complete decision and descriptor bundles. Missing
/// identity fields may retain established values, but an unanchored partial
/// identity cannot delete or replace an existing durable bundle.
pub(crate) fn reconcile_non_approved_evidence(
    attempt: &PaymentAttempt,
    observed: &ProcessorEvidence,
) -> ReconciledNonApprovedEvidence {
    let persisted = attempt.state().processor_evidence();
    let transaction_id_conflict =
        identifiers_conflict(persisted.transaction_id(), observed.transaction_id());
    let payment_method_reference_conflict = identifiers_conflict(
        persisted.payment_method_reference(),
        observed.payment_method_reference(),
    );
    if !attempt.status().is_resolvable() {
        return ReconciledNonApprovedEvidence {
            evidence: observed.clone(),
            transaction_id_conflict,
            payment_method_reference_conflict,
            discarded_transaction_id: false,
            discarded_payment_method_reference: false,
        };
    }
    if transaction_id_conflict || payment_method_reference_conflict {
        return ReconciledNonApprovedEvidence {
            evidence: persisted.clone(),
            transaction_id_conflict,
            payment_method_reference_conflict,
            discarded_transaction_id: false,
            discarded_payment_method_reference: false,
        };
    }

    let observed_transaction_id = observed.transaction_id();
    let observed_payment_method_reference = observed.payment_method_reference();
    let persisted_transaction_id = persisted.transaction_id();
    let persisted_payment_method_reference = persisted.payment_method_reference();
    // A partial identity from a different observation cannot be spliced onto
    // the established sibling. Preserve the durable bundle, but make the
    // rejected field observable to host policy instead of silently dropping
    // it.
    let discarded_transaction_id = observed_transaction_id.is_some()
        && observed_payment_method_reference.is_none()
        && persisted_transaction_id.is_none()
        && persisted_payment_method_reference.is_some();
    let discarded_payment_method_reference = observed_transaction_id.is_none()
        && observed_payment_method_reference.is_some()
        && persisted_transaction_id.is_some()
        && persisted_payment_method_reference.is_none();
    let (transaction_id, payment_method_reference) =
        match (observed_transaction_id, observed_payment_method_reference) {
            (None, None) => (
                persisted_transaction_id.cloned(),
                persisted_payment_method_reference.cloned(),
            ),
            (Some(transaction_id), Some(payment_method_reference)) => (
                Some(transaction_id.clone()),
                Some(payment_method_reference.clone()),
            ),
            (Some(transaction_id), None) if persisted_transaction_id == Some(transaction_id) => (
                Some(transaction_id.clone()),
                persisted_payment_method_reference.cloned(),
            ),
            (None, Some(payment_method_reference))
                if persisted_payment_method_reference == Some(payment_method_reference) =>
            {
                (
                    persisted_transaction_id.cloned(),
                    Some(payment_method_reference.clone()),
                )
            }
            (Some(transaction_id), None) if !persisted.has_gateway_reference() => {
                (Some(transaction_id.clone()), None)
            }
            (None, Some(payment_method_reference)) if !persisted.has_gateway_reference() => {
                (None, Some(payment_method_reference.clone()))
            }
            (Some(_), None) | (None, Some(_)) => (
                persisted_transaction_id.cloned(),
                persisted_payment_method_reference.cloned(),
            ),
        };
    // Approval classification is monotonic. Accepting a newly observed
    // transaction identity may replace the identity bundle, but it must not
    // erase a stronger signal retained from an earlier observation.
    let approval_evidence = persisted
        .approval_evidence()
        .merge(observed.approval_evidence());
    ReconciledNonApprovedEvidence {
        evidence: ProcessorEvidence::new(
            approval_evidence,
            transaction_id,
            payment_method_reference,
            observed.response().cloned(),
            observed.response_code().cloned(),
            observed.response_text().cloned(),
            observed.condition().cloned(),
            observed.descriptor().clone(),
        ),
        transaction_id_conflict: false,
        payment_method_reference_conflict: false,
        discarded_transaction_id,
        discarded_payment_method_reference,
    }
}

fn identifiers_conflict<T: Eq>(persisted: Option<&T>, observed: Option<&T>) -> bool {
    matches!((persisted, observed), (Some(persisted), Some(observed)) if persisted != observed)
}

async fn resolve_locked_outcome(
    connection: &mut PgConnection,
    reservation: OutcomeReservation<'_>,
    attempt: PaymentAttempt,
    evidence: &ProcessorEvidence,
    mut resolution: OutcomeResolutionCommand,
) -> Result<OutcomeApplication, SubscriptionEnrollmentApplicationError> {
    let reconciled = reconcile_non_approved_evidence(&attempt, evidence);
    let diagnostics = reconciled.identity_conflict_diagnostics();
    if reconciled.has_identity_conflict() {
        resolution = OutcomeResolutionCommand::unknown(None);
    }
    let evidence = &reconciled.evidence;
    let prepared_attempt_replay = reservation.prepared_attempt_replay();
    let applied = resolution.may_resolve(
        attempt.status(),
        attempt.state().timestamps().submitted_at().is_some(),
    );
    if applied {
        let status = resolution.resolved_status(reservation.operation(), attempt.status());
        persist_attempt_transition(
            connection,
            &attempt,
            evidence,
            AttemptTransition::Resolved {
                status,
                resolution_code: resolution.resolution_code,
            },
        )
        .await
        .map_err(map_attempt_transition_error)?;
        if resolution.clears_submitted_at() {
            clear_resolved_attempt_submission(connection, &attempt).await?;
        }
        if resolution.records_pending_evidence(status) && evidence.indicates_approved_payment() {
            observe_processor_charge(
                connection,
                &attempt,
                evidence,
                ProcessorChargeProgression::Pending,
            )
            .await?;
        }
    }
    let result = append_subscription_observation_diagnostics(
        payment_result_for_reservation_attempt(connection, reservation).await?,
        &diagnostics,
    );
    Ok(OutcomeApplication {
        payment: result,
        applied,
        prepared_attempt_replay,
    })
}

/// Atomically returns an admitted-but-unsubmitted attempt to its prepared
/// state when a control-plane check proves that the mutation endpoint was not
/// contacted. Concurrent terminal outcomes remain canonical.
async fn restore_admitted_attempt_for_retry(
    pool: &PgPool,
    reservation: OutcomeReservation<'_>,
) -> Result<OutcomeApplication, SubscriptionEnrollmentApplicationError> {
    // Subscriber reservations share this lock/result projection; host charges
    // intentionally use their own domain wrapper around the same atomic
    // submitted-at restoration primitive.
    let identity = reservation.identity();
    let mut transaction = pool.begin().await?;
    set_application_timeouts(&mut transaction).await?;
    lock_subscription_aggregate(
        &mut transaction,
        identity.subscriber_id(),
        reservation.plan_key(),
    )
    .await?;
    let attempt = lock_expected_reservation_attempt(&mut transaction, reservation).await?;
    let restored = attempt.status() == PaymentAttemptStatus::Pending
        && attempt.state().timestamps().submitted_at().is_some();
    if restored {
        restore_prepared_attempt_submission(&mut transaction, &attempt).await?;
    }
    let result = payment_result_for_reservation_attempt(&mut transaction, reservation).await?;
    transaction.commit().await?;
    if restored {
        tracing::warn!(
            target: "syrup_rail::gateway_control_plane",
            attempt_id = %identity.attempt_id().as_uuid(),
            attempt_kind = attempt.kind().as_str(),
            required_gateway_account_mode = identity.required_gateway_account_mode().as_str(),
            "restored admitted payment attempt after pre-submission control-plane failure"
        );
    }
    Ok(OutcomeApplication {
        payment: result,
        applied: restored,
        prepared_attempt_replay: reservation.prepared_attempt_replay(),
    })
}

async fn apply_resumable_not_submitted_policy(
    pool: &PgPool,
    reservation: OutcomeReservation<'_>,
    evidence: &ProcessorEvidence,
    policy: GatewayNotSubmittedPolicy,
) -> Result<OutcomeApplication, SubscriptionEnrollmentApplicationError> {
    if policy.restores_prepared_attempt_when_supported()
        && reservation.prepared_attempt_replay() == PreparedAttemptReplay::Supported
    {
        return restore_admitted_attempt_for_retry(pool, reservation).await;
    }
    resolve_pool_outcome(
        pool,
        reservation,
        evidence,
        OutcomeResolutionCommand::non_approved(
            AttemptResolutionStatus::Failed,
            Some(policy.resolution_code()),
            policy.cooldown(),
            OutcomeResolutionBoundary::AdmittedNotSubmitted,
        ),
    )
    .await
}

/// Clears the economic-submission timestamp after the locked attempt has
/// already transitioned to a terminal or review status. This deliberately
/// cannot use the pending-state predicate of
/// [`restore_prepared_attempt_submission`].
async fn clear_resolved_attempt_submission(
    connection: &mut PgConnection,
    attempt: &PaymentAttempt,
) -> Result<(), SubscriptionEnrollmentApplicationError> {
    let result = sqlx::query(
        "UPDATE billing_payment_attempts SET submitted_at = NULL, updated_at = clock_timestamp() WHERE id = $1",
    )
    .bind(attempt.identity().attempt_id().as_uuid())
    .execute(connection)
    .await?;
    if result.rows_affected() != 1 {
        return Err(SubscriptionEnrollmentApplicationError::InvalidState(
            INVALID_APPLICATION_STATE,
        ));
    }
    Ok(())
}

/// Restores a still-pending, admitted attempt after a trusted control-plane
/// check proves that the mutation endpoint was not contacted. Unlike
/// [`clear_resolved_attempt_submission`], the state predicate is part of the
/// atomic update so this function cannot clear a concurrent resolution.
pub(crate) async fn restore_prepared_attempt_submission(
    connection: &mut PgConnection,
    attempt: &PaymentAttempt,
) -> Result<(), SubscriptionEnrollmentApplicationError> {
    let result = sqlx::query(
        "UPDATE billing_payment_attempts SET submitted_at = NULL, updated_at = clock_timestamp() \
         WHERE id = $1 AND status = 'pending' AND submitted_at IS NOT NULL",
    )
    .bind(attempt.identity().attempt_id().as_uuid())
    .execute(connection)
    .await?;
    if result.rows_affected() != 1 {
        return Err(SubscriptionEnrollmentApplicationError::InvalidState(
            INVALID_APPLICATION_STATE,
        ));
    }
    Ok(())
}

async fn payment_result_for_reservation_attempt(
    connection: &mut PgConnection,
    reservation: OutcomeReservation<'_>,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    let identity = reservation.identity();
    let attempt = find_payment_attempt_by_id_on_connection(
        connection,
        identity.billing_scope_id(),
        identity.attempt_id(),
    )
    .await?
    .ok_or(SubscriptionEnrollmentApplicationError::InvalidState(
        INVALID_APPLICATION_STATE,
    ))?;
    payment_result_for_attempt(connection, attempt).await
}

async fn persist_approved_evidence_without_attempt_lock(
    pool: &PgPool,
    terms: LockFreeApprovedEvidenceTerms<'_>,
    evidence: &ProcessorEvidence,
) -> Result<(), SubscriptionEnrollmentApplicationError> {
    match crate::processor_charges::persist_approved_evidence_without_attempt_lock(
        pool, terms, evidence,
    )
    .await?
    {
        LockFreeApprovedEvidenceOutcome::Persisted
        | LockFreeApprovedEvidenceOutcome::ExactReplay
        | LockFreeApprovedEvidenceOutcome::OwnedByOtherAttempt => Ok(()),
        LockFreeApprovedEvidenceOutcome::NotDurable => {
            Err(SubscriptionEnrollmentApplicationError::ApprovedEvidenceNotDurable)
        }
    }
}

fn is_retryable_evidence_error(error: &SubscriptionEnrollmentApplicationError) -> bool {
    let sqlstate = match error {
        SubscriptionEnrollmentApplicationError::Sql(sqlx::Error::Database(error)) => error.code(),
        SubscriptionEnrollmentApplicationError::Attempt(PaymentAttemptStoreError::Sql(
            sqlx::Error::Database(error),
        )) => error.code(),
        _ => None,
    };
    sqlstate
        .as_deref()
        .is_some_and(crate::transaction_support::is_transient_sqlstate)
}

pub(crate) async fn set_application_timeouts(
    connection: &mut PgConnection,
) -> Result<(), sqlx::Error> {
    crate::transaction_support::set_local_timeouts(
        connection,
        BILLING_ROW_LOCK_TIMEOUT,
        BILLING_OPERATION_TIMEOUT,
    )
    .await
}

async fn upsert_payment_method(
    connection: &mut PgConnection,
    attempt: &PaymentAttempt,
    evidence: &ProcessorEvidence,
) -> Result<PaymentMethodId, SubscriptionEnrollmentApplicationError> {
    let identity = attempt.identity();
    let reference = evidence.payment_method_reference().ok_or(
        SubscriptionEnrollmentApplicationError::InvalidState(INVALID_APPLICATION_STATE),
    )?;
    let descriptor = evidence.descriptor();
    let row_id: Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO billing_payment_methods (
            id, billing_scope_id, subscriber_id, gateway_account_id,
            gateway_payment_method_reference, status, payment_type, card_brand,
            card_last4, card_exp_month, card_exp_year, billing_name, billing_email
        ) VALUES ($1, $2, $3, $4, $5, 'active', $6, $7, $8, $9, $10, $11, $12)
        ON CONFLICT (gateway_account_id, subscriber_id, gateway_payment_method_reference)
        DO UPDATE SET status = 'active', payment_type = EXCLUDED.payment_type,
            card_brand = EXCLUDED.card_brand, card_last4 = EXCLUDED.card_last4,
            card_exp_month = EXCLUDED.card_exp_month,
            card_exp_year = EXCLUDED.card_exp_year,
            billing_name = EXCLUDED.billing_name,
            billing_email = EXCLUDED.billing_email,
            updated_at = clock_timestamp()
        RETURNING id
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(identity.billing_scope_id().as_uuid())
    .bind(identity.subscriber_id().as_uuid())
    .bind(identity.gateway_account_id().as_uuid())
    .bind(reference.expose())
    .bind(descriptor.payment_type().map(GatewayDiagnostic::expose))
    .bind(descriptor.card_brand().map(GatewayDiagnostic::expose))
    .bind(descriptor.card_last_four().map(|value| value.expose()))
    .bind(descriptor.card_exp_month())
    .bind(descriptor.card_exp_year())
    .bind(attempt.request().billing_contact().name())
    .bind(attempt.request().billing_contact().email())
    .fetch_one(connection)
    .await?;
    Ok(PaymentMethodId::new(row_id))
}

async fn mark_attempt_approved(
    connection: &mut PgConnection,
    attempt: &PaymentAttempt,
    evidence: &ProcessorEvidence,
    subscription_id: SubscriptionId,
    method_id: PaymentMethodId,
) -> Result<(), SubscriptionEnrollmentApplicationError> {
    persist_attempt_transition(
        connection,
        attempt,
        evidence,
        AttemptTransition::Approved(AttemptApproval::Subscription {
            subscription_id,
            payment_method_id: method_id,
        }),
    )
    .await
    .map_err(map_attempt_transition_error)
}

pub(crate) async fn park_locked_attempt(
    connection: &mut PgConnection,
    attempt: &PaymentAttempt,
    evidence: &ProcessorEvidence,
    resolution_code: Option<PaymentResolutionCode>,
    message: &'static str,
) -> Result<PaymentAttempt, SubscriptionEnrollmentApplicationError> {
    // A late approval is a separate processor observation. Its raw evidence is
    // stored in the charge ledger before callers park the attempt, so replacing
    // an existing attempt snapshot here would either erase a sparse durable
    // identity or manufacture a cross-observation bundle. Only a still-pending
    // attempt without an established gateway reference adopts this observation
    // as its first attempt-level evidence.
    let durable_evidence = attempt.state().processor_evidence();
    let attempt_evidence = if attempt.status() == PaymentAttemptStatus::Pending
        && !durable_evidence.has_gateway_reference()
    {
        evidence
    } else {
        durable_evidence
    };
    persist_attempt_transition(
        connection,
        attempt,
        attempt_evidence,
        AttemptTransition::LateApprovalReview {
            resolution_code,
            message,
        },
    )
    .await
    .map_err(map_attempt_transition_error)?;
    find_payment_attempt_by_id_on_connection(
        connection,
        attempt.identity().billing_scope_id(),
        attempt.identity().attempt_id(),
    )
    .await?
    .ok_or(SubscriptionEnrollmentApplicationError::InvalidState(
        INVALID_APPLICATION_STATE,
    ))
}

pub(crate) async fn payment_result_for_attempt(
    connection: &mut PgConnection,
    attempt: PaymentAttempt,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    if attempt.status() == PaymentAttemptStatus::Approved {
        let subscription = load_applied_subscription(connection, &attempt)
            .await?
            .ok_or(SubscriptionEnrollmentApplicationError::InvalidState(
                INVALID_APPLICATION_STATE,
            ))?;
        Ok(SubscriptionEnrollmentPaymentResult::applied(
            attempt,
            subscription,
        )?)
    } else {
        Ok(SubscriptionEnrollmentPaymentResult::not_applied(attempt)?)
    }
}

async fn load_applied_subscription(
    connection: &mut PgConnection,
    attempt: &PaymentAttempt,
) -> Result<Option<Subscription>, SubscriptionEnrollmentApplicationError> {
    let Some(subscription_id) = attempt.request().target().subscription_id() else {
        return Ok(None);
    };
    load_subscription(
        connection,
        attempt.identity().billing_scope_id(),
        subscription_id,
    )
    .await
}

async fn load_subscription(
    connection: &mut PgConnection,
    billing_scope_id: BillingScopeId,
    subscription_id: SubscriptionId,
) -> Result<Option<Subscription>, SubscriptionEnrollmentApplicationError> {
    let row = sqlx::query(
        r#"
        SELECT id, plan_key, status, payment_method_id, amount_cents, currency,
            current_period_start_at, current_period_end_at, next_renewal_at,
            phase, recurring_period_kind, recurring_period_count,
            dunning_retry_delays_seconds, dunning_exhaustion, past_due_access,
            next_payment_attempt_at, required_gateway_account_mode
        FROM billing_subscriptions
        WHERE billing_scope_id = $1 AND id = $2
        "#,
    )
    .bind(billing_scope_id.as_uuid())
    .bind(subscription_id.as_uuid())
    .fetch_optional(connection)
    .await?;
    row.map(|row| decode_subscription_row(&row).map_err(map_subscription_persistence_error))
        .transpose()
}

#[cfg(test)]
mod tests;
