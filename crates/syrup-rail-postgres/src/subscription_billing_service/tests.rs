use super::*;

#[test]
fn billing_service_error_name_and_generic_messages_cover_the_whole_facade() {
    let sql = SubscriptionBillingServiceError::Sql(sqlx::Error::RowNotFound);
    assert_eq!(sql.to_string(), "subscription billing storage failed");
    assert_eq!(format!("{sql:?}"), "SubscriptionBillingServiceError::Sql");

    let attempt = SubscriptionBillingServiceError::Attempt(PaymentAttemptStoreError::InvalidState(
        "test payment attempt state",
    ));
    assert_eq!(attempt.to_string(), "payment attempt storage failed");
    assert_eq!(
        format!("{attempt:?}"),
        "SubscriptionBillingServiceError::Attempt"
    );

    let application = SubscriptionBillingServiceError::Application(
        SubscriptionEnrollmentApplicationError::InvalidState("test application state"),
    );
    assert_eq!(
        application.to_string(),
        "subscription payment application failed"
    );
    assert_eq!(
        format!("{application:?}"),
        "SubscriptionBillingServiceError::Application"
    );
}

#[test]
fn subscriber_admission_mapping_preserves_each_error_variant() {
    assert!(map_subscriber_mutation_admission(EndUserMutationAdmissionResult::Allowed).is_ok());
    let retry_after = std::time::Duration::from_secs(7);
    assert!(matches!(
        map_subscriber_mutation_admission(EndUserMutationAdmissionResult::Denied {
            retry_after: syrup_rail::EndUserMutationRetryAfter::new(retry_after)
                .expect("positive retry-after"),
        }),
        Err(SubscriptionBillingServiceError::AdmissionDenied {
            retry_after: actual
        }) if actual == retry_after
    ));
    assert!(matches!(
        map_subscriber_mutation_admission(EndUserMutationAdmissionResult::Timeout),
        Err(SubscriptionBillingServiceError::AdmissionTimeout)
    ));
    assert!(matches!(
        map_subscriber_mutation_admission(EndUserMutationAdmissionResult::Unavailable),
        Err(SubscriptionBillingServiceError::AdmissionUnavailable)
    ));
}

#[test]
fn subscriber_readiness_failure_preserves_codes_cooldowns_and_diagnostics() {
    for (scope, code, detail) in [
        (
            GatewayMutationCooldownScope::Account,
            PaymentResolutionCode::GatewayAccountMutationCooldownBeforeSubmission,
            "gateway account mutation cooldown is active",
        ),
        (
            GatewayMutationCooldownScope::Provider,
            PaymentResolutionCode::GatewayProviderRateLimitedBeforeSubmission,
            "gateway provider cooldown is active",
        ),
    ] {
        let failure = SubscriberReadinessFailure::Cooldown(scope);
        assert_eq!(failure.resolution_code(), code);
        assert!(failure.cooldown().is_none());
        assert_eq!(failure.cooldown_error_scope(), Some(scope));
        assert_eq!(failure.into_detail().expose(), detail);
    }

    let provider = SubscriberReadinessFailure::ProviderRateLimited(GatewayDiagnostic::new(
        "provider asked to retry later",
    ));
    assert_eq!(
        provider.resolution_code(),
        PaymentResolutionCode::GatewayProviderRateLimitedBeforeSubmission
    );
    assert!(matches!(
        provider.cooldown(),
        Some(RateLimitCooldown::Provider)
    ));
    assert_eq!(
        provider.cooldown_error_scope(),
        Some(GatewayMutationCooldownScope::Provider)
    );
    assert_eq!(
        provider.into_detail().expose(),
        "provider asked to retry later"
    );

    let readiness = SubscriberReadinessFailure::LiveModeUnavailable;
    assert_eq!(
        readiness.resolution_code(),
        PaymentResolutionCode::GatewayLiveReadinessFailedBeforeSubmission
    );
    assert!(readiness.cooldown().is_none());
    assert!(readiness.cooldown_error_scope().is_none());
    assert_eq!(readiness.into_detail().expose(), LIVE_READINESS_FAILED_TEXT);
}

#[test]
fn expected_gateway_identity_requires_every_resolved_component() {
    let provider_key = GatewayProviderKey::new("nmi").expect("valid provider key");
    let account = GatewayAccountSnapshot {
        account_id: GatewayAccountId::new(uuid::Uuid::from_u128(1)),
        provider_key: provider_key.clone(),
    };
    let billing_scope_id = BillingScopeId::new(uuid::Uuid::from_u128(2));
    let gateway_configuration_id =
        syrup_rail::GatewayConfigurationId::new(uuid::Uuid::from_u128(3));
    let expected =
        ExpectedGatewayIdentity::for_account(billing_scope_id, gateway_configuration_id, &account);

    assert!(expected.matches_components(
        billing_scope_id,
        account.account_id,
        gateway_configuration_id,
        &provider_key,
    ));
    assert!(!expected.matches_components(
        BillingScopeId::new(uuid::Uuid::from_u128(4)),
        account.account_id,
        gateway_configuration_id,
        &provider_key,
    ));
    assert!(!expected.matches_components(
        billing_scope_id,
        GatewayAccountId::new(uuid::Uuid::from_u128(5)),
        gateway_configuration_id,
        &provider_key,
    ));
    assert!(!expected.matches_components(
        billing_scope_id,
        account.account_id,
        syrup_rail::GatewayConfigurationId::new(uuid::Uuid::from_u128(6)),
        &provider_key,
    ));
    assert!(!expected.matches_components(
        billing_scope_id,
        account.account_id,
        gateway_configuration_id,
        &GatewayProviderKey::new("other_gateway").expect("valid provider key"),
    ));
}
