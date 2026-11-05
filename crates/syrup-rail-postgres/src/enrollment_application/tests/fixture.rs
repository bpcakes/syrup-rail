use super::*;

pub(super) fn enrollment_fixture(project: &str) -> FixtureSetup<'_> {
    FixtureSetup::new(project)
}

pub(super) fn application_fixture(project: &str) -> FixtureSetup<'_> {
    FixtureSetup::new(project).admitted()
}

pub(super) struct FixtureSetup<'a> {
    project: &'a str,
    discounted: bool,
    fail_event: bool,
    prepare_submission: bool,
}

impl<'a> FixtureSetup<'a> {
    fn new(project: &'a str) -> Self {
        Self {
            project,
            discounted: false,
            fail_event: false,
            prepare_submission: false,
        }
    }

    pub(super) fn discounted(mut self) -> Self {
        self.discounted = true;
        self
    }

    pub(super) fn with_failing_event_append(mut self) -> Self {
        self.fail_event = true;
        self
    }

    pub(super) fn admitted(mut self) -> Self {
        self.prepare_submission = true;
        self
    }

    pub(super) async fn build(self) -> Result<ApplicationFixture, Box<dyn Error>> {
        let Self {
            project,
            discounted,
            fail_event,
            prepare_submission,
        } = self;
        let database = TestDatabase::start(project).await?;
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
        let account = create_gateway_account(&database.pool, "nmi").await?;
        sqlx::query(
            "INSERT INTO host_subscription_offers VALUES ($1, 'base_subscription', 1000, 'USD')",
        )
        .bind(account.billing_scope_id)
        .execute(&database.pool)
        .await?;
        let subscriber_id = Uuid::now_v7();
        let expected_charge = if discounted {
            let code_id = Uuid::now_v7();
            sqlx::query(
                r#"
            INSERT INTO billing_subscription_discount_codes (
                id, billing_scope_id, plan_key, code_normalized, display_code,
                status, discount_kind, percent_off_bps, currency,
                duration, duration_months
            ) VALUES (
                $1, $2, 'base_subscription', 'SAVE20', 'SAVE20',
                'active', 'percent_off', 2000, 'USD', 'limited_months', 3
            )
            "#,
            )
            .bind(code_id)
            .bind(account.billing_scope_id)
            .execute(&database.pool)
            .await?;
            sqlx::query(
                r#"
            INSERT INTO billing_subscription_discount_claims (
                id, billing_scope_id, subscriber_id, plan_key,
                discount_code_id, code_snapshot, discount_kind,
                percent_off_bps, currency, duration, duration_months,
                base_amount_cents, discounted_amount_cents, status
            ) VALUES (
                $1, $2, $3, 'base_subscription', $4, 'SAVE20',
                'percent_off', 2000, 'USD', 'limited_months', 3,
                1000, 800, 'saved'
            )
            "#,
            )
            .bind(Uuid::now_v7())
            .bind(account.billing_scope_id)
            .bind(subscriber_id)
            .bind(code_id)
            .execute(&database.pool)
            .await?;
            SubscriptionEnrollmentExpectedTerms::discounted(
                immediate_offer(
                    PlanKey::new("base_subscription")?,
                    ChargeAmount::new(1000, CurrencyCode::new("USD")?)?,
                ),
                SubscriptionDiscountSnapshot::new(
                    SubscriptionDiscountCode::new("SAVE20")?,
                    None,
                    SubscriptionDiscountKind::PercentOffBasisPoints(PercentOffBasisPoints::new(
                        2000,
                    )?),
                    SubscriptionDiscountDuration::LimitedMonths(
                        syrup_rail::LimitedDiscountMonths::new(3)?,
                    ),
                    ChargeAmount::new(1000, CurrencyCode::new("USD")?)?,
                    ChargeAmount::new(800, CurrencyCode::new("USD")?)?,
                )?,
            )?
        } else {
            SubscriptionEnrollmentExpectedTerms::full_price(immediate_offer(
                PlanKey::new("base_subscription")?,
                ChargeAmount::new(1000, CurrencyCode::new("USD")?)?,
            ))
        };
        let attempt_id = PaymentAttemptId::new(Uuid::now_v7());
        let command = EnrollSubscription::new(
            syrup_rail::SubscriptionPaymentContext::new(
                attempt_id,
                BillingScopeId::new(account.billing_scope_id),
                SubscriberId::new(subscriber_id),
                GatewayConfigurationId::new(account.gateway_configuration_id),
                IdempotencyKey::new("application-key")?,
                PaymentToken::new("opaque-payment-token")?,
                BillingContact::new(
                    Some("Ada".to_owned()),
                    Some("Lovelace".to_owned()),
                    Some("ada@example.test".to_owned()),
                )?,
            ),
            expected_charge,
        );
        let gateway = ResolvedGateway::new(
            BillingScopeId::new(account.billing_scope_id),
            GatewayAccountId::new(account.gateway_account_id),
            GatewayConfigurationId::new(account.gateway_configuration_id),
            GatewayProviderKey::new("nmi")?,
            GatewayLifecycleQueryPolicy::new(
                GatewayLifecycleCursorKey::new("test_cursor")?,
                ChronoDuration::minutes(1),
                10,
                2,
                2,
                20,
            )?,
            Arc::new(TestReferenceFactory),
            Arc::new(NeverCalledGateway),
        );
        let reservation = SubscriptionEnrollmentReservation::from_command(
            &command,
            &gateway,
            GatewayAccountMode::Live,
        )?;
        let admission = if prepare_submission {
            let mut transaction = database.pool.begin().await?;
            assert!(matches!(
                reserve_subscription_enrollment_in_transaction(
                    &mut transaction,
                    &TestOfferStore,
                    &reservation,
                )
                .await?,
                SubscriptionEnrollmentReservationOutcome::Reserved(_)
            ));
            transaction.commit().await?;
            Some(
                match admit_subscription_enrollment_submission(
                    &database.pool,
                    &TestOfferStore,
                    &reservation,
                )
                .await?
                {
                    SubscriptionEnrollmentAdmissionOutcome::Admitted(admission) => admission,
                    other => return Err(format!("unexpected final admission: {other:?}").into()),
                },
            )
        } else {
            None
        };
        let coordinator = TestCoordinator {
            pool: database.pool.clone(),
            events: Arc::new(Mutex::new(Vec::new())),
            fail_begin: false,
            fail_event,
        };
        Ok(ApplicationFixture {
            database,
            reservation,
            coordinator,
            command,
            gateway_account: account,
            admission,
        })
    }
}
