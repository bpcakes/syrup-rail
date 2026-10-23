//! A host-composed recovery records a failed gateway readiness check with
//! the same durable policy as `SubscriptionBillingService::recover`.

use syrup_rail::{GatewayDiagnostic, GatewayError};

use super::*;
use crate::{
    GatewayAccountModeVerificationError, SubscriptionRecoveryReadinessResolution,
    resolve_subscription_recovery_readiness_failure,
};

/// A past-due subscriber's committed, never-admitted recovery reservation.
async fn prepared_recovery(
    harness: &Harness,
    key: &str,
) -> Result<syrup_rail::SubscriptionRecoveryReservation, Box<dyn Error>> {
    let (subscriber_id, subscription_id) = harness.trial(suspend_policy()?, key).await?;
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
        &format!("{key}_decline"),
    )
    .await?;
    let command = harness.recovery(subscriber_id, &format!("{key}-recovery"))?;
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
    Ok(*reservation)
}

async fn provider_cooldown_active(harness: &Harness) -> Result<bool, Box<dyn Error>> {
    Ok(sqlx::query_scalar(
        "SELECT coalesce(bool_or(rate_limited_until > clock_timestamp()), false)
         FROM billing_gateway_provider_rate_limits",
    )
    .fetch_one(harness.pool())
    .await?)
}

async fn attempt_state(
    harness: &Harness,
    reservation: &syrup_rail::SubscriptionRecoveryReservation,
) -> Result<(String, Option<String>, Option<DateTime<Utc>>), Box<dyn Error>> {
    Ok(sqlx::query_as(
        "SELECT status, resolution_code, submitted_at FROM billing_payment_attempts
         WHERE id = $1",
    )
    .bind(reservation.identity().attempt_id().as_uuid())
    .fetch_one(harness.pool())
    .await?)
}

#[tokio::test]
async fn host_composed_recovery_records_readiness_failures_like_the_service()
-> Result<(), Box<dyn Error>> {
    let harness = Harness::start("pe_recover_ready").await?;
    let result = async {
        // A transient outage leaves the prepared attempt resumable.
        let outage = prepared_recovery(&harness, "ready_outage").await?;
        let retained = resolve_subscription_recovery_readiness_failure(
            harness.pool(),
            &outage,
            GatewayAccountModeVerificationError::Gateway(GatewayError::Unavailable(
                GatewayDiagnostic::new("account mode query unavailable"),
            )),
        )
        .await?;
        assert!(matches!(
            retained,
            SubscriptionRecoveryReadinessResolution::Retained
        ));
        assert_eq!(
            attempt_state(&harness, &outage).await?,
            ("pending".to_owned(), None, None)
        );

        // An account-mode mismatch resolves the attempt and records no
        // cooldown.
        let mismatch = prepared_recovery(&harness, "ready_mismatch").await?;
        let resolved = resolve_subscription_recovery_readiness_failure(
            harness.pool(),
            &mismatch,
            GatewayAccountModeVerificationError::AccountModeMismatch {
                required: GatewayAccountMode::Live,
                observed: GatewayAccountMode::Test,
            },
        )
        .await?;
        let SubscriptionRecoveryReadinessResolution::Resolved(payment) = resolved else {
            return Err("a mode mismatch resolves the attempt".into());
        };
        assert_eq!(payment.attempt().status(), PaymentAttemptStatus::Failed);
        assert_eq!(
            payment.attempt().state().resolution_code(),
            Some(PaymentResolutionCode::GatewayLiveReadinessFailedBeforeSubmission)
        );
        assert!(!provider_cooldown_active(&harness).await?);

        // A rate-limited readiness query resolves the attempt and records the
        // provider cooldown, exactly once on replay.
        let throttled = prepared_recovery(&harness, "ready_throttled").await?;
        for _ in 0..2 {
            let resolved = resolve_subscription_recovery_readiness_failure(
                harness.pool(),
                &throttled,
                GatewayAccountModeVerificationError::Gateway(GatewayError::RateLimited(
                    GatewayDiagnostic::new("account mode query throttled"),
                )),
            )
            .await?;
            let SubscriptionRecoveryReadinessResolution::Resolved(payment) = resolved else {
                return Err("a rate limit resolves the attempt".into());
            };
            assert_eq!(
                payment.attempt().state().resolution_code(),
                Some(PaymentResolutionCode::GatewayProviderRateLimitedBeforeSubmission)
            );
        }
        let (status, _, submitted_at) = attempt_state(&harness, &throttled).await?;
        assert_eq!((status.as_str(), submitted_at), ("failed", None));
        assert!(provider_cooldown_active(&harness).await?);
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = harness.cleanup().await;
    result?;
    cleanup
}
