use super::*;

struct CapturingGateway {
    requests: std::sync::Mutex<Vec<GatewaySaleRequest>>,
}

#[async_trait]
impl PaymentGateway for CapturingGateway {
    async fn account_mode(&self) -> Result<GatewayAccountMode, GatewayError> {
        panic!("descriptor policy tests only submit sales")
    }

    async fn sale(
        &self,
        request: GatewaySaleRequest,
    ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
        self.requests.lock().unwrap().push(request);
        Err(GatewayMutationError::NotSubmitted(
            crate::GatewayNotSubmittedError::Malformed(crate::GatewayDiagnostic::new("captured")),
        ))
    }

    async fn store_payment_method(
        &self,
        _request: GatewayStorePaymentMethodRequest,
    ) -> Result<GatewayPaymentOutcome, GatewayMutationError> {
        panic!("descriptor policy tests only submit sales")
    }

    async fn query_transaction(
        &self,
        _request: GatewayQueryRequest,
    ) -> Result<Option<GatewayPaymentOutcome>, GatewayError> {
        panic!("descriptor policy tests only submit sales")
    }

    async fn query_transaction_reports(
        &self,
        _request: GatewayTransactionReportRequest,
    ) -> Result<Vec<GatewayTransactionReport>, GatewayError> {
        panic!("descriptor policy tests only submit sales")
    }
}

fn sale(intent: GatewaySaleIntent) -> GatewaySaleRequest {
    GatewaySaleRequest::new(
        ChargeAmount::new(2_699, CurrencyCode::new("USD").unwrap()).unwrap(),
        GatewayOrderId::from_correlation("descriptor_order").unwrap(),
        intent,
        None,
    )
}

fn recurring(first_charge_after_paid_trial: bool) -> GatewaySaleIntent {
    GatewaySaleIntent::RecurringStoredCredential {
        payment_method_reference: crate::GatewayPaymentMethodReference::new("vault_1").unwrap(),
        initial_transaction_id: GatewayTransactionId::new("txn_1").unwrap(),
        first_charge_after_paid_trial,
    }
}

/// Drives a future that completes without suspending; the capturing
/// gateway never awaits I/O, so the core crate needs no async runtime.
fn poll_ready<F: std::future::Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    match future.as_mut().poll(&mut context) {
        std::task::Poll::Ready(output) => output,
        std::task::Poll::Pending => {
            panic!("capturing gateway futures must complete without suspending")
        }
    }
}

#[test]
fn resolved_gateway_applies_the_descriptor_policy_by_sale_intent() {
    use crate::{StatementDescriptor, StatementDescriptorPolicy};

    let default = StatementDescriptor::new("IDENTITYPRO.COM")
        .unwrap()
        .with_phone(Some("800-555-0199".to_owned()))
        .unwrap();
    let trial_end = StatementDescriptor::new("IDENTITYPRO TRIAL").unwrap();
    let explicit = StatementDescriptor::new("EXPLICIT").unwrap();
    let captured = Arc::new(CapturingGateway {
        requests: std::sync::Mutex::new(Vec::new()),
    });
    let gateway = ResolvedGateway::new(
        BillingScopeId::new(Uuid::from_u128(1)),
        GatewayAccountId::new(Uuid::from_u128(2)),
        GatewayConfigurationId::new(Uuid::from_u128(3)),
        GatewayProviderKey::new("test_gateway").expect("valid provider key"),
        test_policy(),
        Arc::new(TestReferenceFactory),
        captured.clone(),
    );
    assert!(gateway.statement_descriptors().is_none());

    // Without a policy every sale is forwarded without a descriptor.
    let _ = poll_ready(gateway.sale(sale(recurring(true))));
    assert!(captured.requests.lock().unwrap()[0].descriptor().is_none());

    let gateway = gateway.with_statement_descriptors(
        StatementDescriptorPolicy::new(default.clone())
            .with_first_charge_after_paid_trial(trial_end.clone()),
    );
    assert!(format!("{gateway:?}").contains("IDENTITYPRO TRIAL"));
    let one_time = GatewaySaleIntent::OneTime {
        payment_token: PaymentToken::new("tok_1").unwrap(),
    };
    let initial = GatewaySaleIntent::InitialStoredCredential {
        payment_token: PaymentToken::new("tok_2").unwrap(),
    };
    for intent in [one_time, initial, recurring(false), recurring(true)] {
        let _ = poll_ready(gateway.sale(sale(intent)));
    }
    // An explicit descriptor on the request wins over the policy.
    let _ = poll_ready(gateway.sale(sale(recurring(true)).with_descriptor(Some(explicit.clone()))));

    let requests = captured.requests.lock().unwrap();
    let descriptors = requests[1..]
        .iter()
        .map(|request| request.descriptor().cloned())
        .collect::<Vec<_>>();
    assert_eq!(
        descriptors,
        [
            Some(default.clone()),
            Some(default.clone()),
            Some(default),
            Some(trial_end),
            Some(explicit),
        ]
    );
    // Intent, order and charge pass through unchanged.
    assert!(matches!(
        requests[4].intent(),
        GatewaySaleIntent::RecurringStoredCredential {
            first_charge_after_paid_trial: true,
            ..
        }
    ));
    assert_eq!(requests[4].charge().cents(), 2_699);
}
