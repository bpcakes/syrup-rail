//! Billing-period expiry, atomic recovery admission, canonical retirement,
//! verified external reversal, and past-due access policy changes against a
//! real PostgreSQL database with controlled gateway outcomes.

use syrup_rail::{
    ActorId, ChangeSubscriptionPastDueAccess, ExternalReversalKind, ExternalReversalReason,
    PaymentAttemptStatus, PaymentResolutionCode, ProcessorChargeId,
    RetireExpiredSubscriptionPeriod, SubscriptionEnrollmentReservationOutcome,
    SubscriptionPastDueAccessChangeOutcome, SubscriptionPeriodExpiryPolicy,
    SubscriptionPeriodRetirementOutcome, SubscriptionRecoverySubmissionRejection,
    SubscriptionRenewalReservationOutcome, SubscriptionRenewalReservationRejection,
    SubscriptionRenewalSubmissionRejection,
};

use super::*;
use crate::{
    ExternalReversalAttestationOutcome, ExternalReversalHostStore, ExternalReversalHostStoreError,
    ExternalReversalHostTransitionOutcome, SubscriptionEnrollmentApplicationError,
    admit_subscription_recovery_submission_with_transaction, attest_external_reversal,
    change_subscription_past_due_access_in_transaction, claim_exact_reconciliation_attempts,
    retire_expired_subscription_period_in_transaction, test_support::immediate_offer,
};

const ENFORCE: SubscriptionPeriodExpiryPolicy =
    SubscriptionPeriodExpiryPolicy::RejectExpiredPeriods;
const EXPIRED_APPROVAL: &str = "subscription_approved_period_expired";
const EXPIRED_BEFORE_CHARGE: &str = "subscription_period_expired_before_charge";

/// The test coordinator with an explicit host expiry policy.
#[derive(Clone)]
struct PolicyCoordinator {
    inner: TestCoordinator,
    policy: SubscriptionPeriodExpiryPolicy,
}

#[async_trait]
impl BillingTransactionCoordinator for PolicyCoordinator {
    async fn begin(
        &self,
        subject: BillingEventSubject,
        lock_timeout: Duration,
    ) -> Result<Box<dyn BillingTransaction>, BillingTransactionError> {
        self.inner.begin(subject, lock_timeout).await
    }

    fn subscription_period_expiry_policy(&self) -> SubscriptionPeriodExpiryPolicy {
        self.policy
    }
}

/// A coordinator whose host transaction never begins, forcing every approval
/// application onto the storage-failure compensation path.
struct UnavailableCoordinator;

#[async_trait]
impl BillingTransactionCoordinator for UnavailableCoordinator {
    async fn begin(
        &self,
        _subject: BillingEventSubject,
        _lock_timeout: Duration,
    ) -> Result<Box<dyn BillingTransaction>, BillingTransactionError> {
        Err(BillingTransactionError::new(std::io::Error::other(
            "host recipient lock is unavailable",
        )))
    }

    fn subscription_period_expiry_policy(&self) -> SubscriptionPeriodExpiryPolicy {
        ENFORCE
    }
}

/// Subscription reversals never release a host-charge target.
struct SubscriptionReversalHost;

#[async_trait]
impl ExternalReversalHostStore for SubscriptionReversalHost {
    async fn release(
        &self,
        _connection: &mut PgConnection,
        _release: syrup_rail::ExternalReversalHostChargeRelease,
    ) -> Result<ExternalReversalHostTransitionOutcome, ExternalReversalHostStoreError> {
        panic!("a subscription charge reversal must not release a host-charge target")
    }
}

struct Harness {
    database: TestDatabase,
    account: GatewayAccountFixture,
    gateway: ResolvedGateway,
    scope: BillingScopeId,
    events: Arc<Mutex<Vec<BillingEvent>>>,
    permissive: PolicyCoordinator,
    enforcing: PolicyCoordinator,
}

impl Harness {
    async fn start(project: &str) -> Result<Self, Box<dyn Error>> {
        let database = TestDatabase::start(project).await?;
        let account = create_gateway_account(&database.pool, "nmi").await?;
        let gateway = resolved_gateway(account)?;
        let events = Arc::new(Mutex::new(Vec::new()));
        let inner = TestCoordinator {
            pool: database.pool.clone(),
            events: Arc::clone(&events),
        };
        Ok(Self {
            scope: BillingScopeId::new(account.billing_scope_id),
            permissive: PolicyCoordinator {
                inner: inner.clone(),
                policy: SubscriptionPeriodExpiryPolicy::Disabled,
            },
            enforcing: PolicyCoordinator {
                inner,
                policy: ENFORCE,
            },
            database,
            account,
            gateway,
            events,
        })
    }

    fn pool(&self) -> &PgPool {
        &self.database.pool
    }

    /// Approves a paid trial under `policy` and returns its owner and lifecycle.
    async fn trial(
        &self,
        policy: RenewalFailurePolicy,
        key: &str,
    ) -> Result<(SubscriberId, syrup_rail::SubscriptionId), Box<dyn Error>> {
        let offer = paid_trial_offer_with_policy(policy)?;
        let subscriber_id = SubscriberId::new(Uuid::now_v7());
        let initial = approve_enrollment(
            self.pool(),
            &StaticOfferStore {
                offer: offer.clone(),
            },
            &self.gateway,
            &self.permissive,
            self.account,
            subscriber_id,
            key,
            SubscriptionEnrollmentExpectedTerms::full_price(offer),
            &format!("{key}_initial"),
            &format!("vault_{key}"),
        )
        .await?;
        Ok((
            subscriber_id,
            initial
                .subscription()
                .expect("approved trial creates subscription")
                .id(),
        ))
    }

    /// Gives the lifecycle a one-day cadence whose due period ends `ends_in`
    /// after the database clock, and returns that period's start.
    async fn due_period_ending_in(
        &self,
        subscription_id: syrup_rail::SubscriptionId,
        ends_in: ChronoDuration,
    ) -> Result<DateTime<Utc>, sqlx::Error> {
        let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
            .fetch_one(self.pool())
            .await?;
        let start_at = now + ends_in - ChronoDuration::days(1);
        sqlx::query(
            r#"
            UPDATE billing_subscriptions
            SET recurring_period_kind = 'fixed_days', recurring_period_count = 1,
                current_period_start_at = $2 - interval '7 days',
                current_period_end_at = $2, next_renewal_at = $2,
                next_payment_attempt_at = $2
            WHERE id = $1
            "#,
        )
        .bind(subscription_id.as_uuid())
        .bind(start_at)
        .execute(self.pool())
        .await?;
        Ok(start_at)
    }

    /// Waits until `end_at` is at or before the database clock.
    async fn wait_until_ended(&self, end_at: DateTime<Utc>) -> Result<(), sqlx::Error> {
        loop {
            let ended: bool = sqlx::query_scalar("SELECT $1::timestamptz <= clock_timestamp()")
                .bind(end_at)
                .fetch_one(self.pool())
                .await?;
            if ended {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn renewal(
        &self,
        subscription_id: syrup_rail::SubscriptionId,
        start: DateTime<Utc>,
    ) -> ChargeRenewal {
        ChargeRenewal::new(self.scope, subscription_id, start)
    }

    fn recovery(
        &self,
        subscriber_id: SubscriberId,
        key: &str,
    ) -> Result<RecoverSubscriptionPayment, Box<dyn Error>> {
        Ok(RecoverSubscriptionPayment::new(
            syrup_rail::SubscriptionPaymentContext::new(
                PaymentAttemptId::new(Uuid::now_v7()),
                self.scope,
                subscriber_id,
                GatewayConfigurationId::new(self.account.gateway_configuration_id),
                IdempotencyKey::new(key)?,
                PaymentToken::new("opaque-recovery-token")?,
                BillingContact::new(None, None, Some("recovery@example.test".to_owned()))?,
            ),
            PlanKey::new("identity_pro")?,
        ))
    }

    fn retirement(
        &self,
        subscriber_id: SubscriberId,
        subscription_id: syrup_rail::SubscriptionId,
        start: DateTime<Utc>,
    ) -> RetireExpiredSubscriptionPeriod {
        RetireExpiredSubscriptionPeriod::new(
            self.scope,
            subscriber_id,
            PlanKey::new("identity_pro").expect("valid plan key"),
            subscription_id,
            start,
        )
    }

    /// A billing service whose resolver counts calls and whose gateway panics
    /// on any provider I/O.
    fn service(
        &self,
        coordinator: &PolicyCoordinator,
    ) -> (SubscriptionBillingService, Arc<CountingResolver>) {
        let resolver = Arc::new(CountingResolver {
            gateway: self.gateway.clone(),
            calls: AtomicUsize::new(0),
        });
        let service = SubscriptionBillingService::new(
            self.pool().clone(),
            Arc::new(StaticOfferStore {
                offer: paid_trial_offer().expect("valid offer"),
            }),
            resolver.clone(),
            Arc::new(PermitAdmission),
            Arc::new(coordinator.clone()),
        );
        (service, resolver)
    }

    async fn retire(
        &self,
        command: &RetireExpiredSubscriptionPeriod,
    ) -> Result<SubscriptionPeriodRetirementOutcome, Box<dyn Error>> {
        let mut transaction = self.pool().begin().await?;
        let outcome =
            retire_expired_subscription_period_in_transaction(&mut transaction, command).await?;
        transaction.commit().await?;
        Ok(outcome)
    }

    async fn renewed_events(&self) -> usize {
        self.events
            .lock()
            .await
            .iter()
            .filter(|event| matches!(event, BillingEvent::SubscriptionRenewed { .. }))
            .count()
    }

    async fn cleanup(self) -> Result<(), Box<dyn Error>> {
        self.database.cleanup().await
    }
}

fn suspend_policy() -> Result<RenewalFailurePolicy, Box<dyn Error>> {
    Ok(RenewalFailurePolicy::new(
        DunningSchedule::from_seconds([86_400, 259_200])?,
        DunningExhaustion::MarkUnpaid,
        PastDueAccessPolicy::SuspendImmediately,
    ))
}

fn continue_policy() -> Result<RenewalFailurePolicy, Box<dyn Error>> {
    Ok(RenewalFailurePolicy::new(
        DunningSchedule::from_seconds([86_400, 259_200])?,
        DunningExhaustion::MarkUnpaid,
        PastDueAccessPolicy::ContinueUntilDunningExhausted,
    ))
}

type SubscriptionProjection = (
    String,
    DateTime<Utc>,
    DateTime<Utc>,
    Option<DateTime<Utc>>,
    Option<DateTime<Utc>>,
    String,
);

async fn subscription_projection(
    pool: &PgPool,
    subscription_id: syrup_rail::SubscriptionId,
) -> Result<SubscriptionProjection, sqlx::Error> {
    sqlx::query_as(
        r#"
        SELECT status, current_period_end_at, next_renewal_at,
            next_payment_attempt_at, unpaid_at, past_due_access
        FROM billing_subscriptions WHERE id = $1
        "#,
    )
    .bind(subscription_id.as_uuid())
    .fetch_one(pool)
    .await
}

async fn attempt_disposition(
    pool: &PgPool,
    attempt_id: PaymentAttemptId,
) -> Result<(String, Option<String>), sqlx::Error> {
    sqlx::query_as("SELECT status, resolution_code FROM billing_payment_attempts WHERE id = $1")
        .bind(attempt_id.as_uuid())
        .fetch_one(pool)
        .await
}

async fn attempt_charges(
    pool: &PgPool,
    attempt_id: PaymentAttemptId,
) -> Result<Vec<(Uuid, String, Option<String>, i32)>, sqlx::Error> {
    sqlx::query_as(
        r#"
        SELECT id, progression_state, state_code, amount_cents
        FROM billing_processor_charges WHERE attempt_id = $1 ORDER BY id
        "#,
    )
    .bind(attempt_id.as_uuid())
    .fetch_all(pool)
    .await
}

/// Counts approved provider sales recorded for one subscriber: every
/// processor charge across that subscriber's attempts.
async fn subscriber_sales(pool: &PgPool, subscriber_id: SubscriberId) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        r#"
        SELECT count(*)
        FROM billing_processor_charges AS charges
        JOIN billing_payment_attempts AS attempts ON attempts.id = charges.attempt_id
        WHERE attempts.subscriber_id = $1
        "#,
    )
    .bind(subscriber_id.as_uuid())
    .fetch_one(pool)
    .await
}

/// Makes an attempt old enough for an exact reconciliation claim; every
/// other lifecycle timestamp keeps its order.
async fn age_for_reconciliation(
    pool: &PgPool,
    attempt_id: PaymentAttemptId,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        UPDATE billing_payment_attempts
        SET created_at = created_at - interval '2 hours',
            submitted_at = submitted_at - interval '2 hours',
            updated_at = clock_timestamp() - interval '1 hour'
        WHERE id = $1
        "#,
    )
    .bind(attempt_id.as_uuid())
    .execute(pool)
    .await?;
    Ok(())
}

async fn claimed_ids(harness: &Harness) -> Result<Vec<PaymentAttemptId>, Box<dyn Error>> {
    Ok(claim_exact_reconciliation_attempts(
        harness.pool(),
        GatewayAccountId::new(harness.account.gateway_account_id),
    )
    .await?
    .into_iter()
    .map(|attempt| attempt.identity().attempt_id())
    .collect())
}

async fn attest_refund(
    harness: &Harness,
    charge_id: Uuid,
    transaction_id: &str,
) -> Result<ExternalReversalAttestationOutcome, Box<dyn Error>> {
    Ok(attest_external_reversal(
        harness.pool(),
        &SubscriptionReversalHost,
        ProcessorChargeId::new(charge_id),
        ActorId::new(Uuid::from_u128(77)),
        ExternalReversalKind::Refund,
        &GatewayTransactionId::new(transaction_id)?,
        &ExternalReversalReason::new("full refund verified in the merchant portal")?,
    )
    .await?)
}

#[tokio::test]
async fn late_automatic_renewal_approval_is_parked_reversed_and_retired_without_catch_up()
-> Result<(), Box<dyn Error>> {
    let harness = Harness::start("pe_late_renew").await?;
    let result = async {
        let (subscriber_id, subscription_id) =
            harness.trial(suspend_policy()?, "late_renewal").await?;
        let start = harness
            .due_period_ending_in(subscription_id, ChronoDuration::seconds(3))
            .await?;
        // Submitted while the period was valid; the approval arrives late.
        let reservation = reserve_and_admit_renewal(
            harness.pool(),
            &harness.gateway,
            harness.renewal(subscription_id, start),
        )
        .await?;
        harness
            .wait_until_ended(*reservation.period().end_at())
            .await?;
        let before = subscription_projection(harness.pool(), subscription_id).await?;

        let late = apply_subscription_renewal_gateway_outcome(
            harness.pool(),
            &harness.enforcing,
            &reservation,
            &approved_outcome("late_renewal_sale"),
        )
        .await?;
        let attempt_id = reservation.identity().attempt_id();
        assert!(
            late.subscription().is_none(),
            "expired approval must not apply"
        );
        assert_eq!(
            attempt_disposition(harness.pool(), attempt_id).await?,
            (
                "review_required".to_owned(),
                Some(EXPIRED_APPROVAL.to_owned())
            )
        );
        let charges = attempt_charges(harness.pool(), attempt_id).await?;
        assert_eq!(charges.len(), 1);
        assert_eq!(
            (charges[0].1.as_str(), charges[0].2.as_deref(), charges[0].3),
            ("external_reversal_required", Some(EXPIRED_APPROVAL), 2_900)
        );
        assert_eq!(
            subscription_projection(harness.pool(), subscription_id).await?,
            before
        );
        assert_eq!(harness.renewed_events().await, 0);
        assert_eq!(subscriber_sales(harness.pool(), subscriber_id).await?, 2);

        // The parked disposition is policy-independent and stable: ordinary
        // reconciliation never re-applies it, even after the policy is off.
        let replay = apply_reconciled_subscription_renewal_gateway_outcome(
            harness.pool(),
            &harness.permissive,
            harness.scope,
            attempt_id,
            &approved_outcome("late_renewal_sale"),
        )
        .await?;
        assert!(replay.subscription().is_none());
        assert_eq!(
            subscription_projection(harness.pool(), subscription_id).await?,
            before
        );
        age_for_reconciliation(harness.pool(), attempt_id).await?;
        assert!(!claimed_ids(&harness).await?.contains(&attempt_id));

        // Discovery and the already-queued renewal make no further sale while
        // money is unresolved, and the obsolete cycle cannot be retired.
        let (service, resolver) = harness.service(&harness.enforcing);
        let queued = harness.renewal(subscription_id, start);
        assert!(matches!(
            service.renew(queued).await?,
            SubscriptionRenewalOutcome::Noop
        ));
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 0);
        assert!(matches!(
            harness
                .retire(&harness.retirement(subscriber_id, subscription_id, start))
                .await?,
            SubscriptionPeriodRetirementOutcome::UnresolvedPayment
        ));
        assert_eq!(
            subscription_projection(harness.pool(), subscription_id).await?,
            before
        );
        assert_eq!(subscriber_sales(harness.pool(), subscriber_id).await?, 2);

        // The operator attests a verified full refund; the expiry reason is
        // retained as the prior resolution and replay is exact.
        let charge_id = charges[0].0;
        let ExternalReversalAttestationOutcome::Attested {
            attestation,
            attempt,
        } = attest_refund(&harness, charge_id, "late_renewal_sale").await?
        else {
            return Err("expected attestation".into());
        };
        assert_eq!(attestation.prior_resolution_code(), EXPIRED_APPROVAL);
        assert_eq!(attempt.status(), PaymentAttemptStatus::Failed);
        assert_eq!(
            attempt.state().resolution_code(),
            Some(PaymentResolutionCode::ProcessorChargeExternallyRefunded)
        );
        assert!(matches!(
            attest_refund(&harness, charge_id, "late_renewal_sale").await?,
            ExternalReversalAttestationOutcome::Replayed { .. }
        ));
        let reversed = attempt_charges(harness.pool(), attempt_id).await?;
        assert_eq!(
            (reversed[0].1.as_str(), reversed[0].2.as_deref()),
            ("externally_reversed", Some(EXPIRED_APPROVAL))
        );

        // A later observation of the original approval is stable terminal
        // history: no error, no review reopening, no charge regression.
        let observed = apply_reconciled_subscription_renewal_gateway_outcome(
            harness.pool(),
            &harness.enforcing,
            harness.scope,
            attempt_id,
            &approved_outcome("late_renewal_sale"),
        )
        .await?;
        assert!(observed.subscription().is_none());
        assert_eq!(
            attempt_disposition(harness.pool(), attempt_id).await?,
            (
                "failed".to_owned(),
                Some("processor_charge_externally_refunded".to_owned())
            )
        );
        assert_eq!(attempt_charges(harness.pool(), attempt_id).await?, reversed);
        age_for_reconciliation(harness.pool(), attempt_id).await?;
        assert!(!claimed_ids(&harness).await?.contains(&attempt_id));

        // With the money resolved, the queued renewal retires the cycle.
        harness.events.lock().await.clear();
        assert!(matches!(
            service.renew(queued).await?,
            SubscriptionRenewalOutcome::Noop
        ));
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 0);
        let (status, period_end, next_renewal_at, next_payment, unpaid_at, _) =
            subscription_projection(harness.pool(), subscription_id).await?;
        assert_eq!(status, "unpaid");
        assert_eq!((period_end, next_renewal_at), (before.1, before.2));
        assert_eq!(next_payment, None);
        let unpaid_at = unpaid_at.expect("retired lifecycle records unpaid_at");
        let events = harness.events.lock().await.clone();
        assert_eq!(
            events,
            vec![BillingEvent::SubscriptionPeriodExpired {
                subscription_id,
                plan_key: PlanKey::new("identity_pro")?,
                period: reservation.period().clone(),
                ended_at: unpaid_at,
                // The lifecycle was still active, so retirement ends access.
                access_ends_at: unpaid_at,
            }]
        );

        // Replays and the already-queued job remain no-ops.
        assert!(matches!(
            harness
                .retire(&harness.retirement(subscriber_id, subscription_id, start))
                .await?,
            SubscriptionPeriodRetirementOutcome::AlreadyUnpaid(_)
        ));
        assert!(matches!(
            service.renew(queued).await?,
            SubscriptionRenewalOutcome::Noop
        ));
        assert_eq!(harness.events.lock().await.len(), 1);
        assert_eq!(subscriber_sales(harness.pool(), subscriber_id).await?, 2);
        assert_eq!(harness.renewed_events().await, 0);
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = harness.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn late_reconciled_recovery_approval_keeps_suspension_and_retires_after_reversal()
-> Result<(), Box<dyn Error>> {
    let harness = Harness::start("pe_late_recover").await?;
    let result = async {
        let (subscriber_id, subscription_id) =
            harness.trial(suspend_policy()?, "late_recovery").await?;
        let start = harness
            .due_period_ending_in(subscription_id, ChronoDuration::seconds(4))
            .await?;
        let (_, first_failure_at) = decline_due_renewal(
            harness.pool(),
            &harness.gateway,
            &harness.permissive,
            harness.scope,
            subscription_id,
            start,
            "late_recovery_decline",
        )
        .await?;
        let command = harness.recovery(subscriber_id, "late-recovery")?;
        let reservation =
            reserve_and_admit_recovery(harness.pool(), &harness.gateway, &command).await?;
        harness
            .wait_until_ended(*reservation.period().end_at())
            .await?;
        let before = subscription_projection(harness.pool(), subscription_id).await?;
        assert_eq!(before.0, "past_due");

        let late = apply_reconciled_subscription_recovery_gateway_outcome(
            harness.pool(),
            &harness.enforcing,
            harness.scope,
            reservation.identity().attempt_id(),
            &approved_outcome("late_recovery_sale"),
        )
        .await?;
        assert!(late.subscription().is_none());
        let attempt_id = reservation.identity().attempt_id();
        assert_eq!(
            attempt_disposition(harness.pool(), attempt_id).await?,
            (
                "review_required".to_owned(),
                Some(EXPIRED_APPROVAL.to_owned())
            )
        );
        assert_eq!(
            subscription_projection(harness.pool(), subscription_id).await?,
            before
        );
        assert!(matches!(
            entitlement(
                harness.pool(),
                &EntitlementQuery::new(harness.scope, subscriber_id, PlanKey::new("identity_pro")?),
            )
            .await?,
            Entitlement::PastDue {
                access: syrup_rail::PastDueAccess::Suspended,
                ..
            }
        ));
        assert_eq!(harness.renewed_events().await, 0);
        // Foreground replay of the same approval is equally stable.
        let replay = apply_subscription_recovery_gateway_outcome(
            harness.pool(),
            &harness.permissive,
            &reservation,
            &approved_outcome("late_recovery_sale"),
        )
        .await?;
        assert!(replay.subscription().is_none());
        assert_eq!(
            subscription_projection(harness.pool(), subscription_id).await?,
            before
        );

        let charge_id = attempt_charges(harness.pool(), attempt_id).await?[0].0;
        assert!(matches!(
            attest_refund(&harness, charge_id, "late_recovery_sale").await?,
            ExternalReversalAttestationOutcome::Attested { .. }
        ));
        let outcome = harness
            .retire(&harness.retirement(subscriber_id, subscription_id, start))
            .await?;
        let SubscriptionPeriodRetirementOutcome::Retired {
            event,
            rejected_attempt_ids,
            ..
        } = outcome
        else {
            return Err(format!("expected retirement, got {outcome:?}").into());
        };
        assert!(rejected_attempt_ids.is_empty());
        // Suspension already ended access at the first definitive failure.
        assert!(matches!(
            event,
            BillingEvent::SubscriptionPeriodExpired { access_ends_at, .. }
                if access_ends_at == first_failure_at
        ));
        assert_eq!(
            subscription_projection(harness.pool(), subscription_id)
                .await?
                .0,
            "unpaid"
        );
        // Only the trial sale and the one reversed recovery sale exist; the
        // declined renewal recorded no sale.
        assert_eq!(subscriber_sales(harness.pool(), subscriber_id).await?, 2);
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = harness.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn late_restart_approval_creates_no_successor_until_reversal_clears_the_fence()
-> Result<(), Box<dyn Error>> {
    let harness = Harness::start("pe_late_start").await?;
    let result = async {
        let offer = immediate_offer(
            PlanKey::new("identity_pro")?,
            ChargeAmount::new(2_699, CurrencyCode::new("USD")?)?,
        );
        let offers = StaticOfferStore {
            offer: offer.clone(),
        };
        let subscriber_id = SubscriberId::new(Uuid::now_v7());
        let enroll = |key: &'static str| {
            syrup_rail::EnrollSubscription::new(
                syrup_rail::SubscriptionPaymentContext::new(
                    PaymentAttemptId::new(Uuid::now_v7()),
                    harness.scope,
                    subscriber_id,
                    GatewayConfigurationId::new(harness.account.gateway_configuration_id),
                    IdempotencyKey::new(key).expect("valid key"),
                    PaymentToken::new("opaque-restart-token").expect("valid token"),
                    BillingContact::new(None, None, Some("restart@example.test".to_owned()))
                        .expect("valid contact"),
                ),
                SubscriptionEnrollmentExpectedTerms::full_price(offer.clone()),
            )
        };
        let reservation = SubscriptionEnrollmentReservation::from_command(
            &enroll("late-restart"),
            &harness.gateway,
            GatewayAccountMode::Live,
        )?;
        let mut transaction = harness.pool().begin().await?;
        assert!(matches!(
            reserve_subscription_enrollment_in_transaction(&mut transaction, &offers, &reservation)
                .await?,
            SubscriptionEnrollmentReservationOutcome::Reserved(_)
        ));
        transaction.commit().await?;
        assert!(matches!(
            admit_subscription_enrollment_submission(harness.pool(), &offers, &reservation).await?,
            SubscriptionEnrollmentAdmissionOutcome::Admitted(_)
        ));
        // The provider approval arrives after the calendar month bought from
        // submission has already ended.
        let attempt_id = reservation.identity().attempt_id();
        sqlx::query(
            r#"
            UPDATE billing_payment_attempts
            SET created_at = created_at - interval '40 days',
                submitted_at = submitted_at - interval '40 days'
            WHERE id = $1
            "#,
        )
        .bind(attempt_id.as_uuid())
        .execute(harness.pool())
        .await?;

        let late = apply_subscription_enrollment_gateway_outcome(
            harness.pool(),
            &harness.enforcing,
            &reservation,
            &approved_outcome("late_restart_sale"),
        )
        .await?;
        assert!(late.subscription().is_none());
        assert_eq!(
            attempt_disposition(harness.pool(), attempt_id).await?,
            (
                "review_required".to_owned(),
                Some(EXPIRED_APPROVAL.to_owned())
            )
        );
        let lifecycles: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM billing_subscriptions WHERE subscriber_id = $1",
        )
        .bind(subscriber_id.as_uuid())
        .fetch_one(harness.pool())
        .await?;
        assert_eq!(
            lifecycles, 0,
            "an expired restart must not create a successor"
        );
        assert!(
            !harness
                .events
                .lock()
                .await
                .iter()
                .any(|event| matches!(event, BillingEvent::SubscriptionStarted { .. }))
        );

        // Repeated reconciliation keeps the parked disposition.
        let replay = apply_reconciled_subscription_enrollment_gateway_outcome(
            harness.pool(),
            &harness.permissive,
            harness.scope,
            attempt_id,
            &approved_outcome("late_restart_sale"),
        )
        .await?;
        assert!(replay.subscription().is_none());
        age_for_reconciliation(harness.pool(), attempt_id).await?;
        assert!(!claimed_ids(&harness).await?.contains(&attempt_id));

        // Unresolved money blocks another restart until the reversal is attested.
        let second = SubscriptionEnrollmentReservation::from_command(
            &enroll("second-restart"),
            &harness.gateway,
            GatewayAccountMode::Live,
        )?;
        let mut transaction = harness.pool().begin().await?;
        let blocked =
            reserve_subscription_enrollment_in_transaction(&mut transaction, &offers, &second)
                .await?;
        transaction.rollback().await?;
        assert!(
            !matches!(
                blocked,
                SubscriptionEnrollmentReservationOutcome::Reserved(_)
            ),
            "{blocked:?}"
        );
        let charge_id = attempt_charges(harness.pool(), attempt_id).await?[0].0;
        let ExternalReversalAttestationOutcome::Attested { attestation, .. } =
            attest_refund(&harness, charge_id, "late_restart_sale").await?
        else {
            return Err("expected attestation".into());
        };
        assert_eq!(attestation.prior_resolution_code(), EXPIRED_APPROVAL);
        let mut transaction = harness.pool().begin().await?;
        assert!(matches!(
            reserve_subscription_enrollment_in_transaction(&mut transaction, &offers, &second)
                .await?,
            SubscriptionEnrollmentReservationOutcome::Reserved(_)
        ));
        transaction.commit().await?;
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = harness.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn expired_automatic_retry_and_queued_job_make_zero_sales_and_retire_once()
-> Result<(), Box<dyn Error>> {
    let harness = Harness::start("pe_queued_retry").await?;
    let result = async {
        let (subscriber_id, subscription_id) =
            harness.trial(continue_policy()?, "queued_retry").await?;
        let start = harness
            .due_period_ending_in(subscription_id, ChronoDuration::seconds(3))
            .await?;
        decline_due_renewal(
            harness.pool(),
            &harness.gateway,
            &harness.permissive,
            harness.scope,
            subscription_id,
            start,
            "queued_retry_decline",
        )
        .await?;
        make_retry_due(harness.pool(), subscription_id).await?;
        // A retry prepared before expiry stays unsubmitted in the queue.
        let mut transaction = harness.pool().begin().await?;
        let SubscriptionRenewalReservationOutcome::Reserved(prepared, _) =
            reserve_subscription_renewal_in_transaction(
                &mut transaction,
                harness.renewal(subscription_id, start),
                &harness.gateway,
                GatewayAccountMode::Live,
            )
            .await?
        else {
            return Err("expected a prepared retry".into());
        };
        transaction.commit().await?;
        harness
            .wait_until_ended(*prepared.period().end_at())
            .await?;
        let sales_before = subscriber_sales(harness.pool(), subscriber_id).await?;

        // Final admission of the prepared retry refuses the expired period
        // with a typed code that is not dunning history.
        let admission =
            crate::enrollment_application::admit_subscription_renewal_submission_with_policy(
                harness.pool(),
                &prepared,
                ENFORCE,
            )
            .await?;
        let SubscriptionRenewalAdmissionOutcome::Rejected { attempt, reason } = admission else {
            return Err(format!("expected rejection, got {admission:?}").into());
        };
        assert_eq!(
            reason,
            SubscriptionRenewalSubmissionRejection::BillingPeriodExpired
        );
        assert_eq!(attempt.status(), PaymentAttemptStatus::Failed);
        assert_eq!(
            attempt.state().resolution_code(),
            Some(PaymentResolutionCode::SubscriptionPeriodExpiredBeforeCharge)
        );
        assert_eq!(
            subscription_projection(harness.pool(), subscription_id)
                .await?
                .0,
            "past_due"
        );

        // Reservation under the policy refuses the expired period outright.
        let mut transaction = harness.pool().begin().await?;
        assert_eq!(
            crate::attempts::reserve_subscription_renewal_with_policy_in_transaction(
                &mut transaction,
                harness.renewal(subscription_id, start),
                &harness.gateway,
                GatewayAccountMode::Live,
                ENFORCE,
            )
            .await?,
            SubscriptionRenewalReservationOutcome::Rejected(
                SubscriptionRenewalReservationRejection::BillingPeriodExpired
            )
        );
        transaction.rollback().await?;

        // Direct execution of the old queued job retires the cycle without
        // gateway work; the dunning access window closes at retirement.
        let (service, resolver) = harness.service(&harness.enforcing);
        assert!(matches!(
            service
                .renew(harness.renewal(subscription_id, start))
                .await?,
            SubscriptionRenewalOutcome::Noop
        ));
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 0);
        let (status, _, next_renewal_at, next_payment, unpaid_at, _) =
            subscription_projection(harness.pool(), subscription_id).await?;
        assert_eq!(
            (status.as_str(), next_renewal_at, next_payment),
            ("unpaid", start, None)
        );
        let unpaid_at = unpaid_at.expect("retired lifecycle records unpaid_at");
        assert!(harness.events.lock().await.iter().any(|event| matches!(
            event,
            BillingEvent::SubscriptionPeriodExpired { access_ends_at, ended_at, .. }
                if *access_ends_at == unpaid_at && *ended_at == unpaid_at
        )));
        assert!(matches!(
            service
                .renew(harness.renewal(subscription_id, start))
                .await?,
            SubscriptionRenewalOutcome::Noop
        ));
        assert_eq!(
            subscriber_sales(harness.pool(), subscriber_id).await?,
            sales_before
        );
        assert_eq!(
            harness
                .events
                .lock()
                .await
                .iter()
                .filter(|event| matches!(event, BillingEvent::SubscriptionPeriodExpired { .. }))
                .count(),
            1
        );
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = harness.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn disabled_policy_preserves_historical_expired_period_collection()
-> Result<(), Box<dyn Error>> {
    let harness = Harness::start("pe_disabled").await?;
    let result = async {
        let (_, subscription_id) = harness.trial(suspend_policy()?, "disabled_policy").await?;
        let start = harness
            .due_period_ending_in(subscription_id, ChronoDuration::hours(-2))
            .await?;
        let reservation = reserve_and_admit_renewal(
            harness.pool(),
            &harness.gateway,
            harness.renewal(subscription_id, start),
        )
        .await?;
        let applied = apply_subscription_renewal_gateway_outcome(
            harness.pool(),
            &harness.permissive,
            &reservation,
            &approved_outcome("historical_expired_sale"),
        )
        .await?;
        assert!(applied.subscription().is_some());
        let (status, period_end, next_renewal_at, ..) =
            subscription_projection(harness.pool(), subscription_id).await?;
        assert_eq!(status, "active");
        assert_eq!(period_end, *reservation.period().end_at());
        assert_eq!(next_renewal_at, *reservation.period().end_at());
        assert_eq!(harness.renewed_events().await, 1);
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = harness.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn recovery_admission_commits_the_callers_transaction_before_yielding_authority()
-> Result<(), Box<dyn Error>> {
    let harness = Harness::start("pe_recover_admit").await?;
    let result = async {
        sqlx::raw_sql(
            r#"
            CREATE TABLE host_recovery_operations (
                id uuid PRIMARY KEY,
                parent_id uuid REFERENCES host_recovery_operations (id)
                    DEFERRABLE INITIALLY DEFERRED
            )
            "#,
        )
        .execute(harness.pool())
        .await?;
        let (subscriber_id, subscription_id) =
            harness.trial(suspend_policy()?, "atomic_admission").await?;
        let start = harness
            .due_period_ending_in(subscription_id, ChronoDuration::hours(12))
            .await?;
        decline_due_renewal(
            harness.pool(),
            &harness.gateway,
            &harness.permissive,
            harness.scope,
            subscription_id,
            start,
            "atomic_admission_decline",
        )
        .await?;
        let command = harness.recovery(subscriber_id, "atomic-recovery")?;
        let mut transaction = harness.pool().begin().await?;
        let syrup_rail::SubscriptionRecoveryReservationOutcome::Reserved(reservation, _) =
            reserve_subscription_recovery_in_transaction(
                &mut transaction,
                &command,
                &harness.gateway,
                GatewayAccountMode::Live,
            )
            .await?
        else {
            return Err("expected a recovery reservation".into());
        };
        transaction.commit().await?;
        let attempt_id = reservation.identity().attempt_id();
        let submitted = |pool: PgPool| async move {
            sqlx::query_scalar::<_, Option<DateTime<Utc>>>(
                "SELECT submitted_at FROM billing_payment_attempts WHERE id = $1",
            )
            .bind(attempt_id.as_uuid())
            .fetch_one(&pool)
            .await
        };

        // A host write whose deferred constraint fails at commit: the whole
        // transaction rolls back and no authority is produced.
        let mut transaction = harness.pool().begin().await?;
        sqlx::query("INSERT INTO host_recovery_operations (id, parent_id) VALUES ($1, $2)")
            .bind(Uuid::now_v7())
            .bind(Uuid::now_v7())
            .execute(&mut *transaction)
            .await?;
        let failed_commit = admit_subscription_recovery_submission_with_transaction(
            transaction,
            &reservation,
            ENFORCE,
        )
        .await;
        assert!(
            matches!(
                failed_commit,
                Err(SubscriptionEnrollmentApplicationError::Sql(_))
            ),
            "{failed_commit:?}"
        );
        assert_eq!(submitted(harness.pool().clone()).await?, None);
        let host_rows: i64 = sqlx::query_scalar("SELECT count(*) FROM host_recovery_operations")
            .fetch_one(harness.pool())
            .await?;
        assert_eq!(host_rows, 0);

        // A nested transaction would commit only a savepoint, so it is
        // rejected before admission and yields no authority.
        let mut outer = harness.pool().begin().await?;
        let nested = sqlx::Connection::begin(&mut *outer).await?;
        let nested_admission =
            admit_subscription_recovery_submission_with_transaction(nested, &reservation, ENFORCE)
                .await;
        assert!(
            matches!(
                nested_admission,
                Err(SubscriptionEnrollmentApplicationError::InvalidState(_))
            ),
            "{nested_admission:?}"
        );
        outer.rollback().await?;
        assert_eq!(submitted(harness.pool().clone()).await?, None);

        // The same caller transaction publishes the host's own write and the
        // admission together, then yields one-shot authority.
        let operation_id = Uuid::now_v7();
        let mut transaction = harness.pool().begin().await?;
        sqlx::query("INSERT INTO host_recovery_operations (id) VALUES ($1)")
            .bind(operation_id)
            .execute(&mut *transaction)
            .await?;
        let admitted = admit_subscription_recovery_submission_with_transaction(
            transaction,
            &reservation,
            ENFORCE,
        )
        .await?;
        let SubscriptionRecoveryAdmissionOutcome::Admitted(admission) = admitted else {
            return Err(format!("expected admission, got {admitted:?}").into());
        };
        assert_eq!(admission.attempt().identity().attempt_id(), attempt_id);
        assert!(submitted(harness.pool().clone()).await?.is_some());
        let host_rows: i64 =
            sqlx::query_scalar("SELECT count(*) FROM host_recovery_operations WHERE id = $1")
                .bind(operation_id)
                .fetch_one(harness.pool())
                .await?;
        assert_eq!(host_rows, 1);

        // A duplicate admission yields no second authority.
        assert!(matches!(
            admit_subscription_recovery_submission_with_transaction(
                harness.pool().begin().await?,
                &reservation,
                ENFORCE,
            )
            .await?,
            SubscriptionRecoveryAdmissionOutcome::AlreadyAdmitted(_)
        ));
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = harness.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn recovery_admission_refuses_an_expired_period_only_under_the_policy()
-> Result<(), Box<dyn Error>> {
    let harness = Harness::start("pe_recover_exp").await?;
    let result = async {
        let mut reservations = Vec::new();
        for key in ["expired_recovery_enforced", "expired_recovery_disabled"] {
            let (subscriber_id, subscription_id) = harness.trial(suspend_policy()?, key).await?;
            let start = harness
                .due_period_ending_in(subscription_id, ChronoDuration::seconds(3))
                .await?;
            decline_due_renewal(
                harness.pool(),
                &harness.gateway,
                &harness.permissive,
                harness.scope,
                subscription_id,
                start,
                &format!("{key}_decline"),
            )
            .await?;
            let command = harness.recovery(subscriber_id, &key.replace('_', "-"))?;
            let mut transaction = harness.pool().begin().await?;
            let syrup_rail::SubscriptionRecoveryReservationOutcome::Reserved(reservation, _) =
                reserve_subscription_recovery_in_transaction(
                    &mut transaction,
                    &command,
                    &harness.gateway,
                    GatewayAccountMode::Live,
                )
                .await?
            else {
                return Err("expected a recovery reservation".into());
            };
            transaction.commit().await?;
            reservations.push(*reservation);
        }
        for reservation in &reservations {
            harness
                .wait_until_ended(*reservation.period().end_at())
                .await?;
        }

        let enforced = admit_subscription_recovery_submission_with_transaction(
            harness.pool().begin().await?,
            &reservations[0],
            ENFORCE,
        )
        .await?;
        let SubscriptionRecoveryAdmissionOutcome::Rejected { attempt, reason } = enforced else {
            return Err(format!("expected rejection, got {enforced:?}").into());
        };
        assert_eq!(
            reason,
            SubscriptionRecoverySubmissionRejection::BillingPeriodExpired
        );
        assert_eq!(
            attempt_disposition(harness.pool(), attempt.identity().attempt_id()).await?,
            ("failed".to_owned(), Some(EXPIRED_BEFORE_CHARGE.to_owned()))
        );

        // The historical pool wrapper is Disabled admission.
        assert!(matches!(
            admit_subscription_recovery_submission(harness.pool(), &reservations[1]).await?,
            SubscriptionRecoveryAdmissionOutcome::Admitted(_)
        ));
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = harness.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn retirement_rechecks_target_period_expiry_and_money_under_locks()
-> Result<(), Box<dyn Error>> {
    let harness = Harness::start("pe_retire_gate").await?;
    let result = async {
        let (subscriber_id, subscription_id) =
            harness.trial(suspend_policy()?, "retire_gates").await?;
        let future_start = harness
            .due_period_ending_in(subscription_id, ChronoDuration::hours(6))
            .await?;
        let other_owner = SubscriberId::new(Uuid::now_v7());
        assert!(matches!(
            harness
                .retire(&harness.retirement(other_owner, subscription_id, future_start))
                .await?,
            SubscriptionPeriodRetirementOutcome::NotFound
        ));
        assert!(matches!(
            harness
                .retire(&harness.retirement(subscriber_id, subscription_id, future_start))
                .await?,
            SubscriptionPeriodRetirementOutcome::NotExpired { period }
                if *period.start_at() == future_start
        ));

        let start = harness
            .due_period_ending_in(subscription_id, ChronoDuration::seconds(3))
            .await?;
        assert!(matches!(
            harness
                .retire(&harness.retirement(subscriber_id, subscription_id, future_start))
                .await?,
            SubscriptionPeriodRetirementOutcome::PeriodChanged(_)
        ));

        // A submitted renewal with no outcome is unresolved money.
        let submitted =
            reserve_and_admit_renewal(harness.pool(), &harness.gateway, harness.renewal(subscription_id, start))
                .await?;
        harness.wait_until_ended(*submitted.period().end_at()).await?;
        assert!(matches!(
            harness
                .retire(&harness.retirement(subscriber_id, subscription_id, start))
                .await?,
            SubscriptionPeriodRetirementOutcome::UnresolvedPayment
        ));

        // A definitive decline does not block retirement, and remaining
        // unsubmitted authority for the period is rejected atomically.
        apply_subscription_renewal_gateway_outcome(
            harness.pool(),
            &harness.permissive,
            &submitted,
            &declined_outcome("retire_gates_decline"),
        )
        .await?;
        sqlx::query(
            "UPDATE billing_subscriptions SET next_payment_attempt_at = next_renewal_at WHERE id = $1",
        )
        .bind(subscription_id.as_uuid())
        .execute(harness.pool())
        .await?;
        let mut transaction = harness.pool().begin().await?;
        let SubscriptionRenewalReservationOutcome::Reserved(prepared, _) =
            reserve_subscription_renewal_in_transaction(
                &mut transaction,
                harness.renewal(subscription_id, start),
                &harness.gateway,
                GatewayAccountMode::Live,
            )
            .await?
        else {
            return Err("expected an unsubmitted renewal".into());
        };
        transaction.commit().await?;
        let outcome = harness
            .retire(&harness.retirement(subscriber_id, subscription_id, start))
            .await?;
        let SubscriptionPeriodRetirementOutcome::Retired {
            subscription,
            rejected_attempt_ids,
            ..
        } = outcome
        else {
            return Err(format!("expected retirement, got {outcome:?}").into());
        };
        assert_eq!(subscription.status(), SubscriptionStatus::Unpaid);
        assert_eq!(*subscription.next_renewal_at(), start);
        assert_eq!(rejected_attempt_ids, vec![prepared.identity().attempt_id()]);
        assert_eq!(
            attempt_disposition(harness.pool(), prepared.identity().attempt_id()).await?,
            ("failed".to_owned(), Some(EXPIRED_BEFORE_CHARGE.to_owned()))
        );
        assert_eq!(
            attempt_disposition(harness.pool(), submitted.identity().attempt_id()).await?,
            ("declined".to_owned(), None)
        );
        // The retired prepared attempt can never be admitted afterwards.
        assert!(matches!(
            admit_subscription_renewal_submission(harness.pool(), &prepared).await?,
            SubscriptionRenewalAdmissionOutcome::AlreadyAdmitted(_)
                | SubscriptionRenewalAdmissionOutcome::Rejected { .. }
        ));
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = harness.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn retirement_never_alters_a_canceled_lifecycle() -> Result<(), Box<dyn Error>> {
    let harness = Harness::start("pe_retire_cancel").await?;
    let result = async {
        let (subscriber_id, subscription_id) =
            harness.trial(suspend_policy()?, "retire_canceled").await?;
        let start = harness
            .due_period_ending_in(subscription_id, ChronoDuration::seconds(-30))
            .await?;
        let mut transaction = harness.pool().begin().await?;
        assert!(matches!(
            cancel_subscription_in_transaction(
                &mut transaction,
                &CancelSubscription::new(
                    harness.scope,
                    subscriber_id,
                    PlanKey::new("identity_pro")?
                ),
            )
            .await?,
            CancelSubscriptionOutcome::Canceled { .. }
        ));
        transaction.commit().await?;
        let before = subscription_projection(harness.pool(), subscription_id).await?;
        assert!(matches!(
            harness
                .retire(&harness.retirement(subscriber_id, subscription_id, start))
                .await?,
            SubscriptionPeriodRetirementOutcome::Canceled(_)
        ));
        let (service, resolver) = harness.service(&harness.enforcing);
        assert!(matches!(
            service
                .renew(harness.renewal(subscription_id, start))
                .await?,
            SubscriptionRenewalOutcome::Noop
        ));
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            subscription_projection(harness.pool(), subscription_id).await?,
            before
        );
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = harness.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn past_due_access_policy_change_is_idempotent_and_reaches_entitlement()
-> Result<(), Box<dyn Error>> {
    let owned = Harness::start("pe_policy").await?;
    let harness = &owned;
    let result = async {
        let plan = || PlanKey::new("identity_pro").expect("valid plan key");
        let change = |subscriber_id, subscription_id, policy| {
            ChangeSubscriptionPastDueAccess::new(harness.scope, subscriber_id, plan(), subscription_id, policy)
        };
        let apply = |command: ChangeSubscriptionPastDueAccess| async move {
            let mut transaction = harness.pool().begin().await?;
            let outcome =
                change_subscription_past_due_access_in_transaction(&mut transaction, &command).await?;
            transaction.commit().await?;
            Ok::<_, Box<dyn Error>>(outcome)
        };

        // Active lifecycle: changes once, then replays as unchanged.
        let (active_owner, active_id) = harness.trial(continue_policy()?, "policy_active").await?;
        let before = subscription_projection(harness.pool(), active_id).await?;
        assert!(matches!(
            apply(change(active_owner, active_id, PastDueAccessPolicy::SuspendImmediately)).await?,
            SubscriptionPastDueAccessChangeOutcome::Changed {
                previous: PastDueAccessPolicy::ContinueUntilDunningExhausted,
                ..
            }
        ));
        assert!(matches!(
            apply(change(active_owner, active_id, PastDueAccessPolicy::SuspendImmediately)).await?,
            SubscriptionPastDueAccessChangeOutcome::Unchanged(_)
        ));
        let after = subscription_projection(harness.pool(), active_id).await?;
        assert_eq!(after.5, "suspend_immediately");
        assert_eq!((after.0, after.1, after.2, after.3, after.4), (before.0, before.1, before.2, before.3, before.4));
        assert!(matches!(
            apply(change(SubscriberId::new(Uuid::now_v7()), active_id, PastDueAccessPolicy::ContinueUntilDunningExhausted)).await?,
            SubscriptionPastDueAccessChangeOutcome::NotFound
        ));

        // Past-due lifecycle in dunning: access is open, then suspended at
        // once, and the next failure reports the first failure boundary.
        let (due_owner, due_id) = harness.trial(continue_policy()?, "policy_past_due").await?;
        let start = harness
            .due_period_ending_in(due_id, ChronoDuration::hours(12))
            .await?;
        let (_, first_failure_at) = decline_due_renewal(
            harness.pool(),
            &harness.gateway,
            &harness.permissive,
            harness.scope,
            due_id,
            start,
            "policy_past_due_decline",
        )
        .await?;
        let query = EntitlementQuery::new(harness.scope, due_owner, plan());
        assert!(matches!(
            entitlement(harness.pool(), &query).await?,
            Entitlement::PastDue {
                access: syrup_rail::PastDueAccess::AllowedDuringDunning,
                ..
            }
        ));
        let retry_before = subscription_projection(harness.pool(), due_id).await?.3;
        assert!(matches!(
            apply(change(due_owner, due_id, PastDueAccessPolicy::SuspendImmediately)).await?,
            SubscriptionPastDueAccessChangeOutcome::Changed { .. }
        ));
        assert!(matches!(
            entitlement(harness.pool(), &query).await?,
            Entitlement::PastDue {
                access: syrup_rail::PastDueAccess::Suspended,
                ..
            }
        ));
        assert_eq!(subscription_projection(harness.pool(), due_id).await?.3, retry_before);
        make_retry_due(harness.pool(), due_id).await?;
        harness.events.lock().await.clear();
        decline_due_renewal(
            harness.pool(),
            &harness.gateway,
            &harness.permissive,
            harness.scope,
            due_id,
            start,
            "policy_past_due_retry",
        )
        .await?;
        assert!(harness.events.lock().await.iter().any(|event| matches!(
            event,
            BillingEvent::SubscriptionPaymentFailed {
                access: SubscriptionPaymentFailureAccess::Ended { access_ended_at },
                ..
            } if *access_ended_at == first_failure_at
        )));

        // Terminal lifecycles keep their historical terms.
        let (unpaid_owner, unpaid_id) = harness.trial(continue_policy()?, "policy_unpaid").await?;
        let (canceled_owner, canceled_id) =
            harness.trial(continue_policy()?, "policy_canceled").await?;
        sqlx::query(
            "UPDATE billing_subscriptions SET status = 'unpaid', unpaid_at = clock_timestamp(), next_payment_attempt_at = NULL WHERE id = $1",
        )
        .bind(unpaid_id.as_uuid())
        .execute(harness.pool())
        .await?;
        let mut transaction = harness.pool().begin().await?;
        cancel_subscription_in_transaction(
            &mut transaction,
            &CancelSubscription::new(harness.scope, canceled_owner, plan()),
        )
        .await?;
        transaction.commit().await?;
        for (owner, id) in [(unpaid_owner, unpaid_id), (canceled_owner, canceled_id)] {
            assert!(matches!(
                apply(change(owner, id, PastDueAccessPolicy::SuspendImmediately)).await?,
                SubscriptionPastDueAccessChangeOutcome::Terminal(_)
            ));
            assert_eq!(
                subscription_projection(harness.pool(), id).await?.5,
                "continue_until_dunning_exhausted"
            );
        }
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = owned.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn parked_expiry_survives_later_non_approved_observations() -> Result<(), Box<dyn Error>> {
    let harness = Harness::start("pe_park_stable").await?;
    let result = async {
        let (_, subscription_id) = harness.trial(suspend_policy()?, "park_stable").await?;
        let start = harness
            .due_period_ending_in(subscription_id, ChronoDuration::seconds(3))
            .await?;
        let reservation = reserve_and_admit_renewal(
            harness.pool(),
            &harness.gateway,
            harness.renewal(subscription_id, start),
        )
        .await?;
        harness
            .wait_until_ended(*reservation.period().end_at())
            .await?;
        apply_subscription_renewal_gateway_outcome(
            harness.pool(),
            &harness.enforcing,
            &reservation,
            &approved_outcome("park_stable_sale"),
        )
        .await?;
        let attempt_id = reservation.identity().attempt_id();
        let parked = (
            "review_required".to_owned(),
            Some(EXPIRED_APPROVAL.to_owned()),
        );
        assert_eq!(
            attempt_disposition(harness.pool(), attempt_id).await?,
            parked
        );
        let before = subscription_projection(harness.pool(), subscription_id).await?;
        let charges = attempt_charges(harness.pool(), attempt_id).await?;
        harness.events.lock().await.clear();

        // A stale exact query can return a decline or an unknown result after
        // the approval was parked. Neither consumes dunning, emits a failure,
        // nor clears the typed disposition, whatever the policy.
        for outcome in [declined_outcome("park_stable_sale"), unknown_outcome()] {
            for coordinator in [&harness.permissive, &harness.enforcing] {
                apply_subscription_renewal_gateway_outcome(
                    harness.pool(),
                    coordinator,
                    &reservation,
                    &outcome,
                )
                .await?;
                assert_eq!(
                    attempt_disposition(harness.pool(), attempt_id).await?,
                    parked
                );
            }
        }
        assert_eq!(
            subscription_projection(harness.pool(), subscription_id).await?,
            before
        );
        assert_eq!(attempt_charges(harness.pool(), attempt_id).await?, charges);
        assert!(harness.events.lock().await.is_empty());

        // The still-parked approval is never applied afterwards.
        let replay = apply_subscription_renewal_gateway_outcome(
            harness.pool(),
            &harness.permissive,
            &reservation,
            &approved_outcome("park_stable_sale"),
        )
        .await?;
        assert!(replay.subscription().is_none());
        assert_eq!(
            subscription_projection(harness.pool(), subscription_id).await?,
            before
        );
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = harness.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn compensation_parking_keeps_the_typed_expiry_disposition() -> Result<(), Box<dyn Error>> {
    let harness = Harness::start("pe_compensate").await?;
    let result = async {
        // Renewal: the host lock never becomes available, so the approval is
        // parked through compensation, and still with the expiry disposition.
        let (_, subscription_id) = harness.trial(suspend_policy()?, "compensate").await?;
        let start = harness
            .due_period_ending_in(subscription_id, ChronoDuration::seconds(3))
            .await?;
        let reservation = reserve_and_admit_renewal(
            harness.pool(),
            &harness.gateway,
            harness.renewal(subscription_id, start),
        )
        .await?;
        harness
            .wait_until_ended(*reservation.period().end_at())
            .await?;
        let before = subscription_projection(harness.pool(), subscription_id).await?;
        for _ in 0..2 {
            let parked = apply_subscription_renewal_gateway_outcome(
                harness.pool(),
                &UnavailableCoordinator,
                &reservation,
                &approved_outcome("compensated_renewal_sale"),
            )
            .await?;
            assert!(parked.subscription().is_none());
            let attempt_id = reservation.identity().attempt_id();
            assert_eq!(
                attempt_disposition(harness.pool(), attempt_id).await?,
                (
                    "review_required".to_owned(),
                    Some(EXPIRED_APPROVAL.to_owned())
                )
            );
            let charges = attempt_charges(harness.pool(), attempt_id).await?;
            assert_eq!(charges.len(), 1);
            assert_eq!(
                (charges[0].1.as_str(), charges[0].2.as_deref()),
                ("external_reversal_required", Some(EXPIRED_APPROVAL))
            );
        }
        assert_eq!(
            subscription_projection(harness.pool(), subscription_id).await?,
            before
        );

        // Restart: the initial compensation path derives the period from the
        // durable attempt.
        let offer = immediate_offer(
            PlanKey::new("identity_pro")?,
            ChargeAmount::new(2_699, CurrencyCode::new("USD")?)?,
        );
        let offers = StaticOfferStore {
            offer: offer.clone(),
        };
        let subscriber_id = SubscriberId::new(Uuid::now_v7());
        let enrollment = SubscriptionEnrollmentReservation::from_command(
            &syrup_rail::EnrollSubscription::new(
                syrup_rail::SubscriptionPaymentContext::new(
                    PaymentAttemptId::new(Uuid::now_v7()),
                    harness.scope,
                    subscriber_id,
                    GatewayConfigurationId::new(harness.account.gateway_configuration_id),
                    IdempotencyKey::new("compensated-restart")?,
                    PaymentToken::new("opaque-restart-token")?,
                    BillingContact::new(None, None, Some("restart@example.test".to_owned()))?,
                ),
                SubscriptionEnrollmentExpectedTerms::full_price(offer),
            ),
            &harness.gateway,
            GatewayAccountMode::Live,
        )?;
        let mut transaction = harness.pool().begin().await?;
        assert!(matches!(
            reserve_subscription_enrollment_in_transaction(&mut transaction, &offers, &enrollment)
                .await?,
            SubscriptionEnrollmentReservationOutcome::Reserved(_)
        ));
        transaction.commit().await?;
        assert!(matches!(
            admit_subscription_enrollment_submission(harness.pool(), &offers, &enrollment).await?,
            SubscriptionEnrollmentAdmissionOutcome::Admitted(_)
        ));
        let restart_id = enrollment.identity().attempt_id();
        sqlx::query(
            r#"
            UPDATE billing_payment_attempts
            SET created_at = created_at - interval '40 days',
                submitted_at = submitted_at - interval '40 days'
            WHERE id = $1
            "#,
        )
        .bind(restart_id.as_uuid())
        .execute(harness.pool())
        .await?;
        let parked = apply_subscription_enrollment_gateway_outcome(
            harness.pool(),
            &UnavailableCoordinator,
            &enrollment,
            &approved_outcome("compensated_restart_sale"),
        )
        .await?;
        assert!(parked.subscription().is_none());
        assert_eq!(
            attempt_disposition(harness.pool(), restart_id).await?,
            (
                "review_required".to_owned(),
                Some(EXPIRED_APPROVAL.to_owned())
            )
        );
        let lifecycles: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM billing_subscriptions WHERE subscriber_id = $1",
        )
        .bind(subscriber_id.as_uuid())
        .fetch_one(harness.pool())
        .await?;
        assert_eq!(lifecycles, 0);
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = harness.cleanup().await;
    result?;
    cleanup
}

#[tokio::test]
async fn wrong_mode_service_never_retires_an_expired_period() -> Result<(), Box<dyn Error>> {
    let harness = Harness::start("pe_wrong_mode").await?;
    let result = async {
        let (_, subscription_id) = harness.trial(suspend_policy()?, "wrong_mode").await?;
        let start = harness
            .due_period_ending_in(subscription_id, ChronoDuration::seconds(-30))
            .await?;
        let before = subscription_projection(harness.pool(), subscription_id).await?;
        harness.events.lock().await.clear();
        let (service, resolver) = harness.service(&harness.enforcing);
        let service = service.with_required_gateway_account_mode(GatewayAccountMode::Test);
        let routed = service.renew(harness.renewal(subscription_id, start)).await;
        assert!(
            matches!(
                routed,
                Err(crate::SubscriptionBillingServiceError::GatewayConfigurationChanged)
            ),
            "{routed:?}"
        );
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            subscription_projection(harness.pool(), subscription_id).await?,
            before
        );
        assert!(harness.events.lock().await.is_empty());
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = harness.cleanup().await;
    result?;
    cleanup
}
