use super::*;

pub(super) struct Fixture {
    pub account: GatewayAccountFixture,
    pub subscriber_id: SubscriberId,
    pub attempt_id: PaymentAttemptId,
    pub method_id: Uuid,
    pub subscription_id: syrup_rail::SubscriptionId,
    pub plan_key: PlanKey,
    pub coordinator: TestCoordinator,
}

impl Fixture {
    pub async fn new(pool: &PgPool) -> Result<Self, Box<dyn Error>> {
        Self::new_with_reference(pool, "vault_metadata").await
    }

    pub async fn new_with_reference(
        pool: &PgPool,
        reference: &str,
    ) -> Result<Self, Box<dyn Error>> {
        let account = create_gateway_account(pool, "nmi").await?;
        let subscriber_id = SubscriberId::new(Uuid::now_v7());
        let gateway = resolved_gateway(account)?;
        let coordinator = TestCoordinator {
            pool: pool.clone(),
            events: Arc::new(Mutex::new(Vec::new())),
        };
        let offer = paid_trial_offer()?;
        let plan_key = offer.plan_key().clone();
        let payment = approve_enrollment(
            pool,
            &StaticOfferStore {
                offer: offer.clone(),
            },
            &gateway,
            &coordinator,
            account,
            subscriber_id,
            "metadata_trial",
            SubscriptionEnrollmentExpectedTerms::full_price(offer),
            "txn_metadata",
            reference,
        )
        .await?;
        let subscription_id = *payment
            .subscription()
            .expect("approved trial")
            .id()
            .as_uuid();
        let method_id =
            sqlx::query_scalar("SELECT payment_method_id FROM billing_subscriptions WHERE id = $1")
                .bind(subscription_id)
                .fetch_one(pool)
                .await?;
        Ok(Self {
            account,
            subscriber_id,
            attempt_id: payment.attempt().identity().attempt_id(),
            method_id,
            subscription_id: syrup_rail::SubscriptionId::new(subscription_id),
            plan_key,
            coordinator,
        })
    }

    pub fn scope(&self) -> BillingScopeId {
        BillingScopeId::new(self.account.billing_scope_id)
    }
    pub fn command(&self) -> RefreshPaymentMethodMetadata {
        RefreshPaymentMethodMetadata::new(self.scope(), self.subscriber_id, self.attempt_id)
    }
    pub fn portal_query(&self) -> SubscriptionBillingPortalQuery {
        SubscriptionBillingPortalQuery::new(self.scope(), self.subscriber_id, self.plan_key.clone())
    }

    pub fn resolver(
        &self,
        provider: Arc<QueryGateway>,
    ) -> Result<CountingResolver, Box<dyn Error>> {
        let baseline = resolved_gateway(self.account)?;
        Ok(CountingResolver {
            gateway: ResolvedGateway::new(
                baseline.billing_scope_id(),
                baseline.gateway_account_id(),
                baseline.gateway_configuration_id(),
                baseline.provider_key().clone(),
                baseline.lifecycle_query_policy().clone(),
                baseline.mutation_reference_factory(),
                provider,
            ),
            calls: AtomicUsize::new(0),
        })
    }

    pub async fn financial_snapshot(&self, pool: &PgPool) -> Result<String, sqlx::Error> {
        // Include every column of all financial rows, not only payment status.
        sqlx::query_scalar("SELECT jsonb_build_array((SELECT jsonb_agg(to_jsonb(a) ORDER BY id) FROM billing_payment_attempts a WHERE billing_scope_id = $1), (SELECT jsonb_agg(to_jsonb(c) ORDER BY id) FROM billing_processor_charges c WHERE billing_scope_id = $1), (SELECT jsonb_agg(to_jsonb(s) ORDER BY id) FROM billing_subscriptions s WHERE billing_scope_id = $1))::text")
            .bind(self.scope().as_uuid()).fetch_one(pool).await
    }

    pub async fn method_snapshot(&self, pool: &PgPool) -> Result<String, sqlx::Error> {
        sqlx::query_scalar("SELECT to_jsonb(m)::text FROM billing_payment_methods m WHERE id = $1")
            .bind(self.method_id)
            .fetch_one(pool)
            .await
    }

    pub async fn method_identity(&self, pool: &PgPool) -> Result<String, sqlx::Error> {
        sqlx::query_scalar("SELECT (to_jsonb(m) - ARRAY['card_brand', 'card_last4', 'card_exp_month', 'card_exp_year', 'updated_at']::text[])::text FROM billing_payment_methods m WHERE id = $1").bind(self.method_id).fetch_one(pool).await
    }
}

pub(super) enum Reply {
    Observation(Box<Option<GatewayPaymentOutcome>>),
    Malformed,
    Unavailable,
    RateLimited,
    Never,
}

impl Reply {
    pub fn observation(outcome: Option<GatewayPaymentOutcome>) -> Self {
        Self::Observation(Box::new(outcome))
    }
}

pub(super) struct QueryGateway {
    pub expected_transaction: &'static str,
    pub queries: AtomicUsize,
    pub reply: Mutex<Reply>,
    pub started: Notify,
    pub release: Notify,
    pub block: bool,
}

impl QueryGateway {
    pub fn new(reply: Reply) -> Self {
        Self {
            expected_transaction: "txn_metadata",
            queries: AtomicUsize::new(0),
            reply: Mutex::new(reply),
            started: Notify::new(),
            release: Notify::new(),
            block: false,
        }
    }
}

#[async_trait]
impl PaymentGateway for QueryGateway {
    async fn account_mode(&self) -> Result<GatewayAccountMode, GatewayError> {
        panic!("metadata needs only one exact query")
    }
    async fn sale(
        &self,
        _: GatewaySaleRequest,
    ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
        panic!("metadata must never submit a sale")
    }
    async fn store_payment_method(
        &self,
        _: GatewayStorePaymentMethodRequest,
    ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
        panic!("metadata must never mutate vault")
    }
    async fn query_transaction_reports(
        &self,
        _: GatewayTransactionReportRequest,
    ) -> Result<Vec<GatewayTransactionReport>, GatewayError> {
        panic!("metadata must never scan reports")
    }
    async fn query_transaction(
        &self,
        request: GatewayQueryRequest,
    ) -> Result<Option<GatewayPaymentOutcome>, GatewayError> {
        self.queries.fetch_add(1, Ordering::SeqCst);
        assert_eq!(
            request.transaction_id().map(GatewayTransactionId::expose),
            Some(self.expected_transaction)
        );
        assert!(request.order_id().is_none());
        self.started.notify_one();
        if self.block {
            self.release.notified().await;
        }
        match &*self.reply.lock().await {
            Reply::Observation(outcome) => Ok((**outcome).clone()),
            Reply::Malformed => Err(GatewayError::Malformed(GatewayDiagnostic::new(
                "synthetic malformed metadata",
            ))),
            Reply::Unavailable => Err(GatewayError::Unavailable(GatewayDiagnostic::new(
                "synthetic query failure",
            ))),
            Reply::RateLimited => Err(GatewayError::RateLimited(GatewayDiagnostic::new(
                "synthetic query throttle",
            ))),
            Reply::Never => std::future::pending().await,
        }
    }
}

pub(super) struct UncheckedResolver(pub ResolvedGateway);

#[async_trait]
impl GatewayResolver for UncheckedResolver {
    async fn resolve(
        &self,
        _: BillingScopeId,
        _: GatewayAccountId,
        _: GatewayConfigurationId,
        _: GatewayProviderKey,
    ) -> Result<ResolvedGateway, GatewayResolutionError> {
        Ok(self.0.clone())
    }
}

pub(super) fn metadata(
    transaction: &str,
    reference: Option<&str>,
    brand: &str,
    last4: &str,
    month: Option<i16>,
    year: Option<i16>,
) -> GatewayPaymentOutcome {
    GatewayPaymentOutcome::new(
        GatewayPaymentStatus::Approved,
        ProcessorEvidence::new(
            Some(GatewayTransactionId::new(transaction).unwrap()),
            reference.map(|value| GatewayPaymentMethodReference::new(value).unwrap()),
            Some(GatewayDiagnostic::new("1")),
            Some(GatewayDiagnostic::new("100")),
            None,
            None,
            GatewayPaymentDescriptor::from_provider_parts(
                None,
                Some(GatewayDiagnostic::new(brand)),
                Some(last4),
                month,
                year,
            ),
        ),
    )
}
