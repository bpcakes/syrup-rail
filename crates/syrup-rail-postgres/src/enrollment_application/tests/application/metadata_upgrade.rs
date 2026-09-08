use super::*;
use std::time::Duration;
use syrup_rail_nmi::{
    NmiPaymentGateway,
    nmi_client::{ClientFactory, Credentials, DuplicateCheck, Endpoint},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const TRANSACTION: &str = "txn_metadata_upgrade";
const XML: &str = "<nm_response><transaction><transaction_id>txn_metadata_upgrade</transaction_id><customer_vault_id>vault_application</customer_vault_id><response>1</response><response_code>100</response_code><response_text>Approved</response_text><condition>complete</condition><transaction_type>creditcard</transaction_type><cc_type>visa</cc_type><cc_number>4xxxxxxxxxxx4242</cc_number><cc_exp>1231</cc_exp></transaction></nm_response>";

// Expected financial normalization from v0.5.2 (9f30107) for XML above. This
// fixture is independent of the current parser; in particular, expiry is absent.
fn retained_v052_outcome() -> GatewayPaymentOutcome {
    GatewayPaymentOutcome::new(
        GatewayPaymentStatus::Approved,
        ProcessorEvidence::new(
            syrup_rail::ProcessorApprovalEvidence::Structured,
            Some(GatewayTransactionId::new(TRANSACTION).unwrap()),
            Some(GatewayPaymentMethodReference::new("vault_application").unwrap()),
            Some(GatewayDiagnostic::new("1")),
            Some(GatewayDiagnostic::new("100")),
            Some(GatewayDiagnostic::new("Approved")),
            Some(GatewayDiagnostic::new("complete")),
            GatewayPaymentDescriptor::from_provider_parts(
                Some(GatewayDiagnostic::new("creditcard")),
                Some(GatewayDiagnostic::new("visa")),
                Some("4242"),
                None,
                None,
            ),
        ),
    )
}

#[tokio::test]
async fn v052_parked_approval_reconciles_before_metadata_expiry_repair()
-> Result<(), Box<dyn Error>> {
    let mut fixture = application_fixture("metadata_upgrade", false, true).await?;
    let retained = retained_v052_outcome();
    let pool = &fixture.database.pool;
    let attempt_id = fixture.command.attempt_id();
    let parked = apply_subscription_enrollment_gateway_outcome(
        pool,
        &fixture.coordinator,
        &fixture.reservation,
        &retained,
    )
    .await?;
    assert_eq!(parked.status(), PaymentAttemptStatus::ReviewRequired);
    assert!(parked.subscription().is_none());
    let before_evidence = charge_evidence_snapshot(pool, attempt_id).await?;
    let expiry: (Option<i16>, Option<i16>) = sqlx::query_as(
        "SELECT card_exp_month, card_exp_year FROM billing_processor_charges WHERE attempt_id = $1",
    )
    .bind(attempt_id.as_uuid())
    .fetch_one(pool)
    .await?;
    assert_eq!(expiry, (None, None));

    fixture.coordinator.fail_event = false;
    let (gateway, server) = query_gateway(XML.to_owned()).await?;
    let observed = gateway
        .query_transaction(GatewayQueryRequest::new(
            Some(GatewayTransactionId::new(TRANSACTION)?),
            None,
        )?)
        .await?
        .expect("exact financial query");
    server.await?;
    assert_eq!(
        observed.evidence(),
        retained.evidence(),
        "the same XML must retain its v0.5.2 financial normalization"
    );
    let applied = apply_reconciled_subscription_enrollment_gateway_outcome(
        pool,
        &fixture.coordinator,
        fixture.command.billing_scope_id(),
        attempt_id,
        &observed,
    )
    .await?;
    assert_eq!(applied.status(), PaymentAttemptStatus::Approved);
    assert!(applied.subscription().is_some());
    assert_eq!(
        charge_evidence_snapshot(pool, attempt_id).await?,
        before_evidence
    );

    let financial = financial_snapshot(pool).await?;
    // A real conflict in a retained, non-null descriptor still fails strict
    // replay; the compatibility fix must not weaken financial equality.
    let evidence = observed.evidence();
    let conflicting = GatewayPaymentOutcome::new(
        GatewayPaymentStatus::Approved,
        ProcessorEvidence::new(
            evidence.approval_evidence(),
            evidence.transaction_id().cloned(),
            evidence.payment_method_reference().cloned(),
            evidence.response().cloned(),
            evidence.response_code().cloned(),
            evidence.response_text().cloned(),
            evidence.condition().cloned(),
            GatewayPaymentDescriptor::from_provider_parts(
                Some(GatewayDiagnostic::new("creditcard")),
                Some(GatewayDiagnostic::new("visa")),
                Some("9999"),
                None,
                None,
            ),
        ),
    );
    let error = apply_reconciled_subscription_enrollment_gateway_outcome(
        pool,
        &fixture.coordinator,
        fixture.command.billing_scope_id(),
        attempt_id,
        &conflicting,
    )
    .await
    .expect_err("conflicting non-null financial evidence must still fail");
    let mut source: Option<&(dyn Error + 'static)> = Some(&error);
    let mut replay_conflict = false;
    while let Some(error) = source {
        replay_conflict |= error
            .to_string()
            .contains("processor charge replay evidence changed");
        source = error.source();
    }
    assert!(replay_conflict);
    assert_eq!(financial_snapshot(pool).await?, financial);

    let command = crate::RefreshPaymentMethodMetadata::new(
        fixture.command.billing_scope_id(),
        fixture.command.subscriber_id(),
        attempt_id,
    );
    for (response, expected) in [
        (
            XML.replace("4xxxxxxxxxxx4242", "4xxxxxxxxxxx9999"),
            crate::PaymentMethodMetadataRefreshOutcome::EvidenceRejected,
        ),
        (
            XML.to_owned(),
            crate::PaymentMethodMetadataRefreshOutcome::Updated,
        ),
    ] {
        let (gateway, server) = query_gateway(response).await?;
        let resolver = StaticResolver {
            gateway: scripted_resolved_gateway(fixture.gateway_account, Arc::new(gateway)),
            calls: AtomicUsize::new(0),
        };
        assert_eq!(
            crate::refresh_payment_method_metadata(pool, &resolver, command).await?,
            expected
        );
        server.await?;
        assert_eq!(financial_snapshot(pool).await?, financial);
        let fields: (String, String, Option<i16>, Option<i16>) = sqlx::query_as(
            "SELECT card_brand, card_last4, card_exp_month, card_exp_year FROM billing_payment_methods",
        ).fetch_one(pool).await?;
        let expiry = if expected == crate::PaymentMethodMetadataRefreshOutcome::Updated {
            (Some(12), Some(2031))
        } else {
            (None, None)
        };
        assert_eq!(fields, ("visa".into(), "4242".into(), expiry.0, expiry.1));
    }
    assert_eq!(
        charge_evidence_snapshot(pool, attempt_id).await?,
        before_evidence
    );
    assert_eq!(fixture.coordinator.events.lock().await.len(), 1);
    fixture.cleanup().await
}

async fn charge_evidence_snapshot(
    pool: &PgPool,
    attempt: PaymentAttemptId,
) -> Result<String, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT jsonb_build_array(gateway_transaction_id, gateway_payment_method_reference, gateway_response, gateway_response_code, gateway_response_text, gateway_condition, payment_type, card_brand, card_last4, card_exp_month, card_exp_year)::text FROM billing_processor_charges WHERE attempt_id = $1",
    ).bind(attempt.as_uuid()).fetch_one(pool).await
}

#[tokio::test]
async fn nmi_query_repairs_all_missing_card_fields_without_financial_changes()
-> Result<(), Box<dyn Error>> {
    let fixture = application_fixture("meta_all_blank", false, false).await?;
    let pool = &fixture.database.pool;
    // The original incident: approval and vault linkage exist, but every
    // display field is absent. Repair must work without reapplying approval.
    let approval = GatewayPaymentOutcome::new(
        GatewayPaymentStatus::Approved,
        ProcessorEvidence::new(
            syrup_rail::ProcessorApprovalEvidence::Structured,
            Some(GatewayTransactionId::new(TRANSACTION)?),
            Some(GatewayPaymentMethodReference::new("vault_application")?),
            Some(GatewayDiagnostic::new("1")),
            Some(GatewayDiagnostic::new("100")),
            Some(GatewayDiagnostic::new("Approved")),
            None,
            GatewayPaymentDescriptor::default(),
        ),
    );
    let applied = apply_subscription_enrollment_gateway_outcome(
        pool,
        &fixture.coordinator,
        &fixture.reservation,
        &approval,
    )
    .await?;
    assert_eq!(applied.status(), PaymentAttemptStatus::Approved);
    let financial = financial_snapshot(pool).await?;
    let identity_sql = "SELECT (to_jsonb(m) - ARRAY['card_brand', 'card_last4', 'card_exp_month', 'card_exp_year', 'updated_at']::text[])::text FROM billing_payment_methods m";
    let identity: String = sqlx::query_scalar(identity_sql).fetch_one(pool).await?;
    let portal_query = syrup_rail::SubscriptionBillingPortalQuery::new(
        fixture.command.billing_scope_id(),
        fixture.command.subscriber_id(),
        syrup_rail::PlanKey::new("base_subscription")?,
    );
    let before = crate::subscription_billing_portal(pool, &portal_query).await?;
    assert!(before.payment_method_display().is_none());
    assert!(before.entitlement().permits_product_access());

    let command = crate::RefreshPaymentMethodMetadata::new(
        fixture.command.billing_scope_id(),
        fixture.command.subscriber_id(),
        fixture.command.attempt_id(),
    );
    for (response, code, condition) in [("2", "200", "declined"), ("3", "300", "failed")] {
        let xml = XML
            .replace(
                "<response>1</response>",
                &format!("<response>{response}</response>"),
            )
            .replace(
                "<response_code>100</response_code>",
                &format!("<response_code>{code}</response_code>"),
            )
            .replace(
                "<condition>complete</condition>",
                &format!("<condition>{condition}</condition>"),
            );
        let (gateway, server) = query_gateway(xml).await?;
        let resolver = StaticResolver {
            gateway: scripted_resolved_gateway(fixture.gateway_account, Arc::new(gateway)),
            calls: AtomicUsize::new(0),
        };
        assert_eq!(
            crate::refresh_payment_method_metadata(pool, &resolver, command).await?,
            crate::PaymentMethodMetadataRefreshOutcome::EvidenceRejected,
        );
        server.await?;
        assert!(
            crate::subscription_billing_portal(pool, &portal_query)
                .await?
                .payment_method_display()
                .is_none()
        );
        assert_eq!(financial_snapshot(pool).await?, financial);
    }
    let (gateway, server) = query_gateway(XML.to_owned()).await?;
    let resolver = StaticResolver {
        gateway: scripted_resolved_gateway(fixture.gateway_account, Arc::new(gateway)),
        calls: AtomicUsize::new(0),
    };
    assert_eq!(
        crate::refresh_payment_method_metadata(pool, &resolver, command).await?,
        crate::PaymentMethodMetadataRefreshOutcome::Updated,
    );
    server.await?;
    let after = crate::subscription_billing_portal(pool, &portal_query).await?;
    let display = after
        .payment_method_display()
        .expect("all-blank display is repaired");
    assert_eq!(display.card_brand(), Some(PaymentCardBrand::Visa));
    assert_eq!(display.card_last_four(), Some("4242"));
    assert_eq!(display.card_expiration_month(), Some(12));
    assert_eq!(display.card_expiration_year(), Some(2031));
    assert!(after.entitlement().permits_product_access());
    assert_eq!(financial_snapshot(pool).await?, financial);
    assert_eq!(
        sqlx::query_scalar::<_, String>(identity_sql)
            .fetch_one(pool)
            .await?,
        identity
    );
    assert_eq!(fixture.coordinator.events.lock().await.len(), 1);
    // A complete method is a local no-op; the loopback server has already exited.
    assert_eq!(
        crate::refresh_payment_method_metadata(pool, &resolver, command).await?,
        crate::PaymentMethodMetadataRefreshOutcome::Unchanged,
    );
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
    fixture.cleanup().await
}

async fn financial_snapshot(pool: &PgPool) -> Result<String, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT jsonb_build_array((SELECT jsonb_agg(to_jsonb(a) ORDER BY id) FROM billing_payment_attempts a), (SELECT jsonb_agg(to_jsonb(c) ORDER BY id) FROM billing_processor_charges c), (SELECT jsonb_agg(to_jsonb(s) ORDER BY id) FROM billing_subscriptions s))::text",
    ).fetch_one(pool).await
}

async fn query_gateway(
    response: String,
) -> Result<(NmiPaymentGateway, tokio::task::JoinHandle<()>), Box<dyn Error>> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = Endpoint::parse_loopback_http(format!("http://{}", listener.local_addr()?))?;
    let server = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(10), async {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0_u8; 1024];
            let header_end = loop {
                let read = stream.read(&mut chunk).await.unwrap();
                assert!(read > 0, "request must contain headers");
                request.extend_from_slice(&chunk[..read]);
                assert!(request.len() <= 16 * 1024);
                if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    break end + 4;
                }
            };
            let headers = String::from_utf8_lossy(&request[..header_end]);
            assert!(headers.starts_with("POST /api/query.php "), "upgrade/repair must only query");
            let length: usize = headers.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse().unwrap())
            }).unwrap();
            assert!(length <= 16 * 1024);
            while request.len() < header_end + length {
                let read = stream.read(&mut chunk).await.unwrap();
                assert!(read > 0);
                request.extend_from_slice(&chunk[..read]);
            }
            let body = String::from_utf8_lossy(&request[header_end..]);
            assert!(body.contains("transaction_id=txn_metadata_upgrade"));
            let headers = format!("HTTP/1.1 200 OK\r\ncontent-type: text/xml\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", response.len());
            stream.write_all(headers.as_bytes()).await.unwrap();
            stream.write_all(response.as_bytes()).await.unwrap();
        }).await.expect("bounded loopback query");
    });
    let client = ClientFactory::new_with_loopback_http()?.client_with_duplicate_check(
        endpoint,
        Credentials::new(
            "unused_private_key".to_owned(),
            "unused_query_key".to_owned(),
        )?,
        DuplicateCheck::ProcessorConfigured,
    )?;
    Ok((NmiPaymentGateway::new(client), server))
}
