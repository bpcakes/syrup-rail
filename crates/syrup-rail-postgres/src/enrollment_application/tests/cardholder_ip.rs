//! The request-scoped cardholder address, observed at the provider boundary:
//! sent with the token sale a customer submits, never stored, never part of
//! replay identity, and never sent with a merchant-initiated renewal.

use std::net::{IpAddr, Ipv4Addr};

use syrup_rail::{CardholderIpAddress, GatewaySaleIntent};

use super::billing_address::{
    context, enroll_command, enroll_with, make_renewal_due, named_contact, service,
};
use super::statement_descriptor::renew_with;
use super::*;

fn address(last: u8) -> CardholderIpAddress {
    CardholderIpAddress::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, last)))
}

/// Every table's rows as text, so a copy in any column is found.
async fn assert_never_stored(pool: &PgPool, addresses: &[&str]) -> Result<(), Box<dyn Error>> {
    let tables = sqlx::query_scalar::<_, String>(
        "SELECT quote_ident(table_schema) || '.' || quote_ident(table_name) \
         FROM information_schema.tables \
         WHERE table_type = 'BASE TABLE' \
           AND table_schema NOT IN ('pg_catalog', 'information_schema')",
    )
    .fetch_all(pool)
    .await?;
    let mut rows_scanned = 0;
    for table in tables {
        let rows = sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(format!(
            "SELECT row_to_json(stored)::text FROM {table} AS stored"
        )))
        .fetch_all(pool)
        .await?;
        rows_scanned += rows.len();
        for address in addresses {
            assert!(
                !rows.iter().any(|row| row.contains(address)),
                "{table} stores a cardholder address"
            );
        }
    }
    assert!(rows_scanned > 0, "the workflow stored nothing to scan");
    Ok(())
}

fn enroll_from(
    fixture: &ApplicationFixture,
    key: &str,
    cardholder_ip: Option<CardholderIpAddress>,
) -> EnrollSubscription {
    let command = enroll_command(fixture, key, named_contact(None));
    EnrollSubscription::new(
        command.context().clone().with_cardholder_ip(cardholder_ip),
        command.expected_terms().clone(),
    )
}

#[tokio::test]
async fn enrollment_sends_the_cardholder_ip_and_replays_ignore_it() -> Result<(), Box<dyn Error>> {
    let fixture = enrollment_fixture("card_ip_enroll").build().await?;
    let result = async {
        let gateway = Arc::new(ScriptedGateway::new(Ok(approved_outcome_with_reference(
            Some("txn_cardholder_ip"),
            "vault_cardholder_ip",
        ))));
        let service = service(&fixture, &gateway);
        let key = "cardholder-ip-enrollment";

        let applied = service
            .enroll(enroll_from(&fixture, key, Some(address(7))))
            .await?;
        assert_eq!(applied.attempt().status(), PaymentAttemptStatus::Approved);
        let requests = gateway.sale_requests.lock().await.clone();
        let [request] = requests.as_slice() else {
            return Err(format!("expected one sale, got {}", requests.len()).into());
        };
        assert!(matches!(
            request.intent(),
            GatewaySaleIntent::InitialStoredCredential { .. }
        ));
        assert_eq!(request.cardholder_ip(), Some(address(7)));

        // The address is not part of the request's identity: the same key
        // replays the committed attempt whether a retry carries another
        // address or none, and never reaches the provider again.
        for retried in [Some(address(8)), None] {
            let replay = service.enroll(enroll_from(&fixture, key, retried)).await?;
            assert_eq!(replay.attempt(), applied.attempt());
        }
        assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 1);
        assert_never_stored(&fixture.database.pool, &["203.0.113.7", "203.0.113.8"]).await?;

        // The merchant-initiated renewal of that subscription carries none.
        let subscription_id = applied
            .subscription()
            .expect("approved enrollment creates a subscription")
            .id();
        let renewal = renew_with(
            &fixture,
            subscription_id,
            "txn_cardholder_ip_renewal",
            60,
            None,
        )
        .await?;
        assert!(matches!(
            renewal.intent(),
            GatewaySaleIntent::RecurringStoredCredential { .. }
        ));
        assert_eq!(renewal.cardholder_ip(), None);
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    fixture.cleanup().await?;
    result
}

#[tokio::test]
async fn enrollment_without_a_cardholder_ip_sends_none() -> Result<(), Box<dyn Error>> {
    let fixture = enrollment_fixture("card_ip_absent").build().await?;
    let result = async {
        let (_, gateway) = enroll_with(
            &fixture,
            "cardholder-ip-absent",
            named_contact(None),
            "txn_cardholder_ip_absent",
            "vault_cardholder_ip_absent",
        )
        .await?;
        let requests = gateway.sale_requests.lock().await.clone();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].cardholder_ip(), None);
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    fixture.cleanup().await?;
    result
}

#[tokio::test]
async fn recovery_sends_the_cardholder_ip_and_replays_ignore_it() -> Result<(), Box<dyn Error>> {
    let fixture = enrollment_fixture("card_ip_recover").build().await?;
    let result = async {
        let (subscription_id, _) = enroll_with(
            &fixture,
            "cardholder-ip-recovery-initial",
            named_contact(None),
            "txn_cardholder_ip_initial",
            "vault_cardholder_ip_initial",
        )
        .await?;
        make_renewal_due(&fixture.database.pool, subscription_id).await?;
        sqlx::query("UPDATE billing_subscriptions SET status = 'past_due' WHERE id = $1")
            .bind(subscription_id.as_uuid())
            .execute(&fixture.database.pool)
            .await?;

        let gateway = Arc::new(ScriptedGateway::new(Ok(approved_outcome_with_reference(
            Some("txn_cardholder_ip_recovery"),
            "vault_cardholder_ip_recovery",
        ))));
        let service = service(&fixture, &gateway);
        let recovery_context = context(&fixture, "cardholder-ip-recovery", named_contact(None));
        let recover_from = |cardholder_ip| {
            RecoverSubscriptionPayment::new(
                recovery_context.clone().with_cardholder_ip(cardholder_ip),
                fixture.command.plan_key().clone(),
            )
        };
        let applied = service.recover(recover_from(Some(address(9)))).await?;
        assert_eq!(applied.attempt().status(), PaymentAttemptStatus::Approved);
        let requests = gateway.sale_requests.lock().await.clone();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].cardholder_ip(), Some(address(9)));

        for retried in [Some(address(10)), None] {
            let replay = service.recover(recover_from(retried)).await?;
            assert_eq!(replay.attempt(), applied.attempt());
        }
        assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 1);
        assert_never_stored(&fixture.database.pool, &["203.0.113.9", "203.0.113.10"]).await?;
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    fixture.cleanup().await?;
    result
}
