use super::*;
use std::{error::Error as _, io};

#[test]
fn billing_service_error_name_and_generic_messages_cover_the_whole_facade() {
    let sql = SubscriptionBillingServiceError::Sql(sqlx::Error::RowNotFound);
    assert_eq!(sql.to_string(), "subscription billing storage failed");
    assert_eq!(format!("{sql:?}"), "SubscriptionBillingServiceError::Sql");

    let transient =
        SubscriptionBillingServiceError::StorageTemporarilyUnavailable(sqlx::Error::PoolTimedOut);
    assert_eq!(
        transient.to_string(),
        "subscription billing storage is temporarily unavailable"
    );
    assert_eq!(
        format!("{transient:?}"),
        "SubscriptionBillingServiceError::StorageTemporarilyUnavailable"
    );
    assert!(transient.source().is_some());

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

    let cancellation = SubscriptionBillingServiceError::Cancellation(
        crate::SubscriptionCancellationError::InvalidState("test cancellation state"),
    );
    assert_eq!(cancellation.to_string(), "subscription cancellation failed");
    assert_eq!(
        format!("{cancellation:?}"),
        "SubscriptionBillingServiceError::Cancellation"
    );
    assert!(cancellation.source().is_some());

    let discount = SubscriptionBillingServiceError::Discount(
        crate::SubscriptionDiscountOperationError::InvalidState("test discount state"),
    );
    assert_eq!(
        discount.to_string(),
        "subscription discount operation failed"
    );
    assert_eq!(
        format!("{discount:?}"),
        "SubscriptionBillingServiceError::Discount"
    );
    assert!(discount.source().is_some());

    let transaction = SubscriptionBillingServiceError::BillingTransaction(
        crate::BillingTransactionError::new(io::Error::other("test transaction failure")),
    );
    assert_eq!(transaction.to_string(), "host billing transaction failed");
    assert_eq!(
        format!("{transaction:?}"),
        "SubscriptionBillingServiceError::BillingTransaction"
    );
    assert!(transaction.source().is_some());

    let event = SubscriptionBillingServiceError::BillingEvent(crate::BillingEventWriteError::new(
        io::Error::other("test event failure"),
    ));
    assert_eq!(event.to_string(), "host billing event append failed");
    assert_eq!(
        format!("{event:?}"),
        "SubscriptionBillingServiceError::BillingEvent"
    );
    assert!(event.source().is_some());
}

#[test]
fn service_error_disposition_matrix_covers_each_current_variant() {
    macro_rules! assert_disposition_matrix {
        ($($error:expr => $expected:expr),+ $(,)?) => {
            $(
                let error = $error;
                assert_eq!(
                    error.disposition(),
                    $expected,
                    "unexpected disposition for {}",
                    stringify!($error),
                );
            )+
        };
    }

    use SubscriptionBillingServiceErrorDisposition::{
        Conflict, Internal, Misconfigured, Rejected, TemporarilyUnavailable,
    };

    // `SubscriptionBillingServiceError::disposition` and its nested helpers
    // intentionally have exhaustive matches with no wildcard arms. Adding a
    // service or mapped nested variant therefore fails compilation until this
    // matrix receives an explicit policy decision.
    assert_disposition_matrix!(
        SubscriptionBillingServiceError::Sql(sqlx::Error::RowNotFound) => Internal,
        SubscriptionBillingServiceError::StorageTemporarilyUnavailable(sqlx::Error::PoolTimedOut) => TemporarilyUnavailable,
        SubscriptionBillingServiceError::Attempt(PaymentAttemptStoreError::InvalidState("test")) => Internal,
        SubscriptionBillingServiceError::Application(SubscriptionEnrollmentApplicationError::InvalidState("test")) => Internal,
        SubscriptionBillingServiceError::HostChargeApplication(HostChargeApplicationError::InvalidState("test")) => Internal,
        SubscriptionBillingServiceError::HostChargeStore(HostChargeStoreError::InvalidState) => Internal,
        SubscriptionBillingServiceError::Cancellation(crate::SubscriptionCancellationError::Sql(sqlx::Error::RowNotFound)) => Internal,
        SubscriptionBillingServiceError::Cancellation(crate::SubscriptionCancellationError::InvalidState("test")) => Internal,
        SubscriptionBillingServiceError::Discount(crate::SubscriptionDiscountOperationError::Sql(sqlx::Error::RowNotFound)) => Internal,
        SubscriptionBillingServiceError::Discount(crate::SubscriptionDiscountOperationError::OfferUnavailable) => Misconfigured,
        SubscriptionBillingServiceError::Discount(crate::SubscriptionDiscountOperationError::OfferPlanMismatch) => Conflict,
        SubscriptionBillingServiceError::Discount(crate::SubscriptionDiscountOperationError::InvalidConfiguration) => Misconfigured,
        SubscriptionBillingServiceError::Discount(crate::SubscriptionDiscountOperationError::LimitedDiscountCadence) => Misconfigured,
        SubscriptionBillingServiceError::Discount(crate::SubscriptionDiscountOperationError::InvalidState("test")) => Internal,
        SubscriptionBillingServiceError::BillingTransaction(crate::BillingTransactionError::new(io::Error::other("test"))) => Internal,
        SubscriptionBillingServiceError::BillingEvent(crate::BillingEventWriteError::new(io::Error::other("test"))) => Internal,
        SubscriptionBillingServiceError::HostChargeUnavailable => Misconfigured,
        SubscriptionBillingServiceError::IdempotencyConflict => Conflict,
        SubscriptionBillingServiceError::AdmissionDenied { retry_after: std::time::Duration::from_secs(7) } => TemporarilyUnavailable,
        SubscriptionBillingServiceError::AdmissionTimeout => TemporarilyUnavailable,
        SubscriptionBillingServiceError::AdmissionUnavailable => TemporarilyUnavailable,
        SubscriptionBillingServiceError::GatewayConfigurationChanged => Conflict,
        SubscriptionBillingServiceError::GatewayResolution(GatewayResolutionError::NotFound) => Misconfigured,
        SubscriptionBillingServiceError::GatewayResolution(GatewayResolutionError::ConfigurationChanged) => Conflict,
        SubscriptionBillingServiceError::GatewayResolution(GatewayResolutionError::InvalidConfiguration) => Misconfigured,
        SubscriptionBillingServiceError::GatewayResolution(GatewayResolutionError::Unavailable) => TemporarilyUnavailable,
        SubscriptionBillingServiceError::ResolvedGatewayIdentityMismatch => Internal,
        SubscriptionBillingServiceError::GatewayMutationCooldown { scope: GatewayMutationCooldownScope::Account } => TemporarilyUnavailable,
        SubscriptionBillingServiceError::GatewayMutationCooldown { scope: GatewayMutationCooldownScope::Provider } => TemporarilyUnavailable,
        SubscriptionBillingServiceError::ReservationRejected(SubscriptionEnrollmentReservationRejection::CurrentSubscription) => Rejected,
        SubscriptionBillingServiceError::ReservationRejected(SubscriptionEnrollmentReservationRejection::ActiveGrant) => Rejected,
        SubscriptionBillingServiceError::ReservationRejected(SubscriptionEnrollmentReservationRejection::UnresolvedProcessorCharge) => Rejected,
        SubscriptionBillingServiceError::ReservationRejected(SubscriptionEnrollmentReservationRejection::EnrollmentTermsChanged) => Conflict,
        SubscriptionBillingServiceError::ReservationRejected(SubscriptionEnrollmentReservationRejection::GatewayConfigurationChanged) => Conflict,
        SubscriptionBillingServiceError::ReservationRejected(SubscriptionEnrollmentReservationRejection::AttemptInProgress) => Rejected,
        SubscriptionBillingServiceError::SubmissionRejected(SubscriptionEnrollmentSubmissionRejection::BillingStateChanged) => Conflict,
        SubscriptionBillingServiceError::SubmissionRejected(SubscriptionEnrollmentSubmissionRejection::EnrollmentTermsChanged) => Conflict,
        SubscriptionBillingServiceError::SubmissionRejected(SubscriptionEnrollmentSubmissionRejection::GatewayConfigurationChanged) => Conflict,
        SubscriptionBillingServiceError::HostChargeReservationRejected(HostChargeTargetRejection::TargetUnavailable) => Rejected,
        SubscriptionBillingServiceError::HostChargeReservationRejected(HostChargeTargetRejection::ChargeChanged) => Conflict,
        SubscriptionBillingServiceError::HostChargeReservationRejected(HostChargeTargetRejection::LedgerUnsafe) => Rejected,
        SubscriptionBillingServiceError::HostChargeSubmissionRejected(HostChargeTargetRejection::TargetUnavailable) => Rejected,
        SubscriptionBillingServiceError::HostChargeSubmissionRejected(HostChargeTargetRejection::ChargeChanged) => Conflict,
        SubscriptionBillingServiceError::HostChargeSubmissionRejected(HostChargeTargetRejection::LedgerUnsafe) => Rejected,
        SubscriptionBillingServiceError::RecoveryReservationRejected(SubscriptionRecoveryReservationRejection::SubscriptionNotFound) => Rejected,
        SubscriptionBillingServiceError::RecoveryReservationRejected(SubscriptionRecoveryReservationRejection::PaymentNotDue) => Rejected,
        SubscriptionBillingServiceError::RecoveryReservationRejected(SubscriptionRecoveryReservationRejection::AttemptInProgress) => Rejected,
        SubscriptionBillingServiceError::RecoveryReservationRejected(SubscriptionRecoveryReservationRejection::PaymentMethodUpdateInProgress) => Rejected,
        SubscriptionBillingServiceError::RecoveryReservationRejected(SubscriptionRecoveryReservationRejection::GatewayConfigurationChanged) => Conflict,
        SubscriptionBillingServiceError::RecoverySubmissionRejected(SubscriptionRecoverySubmissionRejection::BillingStateChanged) => Conflict,
        SubscriptionBillingServiceError::RecoverySubmissionRejected(SubscriptionRecoverySubmissionRejection::GatewayConfigurationChanged) => Conflict,
        SubscriptionBillingServiceError::RenewalReservationRejected(SubscriptionRenewalReservationRejection::SubscriptionNotFound) => Rejected,
        SubscriptionBillingServiceError::RenewalReservationRejected(SubscriptionRenewalReservationRejection::PaymentNotDue) => Rejected,
        SubscriptionBillingServiceError::RenewalReservationRejected(SubscriptionRenewalReservationRejection::AttemptInProgress) => Rejected,
        SubscriptionBillingServiceError::RenewalReservationRejected(SubscriptionRenewalReservationRejection::PaymentMethodUpdateInProgress) => Rejected,
        SubscriptionBillingServiceError::RenewalReservationRejected(SubscriptionRenewalReservationRejection::RetryBlocked) => Rejected,
        SubscriptionBillingServiceError::RenewalReservationRejected(SubscriptionRenewalReservationRejection::GatewayConfigurationChanged) => Conflict,
        SubscriptionBillingServiceError::PaymentMethodReplacementReservationRejected(SubscriptionPaymentMethodReplacementRejection::SubscriptionNotFound) => Rejected,
        SubscriptionBillingServiceError::PaymentMethodReplacementReservationRejected(SubscriptionPaymentMethodReplacementRejection::SubscriptionIneligible) => Rejected,
        SubscriptionBillingServiceError::PaymentMethodReplacementReservationRejected(SubscriptionPaymentMethodReplacementRejection::ChargeAttemptInProgress) => Rejected,
        SubscriptionBillingServiceError::PaymentMethodReplacementReservationRejected(SubscriptionPaymentMethodReplacementRejection::PaymentMethodUpdateInProgress) => Rejected,
        SubscriptionBillingServiceError::PaymentMethodReplacementReservationRejected(SubscriptionPaymentMethodReplacementRejection::GatewayConfigurationChanged) => Conflict,
        SubscriptionBillingServiceError::PaymentMethodReplacementSubmissionRejected(SubscriptionPaymentMethodReplacementSubmissionRejection::BillingStateChanged) => Conflict,
        SubscriptionBillingServiceError::PaymentMethodReplacementSubmissionRejected(SubscriptionPaymentMethodReplacementSubmissionRejection::GatewayConfigurationChanged) => Conflict,
        SubscriptionBillingServiceError::GatewayNotSubmitted(GatewayNotSubmittedError::RequestRejected(GatewayDiagnostic::new("test"))) => Rejected,
        SubscriptionBillingServiceError::GatewayNotSubmitted(GatewayNotSubmittedError::Malformed(GatewayDiagnostic::new("test"))) => Internal,
        SubscriptionBillingServiceError::GatewayNotSubmitted(GatewayNotSubmittedError::Configuration(GatewayDiagnostic::new("test"))) => Misconfigured,
        SubscriptionBillingServiceError::GatewayNotSubmitted(GatewayNotSubmittedError::Unavailable(GatewayDiagnostic::new("test"))) => TemporarilyUnavailable,
        SubscriptionBillingServiceError::GatewayNotSubmitted(GatewayNotSubmittedError::RateLimited(GatewayDiagnostic::new("test"))) => TemporarilyUnavailable,
        SubscriptionBillingServiceError::GatewayReadiness(GatewayError::RequestRejected(GatewayDiagnostic::new("test"))) => Rejected,
        SubscriptionBillingServiceError::GatewayReadiness(GatewayError::Malformed(GatewayDiagnostic::new("test"))) => Internal,
        SubscriptionBillingServiceError::GatewayReadiness(GatewayError::Configuration(GatewayDiagnostic::new("test"))) => Misconfigured,
        SubscriptionBillingServiceError::GatewayReadiness(GatewayError::Unavailable(GatewayDiagnostic::new("test"))) => TemporarilyUnavailable,
        SubscriptionBillingServiceError::GatewayReadiness(GatewayError::RateLimited(GatewayDiagnostic::new("test"))) => TemporarilyUnavailable,
        SubscriptionBillingServiceError::InvalidState("test") => Internal,
    );
}

#[test]
fn provider_free_transaction_retry_policy_is_explicit_and_conservative() {
    assert!(is_retryable_provider_free_transaction_error(
        &sqlx::Error::PoolTimedOut
    ));
    assert!(!is_retryable_provider_free_transaction_error(
        &sqlx::Error::RowNotFound
    ));
    for sqlstate in ["40001", "40P01", "55P03", "57014"] {
        assert!(
            is_retryable_provider_free_transaction_sqlstate(sqlstate),
            "expected {sqlstate} to be retryable"
        );
    }
    for sqlstate in ["00000", "23505", "23514", "42P01", "XX000"] {
        assert!(
            !is_retryable_provider_free_transaction_sqlstate(sqlstate),
            "expected {sqlstate} to remain internal"
        );
    }
}

#[test]
fn provider_free_transaction_mapping_keeps_unrecognized_storage_faults_internal() {
    assert!(matches!(
        provider_free_transaction_error(sqlx::Error::RowNotFound),
        SubscriptionBillingServiceError::Sql(sqlx::Error::RowNotFound)
    ));
    assert!(matches!(
        SubscriptionBillingServiceError::from(crate::SubscriptionCancellationError::Sql(
            sqlx::Error::RowNotFound,
        )),
        SubscriptionBillingServiceError::Cancellation(crate::SubscriptionCancellationError::Sql(
            sqlx::Error::RowNotFound
        ))
    ));
    assert!(matches!(
        SubscriptionBillingServiceError::from(crate::SubscriptionDiscountOperationError::Sql(
            sqlx::Error::RowNotFound
        ),),
        SubscriptionBillingServiceError::Discount(crate::SubscriptionDiscountOperationError::Sql(
            sqlx::Error::RowNotFound
        ))
    ));
}

#[test]
fn service_error_retry_helpers_preserve_exact_admission_delay_only() {
    let retry_after = std::time::Duration::from_secs(7);
    let denied = SubscriptionBillingServiceError::AdmissionDenied { retry_after };
    assert_eq!(
        denied.disposition(),
        SubscriptionBillingServiceErrorDisposition::TemporarilyUnavailable
    );
    assert!(denied.is_retryable());
    assert!(!denied.is_conflict());
    assert_eq!(denied.retry_after(), Some(retry_after));

    for scope in [
        GatewayMutationCooldownScope::Account,
        GatewayMutationCooldownScope::Provider,
    ] {
        let cooldown = SubscriptionBillingServiceError::GatewayMutationCooldown { scope };
        assert!(cooldown.is_retryable());
        assert_eq!(cooldown.retry_after(), None);
    }

    let unavailable = SubscriptionBillingServiceError::GatewayNotSubmitted(
        GatewayNotSubmittedError::Unavailable(GatewayDiagnostic::new("test")),
    );
    assert!(unavailable.is_retryable());
    assert_eq!(unavailable.retry_after(), None);

    let conflict = SubscriptionBillingServiceError::GatewayConfigurationChanged;
    assert!(conflict.is_conflict());
    assert!(!conflict.is_retryable());
    assert_eq!(conflict.retry_after(), None);
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
