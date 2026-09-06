mod admission;
mod foreground;
mod observations;
mod resilience;
mod rollback;

use std::{
    error::Error,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use chrono::Duration as ChronoDuration;
use sqlx::{Postgres, Transaction};
use syrup_rail::{
    BillingContact, BillingEventKey, ChargeAmount, CurrencyCode, EndUserMutationAdmission,
    EndUserMutationAdmissionResult, EndUserMutationCommand, GatewayAccountId, GatewayAccountMode,
    GatewayConfigurationId, GatewayDiagnostic, GatewayError, GatewayLifecycleCursorKey,
    GatewayLifecycleQueryPolicy, GatewayMutationError, GatewayMutationReferenceFactory,
    GatewayOrderId, GatewayPaymentDescriptor, GatewayPaymentDiagnostic, GatewayProviderKey,
    GatewayQueryRequest, GatewayResolutionError, GatewayResolver, GatewayStorePaymentMethodRequest,
    GatewayTransactionId, GatewayTransactionReport, GatewayTransactionReportRequest,
    HostChargeTargetId, HostChargeTargetNoChange, IdempotencyKey, PaymentAttemptId,
    PaymentAttemptKind, PaymentGateway, PaymentToken, ResolvedGateway,
};
use tokio::sync::{Mutex, Notify, oneshot};
use uuid::Uuid;

use super::*;
use crate::{
    BillingEventWriteError, BillingTransaction, HostChargeLedgerAdmission,
    HostChargeLedgerAdmissionMode, HostChargeLedgerAdmissionQuery, HostChargeReservationDecision,
    HostChargeReservationOutcome, HostChargeSubmissionAdmission, HostChargeSubmissionDecision,
    HostChargeTargetReservation, SubscriptionBillingService, SubscriptionOfferStore,
    host_charge_ledger_admission, reserve_host_charge_in_transaction,
    test_support::{TestDatabase, create_gateway_account},
};

mod readiness_replay;

#[test]
fn before_submission_resolution_modes_keep_boundary_and_cooldown_distinct() {
    let prepared = HostChargeBeforeSubmissionResolution::prepared();
    assert_eq!(prepared.boundary, OutcomeResolutionBoundary::Prepared);
    assert!(prepared.cooldown.is_none());

    let admitted = HostChargeBeforeSubmissionResolution::admitted_not_submitted();
    assert_eq!(
        admitted.boundary,
        OutcomeResolutionBoundary::AdmittedNotSubmitted
    );
    assert!(admitted.cooldown.is_none());

    let rate_limited = HostChargeBeforeSubmissionResolution::prepared().with_not_submitted_policy(
        GatewayNotSubmittedPolicy::for_readiness_error(&syrup_rail::GatewayError::RateLimited(
            GatewayDiagnostic::new("rate limited"),
        )),
    );
    assert_eq!(rate_limited.boundary, OutcomeResolutionBoundary::Prepared);
    assert!(matches!(
        rate_limited.cooldown,
        Some(RateLimitCooldown::Provider)
    ));

    let terminal_failure = HostChargeBeforeSubmissionResolution::prepared()
        .with_not_submitted_policy(GatewayNotSubmittedPolicy::for_readiness_error(
            &syrup_rail::GatewayError::Configuration(GatewayDiagnostic::new("configuration")),
        ));
    assert_eq!(
        terminal_failure.boundary,
        OutcomeResolutionBoundary::Prepared
    );
    assert!(terminal_failure.cooldown.is_none());
}

struct TestReferenceFactory;

impl GatewayMutationReferenceFactory for TestReferenceFactory {
    fn for_attempt(
        &self,
        _kind: PaymentAttemptKind,
        attempt_id: PaymentAttemptId,
    ) -> GatewayOrderId {
        GatewayOrderId::from_generated_attempt(
            format!("test_host_{}", attempt_id.as_uuid().simple()),
            attempt_id,
        )
        .expect("valid host test reference")
    }
}

struct ScriptedGateway {
    account_mode: GatewayAccountMode,
    sale_calls: AtomicUsize,
    outcome: Mutex<Option<GatewayPaymentOutcome>>,
}

#[async_trait]
impl PaymentGateway for ScriptedGateway {
    async fn account_mode(&self) -> Result<GatewayAccountMode, GatewayError> {
        Ok(self.account_mode)
    }

    async fn sale(
        &self,
        _request: GatewaySaleRequest,
    ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
        self.sale_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self
            .outcome
            .lock()
            .await
            .take()
            .expect("one sale capability"))
    }

    async fn store_payment_method(
        &self,
        _request: GatewayStorePaymentMethodRequest,
    ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
        panic!("host charge must not store a payment method")
    }

    async fn query_transaction(
        &self,
        _request: GatewayQueryRequest,
    ) -> Result<Option<GatewayPaymentOutcome>, GatewayError> {
        panic!("foreground host charge must not query")
    }

    async fn query_transaction_reports(
        &self,
        _request: GatewayTransactionReportRequest,
    ) -> Result<Vec<GatewayTransactionReport>, GatewayError> {
        panic!("foreground host charge must not query reports")
    }
}

struct RateLimitedAfterReservationGateway {
    readiness_calls: AtomicUsize,
    sale_calls: AtomicUsize,
}

struct TerminalRaceGateway {
    pool: PgPool,
    sale_calls: AtomicUsize,
}

struct RacingPreparedRetryGateway {
    readiness_calls: AtomicUsize,
    sale_calls: AtomicUsize,
    blocked_readiness_started: Mutex<Option<oneshot::Sender<()>>>,
    sale_started: Mutex<Option<oneshot::Sender<()>>>,
    release_readiness: Notify,
    release_sale: Notify,
}

#[async_trait]
impl PaymentGateway for RacingPreparedRetryGateway {
    async fn account_mode(&self) -> Result<GatewayAccountMode, GatewayError> {
        if self.readiness_calls.fetch_add(1, Ordering::SeqCst) == 0 {
            if let Some(started) = self.blocked_readiness_started.lock().await.take() {
                let _ = started.send(());
            }
            self.release_readiness.notified().await;
            Ok(GatewayAccountMode::Test)
        } else {
            Ok(GatewayAccountMode::Live)
        }
    }

    async fn sale(
        &self,
        _request: GatewaySaleRequest,
    ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
        self.sale_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(started) = self.sale_started.lock().await.take() {
            let _ = started.send(());
        }
        self.release_sale.notified().await;
        Ok(approved_outcome("host_txn_prepared_retry"))
    }

    async fn store_payment_method(
        &self,
        _request: GatewayStorePaymentMethodRequest,
    ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
        panic!("host charge must not store a payment method")
    }

    async fn query_transaction(
        &self,
        _request: GatewayQueryRequest,
    ) -> Result<Option<GatewayPaymentOutcome>, GatewayError> {
        panic!("foreground host charge must not query")
    }

    async fn query_transaction_reports(
        &self,
        _request: GatewayTransactionReportRequest,
    ) -> Result<Vec<GatewayTransactionReport>, GatewayError> {
        panic!("foreground host charge must not query reports")
    }
}

#[async_trait]
impl PaymentGateway for TerminalRaceGateway {
    async fn account_mode(&self) -> Result<GatewayAccountMode, GatewayError> {
        Ok(GatewayAccountMode::Live)
    }

    async fn sale(
        &self,
        request: GatewaySaleRequest,
    ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
        self.sale_calls.fetch_add(1, Ordering::SeqCst);
        let updated = sqlx::query(
            r#"
            UPDATE billing_payment_attempts
            SET status = 'failed', resolved_at = clock_timestamp(),
                gateway_response_text = 'simulated terminal race',
                gateway_condition = 'failed', updated_at = clock_timestamp()
            WHERE gateway_order_id = $1 AND status = 'pending'
            "#,
        )
        .bind(request.order_id().expose())
        .execute(&self.pool)
        .await
        .expect("simulate a terminal attempt race");
        assert_eq!(updated.rows_affected(), 1);
        Ok(approved_outcome("host_txn_terminal_race"))
    }

    async fn store_payment_method(
        &self,
        _request: GatewayStorePaymentMethodRequest,
    ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
        panic!("host charge must not store a payment method")
    }

    async fn query_transaction(
        &self,
        _request: GatewayQueryRequest,
    ) -> Result<Option<GatewayPaymentOutcome>, GatewayError> {
        panic!("foreground host charge must not query")
    }

    async fn query_transaction_reports(
        &self,
        _request: GatewayTransactionReportRequest,
    ) -> Result<Vec<GatewayTransactionReport>, GatewayError> {
        panic!("foreground host charge must not query reports")
    }
}

#[async_trait]
impl PaymentGateway for RateLimitedAfterReservationGateway {
    async fn account_mode(&self) -> Result<GatewayAccountMode, GatewayError> {
        self.readiness_calls.fetch_add(1, Ordering::SeqCst);
        Err(GatewayError::RateLimited(GatewayDiagnostic::new(
            "provider throttled the readiness check",
        )))
    }

    async fn sale(
        &self,
        _request: GatewaySaleRequest,
    ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
        self.sale_calls.fetch_add(1, Ordering::SeqCst);
        panic!("rate-limited host charge must not submit a sale")
    }

    async fn store_payment_method(
        &self,
        _request: GatewayStorePaymentMethodRequest,
    ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
        panic!("host charge must not store a payment method")
    }

    async fn query_transaction(
        &self,
        _request: GatewayQueryRequest,
    ) -> Result<Option<GatewayPaymentOutcome>, GatewayError> {
        panic!("foreground host charge must not query")
    }

    async fn query_transaction_reports(
        &self,
        _request: GatewayTransactionReportRequest,
    ) -> Result<Vec<GatewayTransactionReport>, GatewayError> {
        panic!("foreground host charge must not query reports")
    }
}

struct StaticResolver {
    gateway: ResolvedGateway,
    calls: AtomicUsize,
}

#[async_trait]
impl GatewayResolver for StaticResolver {
    async fn resolve(
        &self,
        billing_scope_id: syrup_rail::BillingScopeId,
        gateway_account_id: GatewayAccountId,
        gateway_configuration_id: GatewayConfigurationId,
        provider_key: GatewayProviderKey,
    ) -> Result<ResolvedGateway, GatewayResolutionError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if billing_scope_id != self.gateway.billing_scope_id()
            || gateway_account_id != self.gateway.gateway_account_id()
            || gateway_configuration_id != self.gateway.gateway_configuration_id()
            || provider_key != *self.gateway.provider_key()
        {
            return Err(GatewayResolutionError::ConfigurationChanged);
        }
        Ok(self.gateway.clone())
    }
}

struct PermitAdmission {
    calls: AtomicUsize,
}

#[async_trait]
impl EndUserMutationAdmission for PermitAdmission {
    async fn admit(&self, _command: EndUserMutationCommand) -> EndUserMutationAdmissionResult {
        self.calls.fetch_add(1, Ordering::SeqCst);
        EndUserMutationAdmissionResult::Allowed
    }
}

struct UnusedOffers;

#[async_trait]
impl SubscriptionOfferStore for UnusedOffers {
    async fn lock_current_offer(
        &self,
        _connection: &mut PgConnection,
        _billing_scope_id: syrup_rail::BillingScopeId,
        _plan_key: &syrup_rail::PlanKey,
    ) -> Result<Option<syrup_rail::SubscriptionOffer>, sqlx::Error> {
        panic!("host charge must not load a subscription offer")
    }
}

#[derive(Clone)]
struct TestCoordinator {
    pool: PgPool,
    events: Arc<Mutex<Vec<BillingEvent>>>,
}

#[async_trait]
impl BillingTransactionCoordinator for TestCoordinator {
    async fn begin(
        &self,
        _subject: BillingEventSubject,
        _lock_timeout: Duration,
    ) -> Result<Box<dyn BillingTransaction>, BillingTransactionError> {
        Ok(Box::new(TestTransaction {
            transaction: Some(
                self.pool
                    .begin()
                    .await
                    .map_err(BillingTransactionError::new)?,
            ),
            events: Arc::clone(&self.events),
        }))
    }
}

struct TestTransaction {
    transaction: Option<Transaction<'static, Postgres>>,
    events: Arc<Mutex<Vec<BillingEvent>>>,
}

#[async_trait]
impl BillingTransaction for TestTransaction {
    fn connection(&mut self) -> &mut PgConnection {
        &mut *self.transaction.as_mut().expect("active transaction")
    }

    fn subject_state(&self) -> BillingTransactionSubjectState {
        BillingTransactionSubjectState::LiveRecipient
    }

    async fn append_event(&mut self, event: &BillingEvent) -> Result<(), BillingEventWriteError> {
        self.events.lock().await.push(event.clone());
        Ok(())
    }

    async fn commit(mut self: Box<Self>) -> Result<(), BillingTransactionError> {
        self.transaction
            .take()
            .expect("active transaction")
            .commit()
            .await
            .map_err(BillingTransactionError::new)
    }

    async fn rollback(mut self: Box<Self>) -> Result<(), BillingTransactionError> {
        self.transaction
            .take()
            .expect("active transaction")
            .rollback()
            .await
            .map_err(BillingTransactionError::new)
    }
}

struct TestTargets;

struct AdmissionMustNotRun;

#[async_trait]
impl HostChargeTargetStore for AdmissionMustNotRun {
    async fn preflight_target(
        &self,
        _connection: &mut PgConnection,
        _reservation: &HostChargeTargetReservation,
    ) -> Result<HostChargeReservationDecision, HostChargeTargetError> {
        panic!("terminal admission replay must not preflight the host target")
    }

    async fn reserve_target(
        &self,
        _connection: &mut PgConnection,
        _reservation: &HostChargeTargetReservation,
    ) -> Result<HostChargeReservationDecision, HostChargeTargetError> {
        panic!("terminal admission replay must not reserve the host target")
    }

    async fn ensure_submission_admitted(
        &self,
        _connection: &mut PgConnection,
        _admission: &HostChargeSubmissionAdmission,
    ) -> Result<HostChargeSubmissionDecision, HostChargeTargetError> {
        panic!("terminal admission replay must not invoke the host target")
    }

    async fn apply_transition(
        &self,
        _connection: &mut PgConnection,
        _transition: HostChargeTargetTransition,
    ) -> Result<HostChargeTargetTransitionOutcome, HostChargeTargetError> {
        panic!("terminal admission replay must not transition the host target")
    }
}

#[async_trait]
impl HostChargeTargetStore for TestTargets {
    async fn preflight_target(
        &self,
        connection: &mut PgConnection,
        reservation: &HostChargeTargetReservation,
    ) -> Result<HostChargeReservationDecision, HostChargeTargetError> {
        self.reserve_target(connection, reservation).await
    }

    async fn reserve_target(
        &self,
        connection: &mut PgConnection,
        reservation: &HostChargeTargetReservation,
    ) -> Result<HostChargeReservationDecision, HostChargeTargetError> {
        let row = sqlx::query_as::<_, (String, i32, String)>(
            r#"
            SELECT status, amount_cents, currency
            FROM host_charge_targets
            WHERE id = $1 AND billing_scope_id = $2 AND subscriber_id = $3
            FOR UPDATE
            "#,
        )
        .bind(reservation.target_id().as_uuid())
        .bind(reservation.billing_scope_id().as_uuid())
        .bind(reservation.subscriber_id().as_uuid())
        .fetch_optional(&mut *connection)
        .await
        .map_err(HostChargeTargetError::new)?;
        let Some((status, cents, currency)) = row else {
            return Ok(HostChargeReservationDecision::Rejected {
                reason: syrup_rail::HostChargeTargetRejection::TargetUnavailable,
            });
        };
        let ledger = host_charge_ledger_admission(
            connection,
            &HostChargeLedgerAdmissionQuery::new(
                reservation.billing_scope_id(),
                reservation.subscriber_id(),
                reservation.target_id(),
                HostChargeLedgerAdmissionMode::Reserve {
                    idempotency_key: reservation.idempotency_key().clone(),
                },
            ),
        )
        .await
        .map_err(HostChargeTargetError::new)?;
        let charge = ChargeAmount::new(cents, CurrencyCode::new(&currency).unwrap()).unwrap();
        if ledger == HostChargeLedgerAdmission::IdempotentContender {
            return Ok(HostChargeReservationDecision::IdempotentContender(
                syrup_rail::HostChargeTargetSnapshot::new(reservation.target_id(), charge),
            ));
        }
        if ledger != HostChargeLedgerAdmission::Safe || status != "pending" {
            return Ok(HostChargeReservationDecision::Rejected {
                reason: syrup_rail::HostChargeTargetRejection::LedgerUnsafe,
            });
        }
        Ok(HostChargeReservationDecision::Reserved(
            syrup_rail::HostChargeTargetSnapshot::new(reservation.target_id(), charge),
        ))
    }

    async fn ensure_submission_admitted(
        &self,
        connection: &mut PgConnection,
        admission: &HostChargeSubmissionAdmission,
    ) -> Result<HostChargeSubmissionDecision, HostChargeTargetError> {
        let row = sqlx::query_as::<_, (String, i32, String)>(
            r#"
            SELECT status, amount_cents, currency
            FROM host_charge_targets
            WHERE id = $1 AND billing_scope_id = $2 AND subscriber_id = $3
            FOR UPDATE
            "#,
        )
        .bind(admission.target_id().as_uuid())
        .bind(admission.billing_scope_id().as_uuid())
        .bind(admission.subscriber_id().as_uuid())
        .fetch_optional(&mut *connection)
        .await
        .map_err(HostChargeTargetError::new)?;
        let Some((status, cents, currency)) = row else {
            return Ok(HostChargeSubmissionDecision::Rejected {
                reason: syrup_rail::HostChargeTargetRejection::TargetUnavailable,
            });
        };
        let charge = ChargeAmount::new(cents, CurrencyCode::new(&currency).unwrap()).unwrap();
        let ledger = host_charge_ledger_admission(
            connection,
            &HostChargeLedgerAdmissionQuery::new(
                admission.billing_scope_id(),
                admission.subscriber_id(),
                admission.target_id(),
                HostChargeLedgerAdmissionMode::Submit {
                    attempt_id: admission.attempt_id(),
                },
            ),
        )
        .await
        .map_err(HostChargeTargetError::new)?;
        if ledger == HostChargeLedgerAdmission::Safe
            && status == "pending"
            && charge == admission.expected_charge()
        {
            Ok(HostChargeSubmissionDecision::Admitted(
                syrup_rail::HostChargeTargetSnapshot::new(admission.target_id(), charge),
            ))
        } else {
            Ok(HostChargeSubmissionDecision::Rejected {
                reason: syrup_rail::HostChargeTargetRejection::ChargeChanged,
            })
        }
    }

    async fn apply_transition(
        &self,
        connection: &mut PgConnection,
        transition: HostChargeTargetTransition,
    ) -> Result<HostChargeTargetTransitionOutcome, HostChargeTargetError> {
        let current: Option<String> = sqlx::query_scalar(
            r#"
            SELECT status FROM host_charge_targets
            WHERE id = $1 AND billing_scope_id = $2 AND subscriber_id = $3
            FOR UPDATE
            "#,
        )
        .bind(transition.target_id().as_uuid())
        .bind(transition.billing_scope_id().as_uuid())
        .bind(transition.subscriber_id().as_uuid())
        .fetch_optional(&mut *connection)
        .await
        .map_err(HostChargeTargetError::new)?;
        let Some(current) = current else {
            return Ok(HostChargeTargetTransitionOutcome::StaleTarget);
        };
        match transition.kind() {
            HostChargeTargetTransitionKind::Paid if current == "pending" => {
                sqlx::query(
                    "UPDATE host_charge_targets SET status = 'paid', paid_at = $2 WHERE id = $1",
                )
                .bind(transition.target_id().as_uuid())
                .bind(transition.effective_at())
                .execute(connection)
                .await
                .map_err(HostChargeTargetError::new)?;
                Ok(HostChargeTargetTransitionOutcome::Applied)
            }
            HostChargeTargetTransitionKind::Paid if current == "paid" => {
                Ok(HostChargeTargetTransitionOutcome::ExactReplay)
            }
            HostChargeTargetTransitionKind::Paid => {
                Ok(HostChargeTargetTransitionOutcome::StaleTarget)
            }
            HostChargeTargetTransitionKind::ReleasedBeforeSubmission
            | HostChargeTargetTransitionKind::PaymentFailed
                if current == "pending" =>
            {
                Ok(HostChargeTargetTransitionOutcome::Applied)
            }
            _ => Ok(HostChargeTargetTransitionOutcome::Unchanged {
                reason: HostChargeTargetNoChange::InapplicableState,
            }),
        }
    }
}

struct RefusingTransitionTargets;

#[async_trait]
impl HostChargeTargetStore for RefusingTransitionTargets {
    async fn preflight_target(
        &self,
        connection: &mut PgConnection,
        reservation: &HostChargeTargetReservation,
    ) -> Result<HostChargeReservationDecision, HostChargeTargetError> {
        TestTargets.preflight_target(connection, reservation).await
    }

    async fn reserve_target(
        &self,
        connection: &mut PgConnection,
        reservation: &HostChargeTargetReservation,
    ) -> Result<HostChargeReservationDecision, HostChargeTargetError> {
        TestTargets.reserve_target(connection, reservation).await
    }

    async fn ensure_submission_admitted(
        &self,
        connection: &mut PgConnection,
        admission: &HostChargeSubmissionAdmission,
    ) -> Result<HostChargeSubmissionDecision, HostChargeTargetError> {
        TestTargets
            .ensure_submission_admitted(connection, admission)
            .await
    }

    async fn apply_transition(
        &self,
        _connection: &mut PgConnection,
        _transition: HostChargeTargetTransition,
    ) -> Result<HostChargeTargetTransitionOutcome, HostChargeTargetError> {
        Ok(HostChargeTargetTransitionOutcome::Unchanged {
            reason: HostChargeTargetNoChange::ReleaseUnsafe,
        })
    }
}

fn resolved_gateway(
    account: crate::test_support::GatewayAccountFixture,
    gateway: Arc<dyn PaymentGateway>,
) -> ResolvedGateway {
    ResolvedGateway::new(
        syrup_rail::BillingScopeId::new(account.billing_scope_id),
        GatewayAccountId::new(account.gateway_account_id),
        GatewayConfigurationId::new(account.gateway_configuration_id),
        GatewayProviderKey::new("nmi").unwrap(),
        GatewayLifecycleQueryPolicy::new(
            GatewayLifecycleCursorKey::new("host_test").unwrap(),
            ChronoDuration::minutes(1),
            10,
            2,
            2,
            20,
        )
        .unwrap(),
        Arc::new(TestReferenceFactory),
        gateway,
    )
}

fn approved_outcome(transaction_id: &str) -> GatewayPaymentOutcome {
    approved_outcome_with_reference(transaction_id, None)
}

fn approved_outcome_with_reference(
    transaction_id: &str,
    payment_method_reference: Option<&str>,
) -> GatewayPaymentOutcome {
    GatewayPaymentOutcome::new(
        GatewayPaymentStatus::Approved,
        ProcessorEvidence::new(
            syrup_rail::ProcessorApprovalEvidence::Structured,
            Some(GatewayTransactionId::new(transaction_id).unwrap()),
            payment_method_reference
                .map(|value| syrup_rail::GatewayPaymentMethodReference::new(value).unwrap()),
            Some(GatewayDiagnostic::new("1")),
            None,
            Some(GatewayDiagnostic::new("approved")),
            Some(GatewayDiagnostic::new("complete")),
            GatewayPaymentDescriptor::default(),
        ),
    )
}

fn processor_duplicate_outcome() -> GatewayPaymentOutcome {
    GatewayPaymentOutcome::new(
        GatewayPaymentStatus::Unknown,
        ProcessorEvidence::new(
            syrup_rail::ProcessorApprovalEvidence::Unclassified,
            None,
            None,
            Some(GatewayDiagnostic::new("3")),
            Some(GatewayDiagnostic::new("430")),
            Some(GatewayDiagnostic::new("Duplicate transaction")),
            None,
            GatewayPaymentDescriptor::default(),
        ),
    )
    .with_diagnostics(vec![GatewayPaymentDiagnostic::ProcessorReportedDuplicate])
}
