use super::*;

#[tokio::test]
async fn cooldown_persistence_distinguishes_rotation_from_missing_provider_state()
-> Result<(), Box<dyn std::error::Error>> {
    let database = TestDatabase::start("cooldown_binding").await?;
    let result = async {
        let account = create_gateway_account(&database.pool, "nmi").await?;
        create_gateway_account(&database.pool, "other_gateway").await?;
        let identity = PaymentAttemptIdentity::new(
            PaymentAttemptId::new(Uuid::now_v7()),
            BillingScopeId::new(account.billing_scope_id),
            SubscriberId::new(Uuid::now_v7()),
            GatewayAccountId::new(account.gateway_account_id),
            GatewayConfigurationId::new(account.gateway_configuration_id),
            GatewayAccountMode::Live,
        );
        assert_eq!(
            commit_rate_limit_cooldown_for_identity(
                &database.pool,
                identity,
                &GatewayProviderKey::new("nmi")?,
                RateLimitCooldown::Provider,
            )
            .await?,
            RateLimitCooldownCommitDisposition::IdentityNotDurable,
        );
        let mut connection = database.pool.acquire().await?;
        assert_eq!(
            persist_rate_limit_cooldown(
                &mut connection,
                identity,
                &GatewayProviderKey::new("other_gateway")?,
                RateLimitCooldown::Provider,
            )
            .await?,
            RateLimitCooldownPersistence::IdentityChanged,
        );
        let incorrectly_throttled: bool = sqlx::query_scalar(
            "SELECT rate_limited_until > clock_timestamp() \
             FROM billing_gateway_provider_rate_limits WHERE provider_key = 'other_gateway'",
        )
        .fetch_one(&mut *connection)
        .await?;
        assert!(!incorrectly_throttled);
        assert_eq!(
            persist_rate_limit_cooldown(
                &mut connection,
                identity,
                &GatewayProviderKey::new("nmi")?,
                RateLimitCooldown::Account,
            )
            .await?,
            RateLimitCooldownPersistence::Applied,
        );

        sqlx::query(
            "UPDATE billing_gateway_accounts SET gateway_configuration_id = $2 WHERE id = $1",
        )
        .bind(account.gateway_account_id)
        .bind(Uuid::now_v7())
        .execute(&mut *connection)
        .await?;

        assert_eq!(
            persist_rate_limit_cooldown(
                &mut connection,
                identity,
                &GatewayProviderKey::new("nmi")?,
                RateLimitCooldown::Provider,
            )
            .await?,
            RateLimitCooldownPersistence::Applied,
        );
        assert_eq!(
            persist_rate_limit_cooldown(
                &mut connection,
                identity,
                &GatewayProviderKey::new("nmi")?,
                RateLimitCooldown::Account,
            )
            .await?,
            RateLimitCooldownPersistence::IdentityChanged,
        );
        let correctly_throttled: bool = sqlx::query_scalar(
            "SELECT rate_limited_until > clock_timestamp() \
             FROM billing_gateway_provider_rate_limits WHERE provider_key = 'nmi'",
        )
        .fetch_one(&mut *connection)
        .await?;
        assert!(correctly_throttled);

        sqlx::query(
            "ALTER TABLE billing_gateway_accounts DROP CONSTRAINT billing_gateway_accounts_provider_fk",
        )
        .execute(&mut *connection)
        .await?;
        sqlx::query("DELETE FROM billing_gateway_provider_rate_limits WHERE provider_key = 'nmi'")
            .execute(&mut *connection)
            .await?;
        assert_eq!(
            persist_rate_limit_cooldown(
                &mut connection,
                identity,
                &GatewayProviderKey::new("nmi")?,
                RateLimitCooldown::Provider,
            )
            .await?,
            RateLimitCooldownPersistence::MissingProviderCooldown,
        );
        Ok::<_, Box<dyn std::error::Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}

fn initial_attempt_for_matching(
    identity: PaymentAttemptIdentity,
    plan_key: PlanKey,
    gateway_order_id: GatewayOrderId,
    idempotency_key: &str,
) -> PaymentAttempt {
    let timestamp = Utc::now();
    let request = PaymentAttemptRequest::new(
        PaymentAttemptTarget::SubscriptionInitial {
            terms_version: syrup_rail::SubscriptionEnrollmentTermsVersion::V2,
            offer: immediate_offer(
                plan_key,
                ChargeAmount::new(1_000, CurrencyCode::new("USD").unwrap()).unwrap(),
            ),
            discount: None,
            application: None,
        },
        IdempotencyKey::new(idempotency_key).expect("test idempotency key"),
        PaymentAttemptFingerprint::new(format!("fingerprint-{idempotency_key}"))
            .expect("test fingerprint"),
        Money::new(1_000, CurrencyCode::new("USD").expect("test currency")).expect("test amount"),
        gateway_order_id,
        BillingContactSnapshot::new(None, None),
    );
    PaymentAttempt::new(
        identity,
        request,
        PaymentAttemptState::new(
            PaymentAttemptStatus::Pending,
            None,
            ProcessorEvidence::default(),
            PaymentAttemptLifecycle::default(),
            PaymentAttemptTimestamps::new(None, None, None, timestamp, timestamp),
        ),
    )
    .expect("valid test initial attempt")
}

#[test]
fn reservation_attempt_matching_preserves_initial_and_exact_rules() {
    assert_eq!(
        ReservationOperation::Initial.expected_kind(),
        PaymentAttemptKind::SubscriptionInitial
    );
    assert_eq!(
        ReservationOperation::Recovery.expected_kind(),
        PaymentAttemptKind::SubscriptionRecovery
    );
    assert_eq!(
        ReservationOperation::Renewal.expected_kind(),
        PaymentAttemptKind::SubscriptionRenewal
    );
    assert_eq!(
        ReservationOperation::PaymentMethodReplacement.expected_kind(),
        PaymentAttemptKind::SubscriptionPaymentMethodUpdate
    );
    let identity = PaymentAttemptIdentity::new(
        PaymentAttemptId::new(Uuid::from_u128(1)),
        BillingScopeId::new(Uuid::from_u128(2)),
        SubscriberId::new(Uuid::from_u128(3)),
        GatewayAccountId::new(Uuid::from_u128(4)),
        GatewayConfigurationId::new(Uuid::from_u128(5)),
        GatewayAccountMode::Live,
    );
    let plan_key = PlanKey::new("base_subscription").expect("test plan key");
    let gateway_order_id =
        GatewayOrderId::from_correlation("matching-order").expect("test gateway order");
    let original = initial_attempt_for_matching(
        identity,
        plan_key.clone(),
        gateway_order_id.clone(),
        "matching-key-one",
    );
    let changed_request = initial_attempt_for_matching(
        identity,
        plan_key.clone(),
        gateway_order_id.clone(),
        "matching-key-two",
    );
    let different_order = initial_attempt_for_matching(
        identity,
        plan_key.clone(),
        GatewayOrderId::from_correlation("other-order").expect("test gateway order"),
        "matching-key-three",
    );

    let initial = ReservationAttemptExpectation::Initial {
        identity,
        plan_key: &plan_key,
        gateway_order_id: &gateway_order_id,
    };
    assert!(initial.matches(&original));
    assert!(initial.matches(&changed_request));
    assert!(!initial.matches(&different_order));

    let exact = ReservationAttemptExpectation::Exact {
        identity,
        kind: PaymentAttemptKind::SubscriptionInitial,
        request: original.request(),
    };
    assert!(exact.matches(&original));
    assert!(!exact.matches(&changed_request));
}

#[test]
fn resolution_command_keeps_boundaries_and_replacement_review_typed() {
    let prepared = OutcomeResolutionCommand::non_approved(
        AttemptResolutionStatus::Failed,
        None,
        Some(RateLimitCooldown::Account),
        OutcomeResolutionBoundary::Prepared,
    );
    assert!(prepared.may_resolve(PaymentAttemptStatus::Pending, false));
    assert!(!prepared.may_resolve(PaymentAttemptStatus::Pending, true));
    assert!(!prepared.may_resolve(PaymentAttemptStatus::Declined, false));
    assert!(!prepared.clears_submitted_at());

    let admitted = OutcomeResolutionCommand::non_approved(
        AttemptResolutionStatus::Failed,
        None,
        None,
        OutcomeResolutionBoundary::AdmittedNotSubmitted,
    );
    assert!(!admitted.may_resolve(PaymentAttemptStatus::Pending, false));
    assert!(admitted.may_resolve(PaymentAttemptStatus::Pending, true));
    assert!(admitted.clears_submitted_at());

    let unknown = OutcomeResolutionCommand::unknown(Some(RateLimitCooldown::Provider));
    assert_eq!(
        unknown.resolved_status(
            ReservationOperation::PaymentMethodReplacement,
            PaymentAttemptStatus::ReviewRequired,
        ),
        AttemptResolutionStatus::ReviewRequired,
    );
    assert_eq!(
        unknown.resolved_status(
            ReservationOperation::Recovery,
            PaymentAttemptStatus::ReviewRequired,
        ),
        AttemptResolutionStatus::Unknown,
    );
    assert!(unknown.records_pending_evidence(AttemptResolutionStatus::Unknown));
    assert!(!unknown.records_pending_evidence(AttemptResolutionStatus::ReviewRequired));
}

#[test]
fn terminal_approval_requires_reversal_only_with_charged_transaction() {
    let attempt = initial_attempt_for_matching(
        PaymentAttemptIdentity::new(
            PaymentAttemptId::new(Uuid::from_u128(10)),
            BillingScopeId::new(Uuid::from_u128(11)),
            SubscriberId::new(Uuid::from_u128(12)),
            GatewayAccountId::new(Uuid::from_u128(13)),
            GatewayConfigurationId::new(Uuid::from_u128(14)),
            GatewayAccountMode::Live,
        ),
        PlanKey::new("base_subscription").unwrap(),
        GatewayOrderId::from_correlation("parking-policy-order").unwrap(),
        "parking-policy-key",
    );
    let charged = ProcessorEvidence::new(
        syrup_rail::ProcessorApprovalEvidence::Unclassified,
        Some(GatewayTransactionId::new("charged-transaction").unwrap()),
        None,
        None,
        None,
        None,
        None,
        GatewayPaymentDescriptor::default(),
    );
    assert_eq!(
        terminal_approved_progression(&attempt, &charged),
        ProcessorChargeProgression::ExternalReversalRequired,
    );
    assert_eq!(
        terminal_approved_progression(&attempt, &ProcessorEvidence::default()),
        ProcessorChargeProgression::ReconciliationRequired,
    );
}

#[test]
fn not_submitted_policy_classifies_every_durable_consequence_together() {
    for (error, resolution_code, cooldown, restores_prepared) in [
        (
            GatewayNotSubmittedError::RequestRejected(GatewayDiagnostic::new("request rejected")),
            PaymentResolutionCode::GatewayRequestRejectedBeforeSubmission,
            None,
            false,
        ),
        (
            GatewayNotSubmittedError::Malformed(GatewayDiagnostic::new("malformed")),
            PaymentResolutionCode::GatewayMalformedBeforeSubmission,
            None,
            false,
        ),
        (
            GatewayNotSubmittedError::Configuration(GatewayDiagnostic::new("configuration")),
            PaymentResolutionCode::GatewayConfigurationBeforeSubmission,
            None,
            false,
        ),
        (
            GatewayNotSubmittedError::NotTransmitted(GatewayDiagnostic::new("unavailable")),
            PaymentResolutionCode::GatewayUnavailableBeforeSubmission,
            None,
            true,
        ),
        (
            GatewayNotSubmittedError::RateLimited(GatewayDiagnostic::new("rate limited")),
            PaymentResolutionCode::GatewayAccountRateLimitedBeforeSubmission,
            Some(RateLimitCooldown::Account),
            false,
        ),
        (
            GatewayNotSubmittedError::AccountModeMismatch {
                required: GatewayAccountMode::Live,
                observed: GatewayAccountMode::Test,
                detail: GatewayDiagnostic::new("live mode mismatch"),
            },
            PaymentResolutionCode::GatewayLiveReadinessFailedBeforeSubmission,
            None,
            false,
        ),
        (
            GatewayNotSubmittedError::AccountModeMismatch {
                required: GatewayAccountMode::Test,
                observed: GatewayAccountMode::Live,
                detail: GatewayDiagnostic::new("test mode mismatch"),
            },
            PaymentResolutionCode::GatewayTestReadinessFailedBeforeSubmission,
            None,
            false,
        ),
        (
            GatewayNotSubmittedError::AccountModeVerification(GatewayError::RequestRejected(
                GatewayDiagnostic::new("wrapped request rejected"),
            )),
            PaymentResolutionCode::GatewayRequestRejectedBeforeSubmission,
            None,
            false,
        ),
        (
            GatewayNotSubmittedError::AccountModeVerification(GatewayError::Malformed(
                GatewayDiagnostic::new("wrapped malformed"),
            )),
            PaymentResolutionCode::GatewayMalformedBeforeSubmission,
            None,
            false,
        ),
        (
            GatewayNotSubmittedError::AccountModeVerification(GatewayError::Configuration(
                GatewayDiagnostic::new("wrapped configuration"),
            )),
            PaymentResolutionCode::GatewayConfigurationBeforeSubmission,
            None,
            false,
        ),
        (
            GatewayNotSubmittedError::AccountModeVerification(GatewayError::Unavailable(
                GatewayDiagnostic::new("wrapped unavailable"),
            )),
            PaymentResolutionCode::GatewayUnavailableBeforeSubmission,
            None,
            true,
        ),
        (
            GatewayNotSubmittedError::AccountModeVerification(GatewayError::RateLimited(
                GatewayDiagnostic::new("wrapped rate limited"),
            )),
            PaymentResolutionCode::GatewayProviderRateLimitedBeforeSubmission,
            Some(RateLimitCooldown::Provider),
            false,
        ),
    ] {
        let policy = GatewayNotSubmittedPolicy::for_error(&error);
        assert_eq!(policy.resolution_code(), resolution_code);
        assert_eq!(policy.cooldown(), cooldown);
        assert_eq!(
            policy.restores_prepared_attempt_when_supported(),
            restores_prepared
        );
    }
}

#[test]
fn not_submitted_surface_requires_a_flow_with_prepared_replay() {
    let policy = GatewayNotSubmittedPolicy::for_error(&GatewayNotSubmittedError::NotTransmitted(
        GatewayDiagnostic::new("not transmitted"),
    ));
    let attempt = initial_attempt_for_matching(
        PaymentAttemptIdentity::new(
            PaymentAttemptId::new(Uuid::from_u128(11)),
            BillingScopeId::new(Uuid::from_u128(12)),
            SubscriberId::new(Uuid::from_u128(13)),
            GatewayAccountId::new(Uuid::from_u128(14)),
            GatewayConfigurationId::new(Uuid::from_u128(15)),
            GatewayAccountMode::Live,
        ),
        PlanKey::new("base_subscription").expect("test plan key"),
        GatewayOrderId::from_correlation("not-submitted-surface").expect("test gateway order"),
        "not-submitted-surface",
    );

    assert!(should_surface_not_submitted_application(
        false,
        &attempt,
        policy,
        PreparedAttemptReplay::Supported,
    ));
    assert!(!should_surface_not_submitted_application(
        false,
        &attempt,
        policy,
        PreparedAttemptReplay::Unsupported,
    ));
}

#[test]
fn evidence_retry_policy_excludes_pool_and_non_database_errors() {
    for (codes, expected) in [
        (&["40001", "40P01", "55P03", "57014"][..], true),
        (
            &["00000", "08006", "23505", "23514", "42P01", "XX000"][..],
            false,
        ),
    ] {
        for &code in codes {
            assert_eq!(
                is_retryable_evidence_error(&SubscriptionEnrollmentApplicationError::Sql(
                    crate::test_support::sqlstate_error(code)
                )),
                expected,
                "direct {code}"
            );
            assert_eq!(
                is_retryable_evidence_error(&SubscriptionEnrollmentApplicationError::Attempt(
                    PaymentAttemptStoreError::Sql(crate::test_support::sqlstate_error(code))
                )),
                expected,
                "nested {code}"
            );
        }
    }
    for error in [sqlx::Error::PoolTimedOut, sqlx::Error::RowNotFound] {
        assert!(!is_retryable_evidence_error(
            &SubscriptionEnrollmentApplicationError::Sql(error),
        ));
    }
    assert!(!is_retryable_evidence_error(
        &SubscriptionEnrollmentApplicationError::Attempt(PaymentAttemptStoreError::Sql(
            sqlx::Error::PoolTimedOut
        ),),
    ));
}
