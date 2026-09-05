pub(crate) async fn apply_reconciled_subscription_gateway_outcome(
    pool: &PgPool,
    coordinator: &dyn BillingTransactionCoordinator,
    billing_scope_id: BillingScopeId,
    attempt_id: syrup_rail::PaymentAttemptId,
    outcome: &syrup_rail::GatewayPaymentOutcome,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    apply_reconciled_gateway_outcome_for(
        pool,
        coordinator,
        billing_scope_id,
        attempt_id,
        outcome,
        ReconciledApplicationEntry::SubscriptionBillingService,
    )
    .await
}

async fn apply_reconciled_gateway_outcome_for(
    pool: &PgPool,
    coordinator: &dyn BillingTransactionCoordinator,
    billing_scope_id: BillingScopeId,
    attempt_id: syrup_rail::PaymentAttemptId,
    outcome: &syrup_rail::GatewayPaymentOutcome,
    entry: ReconciledApplicationEntry,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    let mut transaction = pool.begin().await?;
    let attempt = crate::find_payment_attempt_by_id_in_transaction(
        &mut transaction,
        billing_scope_id,
        attempt_id,
    )
    .await?
    .ok_or(SubscriptionEnrollmentApplicationError::InvalidState(
        entry.attempt_not_found_message(),
    ))?;
    let Some(operation) = entry.operation_for_attempt(attempt.kind()) else {
        transaction.commit().await?;
        return Err(SubscriptionEnrollmentApplicationError::InvalidState(
            "attempt kind is not owned by the subscription billing service",
        ));
    };
    let provider_key = sqlx::query_scalar::<_, String>(
        "SELECT provider_key FROM billing_gateway_accounts WHERE billing_scope_id = $1 AND id = $2",
    )
    .bind(billing_scope_id.as_uuid())
    .bind(attempt.identity().gateway_account_id().as_uuid())
    .fetch_optional(&mut *transaction)
    .await?
    .ok_or(SubscriptionEnrollmentApplicationError::InvalidState(
        operation.gateway_account_not_found_message(),
    ))?;
    transaction.commit().await?;

    let provider_key = GatewayProviderKey::new(provider_key).map_err(|_| {
        SubscriptionEnrollmentApplicationError::InvalidState(operation.invalid_provider_message())
    })?;
    match operation {
        ReservationOperation::Initial => {
            let reservation =
                SubscriptionEnrollmentReservation::from_attempt(&attempt, provider_key).map_err(
                    |_| {
                        SubscriptionEnrollmentApplicationError::InvalidState(
                            operation.invalid_attempt_message(),
                        )
                    },
                )?;
            apply_subscription_enrollment_gateway_outcome(pool, coordinator, &reservation, outcome)
                .await
        }
        ReservationOperation::Recovery => {
            let reservation = SubscriptionRecoveryReservation::from_attempt(&attempt, provider_key)
                .map_err(|_| {
                    SubscriptionEnrollmentApplicationError::InvalidState(
                        operation.invalid_attempt_message(),
                    )
                })?;
            apply_subscription_recovery_gateway_outcome(pool, coordinator, &reservation, outcome)
                .await
        }
        ReservationOperation::Renewal => {
            let reservation = SubscriptionRenewalReservation::from_attempt(&attempt, provider_key)
                .map_err(|_| {
                    SubscriptionEnrollmentApplicationError::InvalidState(
                        operation.invalid_attempt_message(),
                    )
                })?;
            apply_subscription_renewal_gateway_outcome(pool, coordinator, &reservation, outcome)
                .await
        }
        ReservationOperation::PaymentMethodReplacement => {
            apply_reconciled_subscription_payment_method_replacement_gateway_outcome(
                pool,
                coordinator,
                billing_scope_id,
                attempt_id,
                outcome,
            )
            .await
        }
    }
}

/// A closed, secret-free view of the durable terms used while applying a
/// provider outcome. It keeps each operation's matching rule explicit while
/// sharing only the common locking and cooldown mechanics.
#[derive(Clone, Copy)]
pub(crate) enum OutcomeReservation<'a> {
    Initial(&'a SubscriptionEnrollmentReservation),
    Recovery(&'a SubscriptionRecoveryReservation),
    Renewal(&'a SubscriptionRenewalReservation),
    PaymentMethodReplacement(&'a SubscriptionPaymentMethodReplacement),
}

impl<'a> OutcomeReservation<'a> {
    const fn operation(self) -> ReservationOperation {
        match self {
            Self::Initial(_) => ReservationOperation::Initial,
            Self::Recovery(_) => ReservationOperation::Recovery,
            Self::Renewal(_) => ReservationOperation::Renewal,
            Self::PaymentMethodReplacement(_) => ReservationOperation::PaymentMethodReplacement,
        }
    }

    const fn identity(self) -> PaymentAttemptIdentity {
        match self {
            Self::Initial(reservation) => reservation.identity(),
            Self::Recovery(reservation) => reservation.identity(),
            Self::Renewal(reservation) => reservation.identity(),
            Self::PaymentMethodReplacement(reservation) => reservation.identity(),
        }
    }

    const fn plan_key(self) -> &'a PlanKey {
        match self {
            Self::Initial(reservation) => reservation.plan_key(),
            Self::Recovery(reservation) => reservation.plan_key(),
            Self::Renewal(reservation) => reservation.plan_key(),
            Self::PaymentMethodReplacement(reservation) => reservation.plan_key(),
        }
    }

    const fn provider_key(self) -> &'a GatewayProviderKey {
        match self {
            Self::Initial(reservation) => reservation.provider_key(),
            Self::Recovery(reservation) => reservation.provider_key(),
            Self::Renewal(reservation) => reservation.provider_key(),
            Self::PaymentMethodReplacement(reservation) => reservation.provider_key(),
        }
    }

    const fn expected_kind(self) -> PaymentAttemptKind {
        self.operation().expected_kind()
    }

    const fn prepared_attempt_replay(self) -> PreparedAttemptReplay {
        match self {
            Self::Initial(_) | Self::Recovery(_) | Self::PaymentMethodReplacement(_) => {
                PreparedAttemptReplay::Supported
            }
            Self::Renewal(_) => PreparedAttemptReplay::Unsupported,
        }
    }

    fn expected_attempt(self) -> ReservationAttemptExpectation<'a> {
        match self {
            Self::Initial(reservation) => ReservationAttemptExpectation::Initial {
                identity: reservation.identity(),
                plan_key: reservation.plan_key(),
                gateway_order_id: reservation.gateway_order_id(),
            },
            Self::Recovery(reservation) => ReservationAttemptExpectation::Exact {
                identity: reservation.identity(),
                kind: self.expected_kind(),
                request: reservation.request(),
            },
            Self::Renewal(reservation) => ReservationAttemptExpectation::Exact {
                identity: reservation.identity(),
                kind: self.expected_kind(),
                request: reservation.request(),
            },
            Self::PaymentMethodReplacement(reservation) => ReservationAttemptExpectation::Exact {
                identity: reservation.identity(),
                kind: self.expected_kind(),
                request: reservation.request(),
            },
        }
    }

    fn matches_attempt(self, attempt: &PaymentAttempt) -> bool {
        self.expected_attempt().matches(attempt)
    }
}

/// Only charged subscription workflows share aggregate-locked approval parking.
/// Payment-method replacement owns a separate zero-value approval path.
#[derive(Clone, Copy)]
enum ApprovedParkingReservation<'a> {
    Initial(&'a SubscriptionEnrollmentReservation),
    Recovery(&'a SubscriptionRecoveryReservation),
    Renewal(&'a SubscriptionRenewalReservation),
}

impl<'a> ApprovedParkingReservation<'a> {
    const fn outcome_reservation(self) -> OutcomeReservation<'a> {
        match self {
            Self::Initial(reservation) => OutcomeReservation::Initial(reservation),
            Self::Recovery(reservation) => OutcomeReservation::Recovery(reservation),
            Self::Renewal(reservation) => OutcomeReservation::Renewal(reservation),
        }
    }

    fn lock_free_approved_evidence_terms(self) -> Option<LockFreeApprovedEvidenceTerms<'a>> {
        match self {
            Self::Initial(reservation) => Some(LockFreeApprovedEvidenceTerms::initial(reservation)),
            Self::Recovery(reservation) => {
                Some(LockFreeApprovedEvidenceTerms::recovery(reservation))
            }
            Self::Renewal(_) => None,
        }
    }
}

fn terminal_approved_progression(
    attempt: &PaymentAttempt,
    evidence: &ProcessorEvidence,
) -> ProcessorChargeProgression {
    if evidence.transaction_id().is_some() && attempt.request().amount().cents() > 0 {
        ProcessorChargeProgression::ExternalReversalRequired
    } else {
        ProcessorChargeProgression::ReconciliationRequired
    }
}

async fn park_approved_outcome(
    pool: &PgPool,
    reservation: ApprovedParkingReservation<'_>,
    approved_evidence: &ApprovedProcessorEvidence,
    message: &'static str,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    let evidence = approved_evidence.evidence();
    match try_park_approved_outcome(pool, reservation, evidence, message).await {
        Ok(result) => Ok(result),
        Err(_) => {
            observe_approved_evidence_with_retry(pool, reservation, evidence).await?;
            let mut transaction = pool.begin().await?;
            let identity = reservation.outcome_reservation().identity();
            let attempt = find_payment_attempt_by_id_on_connection(
                &mut transaction,
                identity.billing_scope_id(),
                identity.attempt_id(),
            )
            .await?
            .ok_or(SubscriptionEnrollmentApplicationError::InvalidState(
                INVALID_APPLICATION_STATE,
            ))?;
            let result = if attempt.status() == PaymentAttemptStatus::Approved {
                payment_result_for_attempt(&mut transaction, attempt).await?
            } else {
                SubscriptionEnrollmentPaymentResult::confirmation_pending(
                    attempt,
                    approved_evidence.clone(),
                )?
            };
            transaction.commit().await?;
            Ok(result)
        }
    }
}

async fn try_park_approved_outcome(
    pool: &PgPool,
    reservation: ApprovedParkingReservation<'_>,
    evidence: &ProcessorEvidence,
    message: &'static str,
) -> Result<SubscriptionEnrollmentPaymentResult, SubscriptionEnrollmentApplicationError> {
    let mut transaction = pool.begin().await?;
    set_application_timeouts(&mut transaction).await?;
    let reservation = reservation.outcome_reservation();
    let identity = reservation.identity();
    lock_subscription_aggregate(
        &mut transaction,
        identity.subscriber_id(),
        reservation.plan_key(),
    )
    .await?;
    let attempt = lock_expected_reservation_attempt(&mut transaction, reservation).await?;
    let attempt = if attempt.status() == PaymentAttemptStatus::Approved {
        observe_processor_charge(
            &mut transaction,
            &attempt,
            evidence,
            ProcessorChargeProgression::Applied,
        )
        .await?;
        attempt
    } else if attempt.status().is_terminal() {
        let progression = terminal_approved_progression(&attempt, evidence);
        observe_processor_charge(&mut transaction, &attempt, evidence, progression).await?;
        attempt
    } else {
        observe_processor_charge(
            &mut transaction,
            &attempt,
            evidence,
            ProcessorChargeProgression::Pending,
        )
        .await?;
        park_locked_attempt(&mut transaction, &attempt, evidence, None, message).await?
    };
    let result = payment_result_for_attempt(&mut transaction, attempt).await?;
    transaction.commit().await?;
    Ok(result)
}

async fn observe_approved_evidence_with_retry(
    pool: &PgPool,
    reservation: ApprovedParkingReservation<'_>,
    evidence: &ProcessorEvidence,
) -> Result<(), SubscriptionEnrollmentApplicationError> {
    for attempt_index in 0..APPROVED_EVIDENCE_WRITE_ATTEMPTS {
        let result = async {
            let mut transaction = pool.begin().await?;
            set_application_timeouts(&mut transaction).await?;
            let attempt = lock_expected_reservation_attempt(
                &mut transaction,
                reservation.outcome_reservation(),
            )
            .await?;
            observe_processor_charge(
                &mut transaction,
                &attempt,
                evidence,
                ProcessorChargeProgression::Pending,
            )
            .await?;
            transaction.commit().await?;
            Ok::<(), SubscriptionEnrollmentApplicationError>(())
        }
        .await;
        match result {
            Ok(()) => return Ok(()),
            Err(error)
                if is_retryable_evidence_error(&error)
                    && attempt_index + 1 < APPROVED_EVIDENCE_WRITE_ATTEMPTS =>
            {
                tokio::time::sleep(APPROVED_EVIDENCE_RETRY_DELAY).await;
            }
            Err(error) if is_retryable_evidence_error(&error) => break,
            Err(error) => return Err(error),
        }
    }

    let Some(terms) = reservation.lock_free_approved_evidence_terms() else {
        return Err(SubscriptionEnrollmentApplicationError::ApprovedEvidenceNotDurable);
    };
    persist_approved_evidence_without_attempt_lock(pool, terms, evidence).await
}

/// Initial enrollment preserves its historical application match: identity,
/// kind, plan, and gateway order. Every later operation validates its entire
/// request exactly.
enum ReservationAttemptExpectation<'a> {
    Initial {
        identity: PaymentAttemptIdentity,
        plan_key: &'a PlanKey,
        gateway_order_id: &'a GatewayOrderId,
    },
    Exact {
        identity: PaymentAttemptIdentity,
        kind: PaymentAttemptKind,
        request: &'a PaymentAttemptRequest,
    },
}

impl ReservationAttemptExpectation<'_> {
    const fn expected_kind(&self) -> PaymentAttemptKind {
        match self {
            Self::Initial { .. } => PaymentAttemptKind::SubscriptionInitial,
            Self::Exact { kind, .. } => *kind,
        }
    }

    fn matches(&self, attempt: &PaymentAttempt) -> bool {
        match self {
            Self::Initial {
                identity,
                plan_key,
                gateway_order_id,
            } => {
                attempt.identity() == *identity
                    && attempt.kind() == self.expected_kind()
                    && attempt.request().target().plan_key() == Some(*plan_key)
                    && attempt.request().gateway_order_id() == *gateway_order_id
            }
            Self::Exact {
                identity,
                kind,
                request,
            } => {
                attempt.identity() == *identity
                    && attempt.kind() == *kind
                    && attempt.request() == *request
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OutcomeResolutionKind {
    NonApproved,
    Unknown,
}

/// Typed durable resolution inputs. This separates a provider outcome's
/// status/code/cooldown/boundary from the mechanics that persist it.
#[derive(Clone, Copy)]
pub(crate) struct OutcomeResolutionCommand {
    kind: OutcomeResolutionKind,
    status: AttemptResolutionStatus,
    resolution_code: Option<PaymentResolutionCode>,
    cooldown: Option<RateLimitCooldown>,
    boundary: OutcomeResolutionBoundary,
}

impl OutcomeResolutionCommand {
    pub(crate) const fn non_approved(
        status: AttemptResolutionStatus,
        resolution_code: Option<PaymentResolutionCode>,
        cooldown: Option<RateLimitCooldown>,
        boundary: OutcomeResolutionBoundary,
    ) -> Self {
        Self {
            kind: OutcomeResolutionKind::NonApproved,
            status,
            resolution_code,
            cooldown,
            boundary,
        }
    }

    const fn unknown(cooldown: Option<RateLimitCooldown>) -> Self {
        Self {
            kind: OutcomeResolutionKind::Unknown,
            status: AttemptResolutionStatus::Unknown,
            resolution_code: None,
            cooldown,
            boundary: OutcomeResolutionBoundary::Submitted,
        }
    }

    const fn may_resolve(self, status: PaymentAttemptStatus, submitted: bool) -> bool {
        status.is_resolvable()
            && match self.boundary {
                OutcomeResolutionBoundary::Prepared => !submitted,
                OutcomeResolutionBoundary::AdmittedNotSubmitted => submitted,
                OutcomeResolutionBoundary::Submitted => true,
            }
    }

    fn resolved_status(
        self,
        operation: ReservationOperation,
        current: PaymentAttemptStatus,
    ) -> AttemptResolutionStatus {
        if self.kind == OutcomeResolutionKind::Unknown
            && operation.preserves_review_required_for_unknown()
            && current == PaymentAttemptStatus::ReviewRequired
        {
            AttemptResolutionStatus::ReviewRequired
        } else {
            self.status
        }
    }

    fn clears_submitted_at(self) -> bool {
        self.boundary == OutcomeResolutionBoundary::AdmittedNotSubmitted
    }

    fn records_pending_evidence(self, status: AttemptResolutionStatus) -> bool {
        self.kind == OutcomeResolutionKind::Unknown
            && status != AttemptResolutionStatus::ReviewRequired
    }

    fn marks_renewal_past_due(self, status: AttemptResolutionStatus) -> bool {
        self.boundary == OutcomeResolutionBoundary::Submitted
            && matches!(
                status,
                AttemptResolutionStatus::Declined | AttemptResolutionStatus::Failed
            )
    }
}

pub(crate) struct OutcomeApplication {
    payment: SubscriptionEnrollmentPaymentResult,
    applied: bool,
    prepared_attempt_replay: PreparedAttemptReplay,
}

impl OutcomeApplication {
    pub(crate) fn into_payment(self) -> SubscriptionEnrollmentPaymentResult {
        self.payment
    }

    fn should_surface_not_submitted(&self, policy: GatewayNotSubmittedPolicy) -> bool {
        should_surface_not_submitted_application(
            self.applied,
            self.payment.attempt(),
            policy,
            self.prepared_attempt_replay,
        )
    }
}

pub(crate) fn append_subscription_observation_diagnostics(
    result: SubscriptionEnrollmentPaymentResult,
    diagnostics: &[GatewayPaymentDiagnostic],
) -> SubscriptionEnrollmentPaymentResult {
    let mut combined = result.observation_diagnostics().to_vec();
    combined.extend_from_slice(diagnostics);
    result.with_observation_diagnostics(combined)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PreparedAttemptReplay {
    Supported,
    Unsupported,
}

pub(crate) fn should_surface_not_submitted_application(
    applied: bool,
    attempt: &PaymentAttempt,
    policy: GatewayNotSubmittedPolicy,
    prepared_attempt_replay: PreparedAttemptReplay,
) -> bool {
    applied
        || (prepared_attempt_replay == PreparedAttemptReplay::Supported
            && policy.restores_prepared_attempt_when_supported()
            && attempt.status() == PaymentAttemptStatus::Pending
            && attempt.state().timestamps().submitted_at().is_none())
}

async fn resolve_pool_outcome(
    pool: &PgPool,
    reservation: OutcomeReservation<'_>,
    evidence: &ProcessorEvidence,
    resolution: OutcomeResolutionCommand,
) -> Result<OutcomeApplication, SubscriptionEnrollmentApplicationError> {
    let identity = reservation.identity();
    if let Some(cooldown) = resolution.cooldown {
        commit_rate_limit_cooldown(pool, reservation, cooldown).await?;
    }
    let mut transaction = pool.begin().await?;
    set_application_timeouts(&mut transaction).await?;
    lock_subscription_aggregate(
        &mut transaction,
        identity.subscriber_id(),
        reservation.plan_key(),
    )
    .await?;
    let attempt = lock_expected_reservation_attempt(&mut transaction, reservation).await?;
    let application =
        resolve_locked_outcome(&mut transaction, reservation, attempt, evidence, resolution)
            .await?;
    transaction.commit().await?;
    Ok(application)
}

pub(crate) struct ReconciledNonApprovedEvidence {
    pub(crate) evidence: ProcessorEvidence,
    pub(crate) transaction_id_conflict: bool,
    pub(crate) payment_method_reference_conflict: bool,
    discarded_transaction_id: bool,
    discarded_payment_method_reference: bool,
}

impl ReconciledNonApprovedEvidence {
    pub(crate) const fn has_identity_conflict(&self) -> bool {
        self.transaction_id_conflict || self.payment_method_reference_conflict
    }

    pub(crate) fn identity_conflict_diagnostics(&self) -> Vec<GatewayPaymentDiagnostic> {
        identity_conflict_diagnostics(
            self.transaction_id_conflict || self.discarded_transaction_id,
            self.payment_method_reference_conflict || self.discarded_payment_method_reference,
        )
    }
}

fn identity_conflict_diagnostics(
    transaction_id_conflict: bool,
    payment_method_reference_conflict: bool,
) -> Vec<GatewayPaymentDiagnostic> {
    let mut diagnostics = Vec::with_capacity(2);
    if transaction_id_conflict {
        diagnostics.push(GatewayPaymentDiagnostic::InvalidOrConflictingTransactionIdentifier);
    }
    if payment_method_reference_conflict {
        diagnostics.push(GatewayPaymentDiagnostic::InvalidOrConflictingPaymentMethodReference);
    }
    diagnostics
}

pub(crate) fn processor_identity_conflict_diagnostics(
    attempt: &PaymentAttempt,
    observed: &ProcessorEvidence,
) -> Vec<GatewayPaymentDiagnostic> {
    let persisted = attempt.state().processor_evidence();
    identity_conflict_diagnostics(
        identifiers_conflict(persisted.transaction_id(), observed.transaction_id()),
        identifiers_conflict(
            persisted.payment_method_reference(),
            observed.payment_method_reference(),
        ),
    )
}

pub(crate) fn same_processor_transaction(
    attempt: &PaymentAttempt,
    observed: &ProcessorEvidence,
) -> bool {
    let persisted = attempt.state().processor_evidence().transaction_id();
    persisted.is_some() && persisted == observed.transaction_id()
}

