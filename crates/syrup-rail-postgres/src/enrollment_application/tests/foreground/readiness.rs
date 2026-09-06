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

include!("readiness/recovery_and_modes.rs");
