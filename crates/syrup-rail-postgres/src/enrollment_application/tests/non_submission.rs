use super::*;

#[tokio::test]
async fn retry_proof_cannot_clear_another_admission_or_an_uncertain_result()
-> Result<(), Box<dyn Error>> {
    let fixture = application_fixture("retry_proof").build().await?;
    let admitted = fixture
        .admission
        .as_ref()
        .expect("admitted fixture")
        .attempt()
        .clone();
    let error = GatewayNotSubmittedError::NotTransmitted(GatewayDiagnostic::new("not transmitted"));
    let proof = NonSubmissionProof::for_error(&admitted, &error)?;
    let identity = admitted.identity();
    let mut connection = fixture.database.pool.acquire().await?;
    restore_prepared_attempt_submission(&mut connection, &admitted, proof).await?;
    let prepared = find_payment_attempt_by_id_on_connection(
        &mut connection,
        identity.billing_scope_id(),
        identity.attempt_id(),
    )
    .await?
    .unwrap();
    assert!(prepared.phase().is_prepared());
    assert!(NonSubmissionProof::for_error(&prepared, &error).is_err());

    // Simulate a later committed admission of the same durable idempotency key.
    sqlx::query(
        "UPDATE billing_payment_attempts SET submitted_at = $2 + interval '1 second' WHERE id = $1",
    )
    .bind(identity.attempt_id().as_uuid())
    .bind(admitted.state().timestamps().submitted_at())
    .execute(&mut *connection)
    .await?;
    assert!(
        restore_prepared_attempt_submission(&mut connection, &admitted, proof)
            .await
            .is_err()
    );
    let next = find_payment_attempt_by_id_on_connection(
        &mut connection,
        identity.billing_scope_id(),
        identity.attempt_id(),
    )
    .await?
    .unwrap();
    assert!(next.phase().is_admitted());
    assert!(!proof.matches(&next));
    let stale_failure = OutcomeResolutionCommand::local_failure(
        OutcomeResolutionBoundary::AdmittedNotSubmitted(proof),
        None,
        None,
    );
    assert!(!stale_failure.matches_attempt(&next));

    let next_proof = NonSubmissionProof::for_error(&next, &error)?;
    let foreign_identity = PaymentAttemptIdentity::new(
        PaymentAttemptId::new(Uuid::now_v7()),
        identity.billing_scope_id(),
        identity.subscriber_id(),
        identity.gateway_account_id(),
        identity.gateway_configuration_id(),
        identity.required_gateway_account_mode(),
    );
    let foreign = PaymentAttempt::new(
        foreign_identity,
        next.request().clone(),
        next.state().clone(),
    )?;
    assert!(!next_proof.matches(&foreign));
    assert!(
        restore_prepared_attempt_submission(&mut connection, &foreign, next_proof)
            .await
            .is_err()
    );

    // Even a matching timestamp cannot restore an uncertain or resolved attempt.
    for status in ["unknown", "failed"] {
        sqlx::query("UPDATE billing_payment_attempts SET status = $2, resolved_at = CASE WHEN $2 = 'failed' THEN clock_timestamp() END WHERE id = $1")
            .bind(identity.attempt_id().as_uuid()).bind(status).execute(&mut *connection).await?;
        assert!(
            restore_prepared_attempt_submission(&mut connection, &next, next_proof)
                .await
                .is_err()
        );
        let current = find_payment_attempt_by_id_on_connection(
            &mut connection,
            identity.billing_scope_id(),
            identity.attempt_id(),
        )
        .await?
        .unwrap();
        assert_eq!(
            current.state().timestamps().submitted_at(),
            next.state().timestamps().submitted_at()
        );
        assert!(!next_proof.matches(&current));
        assert!(NonSubmissionProof::for_error(&current, &error).is_err());
        assert!(
            !OutcomeResolutionCommand::local_failure(
                OutcomeResolutionBoundary::Prepared,
                None,
                None
            )
            .matches_attempt(&current)
        );
    }
    drop(connection);
    fixture.cleanup().await
}
