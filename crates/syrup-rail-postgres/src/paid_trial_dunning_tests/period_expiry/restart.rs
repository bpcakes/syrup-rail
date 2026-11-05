use super::*;

pub(super) enum RestartOutcome {
    AwaitingResponse,
    Declined,
}

/// Reserves and admits a full-price restart, optionally declines it, and
/// backdates its submission so the calendar month it would buy has ended.
pub(super) async fn expired_restart(
    harness: &Harness,
    key: &str,
    outcome: RestartOutcome,
) -> Result<(SubscriberId, SubscriptionEnrollmentReservation), Box<dyn Error>> {
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
                IdempotencyKey::new(key)?,
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
    if matches!(outcome, RestartOutcome::Declined) {
        apply_subscription_enrollment_gateway_outcome(
            harness.pool(),
            &harness.enforcing,
            &enrollment,
            &declined_outcome(&format!("{key}_decline")),
        )
        .await?;
    }
    sqlx::query(
        r#"
        UPDATE billing_payment_attempts
        SET created_at = created_at - interval '40 days',
            submitted_at = submitted_at - interval '40 days',
            resolved_at = resolved_at - interval '40 days'
        WHERE id = $1
        "#,
    )
    .bind(enrollment.identity().attempt_id().as_uuid())
    .execute(harness.pool())
    .await?;
    Ok((subscriber_id, enrollment))
}
