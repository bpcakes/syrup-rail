//! The provider's AVS result is stored with the response that carried it.

use syrup_rail::GatewayAvsResponse;

use super::billing_address::{enroll_command, named_contact, service};
use super::*;
use crate::test_support::billing_address as address;

fn avs_rejection(avs: Option<&str>) -> GatewayPaymentOutcome {
    GatewayPaymentOutcome::new(
        GatewayPaymentStatus::Failed,
        ProcessorEvidence::new(
            Some(GatewayTransactionId::new("txn_avs_rejected").unwrap()),
            None,
            Some(GatewayDiagnostic::new("3")),
            Some(GatewayDiagnostic::new("300")),
            Some(GatewayDiagnostic::new("AVS REJECTED")),
            Some(GatewayDiagnostic::new("failed")),
            GatewayPaymentDescriptor::default(),
        )
        .with_avs_response(avs.and_then(GatewayAvsResponse::parse)),
    )
}

async fn stored_avs(
    pool: &sqlx::PgPool,
    attempt_id: PaymentAttemptId,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT gateway_avs_response FROM billing_payment_attempts WHERE id = $1")
        .bind(attempt_id.as_uuid())
        .fetch_one(pool)
        .await
}

async fn reloaded(
    fixture: &ApplicationFixture,
    attempt_id: PaymentAttemptId,
) -> Result<PaymentAttempt, Box<dyn Error>> {
    let mut transaction = fixture.database.pool.begin().await?;
    let attempt = crate::find_payment_attempt_by_id_in_transaction(
        &mut transaction,
        fixture.command.billing_scope_id(),
        attempt_id,
    )
    .await?
    .expect("the attempt is stored");
    transaction.rollback().await?;
    Ok(attempt)
}

#[tokio::test]
async fn a_rejected_sale_stores_its_avs_result_and_reads_it_back() -> Result<(), Box<dyn Error>> {
    let fixture = enrollment_fixture("avs_rejected").build().await?;
    let gateway = Arc::new(ScriptedGateway::new(Ok(avs_rejection(Some("Z")))));
    let result = service(&fixture, &gateway)
        .enroll(enroll_command(
            &fixture,
            "avs-rejected",
            named_contact(Some(address("1 Main St"))),
        ))
        .await?;
    let attempt = result.attempt();
    assert_eq!(attempt.status(), PaymentAttemptStatus::Failed);
    let attempt_id = attempt.identity().attempt_id();
    assert_eq!(
        stored_avs(&fixture.database.pool, attempt_id).await?,
        Some("Z".to_owned())
    );
    let reloaded = reloaded(&fixture, attempt_id).await?;
    assert_eq!(
        reloaded.state().processor_evidence().avs_response(),
        GatewayAvsResponse::parse("Z")
    );
    assert_eq!(
        reloaded
            .state()
            .processor_evidence()
            .response_code()
            .map(GatewayDiagnostic::expose),
        Some("300")
    );
    fixture.cleanup().await
}

#[tokio::test]
async fn a_response_without_an_avs_code_stores_none() -> Result<(), Box<dyn Error>> {
    let fixture = enrollment_fixture("avs_absent").build().await?;
    // The NMI adapter drops a value that is not one letter or digit, so a
    // two-character provider value reaches storage as no result and never
    // fails the varchar(1) write.
    assert_eq!(GatewayAvsResponse::parse("YY"), None);
    let gateway = Arc::new(ScriptedGateway::new(Ok(avs_rejection(Some("YY")))));
    let result = service(&fixture, &gateway)
        .enroll(enroll_command(
            &fixture,
            "avs-absent",
            named_contact(Some(address("2 Main St"))),
        ))
        .await?;
    assert_eq!(result.attempt().status(), PaymentAttemptStatus::Failed);
    let attempt_id = result.attempt().identity().attempt_id();
    assert_eq!(stored_avs(&fixture.database.pool, attempt_id).await?, None);
    assert_eq!(
        reloaded(&fixture, attempt_id)
            .await?
            .state()
            .processor_evidence()
            .avs_response(),
        None
    );
    fixture.cleanup().await
}

#[tokio::test]
async fn a_stored_value_that_is_not_an_avs_code_reads_as_none() -> Result<(), Box<dyn Error>> {
    let fixture = enrollment_fixture("avs_foreign").build().await?;
    let gateway = Arc::new(ScriptedGateway::new(Ok(avs_rejection(Some("N")))));
    let result = service(&fixture, &gateway)
        .enroll(enroll_command(
            &fixture,
            "avs-foreign",
            named_contact(Some(address("3 Main St"))),
        ))
        .await?;
    let attempt_id = result.attempt().identity().attempt_id();
    sqlx::query("UPDATE billing_payment_attempts SET gateway_avs_response = '-' WHERE id = $1")
        .bind(attempt_id.as_uuid())
        .execute(&fixture.database.pool)
        .await?;
    let reloaded = reloaded(&fixture, attempt_id).await?;
    assert_eq!(reloaded.state().processor_evidence().avs_response(), None);
    assert_eq!(reloaded.status(), PaymentAttemptStatus::Failed);
    fixture.cleanup().await
}
