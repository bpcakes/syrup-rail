//! Schema-v5 billing-address persistence across enrollment, replacement,
//! renewal reservation and submission, replay, and subscriber scrubbing.

use std::time::Duration;

use syrup_rail::{BillingAddress, ScrubSubscriberBillingData, SubscriptionId};

use super::*;
use crate::{
    SubscriptionRenewalAdmissionOutcome, admit_subscription_renewal_submission,
    scrub_subscriber_billing_data,
};

type AddressColumns = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

fn address(line1: &str) -> BillingAddress {
    BillingAddress::new(line1.to_owned(), "US".to_owned())
        .unwrap()
        .with_line2(Some("Suite 2".to_owned()))
        .unwrap()
        .with_city(Some("Boston".to_owned()))
        .unwrap()
        .with_region(Some("MA".to_owned()))
        .unwrap()
        .with_postal_code(Some("02110".to_owned()))
        .unwrap()
}

fn columns(address: &BillingAddress) -> AddressColumns {
    (
        Some(address.line1().to_owned()),
        address.line2().map(ToOwned::to_owned),
        address.city().map(ToOwned::to_owned),
        address.region().map(ToOwned::to_owned),
        address.postal_code().map(ToOwned::to_owned),
        Some(address.country().to_owned()),
    )
}

const NO_ADDRESS: AddressColumns = (None, None, None, None, None, None);

fn named_contact(address: Option<BillingAddress>) -> BillingContact {
    let contact = BillingContact::new(
        Some("Ada".to_owned()),
        Some("Lovelace".to_owned()),
        Some("ada@example.test".to_owned()),
    )
    .unwrap();
    match address {
        Some(address) => contact.with_address(address),
        None => contact,
    }
}

fn context(
    fixture: &ApplicationFixture,
    key: &str,
    contact: BillingContact,
) -> syrup_rail::SubscriptionPaymentContext {
    syrup_rail::SubscriptionPaymentContext::new(
        PaymentAttemptId::new(Uuid::now_v7()),
        fixture.command.billing_scope_id(),
        fixture.command.subscriber_id(),
        fixture.command.gateway_configuration_id(),
        IdempotencyKey::new(key).unwrap(),
        PaymentToken::new(format!("opaque-{key}")).unwrap(),
        contact,
    )
}

fn enroll_command(
    fixture: &ApplicationFixture,
    key: &str,
    contact: BillingContact,
) -> EnrollSubscription {
    EnrollSubscription::new(
        context(fixture, key, contact),
        fixture.command.expected_terms().clone(),
    )
}

fn replacement_command(
    fixture: &ApplicationFixture,
    key: &str,
    contact: BillingContact,
) -> ReplaceSubscriptionPaymentMethod {
    ReplaceSubscriptionPaymentMethod::new(
        context(fixture, key, contact),
        fixture.command.plan_key().clone(),
    )
}

fn service(
    fixture: &ApplicationFixture,
    gateway: &Arc<ScriptedGateway>,
) -> SubscriptionBillingService {
    SubscriptionBillingService::new(
        fixture.database.pool.clone(),
        Arc::new(TestOfferStore),
        Arc::new(StaticResolver {
            gateway: scripted_resolved_gateway(fixture.gateway_account, Arc::clone(gateway)),
            calls: AtomicUsize::new(0),
        }),
        Arc::new(PermitAdmission {
            calls: AtomicUsize::new(0),
        }),
        Arc::new(fixture.coordinator.clone()),
    )
}

fn declined_store_outcome(transaction_id: &str) -> GatewayPaymentOutcome {
    GatewayPaymentOutcome::new(
        GatewayPaymentStatus::Declined,
        ProcessorEvidence::new(
            Some(GatewayTransactionId::new(transaction_id).unwrap()),
            None,
            Some(GatewayDiagnostic::new("2")),
            Some(GatewayDiagnostic::new("200")),
            Some(GatewayDiagnostic::new("Declined")),
            Some(GatewayDiagnostic::new("declined")),
            GatewayPaymentDescriptor::default(),
        ),
    )
}

async fn method_address(
    pool: &sqlx::PgPool,
    payment_method_id: PaymentMethodId,
) -> Result<AddressColumns, sqlx::Error> {
    sqlx::query_as(
        r#"
        SELECT billing_address_line1, billing_address_line2, billing_address_city,
            billing_address_region, billing_address_postal_code, billing_address_country
        FROM billing_payment_methods
        WHERE id = $1
        "#,
    )
    .bind(payment_method_id.as_uuid())
    .fetch_one(pool)
    .await
}

async fn current_method(
    pool: &sqlx::PgPool,
    subscription_id: SubscriptionId,
) -> Result<PaymentMethodId, sqlx::Error> {
    sqlx::query_scalar::<_, Uuid>(
        "SELECT payment_method_id FROM billing_subscriptions WHERE id = $1",
    )
    .bind(subscription_id.as_uuid())
    .fetch_one(pool)
    .await
    .map(PaymentMethodId::new)
}

/// Makes the subscription's next renewal due and returns its period start.
async fn make_renewal_due(
    pool: &sqlx::PgPool,
    subscription_id: SubscriptionId,
) -> Result<DateTime<Utc>, sqlx::Error> {
    sqlx::query_scalar(
        r#"
        UPDATE billing_subscriptions
        SET current_period_start_at = now() - interval '1 month 1 hour',
            current_period_end_at = now() - interval '1 hour',
            next_renewal_at = now() - interval '1 hour',
            next_payment_attempt_at = now() - interval '1 hour',
            updated_at = clock_timestamp()
        WHERE id = $1
        RETURNING next_renewal_at
        "#,
    )
    .bind(subscription_id.as_uuid())
    .fetch_one(pool)
    .await
}

async fn enroll_with(
    fixture: &ApplicationFixture,
    key: &str,
    contact: BillingContact,
    transaction_id: &str,
    reference: &str,
) -> Result<(SubscriptionId, Arc<ScriptedGateway>), Box<dyn Error>> {
    let gateway = Arc::new(ScriptedGateway::new(Ok(approved_outcome_with_reference(
        Some(transaction_id),
        reference,
    ))));
    let result = service(fixture, &gateway)
        .enroll(enroll_command(fixture, key, contact))
        .await?;
    assert_eq!(result.attempt().status(), PaymentAttemptStatus::Approved);
    let subscription_id = result
        .subscription()
        .expect("approved enrollment creates a subscription")
        .id();
    Ok((subscription_id, gateway))
}

async fn renew(
    fixture: &ApplicationFixture,
    subscription_id: SubscriptionId,
    transaction_id: &str,
) -> Result<(PaymentAttempt, Option<BillingContact>), Box<dyn Error>> {
    let period_start_at = make_renewal_due(&fixture.database.pool, subscription_id).await?;
    let gateway = Arc::new(ScriptedGateway::new(Ok(approved_outcome(transaction_id))));
    let outcome = service(fixture, &gateway)
        .renew(ChargeRenewal::new(
            fixture.command.billing_scope_id(),
            subscription_id,
            period_start_at,
        ))
        .await?;
    let SubscriptionRenewalOutcome::Payment(payment) = outcome else {
        return Err(format!("due renewal must submit: {outcome:?}").into());
    };
    assert_eq!(payment.attempt().status(), PaymentAttemptStatus::Approved);
    let contacts = gateway.sale_contacts.lock().await.clone();
    assert_eq!(contacts.len(), 1);
    Ok((
        payment.attempt().clone(),
        contacts.into_iter().next().flatten(),
    ))
}

async fn reserve_renewal(
    fixture: &ApplicationFixture,
    subscription_id: SubscriptionId,
) -> Result<(syrup_rail::SubscriptionRenewalReservation, PaymentAttempt), Box<dyn Error>> {
    let period_start_at = make_renewal_due(&fixture.database.pool, subscription_id).await?;
    let resolved = scripted_resolved_gateway(
        fixture.gateway_account,
        Arc::new(ScriptedGateway::new(Ok(approved_outcome("txn_unused")))),
    );
    let mut transaction = fixture.database.pool.begin().await?;
    let reserved = reserve_subscription_renewal_in_transaction(
        &mut transaction,
        ChargeRenewal::new(
            fixture.command.billing_scope_id(),
            subscription_id,
            period_start_at,
        ),
        &resolved,
        GatewayAccountMode::Live,
    )
    .await?;
    transaction.commit().await?;
    match reserved {
        SubscriptionRenewalReservationOutcome::Reserved(reservation, attempt) => {
            Ok((*reservation, *attempt))
        }
        other => Err(format!("unexpected renewal reservation: {other:?}").into()),
    }
}

/// Waits until exactly `count` sessions are blocked on an advisory lock.
async fn wait_for_advisory_waiters(pool: &sqlx::PgPool, count: i64) -> Result<(), Box<dyn Error>> {
    for _ in 0..500 {
        let waiting: i64 = sqlx::query_scalar(
            r#"
            SELECT count(*)
            FROM pg_locks
            WHERE locktype = 'advisory'
                AND NOT granted
                AND database = (SELECT oid FROM pg_database WHERE datname = current_database())
            "#,
        )
        .fetch_one(pool)
        .await?;
        if waiting == count {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Err(format!("expected {count} advisory-lock waiter(s)").into())
}

#[tokio::test]
async fn enrollment_and_renewal_carry_the_confirmed_address_end_to_end()
-> Result<(), Box<dyn Error>> {
    let fixture = enrollment_fixture("addr_e2e", false, false, false).await?;
    let home = address("1 Main St");
    let (subscription_id, enrollment_gateway) = enroll_with(
        &fixture,
        "address-enrollment",
        named_contact(Some(home.clone())),
        "txn_address_initial",
        "vault_address_initial",
    )
    .await?;
    assert_eq!(
        *enrollment_gateway.sale_contacts.lock().await,
        vec![Some(named_contact(Some(home.clone())))]
    );
    let method_id = current_method(&fixture.database.pool, subscription_id).await?;
    assert_eq!(
        method_address(&fixture.database.pool, method_id).await?,
        columns(&home)
    );

    let (attempt, sent) = renew(&fixture, subscription_id, "txn_address_renewal").await?;
    assert_eq!(sent, Some(BillingContact::from_address(home.clone())));
    let snapshot = attempt.request().billing_contact();
    assert_eq!(snapshot.address(), Some(&home));
    assert_eq!(snapshot.first_name(), None);
    assert_eq!(snapshot.email(), None);
    fixture.cleanup().await
}

#[tokio::test]
async fn addressless_history_renews_without_any_contact() -> Result<(), Box<dyn Error>> {
    let fixture = enrollment_fixture("addr_none", false, false, false).await?;
    let (subscription_id, _) = enroll_with(
        &fixture,
        "addressless-enrollment",
        named_contact(None),
        "txn_addressless_initial",
        "vault_addressless_initial",
    )
    .await?;
    let method_id = current_method(&fixture.database.pool, subscription_id).await?;
    assert_eq!(
        method_address(&fixture.database.pool, method_id).await?,
        NO_ADDRESS
    );
    let (attempt, sent) = renew(&fixture, subscription_id, "txn_addressless_renewal").await?;
    assert_eq!(sent, None, "addressless renewal wire contact is unchanged");
    assert!(attempt.request().billing_contact().is_empty());
    fixture.cleanup().await
}

#[tokio::test]
async fn reserved_renewal_submits_its_snapshot_after_the_method_address_changes()
-> Result<(), Box<dyn Error>> {
    let fixture = enrollment_fixture("addr_snapshot", false, false, false).await?;
    let home = address("1 Main St");
    let (subscription_id, _) = enroll_with(
        &fixture,
        "snapshot-enrollment",
        named_contact(Some(home.clone())),
        "txn_snapshot_initial",
        "vault_snapshot_initial",
    )
    .await?;
    let (reservation, attempt) = reserve_renewal(&fixture, subscription_id).await?;
    assert_eq!(attempt.request().billing_contact().address(), Some(&home));
    let admission =
        match admit_subscription_renewal_submission(&fixture.database.pool, &reservation).await? {
            SubscriptionRenewalAdmissionOutcome::Admitted(admission) => *admission,
            other => return Err(format!("unexpected renewal admission: {other:?}").into()),
        };

    // A newer address on the current method must not leak into the reserved
    // renewal.
    let newer = address("9 Newer Rd");
    let method_id = current_method(&fixture.database.pool, subscription_id).await?;
    let newer_columns = columns(&newer);
    sqlx::query(
        r#"
        UPDATE billing_payment_methods
        SET billing_address_line1 = $2, billing_address_line2 = $3,
            billing_address_city = $4, billing_address_region = $5,
            billing_address_postal_code = $6, billing_address_country = $7
        WHERE id = $1
        "#,
    )
    .bind(method_id.as_uuid())
    .bind(newer_columns.0)
    .bind(newer_columns.1)
    .bind(newer_columns.2)
    .bind(newer_columns.3)
    .bind(newer_columns.4)
    .bind(newer_columns.5)
    .execute(&fixture.database.pool)
    .await?;

    let gateway = Arc::new(ScriptedGateway::new(Ok(approved_outcome(
        "txn_snapshot_renewal",
    ))));
    let resolved = scripted_resolved_gateway(fixture.gateway_account, Arc::clone(&gateway));
    let verified = crate::verify_gateway_account_mode(&resolved, GatewayAccountMode::Live).await?;
    let result = submit_admitted_subscription_renewal(
        &fixture.database.pool,
        &fixture.coordinator,
        admission,
        verified,
    )
    .await?;
    assert!(matches!(
        result,
        SubscriptionRenewalProviderResult::Payment(_)
    ));
    assert_eq!(
        *gateway.sale_contacts.lock().await,
        vec![Some(BillingContact::from_address(home))]
    );
    fixture.cleanup().await
}

#[tokio::test]
async fn renewal_without_an_active_method_stores_no_address_and_admission_fails_as_before()
-> Result<(), Box<dyn Error>> {
    let fixture = enrollment_fixture("addr_inactive", false, false, false).await?;
    let (subscription_id, _) = enroll_with(
        &fixture,
        "inactive-enrollment",
        named_contact(Some(address("1 Main St"))),
        "txn_inactive_initial",
        "vault_inactive_initial",
    )
    .await?;
    let method_id = current_method(&fixture.database.pool, subscription_id).await?;
    sqlx::query("UPDATE billing_payment_methods SET status = 'disabled' WHERE id = $1")
        .bind(method_id.as_uuid())
        .execute(&fixture.database.pool)
        .await?;

    let (reservation, attempt) = reserve_renewal(&fixture, subscription_id).await?;
    assert_eq!(attempt.request().billing_contact().address(), None);
    assert!(matches!(
        admit_subscription_renewal_submission(&fixture.database.pool, &reservation).await,
        Err(SubscriptionEnrollmentApplicationError::InvalidState(_))
    ));
    fixture.cleanup().await
}

#[tokio::test]
async fn approved_replacement_moves_the_next_renewal_address_and_failed_replacement_keeps_it()
-> Result<(), Box<dyn Error>> {
    let fixture = enrollment_fixture("addr_replace", false, false, false).await?;
    let home = address("1 Main St");
    let (subscription_id, _) = enroll_with(
        &fixture,
        "replace-enrollment",
        named_contact(Some(home.clone())),
        "txn_replace_initial",
        "vault_replace_initial",
    )
    .await?;
    let original_method = current_method(&fixture.database.pool, subscription_id).await?;

    let office = address("2 Office Park");
    let replacement_gateway = Arc::new(ScriptedGateway::for_stored_method(Ok(
        approved_outcome_with_reference(Some("txn_replace_card"), "vault_replace_card"),
    )));
    service(&fixture, &replacement_gateway)
        .replace_payment_method(replacement_command(
            &fixture,
            "replace-approved",
            named_contact(Some(office.clone())),
        ))
        .await?;
    assert_eq!(
        *replacement_gateway.store_contacts.lock().await,
        vec![Some(named_contact(Some(office.clone())))]
    );
    let replacement_method = current_method(&fixture.database.pool, subscription_id).await?;
    assert_ne!(replacement_method, original_method);
    assert_eq!(
        method_address(&fixture.database.pool, replacement_method).await?,
        columns(&office)
    );

    // A declined replacement leaves the current method and its address.
    let declined_gateway = Arc::new(ScriptedGateway::for_stored_method(Ok(
        declined_store_outcome("txn_replace_declined"),
    )));
    let declined = service(&fixture, &declined_gateway)
        .replace_payment_method(replacement_command(
            &fixture,
            "replace-declined",
            named_contact(Some(address("3 Declined Way"))),
        ))
        .await?;
    assert_eq!(declined.attempt().status(), PaymentAttemptStatus::Declined);
    assert_eq!(
        current_method(&fixture.database.pool, subscription_id).await?,
        replacement_method
    );
    assert_eq!(
        method_address(&fixture.database.pool, replacement_method).await?,
        columns(&office)
    );
    let (_, sent) = renew(&fixture, subscription_id, "txn_replace_renewal").await?;
    assert_eq!(sent, Some(BillingContact::from_address(office)));
    fixture.cleanup().await
}

#[tokio::test]
async fn same_reference_approval_replaces_or_keeps_the_whole_address() -> Result<(), Box<dyn Error>>
{
    let fixture = enrollment_fixture("addr_same_ref", false, false, false).await?;
    let home = address("1 Main St");
    let (subscription_id, _) = enroll_with(
        &fixture,
        "same-ref-enrollment",
        named_contact(Some(home.clone())),
        "txn_same_ref_initial",
        "vault_same_ref",
    )
    .await?;
    let method_id = current_method(&fixture.database.pool, subscription_id).await?;

    // An addressless approval of the same card keeps all six columns while
    // names follow the newest approval.
    let gateway = Arc::new(ScriptedGateway::for_stored_method(Ok(
        approved_outcome_with_reference(Some("txn_same_ref_names"), "vault_same_ref"),
    )));
    service(&fixture, &gateway)
        .replace_payment_method(replacement_command(
            &fixture,
            "same-ref-names",
            BillingContact::new(Some("Grace".to_owned()), Some("Hopper".to_owned()), None)?,
        ))
        .await?;
    assert_eq!(
        current_method(&fixture.database.pool, subscription_id).await?,
        method_id
    );
    assert_eq!(
        method_address(&fixture.database.pool, method_id).await?,
        columns(&home)
    );
    let name: Option<String> =
        sqlx::query_scalar("SELECT billing_name FROM billing_payment_methods WHERE id = $1")
            .bind(method_id.as_uuid())
            .fetch_one(&fixture.database.pool)
            .await?;
    assert_eq!(name.as_deref(), Some("Grace Hopper"));

    // A sparser address replaces the whole value; no old field survives.
    let sparse = BillingAddress::new("7 Sparse St".to_owned(), "CA".to_owned())?;
    let gateway = Arc::new(ScriptedGateway::for_stored_method(Ok(
        approved_outcome_with_reference(Some("txn_same_ref_sparse"), "vault_same_ref"),
    )));
    service(&fixture, &gateway)
        .replace_payment_method(replacement_command(
            &fixture,
            "same-ref-sparse",
            BillingContact::from_address(sparse.clone()),
        ))
        .await?;
    assert_eq!(
        method_address(&fixture.database.pool, method_id).await?,
        columns(&sparse)
    );
    fixture.cleanup().await
}

#[tokio::test]
async fn same_key_address_changes_conflict_before_provider_io() -> Result<(), Box<dyn Error>> {
    // The prepared fixture attempt is addressless, like every pre-v5 attempt.
    let fixture = enrollment_fixture("addr_conflict", false, false, true).await?;
    let gateway = Arc::new(ScriptedGateway::new(Ok(approved_outcome(
        "txn_conflict_must_not_submit",
    ))));
    let enrollment_service = service(&fixture, &gateway);
    let with_address = EnrollSubscription::new(
        syrup_rail::SubscriptionPaymentContext::new(
            PaymentAttemptId::new(Uuid::now_v7()),
            fixture.command.billing_scope_id(),
            fixture.command.subscriber_id(),
            fixture.command.gateway_configuration_id(),
            fixture.command.idempotency_key().clone(),
            PaymentToken::new("refreshed-conflict-token")?,
            fixture
                .command
                .billing_contact()
                .clone()
                .with_address(address("1 Main St")),
        ),
        fixture.command.expected_terms().clone(),
    );
    assert!(matches!(
        enrollment_service
            .enroll(with_address)
            .await
            .expect_err("adding an address to an addressless attempt must conflict"),
        SubscriptionBillingServiceError::IdempotencyConflict,
    ));
    assert_eq!(gateway.sale_calls.load(Ordering::SeqCst), 0);

    // An addressed durable attempt conflicts with a changed address and still
    // replays the identical one. Approve the prepared enrollment first so a
    // replacement is eligible.
    let subscription_id = apply_subscription_enrollment_gateway_outcome(
        &fixture.database.pool,
        &fixture.coordinator,
        &fixture.reservation,
        &approved_outcome("txn_conflict_initial"),
    )
    .await?
    .subscription()
    .expect("approved enrollment creates a subscription")
    .id();
    let replacement_gateway = Arc::new(ScriptedGateway::for_stored_method(Ok(
        approved_outcome_with_reference(Some("txn_conflict_replacement"), "vault_conflict_new"),
    )));
    let replacement_service = service(&fixture, &replacement_gateway);
    let original = replacement_command(
        &fixture,
        "conflict-replacement",
        named_contact(Some(address("1 Main St"))),
    );
    let mut transaction = fixture.database.pool.begin().await?;
    assert!(matches!(
        reserve_subscription_payment_method_replacement_in_transaction(
            &mut transaction,
            &original,
            &scripted_resolved_gateway(fixture.gateway_account, Arc::clone(&replacement_gateway)),
            GatewayAccountMode::Live,
        )
        .await?,
        SubscriptionPaymentMethodReplacementReservationOutcome::Reserved(..)
    ));
    transaction.commit().await?;
    let changed = ReplaceSubscriptionPaymentMethod::new(
        syrup_rail::SubscriptionPaymentContext::new(
            PaymentAttemptId::new(Uuid::now_v7()),
            original.billing_scope_id(),
            original.subscriber_id(),
            original.gateway_configuration_id(),
            original.idempotency_key().clone(),
            PaymentToken::new("refreshed-replacement-token")?,
            named_contact(Some(address("2 Changed St"))),
        ),
        original.plan_key().clone(),
    );
    assert!(matches!(
        replacement_service
            .replace_payment_method(changed)
            .await
            .expect_err("a changed address under the same key must conflict"),
        SubscriptionBillingServiceError::IdempotencyConflict,
    ));
    assert_eq!(replacement_gateway.store_calls.load(Ordering::SeqCst), 0);
    let identical = ReplaceSubscriptionPaymentMethod::new(
        syrup_rail::SubscriptionPaymentContext::new(
            PaymentAttemptId::new(Uuid::now_v7()),
            original.billing_scope_id(),
            original.subscriber_id(),
            original.gateway_configuration_id(),
            original.idempotency_key().clone(),
            PaymentToken::new("refreshed-replacement-token")?,
            original.billing_contact().clone(),
        ),
        original.plan_key().clone(),
    );
    let result = replacement_service
        .replace_payment_method(identical)
        .await?;
    assert_eq!(result.attempt().status(), PaymentAttemptStatus::Approved);
    assert_eq!(replacement_gateway.store_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        method_address(
            &fixture.database.pool,
            current_method(&fixture.database.pool, subscription_id).await?
        )
        .await?,
        columns(&address("1 Main St"))
    );
    fixture.cleanup().await
}

async fn assert_subscriber_address_pii_cleared(
    pool: &sqlx::PgPool,
    subscriber_id: SubscriberId,
) -> Result<(), Box<dyn Error>> {
    let remaining: i64 = sqlx::query_scalar(
        r#"
        SELECT
            (SELECT count(*) FROM billing_payment_methods
             WHERE subscriber_id = $1
                AND num_nonnulls(billing_name, billing_email, billing_address_line1,
                    billing_address_line2, billing_address_city, billing_address_region,
                    billing_address_postal_code, billing_address_country) <> 0)
            + (SELECT count(*) FROM billing_payment_attempts
             WHERE subscriber_id = $1
                AND num_nonnulls(billing_first_name, billing_last_name, billing_email,
                    billing_address_line1, billing_address_line2, billing_address_city,
                    billing_address_region, billing_address_postal_code,
                    billing_address_country) <> 0)
        "#,
    )
    .bind(subscriber_id.as_uuid())
    .fetch_one(pool)
    .await?;
    assert_eq!(
        remaining, 0,
        "no contact or address PII may survive the scrub"
    );
    Ok(())
}

async fn addressed_due_subscription(
    fixture: &ApplicationFixture,
    key: &str,
) -> Result<(SubscriptionId, DateTime<Utc>, ResolvedGateway), Box<dyn Error>> {
    let (subscription_id, _) = enroll_with(
        fixture,
        key,
        named_contact(Some(address("1 Main St"))),
        &format!("txn_{key}"),
        &format!("vault_{key}"),
    )
    .await?;
    let period_start_at = make_renewal_due(&fixture.database.pool, subscription_id).await?;
    let resolved = scripted_resolved_gateway(
        fixture.gateway_account,
        Arc::new(ScriptedGateway::new(Ok(approved_outcome("txn_unused")))),
    );
    Ok((subscription_id, period_start_at, resolved))
}

#[tokio::test]
async fn scrub_waits_for_a_held_renewal_reservation_then_clears_its_snapshot()
-> Result<(), Box<dyn Error>> {
    let fixture = enrollment_fixture("addr_ren_scrub", false, false, false).await?;
    let subscriber_id = fixture.command.subscriber_id();
    let (subscription_id, period_start_at, resolved) =
        addressed_due_subscription(&fixture, "reservation_first").await?;

    let mut reservation_transaction = fixture.database.pool.begin().await?;
    let reserved = reserve_subscription_renewal_in_transaction(
        &mut reservation_transaction,
        ChargeRenewal::new(
            fixture.command.billing_scope_id(),
            subscription_id,
            period_start_at,
        ),
        &resolved,
        GatewayAccountMode::Live,
    )
    .await?;
    let SubscriptionRenewalReservationOutcome::Reserved(_, attempt) = reserved else {
        return Err(format!("unexpected renewal reservation: {reserved:?}").into());
    };
    assert!(attempt.request().billing_contact().address().is_some());
    let pool = fixture.database.pool.clone();
    let scrub_command =
        ScrubSubscriberBillingData::new(fixture.command.billing_scope_id(), subscriber_id);
    let scrub = tokio::spawn(async move {
        let mut transaction = pool.begin().await?;
        let rows = scrub_subscriber_billing_data(&mut transaction, scrub_command).await?;
        transaction.commit().await?;
        Ok::<_, sqlx::Error>(rows)
    });
    wait_for_advisory_waiters(&fixture.database.pool, 1).await?;
    reservation_transaction.commit().await?;
    let scrubbed = scrub.await??;
    assert_eq!(scrubbed.payment_attempts(), 2);
    assert_subscriber_address_pii_cleared(&fixture.database.pool, subscriber_id).await?;
    fixture.cleanup().await
}

#[tokio::test]
async fn renewal_reservation_waits_for_a_held_scrub_and_restores_no_address()
-> Result<(), Box<dyn Error>> {
    let fixture = enrollment_fixture("addr_scrub_ren", false, false, false).await?;
    let subscriber_id = fixture.command.subscriber_id();
    let (subscription_id, period_start_at, resolved) =
        addressed_due_subscription(&fixture, "scrub_first").await?;

    let mut scrub_transaction = fixture.database.pool.begin().await?;
    scrub_subscriber_billing_data(
        &mut scrub_transaction,
        ScrubSubscriberBillingData::new(fixture.command.billing_scope_id(), subscriber_id),
    )
    .await?;
    let pool = fixture.database.pool.clone();
    let billing_scope_id = fixture.command.billing_scope_id();
    let reservation = tokio::spawn(async move {
        let mut transaction = pool.begin().await?;
        let outcome = reserve_subscription_renewal_in_transaction(
            &mut transaction,
            ChargeRenewal::new(billing_scope_id, subscription_id, period_start_at),
            &resolved,
            GatewayAccountMode::Live,
        )
        .await?;
        transaction.commit().await?;
        Ok::<_, PaymentAttemptStoreError>(outcome)
    });
    wait_for_advisory_waiters(&fixture.database.pool, 1).await?;
    scrub_transaction.commit().await?;
    let outcome = reservation.await??;
    let SubscriptionRenewalReservationOutcome::Reserved(_, attempt) = outcome else {
        return Err(format!("unexpected renewal reservation: {outcome:?}").into());
    };
    // The scrub disabled the method, so no active row supplies an address.
    assert_eq!(attempt.request().billing_contact().address(), None);
    assert_subscriber_address_pii_cleared(&fixture.database.pool, subscriber_id).await?;
    fixture.cleanup().await
}

#[tokio::test]
async fn approval_waits_for_a_held_scrub_and_restores_no_pii() -> Result<(), Box<dyn Error>> {
    let fixture = enrollment_fixture("addr_scrub_app", false, false, false).await?;
    let subscriber_id = fixture.command.subscriber_id();
    let command = enroll_command(
        &fixture,
        "scrub-approval-enrollment",
        named_contact(Some(address("1 Main St"))),
    );
    let reservation = SubscriptionEnrollmentReservation::from_command(
        &command,
        &scripted_resolved_gateway(
            fixture.gateway_account,
            Arc::new(ScriptedGateway::new(Ok(approved_outcome("txn_unused")))),
        ),
        GatewayAccountMode::Live,
    )?;
    let mut transaction = fixture.database.pool.begin().await?;
    assert!(matches!(
        reserve_subscription_enrollment_in_transaction(
            &mut transaction,
            &TestOfferStore,
            &reservation
        )
        .await?,
        SubscriptionEnrollmentReservationOutcome::Reserved(_)
    ));
    transaction.commit().await?;
    assert!(matches!(
        admit_subscription_enrollment_submission(
            &fixture.database.pool,
            &TestOfferStore,
            &reservation
        )
        .await?,
        SubscriptionEnrollmentAdmissionOutcome::Admitted(_)
    ));

    let mut scrub_transaction = fixture.database.pool.begin().await?;
    scrub_subscriber_billing_data(
        &mut scrub_transaction,
        ScrubSubscriberBillingData::new(fixture.command.billing_scope_id(), subscriber_id),
    )
    .await?;
    let pool = fixture.database.pool.clone();
    let coordinator = fixture.coordinator.clone();
    let approval = tokio::spawn(async move {
        apply_subscription_enrollment_gateway_outcome(
            &pool,
            &coordinator,
            &reservation,
            &approved_outcome_with_reference(Some("txn_scrub_approval"), "vault_scrub_approval"),
        )
        .await
    });
    wait_for_advisory_waiters(&fixture.database.pool, 1).await?;
    scrub_transaction.commit().await?;
    let approved = approval.await??;
    assert_eq!(approved.attempt().status(), PaymentAttemptStatus::Approved);
    assert_subscriber_address_pii_cleared(&fixture.database.pool, subscriber_id).await?;
    fixture.cleanup().await
}

#[tokio::test]
async fn renewal_reservation_rejects_an_account_changed_between_locator_and_lock()
-> Result<(), Box<dyn Error>> {
    let fixture = enrollment_fixture("addr_locator", false, false, false).await?;
    let (subscription_id, period_start_at, _) =
        addressed_due_subscription(&fixture, "locator_switch").await?;
    let pool = &fixture.database.pool;
    // Shipped schemas allow one account per scope, so an account switch needs
    // this test-only relaxation to reach the defensive locator check.
    sqlx::raw_sql(
        "ALTER TABLE billing_gateway_accounts DROP CONSTRAINT billing_gateway_accounts_scope_key",
    )
    .execute(pool)
    .await?;
    let switched = crate::test_support::GatewayAccountFixture {
        billing_scope_id: fixture.gateway_account.billing_scope_id,
        gateway_account_id: Uuid::now_v7(),
        gateway_configuration_id: Uuid::now_v7(),
    };
    sqlx::query(
        "INSERT INTO billing_gateway_accounts (id, billing_scope_id, provider_key, gateway_configuration_id) VALUES ($1, $2, 'nmi', $3)",
    )
    .bind(switched.gateway_account_id)
    .bind(switched.billing_scope_id)
    .bind(switched.gateway_configuration_id)
    .execute(pool)
    .await?;
    let switched_method = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO billing_payment_methods (
            id, billing_scope_id, subscriber_id, gateway_account_id,
            gateway_payment_method_reference, status
        ) VALUES ($1, $2, $3, $4, 'vault_switched', 'active')
        "#,
    )
    .bind(switched_method)
    .bind(switched.billing_scope_id)
    .bind(fixture.command.subscriber_id().as_uuid())
    .bind(switched.gateway_account_id)
    .execute(pool)
    .await?;

    // The caller already resolved the switched account; the reservation's
    // unlocked locator still sees the original account and enters its domain.
    let mut holder = pool.begin().await?;
    crate::attempts::lock_subscription_aggregate(
        &mut holder,
        fixture.command.subscriber_id(),
        fixture.command.plan_key(),
    )
    .await?;
    let reservation_pool = pool.clone();
    let billing_scope_id = fixture.command.billing_scope_id();
    let resolved = scripted_resolved_gateway(
        switched,
        Arc::new(ScriptedGateway::new(Ok(approved_outcome("txn_unused")))),
    );
    let reservation = tokio::spawn(async move {
        let mut transaction = reservation_pool.begin().await?;
        let outcome = reserve_subscription_renewal_in_transaction(
            &mut transaction,
            ChargeRenewal::new(billing_scope_id, subscription_id, period_start_at),
            &resolved,
            GatewayAccountMode::Live,
        )
        .await?;
        transaction.commit().await?;
        Ok::<_, PaymentAttemptStoreError>(outcome)
    });
    wait_for_advisory_waiters(pool, 1).await?;
    // Historical attempts pin the account through foreign keys; skip their
    // referential triggers for this single simulated switch.
    sqlx::query("SET LOCAL session_replication_role = replica")
        .execute(&mut *holder)
        .await?;
    sqlx::query(
        "UPDATE billing_subscriptions SET gateway_account_id = $2, payment_method_id = $3 WHERE id = $1",
    )
    .bind(subscription_id.as_uuid())
    .bind(switched.gateway_account_id)
    .bind(switched_method)
    .execute(&mut *holder)
    .await?;
    holder.commit().await?;
    assert_eq!(
        reservation.await??,
        SubscriptionRenewalReservationOutcome::Rejected(
            SubscriptionRenewalReservationRejection::GatewayConfigurationChanged
        )
    );
    let renewals: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM billing_payment_attempts WHERE subscription_id = $1 AND attempt_kind = 'subscription_renewal'",
    )
    .bind(subscription_id.as_uuid())
    .fetch_one(pool)
    .await?;
    assert_eq!(renewals, 0);
    fixture.cleanup().await
}
