use super::*;

#[tokio::test]
async fn submission_admission_validates_identity_and_skips_nonprepared_replay()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("rail_host_ident").await?;
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
        let gateway = resolved_gateway(
            account,
            Arc::new(ScriptedGateway {
                account_mode: GatewayAccountMode::Live,
                sale_calls: AtomicUsize::new(0),
                outcome: Mutex::new(None),
            }),
        );
        let snapshot = syrup_rail::HostChargeTargetSnapshot::new(
            HostChargeTargetId::new(target_id),
            ChargeAmount::new(1250, CurrencyCode::new("USD")?)?,
        );
        let command = ChargeHostTarget::new(
            syrup_rail::BillingScopeId::new(account.billing_scope_id),
            syrup_rail::SubscriberId::new(subscriber_id),
            HostChargeTargetId::new(target_id),
            GatewayConfigurationId::new(account.gateway_configuration_id),
            PaymentToken::new("tok_host_admission_identity")?,
            IdempotencyKey::new("host-admission-identity")?,
            None,
        );
        let attempt_id = PaymentAttemptId::new(Uuid::now_v7());
        let reservation = HostChargeReservation::from_command(
            &command,
            snapshot,
            &gateway,
            attempt_id,
            GatewayAccountMode::Live,
        )?;
        let mut transaction = database.pool.begin().await?;
        assert!(matches!(
            reserve_host_charge_in_transaction(&mut transaction, &TestTargets, &reservation)
                .await?,
            HostChargeReservationOutcome::Reserved(_)
        ));
        transaction.commit().await?;

        let wrong_mode = HostChargeReservation::from_command(
            &command,
            snapshot,
            &gateway,
            attempt_id,
            GatewayAccountMode::Test,
        )?;
        assert!(matches!(
            admit_host_charge_submission(&database.pool, &TestTargets, &wrong_mode).await,
            Err(HostChargeApplicationError::Store(
                HostChargeStoreError::InvalidState
            ))
        ));

        let mut transaction = database.pool.begin().await?;
        let attempt = crate::find_payment_attempt_by_id_in_transaction(
            &mut transaction,
            command.billing_scope_id(),
            attempt_id,
        )
        .await?
        .expect("reserved attempt");
        transaction.commit().await?;
        assert_eq!(attempt.status(), PaymentAttemptStatus::Pending);
        assert!(attempt.state().timestamps().submitted_at().is_none());

        sqlx::query("UPDATE billing_payment_attempts SET status = 'review_required' WHERE id = $1")
            .bind(attempt_id.as_uuid())
            .execute(&database.pool)
            .await?;
        let HostChargeAdmissionOutcome::AlreadyAdmitted(review) =
            admit_host_charge_submission(&database.pool, &AdmissionMustNotRun, &wrong_mode).await?
        else {
            panic!("wrong-mode review replay must return the canonical attempt");
        };
        assert_eq!(review.status(), PaymentAttemptStatus::ReviewRequired);
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup?;
    Ok(())
}

#[tokio::test]
async fn reservation_race_replays_equivalent_contact_and_rejects_changed_contact()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("rail_host_race").await?;
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
        let gateway = resolved_gateway(
            account,
            Arc::new(ScriptedGateway {
                account_mode: GatewayAccountMode::Live,
                sale_calls: AtomicUsize::new(0),
                outcome: Mutex::new(None),
            }),
        );
        let snapshot = syrup_rail::HostChargeTargetSnapshot::new(
            HostChargeTargetId::new(target_id),
            ChargeAmount::new(1250, CurrencyCode::new("USD")?)?,
        );
        let command = ChargeHostTarget::new(
            syrup_rail::BillingScopeId::new(account.billing_scope_id),
            syrup_rail::SubscriberId::new(subscriber_id),
            HostChargeTargetId::new(target_id),
            GatewayConfigurationId::new(account.gateway_configuration_id),
            PaymentToken::new("tok_host_winner")?,
            IdempotencyKey::new("host-reservation-race")?,
            Some(BillingContact::new(
                Some("Mary Ann".into()),
                Some("Smith".into()),
                Some("winner@example.test".into()),
            )?),
        );
        let winner_id = PaymentAttemptId::new(Uuid::now_v7());
        let winner = HostChargeReservation::from_command(
            &command,
            snapshot,
            &gateway,
            winner_id,
            GatewayAccountMode::Live,
        )?;
        let mut transaction = database.pool.begin().await?;
        let outcome =
            reserve_host_charge_in_transaction(&mut transaction, &TestTargets, &winner).await?;
        assert!(matches!(outcome, HostChargeReservationOutcome::Reserved(_)));
        transaction.commit().await?;

        let retry = ChargeHostTarget::new(
            command.billing_scope_id(),
            command.subscriber_id(),
            command.target_id(),
            command.gateway_configuration_id(),
            PaymentToken::new("tok_host_retry")?,
            command.idempotency_key().clone(),
            command.billing_contact().cloned(),
        );
        let contender = HostChargeReservation::from_command(
            &retry,
            snapshot,
            &gateway,
            PaymentAttemptId::new(Uuid::now_v7()),
            GatewayAccountMode::Live,
        )?;
        let mut transaction = database.pool.begin().await?;
        let outcome =
            reserve_host_charge_in_transaction(&mut transaction, &TestTargets, &contender).await?;
        transaction.commit().await?;
        let HostChargeReservationOutcome::Replay(attempt) = outcome else {
            panic!("matching contender should replay the durable winner");
        };
        assert_eq!(attempt.identity().attempt_id(), winner_id);
        assert_eq!(
            attempt.request().billing_contact().email(),
            Some("winner@example.test")
        );

        let changed_mode_contender = HostChargeReservation::from_command(
            &retry,
            snapshot,
            &gateway,
            PaymentAttemptId::new(Uuid::now_v7()),
            GatewayAccountMode::Test,
        )?;
        let mut transaction = database.pool.begin().await?;
        let outcome = reserve_host_charge_in_transaction(
            &mut transaction,
            &TestTargets,
            &changed_mode_contender,
        )
        .await?;
        transaction.rollback().await?;
        assert_eq!(
            outcome,
            HostChargeReservationOutcome::GatewayAccountModeChanged
        );

        sqlx::query("UPDATE billing_payment_attempts SET status = 'review_required' WHERE id = $1")
            .bind(winner_id.as_uuid())
            .execute(&database.pool)
            .await?;
        let mut transaction = database.pool.begin().await?;
        let outcome = reserve_host_charge_in_transaction(
            &mut transaction,
            &TestTargets,
            &changed_mode_contender,
        )
        .await?;
        transaction.commit().await?;
        let HostChargeReservationOutcome::Replay(review) = outcome else {
            panic!("an unsubmitted review attempt cannot resume provider submission");
        };
        assert_eq!(review.identity().attempt_id(), winner_id);
        assert_eq!(review.status(), PaymentAttemptStatus::ReviewRequired);

        let changed_contact_retry = ChargeHostTarget::new(
            command.billing_scope_id(),
            command.subscriber_id(),
            command.target_id(),
            command.gateway_configuration_id(),
            PaymentToken::new("tok_host_changed_contact")?,
            command.idempotency_key().clone(),
            Some(BillingContact::new(
                Some("Mary".into()),
                Some("Ann Smith".into()),
                Some("winner@example.test".into()),
            )?),
        );
        let changed_contact_contender = HostChargeReservation::from_command(
            &changed_contact_retry,
            snapshot,
            &gateway,
            PaymentAttemptId::new(Uuid::now_v7()),
            GatewayAccountMode::Live,
        )?;
        let mut transaction = database.pool.begin().await?;
        let outcome = reserve_host_charge_in_transaction(
            &mut transaction,
            &TestTargets,
            &changed_contact_contender,
        )
        .await?;
        transaction.rollback().await?;
        assert_eq!(outcome, HostChargeReservationOutcome::IdempotencyConflict);
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup?;
    Ok(())
}
