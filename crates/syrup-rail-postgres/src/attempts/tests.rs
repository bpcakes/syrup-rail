use std::{error::Error, sync::Arc};

use async_trait::async_trait;
use chrono::Duration;

use super::*;
use crate::test_support::{TestDatabase, create_gateway_account};

struct TestOfferStore;

#[async_trait]
impl crate::SubscriptionOfferStore for TestOfferStore {
    async fn lock_current_offer(
        &self,
        connection: &mut sqlx::PgConnection,
        billing_scope_id: BillingScopeId,
        plan_key: &PlanKey,
    ) -> Result<Option<syrup_rail::SubscriptionOffer>, sqlx::Error> {
        let row = sqlx::query_as::<_, (i32, String)>(
            r#"
                SELECT amount_cents, currency FROM host_subscription_offers
                WHERE billing_scope_id = $1 AND plan_key = $2 FOR SHARE
                "#,
        )
        .bind(billing_scope_id.as_uuid())
        .bind(plan_key.as_str())
        .fetch_optional(connection)
        .await?;
        row.map(|(amount, currency)| {
            let currency = CurrencyCode::new(&currency)
                .map_err(|_| sqlx::Error::Protocol("invalid host currency".to_owned()))?;
            let charge = ChargeAmount::new(amount, currency)
                .map_err(|_| sqlx::Error::Protocol("invalid host charge".to_owned()))?;
            Ok(crate::test_support::immediate_offer(
                plan_key.clone(),
                charge,
            ))
        })
        .transpose()
    }
}

struct TestReferenceFactory;

impl syrup_rail::GatewayMutationReferenceFactory for TestReferenceFactory {
    fn for_attempt(
        &self,
        kind: PaymentAttemptKind,
        attempt_id: PaymentAttemptId,
    ) -> GatewayOrderId {
        assert_eq!(kind, PaymentAttemptKind::SubscriptionInitial);
        GatewayOrderId::from_generated_attempt(
            format!("sr_initial_{}", attempt_id.as_uuid().simple()),
            attempt_id,
        )
        .expect("test order ID should be canonical")
    }
}

struct NeverCalledGateway;

#[async_trait]
impl syrup_rail::PaymentGateway for NeverCalledGateway {
    async fn account_mode(
        &self,
    ) -> Result<syrup_rail::GatewayAccountMode, syrup_rail::GatewayError> {
        panic!("reservation must not perform provider I/O")
    }

    async fn sale(
        &self,
        _request: syrup_rail::GatewaySaleRequest,
    ) -> Result<syrup_rail::GatewayPaymentOutcome, syrup_rail::GatewayMutationError> {
        panic!("reservation must not perform provider I/O")
    }

    async fn store_payment_method(
        &self,
        _request: syrup_rail::GatewayStorePaymentMethodRequest,
    ) -> Result<syrup_rail::GatewayPaymentOutcome, syrup_rail::GatewayMutationError> {
        panic!("reservation must not perform provider I/O")
    }

    async fn query_transaction(
        &self,
        _request: syrup_rail::GatewayQueryRequest,
    ) -> Result<Option<syrup_rail::GatewayPaymentOutcome>, syrup_rail::GatewayError> {
        panic!("reservation must not perform provider I/O")
    }

    async fn query_transaction_reports(
        &self,
        _request: syrup_rail::GatewayTransactionReportRequest,
    ) -> Result<Vec<syrup_rail::GatewayTransactionReport>, syrup_rail::GatewayError> {
        panic!("reservation must not perform provider I/O")
    }
}

async fn install_host_offers(database: &TestDatabase) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
            CREATE TABLE host_subscription_offers (
                billing_scope_id uuid NOT NULL,
                plan_key text NOT NULL,
                amount_cents integer NOT NULL,
                currency text NOT NULL,
                PRIMARY KEY (billing_scope_id, plan_key)
            )
            "#,
    )
    .execute(&database.pool)
    .await?;
    Ok(())
}

async fn set_offer(
    database: &TestDatabase,
    scope_id: Uuid,
    plan_key: &str,
    amount_cents: i32,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
            INSERT INTO host_subscription_offers (
                billing_scope_id, plan_key, amount_cents, currency
            ) VALUES ($1, $2, $3, 'USD')
            ON CONFLICT (billing_scope_id, plan_key)
            DO UPDATE SET amount_cents = EXCLUDED.amount_cents
            "#,
    )
    .bind(scope_id)
    .bind(plan_key)
    .bind(amount_cents)
    .execute(&database.pool)
    .await?;
    Ok(())
}

fn resolved_gateway(
    account: crate::test_support::GatewayAccountFixture,
) -> syrup_rail::ResolvedGateway {
    syrup_rail::ResolvedGateway::new(
        BillingScopeId::new(account.billing_scope_id),
        GatewayAccountId::new(account.gateway_account_id),
        GatewayConfigurationId::new(account.gateway_configuration_id),
        syrup_rail::GatewayProviderKey::new("nmi").unwrap(),
        syrup_rail::GatewayLifecycleQueryPolicy::new(
            syrup_rail::GatewayLifecycleCursorKey::new("test_cursor").unwrap(),
            Duration::minutes(1),
            10,
            2,
            2,
            20,
        )
        .unwrap(),
        Arc::new(TestReferenceFactory),
        Arc::new(NeverCalledGateway),
    )
}

fn enrollment_command(
    account: crate::test_support::GatewayAccountFixture,
    subscriber_id: Uuid,
    attempt_id: Uuid,
    idempotency_key: &str,
    expected_charge: syrup_rail::SubscriptionEnrollmentExpectedTerms,
) -> syrup_rail::EnrollSubscription {
    syrup_rail::EnrollSubscription::new(
        PaymentAttemptId::new(attempt_id),
        BillingScopeId::new(account.billing_scope_id),
        SubscriberId::new(subscriber_id),
        GatewayConfigurationId::new(account.gateway_configuration_id),
        IdempotencyKey::new(idempotency_key).unwrap(),
        syrup_rail::PaymentToken::new("token-secret").unwrap(),
        syrup_rail::BillingContact::new(
            Some("Sensitive".to_owned()),
            Some("Name".to_owned()),
            Some("secret@example.test".to_owned()),
        )
        .unwrap(),
        expected_charge,
    )
}

fn full_price(
    plan_key: &str,
    amount_cents: i32,
) -> syrup_rail::SubscriptionEnrollmentExpectedTerms {
    syrup_rail::SubscriptionEnrollmentExpectedTerms::full_price(
        crate::test_support::immediate_offer(
            PlanKey::new(plan_key).unwrap(),
            ChargeAmount::new(amount_cents, CurrencyCode::new("USD").unwrap()).unwrap(),
        ),
    )
}

async fn create_discount_code(
    database: &TestDatabase,
    scope_id: Uuid,
    plan_key: &str,
) -> Result<Uuid, sqlx::Error> {
    let code_id = Uuid::now_v7();
    sqlx::query(
        r#"
            INSERT INTO billing_subscription_discount_codes (
                id, billing_scope_id, plan_key, code_normalized, display_code,
                label, status, discount_kind, percent_off_bps, currency,
                duration, duration_months
            ) VALUES (
                $1, $2, $3, 'SAVE20', 'SAVE20', 'Sensitive campaign',
                'active', 'percent_off', 2000, 'USD', 'limited_months', 3
            )
            "#,
    )
    .bind(code_id)
    .bind(scope_id)
    .bind(plan_key)
    .execute(&database.pool)
    .await?;
    Ok(code_id)
}

async fn create_saved_claim(
    database: &TestDatabase,
    scope_id: Uuid,
    subscriber_id: Uuid,
    plan_key: &str,
    code_id: Uuid,
) -> Result<Uuid, sqlx::Error> {
    let claim_id = Uuid::now_v7();
    sqlx::query(
        r#"
            INSERT INTO billing_subscription_discount_claims (
                id, billing_scope_id, subscriber_id, plan_key,
                discount_code_id, code_snapshot, label_snapshot,
                discount_kind, percent_off_bps, currency, duration,
                duration_months, base_amount_cents, discounted_amount_cents,
                status
            ) VALUES (
                $1, $2, $3, $4, $5, 'SAVE20', 'Sensitive campaign',
                'percent_off', 2000, 'USD', 'limited_months', 3,
                1000, 800, 'saved'
            )
            "#,
    )
    .bind(claim_id)
    .bind(scope_id)
    .bind(subscriber_id)
    .bind(plan_key)
    .bind(code_id)
    .execute(&database.pool)
    .await?;
    Ok(claim_id)
}

fn discounted_expected(plan_key: &str) -> syrup_rail::SubscriptionEnrollmentExpectedTerms {
    let currency = CurrencyCode::new("USD").unwrap();
    syrup_rail::SubscriptionEnrollmentExpectedTerms::discounted(
        crate::test_support::immediate_offer(
            PlanKey::new(plan_key).unwrap(),
            ChargeAmount::new(1_000, currency).unwrap(),
        ),
        syrup_rail::SubscriptionDiscountSnapshot::new(
            syrup_rail::SubscriptionDiscountCode::new("SAVE20").unwrap(),
            Some("Sensitive campaign".to_owned()),
            SubscriptionDiscountKind::PercentOffBasisPoints(
                PercentOffBasisPoints::new(2_000).unwrap(),
            ),
            SubscriptionDiscountDuration::LimitedMonths(LimitedDiscountMonths::new(3).unwrap()),
            ChargeAmount::new(1_000, currency).unwrap(),
            ChargeAmount::new(800, currency).unwrap(),
        )
        .unwrap(),
    )
    .unwrap()
}

mod boundaries;
mod enrollment;
