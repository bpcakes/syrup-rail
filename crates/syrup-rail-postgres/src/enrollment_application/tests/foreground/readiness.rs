use super::*;

#[tokio::test]
async fn foreground_renewal_projects_every_gateway_readiness_error_once()
-> Result<(), Box<dyn Error>> {
    for (project, error, expected_code, expected_condition, expects_cooldown) in [
        (
            "rr_reject",
            GatewayError::RequestRejected(GatewayDiagnostic::new("request rejected")),
            "gateway_request_rejected_before_submission",
            Some("failed"),
            false,
        ),
        (
            "rr_malformed",
            GatewayError::Malformed(GatewayDiagnostic::new("malformed response")),
            "gateway_malformed_before_submission",
            Some("failed"),
            false,
        ),
        (
            "rr_config",
            GatewayError::Configuration(GatewayDiagnostic::new("bad configuration")),
            "gateway_configuration_before_submission",
            Some("failed"),
            false,
        ),
        (
            "rr_unavailable",
            GatewayError::Unavailable(GatewayDiagnostic::new(
                "temporary readiness transport failure",
            )),
            "gateway_unavailable_before_submission",
            Some("failed"),
            false,
        ),
        (
            "rr_limited",
            GatewayError::RateLimited(GatewayDiagnostic::new("retry later")),
            "gateway_provider_rate_limited_before_submission",
            None,
            true,
        ),
    ] {
        let fixture = enrollment_fixture(project, false, false, false).await?;
        let initial_gateway = Arc::new(ScriptedGateway::new(Ok(approved_outcome(&format!(
            "txn_{project}_initial"
        )))));
        let initial_service = SubscriptionBillingService::new(
            fixture.database.pool.clone(),
            Arc::new(TestOfferStore),
            Arc::new(StaticResolver {
                gateway: scripted_resolved_gateway(
                    fixture.gateway_account,
                    Arc::clone(&initial_gateway),
                ),
                calls: AtomicUsize::new(0),
            }),
            Arc::new(PermitAdmission {
                calls: AtomicUsize::new(0),
            }),
            Arc::new(fixture.coordinator.clone()),
        );
        let initial = initial_service.enroll(fixture.command.clone()).await?;
        let subscription_id = initial
            .subscription()
            .expect("approved enrollment creates subscription")
            .id();
        let requested_period_start_at = Utc::now() - ChronoDuration::hours(1);
        let period_start_at: DateTime<Utc> = sqlx::query_scalar(
            r#"
            UPDATE billing_subscriptions
            SET current_period_start_at = $2 - interval '1 month',
                current_period_end_at = $2,
                next_renewal_at = $2,
                next_payment_attempt_at = $2,
                updated_at = clock_timestamp()
            WHERE id = $1
            RETURNING next_renewal_at
            "#,
        )
        .bind(subscription_id.as_uuid())
        .bind(requested_period_start_at)
        .fetch_one(&fixture.database.pool)
        .await?;

        let gateway = Arc::new(ScriptedGateway::for_sale_with_readiness(
            [Ok(GatewayAccountMode::Live), Err(error)],
            Ok(approved_outcome(&format!("txn_{project}_must_not_submit"))),
        ));
        let service = SubscriptionBillingService::new(
            fixture.database.pool.clone(),
            Arc::new(TestOfferStore),
            Arc::new(StaticResolver {
                gateway: scripted_resolved_gateway(fixture.gateway_account, Arc::clone(&gateway)),
                calls: AtomicUsize::new(0),
            }),
            Arc::new(PermitAdmission {
                calls: AtomicUsize::new(0),
            }),
            Arc::new(fixture.coordinator.clone()),
        );
        assert!(matches!(
            service
                .renew(ChargeRenewal::new(
                    fixture.command.billing_scope_id(),
                    subscription_id,
                    period_start_at,
                ))
                .await?,
            SubscriptionRenewalOutcome::Noop
        ));
        let state: (String, Option<String>, Option<String>, bool, bool) = sqlx::query_as(
            r#"
            SELECT attempt.status, attempt.resolution_code, attempt.gateway_condition,
                attempt.submitted_at IS NULL,
                provider.rate_limited_until > clock_timestamp()
            FROM billing_payment_attempts AS attempt
            INNER JOIN billing_gateway_accounts AS account
                ON account.id = attempt.gateway_account_id
            INNER JOIN billing_gateway_provider_rate_limits AS provider
                ON provider.provider_key = account.provider_key
            WHERE attempt.subscription_id = $1
                AND attempt.attempt_kind = 'subscription_renewal'
            "#,
        )
        .bind(subscription_id.as_uuid())
        .fetch_one(&fixture.database.pool)
        .await?;
        assert_eq!(state.0, "failed");
        assert_eq!(state.1.as_deref(), Some(expected_code));
        assert_eq!(state.2.as_deref(), expected_condition);
        assert!(state.3);
        assert_eq!(state.4, expects_cooldown);
        assert_eq!(gateway.account_mode_calls.load(Ordering::SeqCst), 2);
        assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 0);
        fixture.cleanup().await?;
    }
    Ok(())
}

#[tokio::test]
async fn foreground_subscriber_projects_every_gateway_readiness_error_once()
-> Result<(), Box<dyn Error>> {
    for (project, gateway_error, status, expected_code, expected_condition, expects_cooldown) in [
        (
            "sr_reject",
            GatewayError::RequestRejected(GatewayDiagnostic::new("request rejected")),
            "failed",
            Some("gateway_request_rejected_before_submission"),
            Some("failed"),
            false,
        ),
        (
            "sr_malformed",
            GatewayError::Malformed(GatewayDiagnostic::new("malformed response")),
            "failed",
            Some("gateway_malformed_before_submission"),
            Some("failed"),
            false,
        ),
        (
            "sr_config",
            GatewayError::Configuration(GatewayDiagnostic::new("bad configuration")),
            "failed",
            Some("gateway_configuration_before_submission"),
            Some("failed"),
            false,
        ),
        (
            "sr_unavailable",
            GatewayError::Unavailable(GatewayDiagnostic::new("transport unavailable")),
            "pending",
            None,
            None,
            false,
        ),
        (
            "sr_limited",
            GatewayError::RateLimited(GatewayDiagnostic::new("retry later")),
            "failed",
            Some("gateway_provider_rate_limited_before_submission"),
            None,
            true,
        ),
    ] {
        let fixture = enrollment_fixture(project, false, false, false).await?;
        let gateway = Arc::new(ScriptedGateway::for_sale_with_readiness(
            [Err(gateway_error)],
            Ok(approved_outcome(&format!("txn_{project}_must_not_submit"))),
        ));
        let service = SubscriptionBillingService::new(
            fixture.database.pool.clone(),
            Arc::new(TestOfferStore),
            Arc::new(StaticResolver {
                gateway: scripted_resolved_gateway(fixture.gateway_account, Arc::clone(&gateway)),
                calls: AtomicUsize::new(0),
            }),
            Arc::new(PermitAdmission {
                calls: AtomicUsize::new(0),
            }),
            Arc::new(fixture.coordinator.clone()),
        );

        let error = service
            .enroll(fixture.command.clone())
            .await
            .expect_err("readiness failure must prevent subscriber submission");
        if expects_cooldown {
            assert!(matches!(
                error,
                SubscriptionBillingServiceError::GatewayMutationCooldown {
                    scope: GatewayMutationCooldownScope::Provider
                }
            ));
        } else {
            assert!(matches!(
                error,
                SubscriptionBillingServiceError::GatewayReadiness(_)
            ));
        }
        let state: (String, Option<String>, Option<String>, bool, bool) = sqlx::query_as(
            r#"
            SELECT attempt.status, attempt.resolution_code, attempt.gateway_condition,
                attempt.submitted_at IS NULL,
                provider.rate_limited_until > clock_timestamp()
            FROM billing_payment_attempts AS attempt
            INNER JOIN billing_gateway_accounts AS account
                ON account.id = attempt.gateway_account_id
            INNER JOIN billing_gateway_provider_rate_limits AS provider
                ON provider.provider_key = account.provider_key
            WHERE attempt.id = $1
            "#,
        )
        .bind(fixture.command.attempt_id().as_uuid())
        .fetch_one(&fixture.database.pool)
        .await?;
        assert_eq!(state.0, status);
        assert_eq!(state.1.as_deref(), expected_code);
        assert_eq!(state.2.as_deref(), expected_condition);
        assert!(state.3);
        assert_eq!(state.4, expects_cooldown);
        assert_eq!(gateway.account_mode_calls.load(Ordering::SeqCst), 1);
        assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 0);
        fixture.cleanup().await?;
    }
    Ok(())
}

#[tokio::test]
async fn terminal_live_enrollment_replays_under_test_policy_without_provider_io()
-> Result<(), Box<dyn Error>> {
    let fixture = enrollment_fixture("terminal_mode", false, false, false).await?;
    let gateway = Arc::new(ScriptedGateway::new(Ok(approved_outcome(
        "txn_terminal_cross_mode",
    ))));
    let resolver = Arc::new(StaticResolver {
        gateway: scripted_resolved_gateway(fixture.gateway_account, Arc::clone(&gateway)),
        calls: AtomicUsize::new(0),
    });
    let live_service = SubscriptionBillingService::new(
        fixture.database.pool.clone(),
        Arc::new(TestOfferStore),
        resolver.clone(),
        Arc::new(PermitAdmission {
            calls: AtomicUsize::new(0),
        }),
        Arc::new(fixture.coordinator.clone()),
    );
    let live_result = live_service.enroll(fixture.command.clone()).await?;
    assert_eq!(
        live_result.attempt().status(),
        PaymentAttemptStatus::Approved
    );

    let test_service = SubscriptionBillingService::new(
        fixture.database.pool.clone(),
        Arc::new(TestOfferStore),
        resolver,
        Arc::new(PermitAdmission {
            calls: AtomicUsize::new(0),
        }),
        Arc::new(fixture.coordinator.clone()),
    )
    .with_required_gateway_account_mode(GatewayAccountMode::Test);
    let replay = test_service.enroll(fixture.command.clone()).await?;

    assert_eq!(replay, live_result);
    assert_eq!(gateway.account_mode_calls.load(Ordering::SeqCst), 2);
    assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 1);
    fixture.cleanup().await
}

#[tokio::test]
async fn foreground_renewal_mode_mismatch_does_not_consume_retry_budget()
-> Result<(), Box<dyn Error>> {
    let fixture = enrollment_fixture("renew_ready", false, false, false).await?;
    let initial_gateway = Arc::new(ScriptedGateway::new(Ok(approved_outcome(
        "txn_renew_ready_initial",
    ))));
    let initial_service = SubscriptionBillingService::new(
        fixture.database.pool.clone(),
        Arc::new(TestOfferStore),
        Arc::new(StaticResolver {
            gateway: scripted_resolved_gateway(
                fixture.gateway_account,
                Arc::clone(&initial_gateway),
            ),
            calls: AtomicUsize::new(0),
        }),
        Arc::new(PermitAdmission {
            calls: AtomicUsize::new(0),
        }),
        Arc::new(fixture.coordinator.clone()),
    );
    let initial = initial_service.enroll(fixture.command.clone()).await?;
    let subscription_id = initial
        .subscription()
        .expect("approved enrollment creates subscription")
        .id();
    let requested_period_start_at = Utc::now() - ChronoDuration::hours(1);
    let period_start_at: DateTime<Utc> = sqlx::query_scalar(
        r#"
        UPDATE billing_subscriptions
        SET current_period_start_at = $2 - interval '1 month',
            current_period_end_at = $2,
            next_renewal_at = $2,
            next_payment_attempt_at = $2,
            updated_at = clock_timestamp()
        WHERE id = $1
        RETURNING next_renewal_at
        "#,
    )
    .bind(subscription_id.as_uuid())
    .bind(requested_period_start_at)
    .fetch_one(&fixture.database.pool)
    .await?;

    let gateway = Arc::new(ScriptedGateway::for_sale_with_readiness(
        [
            Ok(GatewayAccountMode::Test),
            Ok(GatewayAccountMode::Live),
            Ok(GatewayAccountMode::Live),
        ],
        Ok(approved_outcome("txn_renew_ready_after_mode_fix")),
    ));
    let service = SubscriptionBillingService::new(
        fixture.database.pool.clone(),
        Arc::new(TestOfferStore),
        Arc::new(StaticResolver {
            gateway: scripted_resolved_gateway(fixture.gateway_account, Arc::clone(&gateway)),
            calls: AtomicUsize::new(0),
        }),
        Arc::new(PermitAdmission {
            calls: AtomicUsize::new(0),
        }),
        Arc::new(fixture.coordinator.clone()),
    );
    assert!(matches!(
        service
            .renew(ChargeRenewal::new(
                fixture.command.billing_scope_id(),
                subscription_id,
                period_start_at,
            ))
            .await,
        Err(SubscriptionBillingServiceError::GatewayReadiness(
            GatewayError::Configuration(_)
        ))
    ));
    let attempt_count: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(*)
        FROM billing_payment_attempts
        WHERE subscription_id = $1 AND attempt_kind = 'subscription_renewal'
        "#,
    )
    .bind(subscription_id.as_uuid())
    .fetch_one(&fixture.database.pool)
    .await?;
    assert_eq!(attempt_count, 0);
    assert!(matches!(
        service
            .renew(ChargeRenewal::new(
                fixture.command.billing_scope_id(),
                subscription_id,
                period_start_at,
            ))
            .await?,
        SubscriptionRenewalOutcome::Payment(_)
    ));
    assert_eq!(gateway.account_mode_calls.load(Ordering::SeqCst), 4);
    assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 1);
    fixture.cleanup().await
}

#[tokio::test]
async fn foreground_renewal_preserves_final_mode_query_unavailability() -> Result<(), Box<dyn Error>>
{
    let fixture = enrollment_fixture("renew_final", false, false, false).await?;
    let initial_gateway = Arc::new(ScriptedGateway::new(Ok(approved_outcome(
        "txn_renew_final_ready_initial",
    ))));
    let initial_service = SubscriptionBillingService::new(
        fixture.database.pool.clone(),
        Arc::new(TestOfferStore),
        Arc::new(StaticResolver {
            gateway: scripted_resolved_gateway(
                fixture.gateway_account,
                Arc::clone(&initial_gateway),
            ),
            calls: AtomicUsize::new(0),
        }),
        Arc::new(PermitAdmission {
            calls: AtomicUsize::new(0),
        }),
        Arc::new(fixture.coordinator.clone()),
    );
    let initial = initial_service.enroll(fixture.command.clone()).await?;
    let subscription_id = initial
        .subscription()
        .expect("approved enrollment creates subscription")
        .id();
    let requested_period_start_at = Utc::now() - ChronoDuration::hours(1);
    let period_start_at: DateTime<Utc> = sqlx::query_scalar(
        r#"
        UPDATE billing_subscriptions
        SET current_period_start_at = $2 - interval '1 month',
            current_period_end_at = $2,
            next_renewal_at = $2,
            next_payment_attempt_at = $2,
            updated_at = clock_timestamp()
        WHERE id = $1
        RETURNING next_renewal_at
        "#,
    )
    .bind(subscription_id.as_uuid())
    .bind(requested_period_start_at)
    .fetch_one(&fixture.database.pool)
    .await?;

    let gateway = Arc::new(ScriptedGateway::for_sale_with_readiness(
        [
            Ok(GatewayAccountMode::Live),
            Ok(GatewayAccountMode::Live),
            Err(GatewayError::Unavailable(GatewayDiagnostic::new(
                "final account-mode query unavailable",
            ))),
        ],
        Ok(approved_outcome("txn_renew_final_ready_must_not_submit")),
    ));
    let service = SubscriptionBillingService::new(
        fixture.database.pool.clone(),
        Arc::new(TestOfferStore),
        Arc::new(StaticResolver {
            gateway: scripted_resolved_gateway(fixture.gateway_account, Arc::clone(&gateway)),
            calls: AtomicUsize::new(0),
        }),
        Arc::new(PermitAdmission {
            calls: AtomicUsize::new(0),
        }),
        Arc::new(fixture.coordinator.clone()),
    );
    let outcome = service
        .renew(ChargeRenewal::new(
            fixture.command.billing_scope_id(),
            subscription_id,
            period_start_at,
        ))
        .await?;
    let SubscriptionRenewalOutcome::NotSubmitted { payment, error } = outcome else {
        return Err("final account-mode query failure must remain not-submitted".into());
    };
    assert!(matches!(
        error,
        GatewayNotSubmittedError::AccountModeVerification(GatewayError::Unavailable(_))
    ));
    assert_eq!(payment.attempt().status(), PaymentAttemptStatus::Failed);
    assert_eq!(
        payment.attempt().state().resolution_code(),
        Some(PaymentResolutionCode::GatewayUnavailableBeforeSubmission)
    );
    assert!(
        payment
            .attempt()
            .state()
            .timestamps()
            .submitted_at()
            .is_none()
    );
    assert_eq!(gateway.account_mode_calls.load(Ordering::SeqCst), 3);
    assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 0);
    fixture.cleanup().await
}

#[tokio::test]
async fn foreground_unavailable_readiness_keeps_the_prepared_attempt_retryable()
-> Result<(), Box<dyn Error>> {
    let fixture = enrollment_fixture("svc_ready_retry", false, false, false).await?;
    let gateway = Arc::new(ScriptedGateway::for_sale_with_readiness(
        [
            Err(GatewayError::Unavailable(GatewayDiagnostic::new(
                "temporary transport failure",
            ))),
            Ok(GatewayAccountMode::Live),
            Ok(GatewayAccountMode::Live),
        ],
        Ok(approved_outcome("txn_ready_retry")),
    ));
    let resolver = Arc::new(StaticResolver {
        gateway: scripted_resolved_gateway(fixture.gateway_account, Arc::clone(&gateway)),
        calls: AtomicUsize::new(0),
    });
    let admission = Arc::new(PermitAdmission {
        calls: AtomicUsize::new(0),
    });
    let service = SubscriptionBillingService::new(
        fixture.database.pool.clone(),
        Arc::new(TestOfferStore),
        resolver.clone(),
        admission.clone(),
        Arc::new(fixture.coordinator.clone()),
    );

    let error = service
        .enroll(fixture.command.clone())
        .await
        .expect_err("temporary readiness failure must remain retryable");
    assert!(matches!(
        &error,
        SubscriptionBillingServiceError::GatewayReadiness(GatewayError::Unavailable(_))
    ));
    assert!(error.is_retryable());
    let prepared: (String, Option<String>, Option<DateTime<Utc>>) = sqlx::query_as(
        "SELECT status, resolution_code, submitted_at FROM billing_payment_attempts WHERE id = $1",
    )
    .bind(fixture.command.attempt_id().as_uuid())
    .fetch_one(&fixture.database.pool)
    .await?;
    assert_eq!(prepared, ("pending".to_owned(), None, None));
    assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 0);

    let result = service.enroll(fixture.command.clone()).await?;
    assert_eq!(result.attempt().status(), PaymentAttemptStatus::Approved);
    assert_eq!(
        result.attempt().identity().attempt_id(),
        fixture.command.attempt_id()
    );
    assert_eq!(gateway.account_mode_calls.load(Ordering::SeqCst), 3);
    assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 1);
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 2);
    assert_eq!(admission.calls.load(Ordering::SeqCst), 2);
    fixture.cleanup().await
}

#[tokio::test]
async fn final_unavailable_mode_query_restores_enrollment_for_same_key_retry()
-> Result<(), Box<dyn Error>> {
    let fixture = enrollment_fixture("final_retry", false, false, false).await?;
    let gateway = Arc::new(ScriptedGateway::for_sale_with_readiness(
        [
            Ok(GatewayAccountMode::Live),
            Err(GatewayError::Unavailable(GatewayDiagnostic::new(
                "final mode query unavailable",
            ))),
            Ok(GatewayAccountMode::Live),
            Ok(GatewayAccountMode::Live),
        ],
        Ok(approved_outcome("txn_final_ready_retry")),
    ));
    let service = SubscriptionBillingService::new(
        fixture.database.pool.clone(),
        Arc::new(TestOfferStore),
        Arc::new(StaticResolver {
            gateway: scripted_resolved_gateway(fixture.gateway_account, Arc::clone(&gateway)),
            calls: AtomicUsize::new(0),
        }),
        Arc::new(PermitAdmission {
            calls: AtomicUsize::new(0),
        }),
        Arc::new(fixture.coordinator.clone()),
    );

    let error = service
        .enroll(fixture.command.clone())
        .await
        .expect_err("the final unavailable query must remain retryable");
    assert!(matches!(
        error,
        SubscriptionBillingServiceError::GatewayNotSubmitted(
            GatewayNotSubmittedError::AccountModeVerification(GatewayError::Unavailable(_))
        )
    ));
    let prepared: (String, Option<String>, Option<DateTime<Utc>>) = sqlx::query_as(
        "SELECT status, resolution_code, submitted_at FROM billing_payment_attempts WHERE id = $1",
    )
    .bind(fixture.command.attempt_id().as_uuid())
    .fetch_one(&fixture.database.pool)
    .await?;
    assert_eq!(prepared, ("pending".to_owned(), None, None));
    assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 0);

    let payment = service.enroll(fixture.command.clone()).await?;
    assert_eq!(payment.attempt().status(), PaymentAttemptStatus::Approved);
    assert_eq!(
        payment.attempt().identity().attempt_id(),
        fixture.command.attempt_id()
    );
    assert_eq!(gateway.account_mode_calls.load(Ordering::SeqCst), 4);
    assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 1);
    fixture.cleanup().await
}

#[tokio::test]
async fn foreground_configuration_readiness_is_typed_and_same_key_recovers_terminal_result()
-> Result<(), Box<dyn Error>> {
    let fixture = enrollment_fixture("svc_ready_config", false, false, false).await?;
    let gateway = Arc::new(ScriptedGateway::for_sale_with_readiness(
        [Err(GatewayError::Configuration(GatewayDiagnostic::new(
            "invalid merchant configuration",
        )))],
        Ok(approved_outcome("txn_config_must_not_submit")),
    ));
    let resolver = Arc::new(StaticResolver {
        gateway: scripted_resolved_gateway(fixture.gateway_account, Arc::clone(&gateway)),
        calls: AtomicUsize::new(0),
    });
    let admission = Arc::new(PermitAdmission {
        calls: AtomicUsize::new(0),
    });
    let service = SubscriptionBillingService::new(
        fixture.database.pool.clone(),
        Arc::new(TestOfferStore),
        resolver.clone(),
        admission.clone(),
        Arc::new(fixture.coordinator.clone()),
    );

    let error = service
        .enroll(fixture.command.clone())
        .await
        .expect_err("configuration readiness failure must stay typed");
    assert!(matches!(
        error,
        SubscriptionBillingServiceError::GatewayReadiness(GatewayError::Configuration(_))
    ));
    let state: (String, Option<String>) = sqlx::query_as(
        "SELECT status, resolution_code FROM billing_payment_attempts WHERE id = $1",
    )
    .bind(fixture.command.attempt_id().as_uuid())
    .fetch_one(&fixture.database.pool)
    .await?;
    assert_eq!(
        state,
        (
            "failed".to_owned(),
            Some("gateway_configuration_before_submission".to_owned())
        )
    );
    let replay = service.enroll(fixture.command.clone()).await?;
    assert_eq!(replay.attempt().status(), PaymentAttemptStatus::Failed);
    assert_eq!(
        replay.attempt().identity().attempt_id(),
        fixture.command.attempt_id()
    );
    assert_eq!(
        replay.attempt().state().resolution_code(),
        Some(PaymentResolutionCode::GatewayConfigurationBeforeSubmission)
    );
    assert_eq!(gateway.account_mode_calls.load(Ordering::SeqCst), 1);
    assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 0);
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
    assert_eq!(admission.calls.load(Ordering::SeqCst), 1);
    fixture.cleanup().await
}

#[tokio::test]
async fn foreground_service_requires_live_mode_by_default() -> Result<(), Box<dyn Error>> {
    let fixture = enrollment_fixture("mode_live_def", false, false, false).await?;
    let gateway = Arc::new(ScriptedGateway::for_sale_with_readiness(
        [Ok(GatewayAccountMode::Test)],
        Ok(approved_outcome("txn_live_default_must_not_submit")),
    ));
    let service = SubscriptionBillingService::new(
        fixture.database.pool.clone(),
        Arc::new(TestOfferStore),
        Arc::new(StaticResolver {
            gateway: scripted_resolved_gateway(fixture.gateway_account, Arc::clone(&gateway)),
            calls: AtomicUsize::new(0),
        }),
        Arc::new(PermitAdmission {
            calls: AtomicUsize::new(0),
        }),
        Arc::new(fixture.coordinator.clone()),
    );

    let result = service.enroll(fixture.command.clone()).await?;
    assert_eq!(result.attempt().status(), PaymentAttemptStatus::Failed);
    assert_eq!(
        result.attempt().state().resolution_code(),
        Some(PaymentResolutionCode::GatewayLiveReadinessFailedBeforeSubmission)
    );
    assert!(
        result
            .attempt()
            .state()
            .timestamps()
            .submitted_at()
            .is_none()
    );
    let persisted_mode: String = sqlx::query_scalar(
        "SELECT required_gateway_account_mode FROM billing_payment_attempts WHERE id = $1",
    )
    .bind(fixture.command.attempt_id().as_uuid())
    .fetch_one(&fixture.database.pool)
    .await?;
    assert_eq!(persisted_mode, "live");
    assert_eq!(gateway.account_mode_calls.load(Ordering::SeqCst), 1);
    assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 0);
    fixture.cleanup().await
}

#[tokio::test]
async fn foreground_service_rechecks_mode_at_submission() -> Result<(), Box<dyn Error>> {
    let fixture = enrollment_fixture("mode_flip_guard", false, false, false).await?;
    let gateway = Arc::new(ScriptedGateway::for_sale_with_readiness(
        [Ok(GatewayAccountMode::Test), Ok(GatewayAccountMode::Test)],
        Ok(approved_outcome("txn_single_mode_capability")),
    ));
    let service = SubscriptionBillingService::new(
        fixture.database.pool.clone(),
        Arc::new(TestOfferStore),
        Arc::new(StaticResolver {
            gateway: scripted_resolved_gateway(fixture.gateway_account, Arc::clone(&gateway)),
            calls: AtomicUsize::new(0),
        }),
        Arc::new(PermitAdmission {
            calls: AtomicUsize::new(0),
        }),
        Arc::new(fixture.coordinator.clone()),
    )
    .with_required_gateway_account_mode(GatewayAccountMode::Test);

    let result = service.enroll(fixture.command.clone()).await?;
    assert_eq!(result.attempt().status(), PaymentAttemptStatus::Approved);
    assert!(
        result
            .attempt()
            .state()
            .timestamps()
            .submitted_at()
            .is_some()
    );
    assert_eq!(gateway.account_mode_calls.load(Ordering::SeqCst), 2);
    assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 1);
    fixture.cleanup().await
}

#[tokio::test]
async fn foreground_service_blocks_mode_flip_after_early_readiness() -> Result<(), Box<dyn Error>> {
    let fixture = enrollment_fixture("mode_flip_final", false, false, false).await?;
    let gateway = Arc::new(ScriptedGateway::for_sale_with_readiness(
        [Ok(GatewayAccountMode::Live), Ok(GatewayAccountMode::Test)],
        Ok(approved_outcome("txn_mode_flip_must_not_submit")),
    ));
    let service = SubscriptionBillingService::new(
        fixture.database.pool.clone(),
        Arc::new(TestOfferStore),
        Arc::new(StaticResolver {
            gateway: scripted_resolved_gateway(fixture.gateway_account, Arc::clone(&gateway)),
            calls: AtomicUsize::new(0),
        }),
        Arc::new(PermitAdmission {
            calls: AtomicUsize::new(0),
        }),
        Arc::new(fixture.coordinator.clone()),
    );

    let error = service
        .enroll(fixture.command.clone())
        .await
        .expect_err("a final mode mismatch must prevent the provider mutation");
    assert!(matches!(
        error,
        SubscriptionBillingServiceError::GatewayNotSubmitted(
            GatewayNotSubmittedError::AccountModeMismatch {
                required: GatewayAccountMode::Live,
                observed: GatewayAccountMode::Test,
                ..
            }
        )
    ));
    let persisted: (String, Option<String>, Option<DateTime<Utc>>) = sqlx::query_as(
        "SELECT status, resolution_code, submitted_at FROM billing_payment_attempts WHERE id = $1",
    )
    .bind(fixture.command.attempt_id().as_uuid())
    .fetch_one(&fixture.database.pool)
    .await?;
    assert_eq!(persisted.0, "failed");
    assert_eq!(
        persisted.1.as_deref(),
        Some(PaymentResolutionCode::GatewayLiveReadinessFailedBeforeSubmission.as_str())
    );
    assert!(persisted.2.is_none());
    assert_eq!(gateway.account_mode_calls.load(Ordering::SeqCst), 2);
    assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 0);
    fixture.cleanup().await
}

#[tokio::test]
async fn prepared_test_attempt_cannot_resume_under_live_service_mode() -> Result<(), Box<dyn Error>>
{
    let fixture = enrollment_fixture("mode_retry_guard", false, false, false).await?;
    let gateway = Arc::new(ScriptedGateway::for_sale_with_readiness(
        [Err(GatewayError::Unavailable(GatewayDiagnostic::new(
            "temporary readiness failure",
        )))],
        Ok(approved_outcome("txn_cross_mode_must_not_submit")),
    ));
    let resolver = Arc::new(StaticResolver {
        gateway: scripted_resolved_gateway(fixture.gateway_account, Arc::clone(&gateway)),
        calls: AtomicUsize::new(0),
    });
    let test_service = SubscriptionBillingService::new(
        fixture.database.pool.clone(),
        Arc::new(TestOfferStore),
        resolver.clone(),
        Arc::new(PermitAdmission {
            calls: AtomicUsize::new(0),
        }),
        Arc::new(fixture.coordinator.clone()),
    )
    .with_required_gateway_account_mode(GatewayAccountMode::Test);
    assert!(matches!(
        test_service.enroll(fixture.command.clone()).await,
        Err(SubscriptionBillingServiceError::GatewayReadiness(
            GatewayError::Unavailable(_)
        ))
    ));

    let live_service = SubscriptionBillingService::new(
        fixture.database.pool.clone(),
        Arc::new(TestOfferStore),
        resolver,
        Arc::new(PermitAdmission {
            calls: AtomicUsize::new(0),
        }),
        Arc::new(fixture.coordinator.clone()),
    );
    assert!(matches!(
        live_service.enroll(fixture.command.clone()).await,
        Err(SubscriptionBillingServiceError::GatewayConfigurationChanged)
    ));
    let persisted: (String, String, Option<DateTime<Utc>>) = sqlx::query_as(
        "SELECT required_gateway_account_mode, status, submitted_at FROM billing_payment_attempts WHERE id = $1",
    )
    .bind(fixture.command.attempt_id().as_uuid())
    .fetch_one(&fixture.database.pool)
    .await?;
    assert_eq!(persisted, ("test".to_owned(), "pending".to_owned(), None));
    assert_eq!(gateway.account_mode_calls.load(Ordering::SeqCst), 1);
    assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 0);
    fixture.cleanup().await
}

#[tokio::test]
async fn foreground_service_submits_when_test_mode_is_required_and_observed()
-> Result<(), Box<dyn Error>> {
    let fixture = enrollment_fixture("mode_test_ok", false, false, false).await?;
    let gateway = Arc::new(ScriptedGateway::for_sale_with_readiness(
        [Ok(GatewayAccountMode::Test), Ok(GatewayAccountMode::Test)],
        Ok(approved_outcome("txn_test_mode_allowed")),
    ));
    let service = SubscriptionBillingService::new(
        fixture.database.pool.clone(),
        Arc::new(TestOfferStore),
        Arc::new(StaticResolver {
            gateway: scripted_resolved_gateway(fixture.gateway_account, Arc::clone(&gateway)),
            calls: AtomicUsize::new(0),
        }),
        Arc::new(PermitAdmission {
            calls: AtomicUsize::new(0),
        }),
        Arc::new(fixture.coordinator.clone()),
    )
    .with_required_gateway_account_mode(GatewayAccountMode::Test);

    let result = service.enroll(fixture.command.clone()).await?;
    assert_eq!(result.attempt().status(), PaymentAttemptStatus::Approved);
    let subscription = result
        .subscription()
        .expect("approved test enrollment creates a subscription");
    let subscription_id = subscription.id();
    assert_eq!(
        subscription.required_gateway_account_mode(),
        GatewayAccountMode::Test
    );
    assert!(
        result
            .attempt()
            .state()
            .timestamps()
            .submitted_at()
            .is_some()
    );
    let persisted_modes: (String, String) = sqlx::query_as(
        r#"
        SELECT attempts.required_gateway_account_mode,
            subscriptions.required_gateway_account_mode
        FROM billing_payment_attempts AS attempts
        JOIN billing_subscriptions AS subscriptions
            ON subscriptions.id = attempts.subscription_id
        WHERE attempts.id = $1
        "#,
    )
    .bind(fixture.command.attempt_id().as_uuid())
    .fetch_one(&fixture.database.pool)
    .await?;
    assert_eq!(persisted_modes, ("test".to_owned(), "test".to_owned()));
    assert_eq!(gateway.account_mode_calls.load(Ordering::SeqCst), 2);
    assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 1);

    let due_at = Utc::now() - ChronoDuration::hours(1);
    let due_at: DateTime<Utc> = sqlx::query_scalar(
        r#"
        UPDATE billing_subscriptions
        SET current_period_start_at = $2 - interval '1 month',
            current_period_end_at = $2,
            next_renewal_at = $2,
            next_payment_attempt_at = $2,
            updated_at = clock_timestamp()
        WHERE id = $1
        RETURNING next_renewal_at
        "#,
    )
    .bind(subscription_id.as_uuid())
    .bind(due_at)
    .fetch_one(&fixture.database.pool)
    .await?;
    let dispatch = due_renewals(&fixture.database.pool)
        .await?
        .into_iter()
        .find(|dispatch| dispatch.subscription_id() == subscription_id)
        .expect("test subscription is dispatched with its durable mode");
    assert_eq!(
        dispatch.required_gateway_account_mode(),
        GatewayAccountMode::Test
    );

    let live_gateway = Arc::new(ScriptedGateway::for_sale_with_readiness(
        [Ok(GatewayAccountMode::Live), Ok(GatewayAccountMode::Live)],
        Ok(approved_outcome("txn_cross_mode_renewal_must_not_run")),
    ));
    let live_resolved_gateway =
        scripted_resolved_gateway(fixture.gateway_account, Arc::clone(&live_gateway));
    let mut transaction = fixture.database.pool.begin().await?;
    let low_level_mismatch = reserve_subscription_renewal_in_transaction(
        &mut transaction,
        ChargeRenewal::new(fixture.command.billing_scope_id(), subscription_id, due_at),
        &live_resolved_gateway,
        GatewayAccountMode::Live,
    )
    .await?;
    transaction.rollback().await?;
    assert!(matches!(
        low_level_mismatch,
        SubscriptionRenewalReservationOutcome::Rejected(
            SubscriptionRenewalReservationRejection::GatewayAccountModeChanged
        )
    ));
    let live_resolver = Arc::new(StaticResolver {
        gateway: live_resolved_gateway,
        calls: AtomicUsize::new(0),
    });
    let live_service = SubscriptionBillingService::new(
        fixture.database.pool.clone(),
        Arc::new(TestOfferStore),
        live_resolver.clone(),
        Arc::new(PermitAdmission {
            calls: AtomicUsize::new(0),
        }),
        Arc::new(fixture.coordinator.clone()),
    );
    assert!(matches!(
        live_service
            .renew(ChargeRenewal::new(
                fixture.command.billing_scope_id(),
                subscription_id,
                due_at,
            ))
            .await,
        Err(SubscriptionBillingServiceError::GatewayConfigurationChanged)
    ));
    assert_eq!(live_resolver.calls.load(Ordering::SeqCst), 0);
    assert_eq!(live_gateway.account_mode_calls.load(Ordering::SeqCst), 0);
    assert_eq!(live_gateway.sale_calls.load(Ordering::SeqCst), 0);
    let renewal_attempts: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM billing_payment_attempts WHERE subscription_id = $1 AND attempt_kind = 'subscription_renewal'",
    )
    .bind(subscription_id.as_uuid())
    .fetch_one(&fixture.database.pool)
    .await?;
    assert_eq!(renewal_attempts, 0);
    fixture.cleanup().await
}

#[tokio::test]
async fn foreground_service_rejects_live_mode_when_test_mode_is_required()
-> Result<(), Box<dyn Error>> {
    let fixture = enrollment_fixture("mode_test_guard", false, false, false).await?;
    let gateway = Arc::new(ScriptedGateway::for_sale_with_readiness(
        [Ok(GatewayAccountMode::Live)],
        Ok(approved_outcome("txn_test_guard_must_not_submit")),
    ));
    let service = SubscriptionBillingService::new(
        fixture.database.pool.clone(),
        Arc::new(TestOfferStore),
        Arc::new(StaticResolver {
            gateway: scripted_resolved_gateway(fixture.gateway_account, Arc::clone(&gateway)),
            calls: AtomicUsize::new(0),
        }),
        Arc::new(PermitAdmission {
            calls: AtomicUsize::new(0),
        }),
        Arc::new(fixture.coordinator.clone()),
    )
    .with_required_gateway_account_mode(GatewayAccountMode::Test);

    let result = service.enroll(fixture.command.clone()).await?;
    assert_eq!(result.attempt().status(), PaymentAttemptStatus::Failed);
    assert_eq!(
        result.attempt().state().resolution_code(),
        Some(PaymentResolutionCode::GatewayTestReadinessFailedBeforeSubmission)
    );
    assert!(
        result
            .attempt()
            .state()
            .timestamps()
            .submitted_at()
            .is_none()
    );
    assert_eq!(gateway.account_mode_calls.load(Ordering::SeqCst), 1);
    assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 0);
    fixture.cleanup().await
}
