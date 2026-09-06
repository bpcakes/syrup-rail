use super::*;

#[tokio::test]
async fn foreground_host_charge_is_one_shot_atomic_and_replay_first() -> Result<(), Box<dyn Error>>
{
    let database = TestDatabase::start("rail_host_svc").await?;
    let result = async {
        sqlx::query(
            r#"
            CREATE TABLE host_charge_targets (
                id uuid PRIMARY KEY,
                billing_scope_id uuid NOT NULL,
                subscriber_id uuid NOT NULL,
                status text NOT NULL,
                amount_cents integer NOT NULL,
                currency text NOT NULL,
                paid_at timestamptz
            )
            "#,
        )
        .execute(&database.pool)
        .await?;
        let account = create_gateway_account(&database.pool, "nmi").await?;
        let subscriber_id = Uuid::now_v7();
        let target_id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO host_charge_targets VALUES ($1, $2, $3, 'pending', 1250, 'USD', NULL)",
        )
        .bind(target_id)
        .bind(account.billing_scope_id)
        .bind(subscriber_id)
        .execute(&database.pool)
        .await?;

        let gateway = Arc::new(ScriptedGateway {
            account_mode: GatewayAccountMode::Test,
            sale_calls: AtomicUsize::new(0),
            outcome: Mutex::new(Some(approved_outcome_with_reference(
                "host_txn_approved",
                Some("host_vault_observed"),
            ))),
        });
        let resolver = Arc::new(StaticResolver {
            gateway: resolved_gateway(account, gateway.clone()),
            calls: AtomicUsize::new(0),
        });
        let admission = Arc::new(PermitAdmission {
            calls: AtomicUsize::new(0),
        });
        let events = Arc::new(Mutex::new(Vec::new()));
        let coordinator = Arc::new(TestCoordinator {
            pool: database.pool.clone(),
            events: Arc::clone(&events),
        });
        let targets = Arc::new(TestTargets);
        let service = SubscriptionBillingService::new(
            database.pool.clone(),
            Arc::new(UnusedOffers),
            resolver.clone(),
            admission.clone(),
            coordinator.clone(),
        )
        .with_required_gateway_account_mode(GatewayAccountMode::Test)
        .with_host_charge_targets(targets);
        let command = ChargeHostTarget::new(
            syrup_rail::BillingScopeId::new(account.billing_scope_id),
            syrup_rail::SubscriberId::new(subscriber_id),
            HostChargeTargetId::new(target_id),
            GatewayConfigurationId::new(account.gateway_configuration_id),
            PaymentToken::new("tok_host_once")?,
            IdempotencyKey::new("host-idempotency")?,
            Some(BillingContact::new(
                None,
                None,
                Some("host@example.test".into()),
            )?),
        );

        let first = service.charge_host_target(command.clone()).await?;
        assert_eq!(first.status(), PaymentAttemptStatus::Approved);
        assert!(first.observation_diagnostics().is_empty());
        assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 1);
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
        assert_eq!(admission.calls.load(Ordering::SeqCst), 1);
        sqlx::query(
            "UPDATE billing_payment_attempts \
             SET gateway_payment_method_reference = 'host_vault_durable' WHERE id = $1",
        )
        .bind(first.attempt().identity().attempt_id().as_uuid())
        .execute(&database.pool)
        .await?;
        let applied_reservation = HostChargeReservation::from_command(
            &command,
            syrup_rail::HostChargeTargetSnapshot::new(
                HostChargeTargetId::new(target_id),
                ChargeAmount::new(1250, CurrencyCode::new("USD")?)?,
            ),
            &resolver.gateway,
            first.attempt().identity().attempt_id(),
            GatewayAccountMode::Test,
        )?;
        let applied_conflicting_evidence = ProcessorEvidence::new(
            syrup_rail::ProcessorApprovalEvidence::Unclassified,
            Some(GatewayTransactionId::new("host_txn_approved")?),
            Some(syrup_rail::GatewayPaymentMethodReference::new(
                "host_vault_observed",
            )?),
            Some(GatewayDiagnostic::new("1")),
            None,
            Some(GatewayDiagnostic::new("approved")),
            Some(GatewayDiagnostic::new("complete")),
            GatewayPaymentDescriptor::default(),
        );
        let applied_conflict = park_host_charge_approved(
            &database.pool,
            &applied_reservation,
            &applied_conflicting_evidence,
        )
        .await?;
        assert_eq!(applied_conflict.status(), PaymentAttemptStatus::Approved);
        assert_eq!(
            applied_conflict.observation_diagnostics(),
            &[GatewayPaymentDiagnostic::InvalidOrConflictingPaymentMethodReference]
        );
        let applied_progression: String = sqlx::query_scalar(
            "SELECT progression_state FROM billing_processor_charges \
             WHERE attempt_id = $1 AND gateway_transaction_id = $2",
        )
        .bind(first.attempt().identity().attempt_id().as_uuid())
        .bind("host_txn_approved")
        .fetch_one(&database.pool)
        .await?;
        assert_eq!(applied_progression, "applied");
        sqlx::query(
            "UPDATE billing_payment_attempts \
             SET gateway_payment_method_reference = 'host_vault_observed' WHERE id = $1",
        )
        .bind(first.attempt().identity().attempt_id().as_uuid())
        .execute(&database.pool)
        .await?;
        sqlx::query("UPDATE host_charge_targets SET status = 'reversed' WHERE id = $1")
            .bind(target_id)
            .execute(&database.pool)
            .await?;
        let duplicate_replay = processor_duplicate_outcome();
        let reconciled_replay = apply_reconciled_host_charge_gateway_outcome(
            &database.pool,
            coordinator.as_ref(),
            &RefusingTransitionTargets,
            syrup_rail::BillingScopeId::new(account.billing_scope_id),
            first.attempt().identity().attempt_id(),
            &duplicate_replay,
        )
        .await?;
        assert_eq!(reconciled_replay.attempt(), first.attempt());
        assert_eq!(reconciled_replay.status(), PaymentAttemptStatus::Approved);
        assert_eq!(
            reconciled_replay.observation_diagnostics(),
            &[GatewayPaymentDiagnostic::ProcessorReportedDuplicate],
            "the duplicate observation annotates but cannot override durable approval"
        );
        let live_service = service
            .clone()
            .with_required_gateway_account_mode(GatewayAccountMode::Live);
        let replay = live_service.charge_host_target(command).await?;
        assert_eq!(replay.attempt(), first.attempt());
        assert!(replay.observation_diagnostics().is_empty());
        assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 1);
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
        assert_eq!(admission.calls.load(Ordering::SeqCst), 1);

        let target_status: String =
            sqlx::query_scalar("SELECT status FROM host_charge_targets WHERE id = $1")
                .bind(target_id)
                .fetch_one(&database.pool)
                .await?;
        assert_eq!(target_status, "reversed");
        let charge_state: String = sqlx::query_scalar(
            "SELECT progression_state FROM billing_processor_charges WHERE attempt_id = $1",
        )
        .bind(first.attempt().identity().attempt_id().as_uuid())
        .fetch_one(&database.pool)
        .await?;
        assert_eq!(charge_state, "applied");
        let late_decline = GatewayPaymentOutcome::new(
            GatewayPaymentStatus::Declined,
            ProcessorEvidence::new(
                syrup_rail::ProcessorApprovalEvidence::Unclassified,
                Some(GatewayTransactionId::new("host_txn_late_decline")?),
                None,
                Some(GatewayDiagnostic::new("2")),
                Some(GatewayDiagnostic::new("200")),
                Some(GatewayDiagnostic::new("Declined")),
                Some(GatewayDiagnostic::new("declined")),
                GatewayPaymentDescriptor::default(),
            ),
        );
        let terminal_decline = apply_reconciled_host_charge_gateway_outcome(
            &database.pool,
            coordinator.as_ref(),
            &RefusingTransitionTargets,
            syrup_rail::BillingScopeId::new(account.billing_scope_id),
            first.attempt().identity().attempt_id(),
            &late_decline,
        )
        .await?;
        assert_eq!(terminal_decline.attempt(), first.attempt());
        assert_eq!(
            terminal_decline.observation_diagnostics(),
            &[GatewayPaymentDiagnostic::InvalidOrConflictingTransactionIdentifier]
        );
        let terminal_approval = apply_reconciled_host_charge_gateway_outcome(
            &database.pool,
            coordinator.as_ref(),
            &TestTargets,
            syrup_rail::BillingScopeId::new(account.billing_scope_id),
            first.attempt().identity().attempt_id(),
            &approved_outcome("host_txn_late_approval"),
        )
        .await?;
        assert_eq!(terminal_approval.attempt(), first.attempt());
        assert_eq!(
            terminal_approval.observation_diagnostics(),
            &[GatewayPaymentDiagnostic::InvalidOrConflictingTransactionIdentifier]
        );
        let terminal_approval_charge: String = sqlx::query_scalar(
            "SELECT progression_state FROM billing_processor_charges \
             WHERE attempt_id = $1 AND gateway_transaction_id = $2",
        )
        .bind(first.attempt().identity().attempt_id().as_uuid())
        .bind("host_txn_late_approval")
        .fetch_one(&database.pool)
        .await?;
        assert_eq!(terminal_approval_charge, "external_reversal_required");

        sqlx::query(
            "UPDATE billing_processor_charges \
             SET progression_state = 'reconciliation_required', \
                 reconciliation_required_at = clock_timestamp(), \
                 external_reversal_required_at = NULL \
             WHERE attempt_id = $1 AND gateway_transaction_id = $2",
        )
        .bind(first.attempt().identity().attempt_id().as_uuid())
        .bind("host_txn_late_approval")
        .execute(&database.pool)
        .await?;
        let promoted_terminal_approval = apply_reconciled_host_charge_gateway_outcome(
            &database.pool,
            coordinator.as_ref(),
            &TestTargets,
            syrup_rail::BillingScopeId::new(account.billing_scope_id),
            first.attempt().identity().attempt_id(),
            &approved_outcome("host_txn_late_approval"),
        )
        .await?;
        assert_eq!(promoted_terminal_approval.attempt(), first.attempt());
        assert_eq!(
            promoted_terminal_approval.observation_diagnostics(),
            &[GatewayPaymentDiagnostic::InvalidOrConflictingTransactionIdentifier]
        );
        let promoted_terminal_charge: String = sqlx::query_scalar(
            "SELECT progression_state FROM billing_processor_charges \
             WHERE attempt_id = $1 AND gateway_transaction_id = $2",
        )
        .bind(first.attempt().identity().attempt_id().as_uuid())
        .bind("host_txn_late_approval")
        .fetch_one(&database.pool)
        .await?;
        assert_eq!(promoted_terminal_charge, "external_reversal_required");

        let conflicting_target_id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO host_charge_targets VALUES ($1, $2, $3, 'pending', 1250, 'USD', NULL)",
        )
        .bind(conflicting_target_id)
        .bind(account.billing_scope_id)
        .bind(subscriber_id)
        .execute(&database.pool)
        .await?;
        let conflicting_command = ChargeHostTarget::new(
            syrup_rail::BillingScopeId::new(account.billing_scope_id),
            syrup_rail::SubscriberId::new(subscriber_id),
            HostChargeTargetId::new(conflicting_target_id),
            GatewayConfigurationId::new(account.gateway_configuration_id),
            PaymentToken::new("tok_host_conflict")?,
            IdempotencyKey::new("host-conflict")?,
            None,
        );
        let conflicting_attempt_id = PaymentAttemptId::new(Uuid::now_v7());
        let conflicting_reservation = HostChargeReservation::from_command(
            &conflicting_command,
            syrup_rail::HostChargeTargetSnapshot::new(
                HostChargeTargetId::new(conflicting_target_id),
                ChargeAmount::new(1250, CurrencyCode::new("USD")?)?,
            ),
            &resolver.gateway,
            conflicting_attempt_id,
            GatewayAccountMode::Test,
        )?;
        let mut transaction = database.pool.begin().await?;
        assert!(matches!(
            reserve_host_charge_in_transaction(
                &mut transaction,
                &TestTargets,
                &conflicting_reservation,
            )
            .await?,
            HostChargeReservationOutcome::Reserved(_)
        ));
        transaction.commit().await?;
        assert!(matches!(
            admit_host_charge_submission(&database.pool, &TestTargets, &conflicting_reservation,)
                .await?,
            HostChargeAdmissionOutcome::Admitted(_)
        ));
        let unknown = GatewayPaymentOutcome::new(
            GatewayPaymentStatus::Unknown,
            ProcessorEvidence::new(
                syrup_rail::ProcessorApprovalEvidence::Unclassified,
                Some(GatewayTransactionId::new("host_txn_durable")?),
                Some(syrup_rail::GatewayPaymentMethodReference::new(
                    "host_vault_durable",
                )?),
                Some(GatewayDiagnostic::new("3")),
                Some(GatewayDiagnostic::new("400")),
                Some(GatewayDiagnostic::new("Processor outcome unknown")),
                Some(GatewayDiagnostic::new("unknown")),
                GatewayPaymentDescriptor::default(),
            ),
        );
        let durable = apply_host_charge_gateway_outcome(
            &database.pool,
            coordinator.as_ref(),
            &TestTargets,
            &conflicting_reservation,
            &unknown,
        )
        .await?;
        assert_eq!(durable.status(), PaymentAttemptStatus::Unknown);
        let same_transaction_conflict =
            approved_outcome_with_reference("host_txn_durable", Some("host_vault_conflict"));
        let conflict = apply_reconciled_host_charge_gateway_outcome(
            &database.pool,
            coordinator.as_ref(),
            &TestTargets,
            syrup_rail::BillingScopeId::new(account.billing_scope_id),
            conflicting_attempt_id,
            &same_transaction_conflict,
        )
        .await?;
        assert_eq!(conflict.status(), PaymentAttemptStatus::Unknown);
        assert_eq!(
            conflict.observation_diagnostics(),
            &[GatewayPaymentDiagnostic::InvalidOrConflictingPaymentMethodReference]
        );
        let conflict_state: (Option<String>, Option<String>, String) = sqlx::query_as(
            "SELECT gateway_transaction_id, gateway_payment_method_reference, \
                    progression_state \
             FROM billing_processor_charges WHERE attempt_id = $1",
        )
        .bind(conflicting_attempt_id.as_uuid())
        .fetch_one(&database.pool)
        .await?;
        assert_eq!(
            conflict_state,
            (
                Some("host_txn_durable".to_owned()),
                Some("host_vault_conflict".to_owned()),
                "reconciliation_required".to_owned(),
            )
        );
        let fallback_evidence = approved_outcome("host_txn_fallback_conflict")
            .evidence()
            .clone();
        let fallback =
            park_host_charge_approved(&database.pool, &conflicting_reservation, &fallback_evidence)
                .await?;
        assert_eq!(fallback.status(), PaymentAttemptStatus::Unknown);
        assert_eq!(
            fallback.observation_diagnostics(),
            &[GatewayPaymentDiagnostic::InvalidOrConflictingTransactionIdentifier]
        );
        let fallback_charge_state: String = sqlx::query_scalar(
            "SELECT progression_state FROM billing_processor_charges \
             WHERE attempt_id = $1 AND gateway_transaction_id = $2",
        )
        .bind(conflicting_attempt_id.as_uuid())
        .bind("host_txn_fallback_conflict")
        .fetch_one(&database.pool)
        .await?;
        assert_eq!(fallback_charge_state, "reconciliation_required");
        let target_status: String =
            sqlx::query_scalar("SELECT status FROM host_charge_targets WHERE id = $1")
                .bind(conflicting_target_id)
                .fetch_one(&database.pool)
                .await?;
        assert_eq!(target_status, "pending");
        let event_keys = events
            .lock()
            .await
            .iter()
            .map(BillingEvent::semantic_key)
            .collect::<Vec<_>>();
        assert_eq!(
            event_keys,
            vec![BillingEventKey::HostChargePaid(HostChargeTargetId::new(
                target_id
            ))]
        );
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup?;
    Ok(())
}
