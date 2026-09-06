async fn observe_conflicting_host_charge_approval(
    coordinator: &dyn BillingTransactionCoordinator,
    reservation: &HostChargeReservation,
    evidence: &ProcessorEvidence,
) -> Result<HostChargePaymentResult, HostChargeApplicationError> {
    let identity = reservation.identity();
    let mut transaction = coordinator
        .begin(
            BillingEventSubject::new(identity.billing_scope_id(), identity.subscriber_id()),
            BILLING_LOCK_TIMEOUT,
        )
        .await?;
    let observation = observe_conflicting_host_charge_approval_on_connection(
        transaction.connection(),
        reservation,
        evidence,
    )
    .await;
    finalize_host_charge_observation(transaction, observation).await
}

async fn observe_conflicting_host_charge_approval_on_connection(
    connection: &mut PgConnection,
    reservation: &HostChargeReservation,
    evidence: &ProcessorEvidence,
) -> Result<HostChargeObservation, HostChargeApplicationError> {
    set_application_timeouts(connection).await?;
    let attempt = lock_expected_host_charge(connection, reservation).await?;
    let diagnostics = processor_identity_conflict_diagnostics(&attempt, evidence);
    if diagnostics.is_empty() {
        return Ok(HostChargeObservation::rollback(
            HostChargePaymentResult::new(attempt)?,
        ));
    }
    if attempt.status() == PaymentAttemptStatus::Approved
        && same_processor_transaction(&attempt, evidence)
    {
        return Ok(HostChargeObservation::commit(
            append_host_observation_diagnostics(
                HostChargePaymentResult::new(attempt)?,
                &diagnostics,
            ),
        ));
    }
    let progression = if attempt.status().is_resolvable() {
        ProcessorChargeProgression::ReconciliationRequired
    } else {
        ProcessorChargeProgression::ExternalReversalRequired
    };
    let observation = observe_processor_charge(connection, &attempt, evidence, progression).await?;
    if progression == ProcessorChargeProgression::ExternalReversalRequired
        && evidence.transaction_id().is_some()
    {
        promote_conflicting_charge_to_external_reversal(connection, observation).await?;
    }
    Ok(HostChargeObservation::commit(
        append_host_observation_diagnostics(HostChargePaymentResult::new(attempt)?, &diagnostics),
    ))
}

async fn observe_terminal_host_charge_approval(
    coordinator: &dyn BillingTransactionCoordinator,
    reservation: &HostChargeReservation,
    terminal_attempt: &PaymentAttempt,
    approved_evidence: &ApprovedProcessorEvidence,
) -> Result<HostChargePaymentResult, HostChargeApplicationError> {
    let identity = reservation.identity();
    let mut transaction = coordinator
        .begin(
            BillingEventSubject::new(identity.billing_scope_id(), identity.subscriber_id()),
            BILLING_LOCK_TIMEOUT,
        )
        .await?;
    let observation = observe_terminal_host_charge_approval_on_connection(
        transaction.connection(),
        reservation,
        terminal_attempt,
        approved_evidence,
    )
    .await;
    finalize_host_charge_observation(transaction, observation).await
}

async fn observe_terminal_host_charge_approval_on_connection(
    connection: &mut PgConnection,
    reservation: &HostChargeReservation,
    terminal_attempt: &PaymentAttempt,
    approved_evidence: &ApprovedProcessorEvidence,
) -> Result<HostChargeObservation, HostChargeApplicationError> {
    let evidence = approved_evidence.evidence();
    set_application_timeouts(connection).await?;
    if matches!(
        observe_processor_charge(
            connection,
            terminal_attempt,
            evidence,
            ProcessorChargeProgression::Pending,
        )
        .await?,
        ObservedCharge::OwnedByOtherAttempt
    ) {
        return Err(HostChargeApplicationError::InvalidState(
            "the approved gateway transaction belongs to another payment attempt",
        ));
    }
    let locked = lock_expected_host_charge(connection, reservation).await?;
    if !locked.status().is_terminal() || locked.status() == PaymentAttemptStatus::Approved {
        return Err(HostChargeApplicationError::InvalidState(
            INVALID_HOST_CHARGE_STATE,
        ));
    }
    Ok(HostChargeObservation::commit(
        HostChargePaymentResult::confirmation_pending(locked, approved_evidence.clone())?,
    ))
}

async fn finalize_host_charge_observation(
    transaction: Box<dyn crate::BillingTransaction>,
    observation: Result<HostChargeObservation, HostChargeApplicationError>,
) -> Result<HostChargePaymentResult, HostChargeApplicationError> {
    match observation {
        Err(error) => {
            let _ = transaction.rollback().await;
            Err(error)
        }
        Ok(observation) => {
            match observation.disposition {
                HostChargeObservationDisposition::Commit => transaction.commit().await?,
                HostChargeObservationDisposition::Rollback => transaction.rollback().await?,
            }
            Ok(observation.payment)
        }
    }
}

async fn resolve_host_charge_non_approved(
    pool: &PgPool,
    targets: &dyn HostChargeTargetStore,
    reservation: &HostChargeReservation,
    evidence: &ProcessorEvidence,
    status: AttemptResolutionStatus,
    resolution_code: Option<PaymentResolutionCode>,
    resolution: HostChargeNonApprovedResolution<'_>,
) -> Result<HostChargeResolutionApplication, HostChargeApplicationError> {
    let identity = reservation.identity();
    // Provider/account backoff is independent of whether this attempt still
    // owns the host target or can still be resolved. Commit it first so a
    // concurrent operator/reconciliation resolution, stale target, or later
    // host callback failure cannot roll the observed throttle back. A crash
    // after this commit is fail-safe: it may delay work, but same-key replay
    // can still finish the unresolved attempt.
    if let Some((provider_key, cooldown)) = resolution.cooldown {
        commit_host_charge_cooldown(pool, reservation, provider_key, cooldown).await?;
    }
    let mut transaction = pool.begin().await?;
    set_application_timeouts(&mut transaction).await?;
    let effective_at = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *transaction)
        .await?;
    let target_outcome = targets
        .apply_transition(
            &mut transaction,
            HostChargeTargetTransition::new(
                identity.billing_scope_id(),
                identity.subscriber_id(),
                identity.attempt_id(),
                reservation.snapshot().target_id(),
                host_charge_target_transition_kind(resolution.boundary),
                effective_at,
            ),
        )
        .await?;
    if !target_outcome.is_applied() {
        log_refused_host_charge_target(reservation, resolution.boundary, target_outcome);
        transaction.rollback().await?;
        return refused_host_charge_resolution(pool, reservation, evidence, resolution.boundary)
            .await;
    }
    let attempt = lock_expected_host_charge(&mut transaction, reservation).await?;
    let reconciled = reconcile_non_approved_evidence(&attempt, evidence);
    let diagnostics = reconciled.identity_conflict_diagnostics();
    let may_resolve = host_charge_attempt_may_resolve(&attempt, resolution.boundary);
    if !may_resolve || reconciled.has_identity_conflict() {
        transaction.rollback().await?;
        return canonical_host_charge_resolution_with_diagnostics(pool, reservation, &diagnostics)
            .await;
    }
    persist_non_approved_host_charge_details(
        &mut transaction,
        &attempt,
        &reconciled,
        status,
        resolution_code,
        resolution.boundary,
    )
    .await?;
    let attempt = find_payment_attempt_by_id_on_connection(
        &mut transaction,
        identity.billing_scope_id(),
        identity.attempt_id(),
    )
    .await?
    .ok_or(HostChargeApplicationError::InvalidState(
        INVALID_HOST_CHARGE_STATE,
    ))?;
    transaction.commit().await?;
    Ok(HostChargeResolutionApplication {
        payment: HostChargePaymentResult::new(attempt)?,
        applied: true,
    })
}

fn host_charge_target_transition_kind(
    boundary: OutcomeResolutionBoundary,
) -> HostChargeTargetTransitionKind {
    match boundary {
        OutcomeResolutionBoundary::Prepared | OutcomeResolutionBoundary::AdmittedNotSubmitted => {
            HostChargeTargetTransitionKind::ReleasedBeforeSubmission
        }
        OutcomeResolutionBoundary::Submitted => HostChargeTargetTransitionKind::PaymentFailed,
    }
}

fn log_refused_host_charge_target(
    reservation: &HostChargeReservation,
    boundary: OutcomeResolutionBoundary,
    target_outcome: HostChargeTargetTransitionOutcome,
) {
    let identity = reservation.identity();
    tracing::warn!(
        target: "syrup_rail::host_charge_target",
        billing_scope_id = %identity.billing_scope_id().as_uuid(),
        subscriber_id = %identity.subscriber_id().as_uuid(),
        attempt_id = %identity.attempt_id().as_uuid(),
        target_id = %reservation.snapshot().target_id().as_uuid(),
        ?boundary,
        ?target_outcome,
        "host target refused a payment outcome; leaving the canonical attempt unresolved"
    );
}

async fn refused_host_charge_resolution(
    pool: &PgPool,
    reservation: &HostChargeReservation,
    evidence: &ProcessorEvidence,
    boundary: OutcomeResolutionBoundary,
) -> Result<HostChargeResolutionApplication, HostChargeApplicationError> {
    let mut application = canonical_host_charge_resolution_application(pool, reservation).await?;
    let diagnostics =
        processor_identity_conflict_diagnostics(application.payment.attempt(), evidence);
    application.payment = append_host_observation_diagnostics(application.payment, &diagnostics);
    if !host_charge_attempt_may_resolve(application.payment.attempt(), boundary) {
        return Ok(application);
    }
    Err(HostChargeApplicationError::InvalidState(
        INVALID_HOST_CHARGE_STATE,
    ))
}

async fn persist_non_approved_host_charge_details(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    attempt: &PaymentAttempt,
    reconciled: &crate::enrollment_application::ReconciledNonApprovedEvidence<'_>,
    status: AttemptResolutionStatus,
    resolution_code: Option<PaymentResolutionCode>,
    boundary: OutcomeResolutionBoundary,
) -> Result<(), HostChargeApplicationError> {
    persist_attempt_transition(
        transaction,
        attempt,
        &reconciled.attempt_evidence,
        AttemptTransition::Resolved {
            status,
            resolution_code,
        },
    )
    .await
    .map_err(map_attempt_transition_error)?;
    if let Some(observation) = reconciled.charge_observation() {
        observe_processor_charge(
            transaction,
            attempt,
            observation,
            ProcessorChargeProgression::Pending,
        )
        .await?;
    }
    if boundary == OutcomeResolutionBoundary::AdmittedNotSubmitted {
        let cleared = sqlx::query(
            "UPDATE billing_payment_attempts SET submitted_at = NULL, updated_at = clock_timestamp() WHERE id = $1 AND status = $2",
        )
        .bind(attempt.identity().attempt_id().as_uuid())
        .bind(status.as_str())
        .execute(&mut **transaction)
        .await?;
        if cleared.rows_affected() != 1 {
            return Err(HostChargeApplicationError::InvalidState(
                INVALID_HOST_CHARGE_STATE,
            ));
        }
    }
    Ok(())
}

fn host_charge_attempt_may_resolve(
    attempt: &PaymentAttempt,
    boundary: OutcomeResolutionBoundary,
) -> bool {
    attempt.status().is_resolvable()
        && match boundary {
            OutcomeResolutionBoundary::Prepared => {
                attempt.state().timestamps().submitted_at().is_none()
            }
            OutcomeResolutionBoundary::AdmittedNotSubmitted => {
                attempt.state().timestamps().submitted_at().is_some()
            }
            OutcomeResolutionBoundary::Submitted => true,
        }
}

async fn canonical_host_charge_resolution_application(
    pool: &PgPool,
    reservation: &HostChargeReservation,
) -> Result<HostChargeResolutionApplication, HostChargeApplicationError> {
    let identity = reservation.identity();
    let mut reload = pool.begin().await?;
    let attempt = crate::find_payment_attempt_by_id_in_transaction(
        &mut reload,
        identity.billing_scope_id(),
        identity.attempt_id(),
    )
    .await?
    .ok_or(HostChargeApplicationError::InvalidState(
        INVALID_HOST_CHARGE_STATE,
    ))?;
    reload.commit().await?;
    Ok(HostChargeResolutionApplication {
        payment: HostChargePaymentResult::new(attempt)?,
        applied: false,
    })
}

async fn canonical_host_charge_resolution_with_diagnostics(
    pool: &PgPool,
    reservation: &HostChargeReservation,
    diagnostics: &[GatewayPaymentDiagnostic],
) -> Result<HostChargeResolutionApplication, HostChargeApplicationError> {
    let mut application = canonical_host_charge_resolution_application(pool, reservation).await?;
    application.payment = append_host_observation_diagnostics(application.payment, diagnostics);
    Ok(application)
}

async fn restore_admitted_host_charge_for_retry(
    pool: &PgPool,
    reservation: &HostChargeReservation,
) -> Result<HostChargeResolutionApplication, HostChargeApplicationError> {
    // Keep this domain wrapper separate from subscriber restoration: host
    // charges have their own exact lock and result projection. Both wrappers
    // delegate the atomic submitted-at transition to the shared primitive.
    let identity = reservation.identity();
    let mut transaction = pool.begin().await?;
    set_application_timeouts(&mut transaction).await?;
    let attempt = lock_expected_host_charge(&mut transaction, reservation).await?;
    let restored = attempt.status() == PaymentAttemptStatus::Pending
        && attempt.state().timestamps().submitted_at().is_some();
    if restored {
        restore_prepared_attempt_submission(&mut transaction, &attempt).await?;
    }
    let attempt = find_payment_attempt_by_id_on_connection(
        &mut transaction,
        identity.billing_scope_id(),
        identity.attempt_id(),
    )
    .await?
    .ok_or(HostChargeApplicationError::InvalidState(
        INVALID_HOST_CHARGE_STATE,
    ))?;
    transaction.commit().await?;
    if restored {
        tracing::warn!(
            target: "syrup_rail::gateway_control_plane",
            attempt_id = %identity.attempt_id().as_uuid(),
            attempt_kind = attempt.kind().as_str(),
            required_gateway_account_mode = identity.required_gateway_account_mode().as_str(),
            "restored admitted host charge after pre-submission control-plane failure"
        );
    }
    Ok(HostChargeResolutionApplication {
        payment: HostChargePaymentResult::new(attempt)?,
        applied: restored,
    })
}

async fn resolve_host_charge_unknown(
    pool: &PgPool,
    reservation: &HostChargeReservation,
    evidence: &ProcessorEvidence,
    cooldown: Option<(&GatewayProviderKey, RateLimitCooldown)>,
) -> Result<HostChargePaymentResult, HostChargeApplicationError> {
    let identity = reservation.identity();
    if let Some((provider_key, cooldown)) = cooldown {
        commit_host_charge_cooldown(pool, reservation, provider_key, cooldown).await?;
    }
    let mut transaction = pool.begin().await?;
    set_application_timeouts(&mut transaction).await?;
    let attempt = lock_expected_host_charge(&mut transaction, reservation).await?;
    let reconciled = reconcile_non_approved_evidence(&attempt, evidence);
    let diagnostics = reconciled.identity_conflict_diagnostics();
    if !attempt.status().is_terminal() {
        let evidence = &reconciled.attempt_evidence;
        persist_attempt_transition(
            &mut transaction,
            &attempt,
            evidence,
            AttemptTransition::Resolved {
                status: AttemptResolutionStatus::Unknown,
                resolution_code: None,
            },
        )
        .await
        .map_err(map_attempt_transition_error)?;
        if let Some(observation) = reconciled.charge_observation() {
            observe_processor_charge(
                &mut transaction,
                &attempt,
                observation,
                ProcessorChargeProgression::Pending,
            )
            .await?;
        }
    }
    let attempt = find_payment_attempt_by_id_on_connection(
        &mut transaction,
        identity.billing_scope_id(),
        identity.attempt_id(),
    )
    .await?
    .ok_or(HostChargeApplicationError::InvalidState(
        INVALID_HOST_CHARGE_STATE,
    ))?;
    transaction.commit().await?;
    Ok(append_host_observation_diagnostics(
        HostChargePaymentResult::new(attempt)?,
        &diagnostics,
    ))
}

async fn commit_host_charge_cooldown(
    pool: &PgPool,
    reservation: &HostChargeReservation,
    provider_key: &GatewayProviderKey,
    cooldown: RateLimitCooldown,
) -> Result<(), HostChargeApplicationError> {
    match commit_rate_limit_cooldown_for_operation(
        pool,
        reservation.identity(),
        provider_key,
        cooldown,
        RateLimitCooldownOperation::HostCharge,
    )
    .await
    {
        Ok(()) => Ok(()),
        Err(RateLimitCooldownCommitError::Sql(error)) => Err(error.into()),
        Err(RateLimitCooldownCommitError::MissingProviderCooldown) => Err(
            HostChargeApplicationError::InvalidState(INVALID_HOST_CHARGE_STATE),
        ),
    }
}

async fn park_host_charge_approved(
    pool: &PgPool,
    reservation: &HostChargeReservation,
    evidence: &ProcessorEvidence,
) -> Result<HostChargePaymentResult, HostChargeApplicationError> {
    let mut transaction = pool.begin().await?;
    set_application_timeouts(&mut transaction).await?;
    let attempt = lock_expected_host_charge(&mut transaction, reservation).await?;
    let diagnostics = processor_identity_conflict_diagnostics(&attempt, evidence);
    if !diagnostics.is_empty() {
        if attempt.status() == PaymentAttemptStatus::Approved
            && same_processor_transaction(&attempt, evidence)
        {
            transaction.commit().await?;
            return Ok(append_host_observation_diagnostics(
                HostChargePaymentResult::new(attempt)?,
                &diagnostics,
            ));
        }
        let progression = if attempt.status().is_resolvable() {
            ProcessorChargeProgression::ReconciliationRequired
        } else {
            ProcessorChargeProgression::ExternalReversalRequired
        };
        let observation =
            observe_processor_charge(&mut transaction, &attempt, evidence, progression).await?;
        if progression == ProcessorChargeProgression::ExternalReversalRequired {
            promote_conflicting_charge_to_external_reversal(&mut transaction, observation).await?;
        } else if let ObservedCharge::Owned(charge) = observation {
            transition_charge(&mut transaction, charge.id, progression, None).await?;
        }
        transaction.commit().await?;
        return Ok(append_host_observation_diagnostics(
            HostChargePaymentResult::new(attempt)?,
            &diagnostics,
        ));
    }
    let progression = if evidence.transaction_id().is_some() {
        ProcessorChargeProgression::ExternalReversalRequired
    } else {
        ProcessorChargeProgression::ReconciliationRequired
    };
    let observation =
        observe_processor_charge(&mut transaction, &attempt, evidence, progression).await?;
    if let ObservedCharge::Owned(charge) = observation {
        transition_charge(&mut transaction, charge.id, progression, None).await?;
    }
    let parked = park_locked_attempt(
        &mut transaction,
        &attempt,
        evidence,
        None,
        APPROVED_STORAGE_FAILURE_TEXT,
    )
    .await?;
    transaction.commit().await?;
    Ok(HostChargePaymentResult::new(parked)?)
}

async fn durably_park_host_charge_approved(
    pool: &PgPool,
    reservation: &HostChargeReservation,
    approved_evidence: &ApprovedProcessorEvidence,
) -> Result<HostChargePaymentResult, HostChargeApplicationError> {
    let evidence = approved_evidence.evidence();
    if let Ok(payment) = park_host_charge_approved(pool, reservation, evidence).await {
        return Ok(payment);
    }
    crate::store_compensating_processor_charge(
        pool,
        reservation.identity().attempt_id(),
        reservation.request().gateway_order_id(),
        evidence,
    )
    .await?;
    let mut transaction = pool.begin().await?;
    let attempt = crate::find_payment_attempt_by_id_in_transaction(
        &mut transaction,
        reservation.identity().billing_scope_id(),
        reservation.identity().attempt_id(),
    )
    .await?
    .ok_or(HostChargeApplicationError::InvalidState(
        INVALID_HOST_CHARGE_STATE,
    ))?;
    transaction.commit().await?;
    Ok(HostChargePaymentResult::confirmation_pending(
        attempt,
        approved_evidence.clone(),
    )?)
}

async fn lock_expected_host_charge(
    connection: &mut PgConnection,
    reservation: &HostChargeReservation,
) -> Result<PaymentAttempt, HostChargeApplicationError> {
    let identity = reservation.identity();
    let attempt = lock_payment_attempt_by_id_on_connection(
        connection,
        identity.billing_scope_id(),
        identity.attempt_id(),
    )
    .await?
    .ok_or(HostChargeApplicationError::InvalidState(
        INVALID_HOST_CHARGE_STATE,
    ))?;
    if !host_charge_attempt_matches_reservation(&attempt, reservation) {
        return Err(HostChargeApplicationError::InvalidState(
            INVALID_HOST_CHARGE_STATE,
        ));
    }
    Ok(attempt)
}

#[cfg(test)]
mod tests;
