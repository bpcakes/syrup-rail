use std::{fmt, mem::size_of, sync::Arc};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::configuration::{
    ConfigurationError, Credentials, DuplicateCheck, Endpoint, MAX_NMI_CONCURRENT_REPORTS,
    MAX_NMI_IDLE_CONNECTIONS_PER_HOST, configured_http_client,
};
use crate::{
    AccountMode, MutationError, PaymentOutcome, PaymentSource, PaymentStatus, QueryError,
    ReportQuery, SaleRequest, StorePaymentMethodRequest, TransactionAction, TransactionQuery,
    TransactionReport, VaultAction,
};

use self::form::{
    amount_string, classic_sale_params, classic_store_payment_method_params,
    query_account_mode_params, query_transaction_params, query_transaction_report_params,
};
use self::response::common::{ApprovedIdentityRequirement, require_approved_identities};
use self::response::form::classic_payment_outcome_from_form;
use self::response::json::payment_outcome_from_json;
use self::response::xml::{
    query_account_mode_from_xml, query_outcome_for_request_from_xml,
    query_transaction_reports_from_xml,
};
use self::v5::{amount_value, sale_body_json};
use self::validation::{
    ensure_supported_sale_currency, validate_report_query, validate_sale_request,
    validate_store_payment_method_request, validate_transaction_query,
};

mod form;
mod request_budget;
mod response;
mod text;
mod transport;
mod v5;
mod validation;

const MAX_NMI_STANDARD_RESPONSE_BYTES: usize = 256 * 1024;
const MAX_NMI_REPORT_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_NMI_TRANSACTION_REPORTS: usize = 100;
// NMI documents report pagination in transactions, not a separate semantic
// action limit. The fixed cap is independent of TransactionAction's layout and
// approximates the former layout-derived limit at 144 bytes per action. The
// compile-time assertion deliberately fails if a larger layout would exceed
// one additional response-sized budget; if it fires, re-evaluate and lower the
// fixed cap instead of silently deriving a different limit from the layout.
const MAX_NMI_REPORT_ACTIONS: usize = 29_000;
const _: () = assert!(
    MAX_NMI_REPORT_ACTIONS * size_of::<TransactionAction>() <= MAX_NMI_REPORT_RESPONSE_BYTES,
    "TransactionAction grew; lower MAX_NMI_REPORT_ACTIONS instead of deriving it from the layout",
);
const MAX_NMI_FIELD_CHARS: usize = 512;
const MAX_NMI_PAYMENT_TOKEN_BYTES: usize = 4_096;
const MAX_NMI_IDENTIFIER_BYTES: usize = 512;
const MAX_NMI_ORDER_ID_BYTES: usize = 512;
const MAX_NMI_CONTACT_NAME_BYTES: usize = 256;
const MAX_NMI_EMAIL_BYTES: usize = 320;
const MAX_NMI_REPORT_DATE_BYTES: usize = 64;
const MAX_NMI_OUTBOUND_REQUEST_BYTES: usize = 16 * 1024;
const MAX_NMI_FIXED_REQUEST_BYTES: usize = 1_024;
const SUPPORTED_NMI_CURRENCY: &str = "USD";

/// A credential-free HTTP client factory that can be shared across accounts.
#[derive(Clone)]
pub struct ClientFactory {
    pub(crate) https: reqwest::Client,
    pub(crate) loopback_http: Option<reqwest::Client>,
    pub(crate) report_admission: Arc<Semaphore>,
}

impl ClientFactory {
    pub fn new() -> Result<Self, ConfigurationError> {
        let https = configured_http_client(reqwest::Client::builder().https_only(true))?;
        Ok(Self {
            https,
            loopback_http: None,
            report_admission: Arc::new(Semaphore::new(MAX_NMI_CONCURRENT_REPORTS)),
        })
    }

    /// Builds a factory that additionally permits explicit loopback HTTP
    /// endpoints for local integration tests.
    pub fn new_with_loopback_http() -> Result<Self, ConfigurationError> {
        let mut factory = Self::new()?;
        factory.loopback_http = Some(configured_http_client(reqwest::Client::builder())?);
        Ok(factory)
    }

    /// Builds an account-bound client using the processor-configured policy.
    ///
    /// Releases through 0.4.0 sent the invalid `dup_seconds=0` override from
    /// this constructor. It now omits the field so NMI applies the account's
    /// processor configuration. New and migrated callers should use
    /// [`Self::client_with_duplicate_check`] and choose the account policy
    /// explicitly.
    #[deprecated(
        note = "wire behavior changed: invalid dup_seconds=0 is now omitted; choose an explicit policy with ClientFactory::client_with_duplicate_check"
    )]
    pub fn client(
        &self,
        endpoint: Endpoint,
        credentials: Credentials,
    ) -> Result<Client, ConfigurationError> {
        self.client_with_duplicate_check(endpoint, credentials, DuplicateCheck::ProcessorConfigured)
    }

    /// Builds an account-bound client with an explicit duplicate-check policy.
    ///
    /// This policy applies to every sale submitted by the returned client.
    /// [`DuplicateCheck::ProcessorConfigured`] retains the account's
    /// processor-level defense in depth. An account owner may instead choose an
    /// explicit override after verifying both that the processor configuration
    /// permits it and that the host accepts the resulting duplicate-risk
    /// tradeoff.
    pub fn client_with_duplicate_check(
        &self,
        endpoint: Endpoint,
        credentials: Credentials,
        duplicate_check: DuplicateCheck,
    ) -> Result<Client, ConfigurationError> {
        let http = match endpoint.url.scheme() {
            "https" => self.https.clone(),
            "http" => self
                .loopback_http
                .clone()
                .ok_or(ConfigurationError::LoopbackHttpDisabled)?,
            _ => unreachable!("Endpoint constructors admit only HTTPS or loopback HTTP"),
        };
        Ok(Client {
            http,
            endpoint,
            credentials,
            duplicate_check,
            report_admission: self.report_admission.clone(),
        })
    }
}

impl fmt::Debug for ClientFactory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClientFactory")
            .field("https", &self.https)
            .field("loopback_http", &self.loopback_http)
            .field("report_limit", &MAX_NMI_CONCURRENT_REPORTS)
            .field(
                "idle_connections_per_host",
                &MAX_NMI_IDLE_CONNECTIONS_PER_HOST,
            )
            .finish()
    }
}

/// An account-bound NMI client.
pub struct Client {
    pub(crate) http: reqwest::Client,
    pub(crate) endpoint: Endpoint,
    pub(crate) credentials: Credentials,
    duplicate_check: DuplicateCheck,
    pub(crate) report_admission: Arc<Semaphore>,
}

impl fmt::Debug for Client {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Client")
            .field("http", &self.http)
            .field("endpoint", &self.endpoint)
            .field("credentials", &self.credentials)
            .field("duplicate_check", &self.duplicate_check)
            .finish()
    }
}

enum WireError {
    Indeterminate(String),
    LocalInvalidRequest(String),
    RequestRejected(String),
    MalformedResponse(String),
    Configuration(String),
    TransportRateLimited(String),
    RateLimited(String),
    Unavailable(String),
}

impl fmt::Debug for WireError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Indeterminate(_) => "WireError::Indeterminate([redacted])",
            Self::LocalInvalidRequest(_) => "WireError::LocalInvalidRequest([redacted])",
            Self::RequestRejected(_) => "WireError::RequestRejected([redacted])",
            Self::MalformedResponse(_) => "WireError::MalformedResponse([redacted])",
            Self::Configuration(_) => "WireError::Configuration([redacted])",
            Self::TransportRateLimited(_) => "WireError::TransportRateLimited([redacted])",
            Self::RateLimited(_) => "WireError::RateLimited([redacted])",
            Self::Unavailable(_) => "WireError::Unavailable([redacted])",
        })
    }
}

impl WireError {
    fn into_mutation(self) -> MutationError {
        match self {
            Self::Indeterminate(detail) => MutationError::Indeterminate(detail.into()),
            Self::LocalInvalidRequest(detail) => MutationError::InvalidRequest(detail.into()),
            Self::RequestRejected(detail) => MutationError::RequestRejected(detail.into()),
            Self::MalformedResponse(detail) => MutationError::Indeterminate(detail.into()),
            Self::Configuration(detail) => MutationError::Configuration(detail.into()),
            // NMI documents HTTP 429 as a system-wide throttle but does not
            // guarantee that a payment mutation was rejected before
            // processing. Preserve mutation uncertainty; only the in-band
            // payment response code 301 proves a not-submitted throttle.
            Self::TransportRateLimited(detail) => {
                MutationError::RateLimitedIndeterminate(detail.into())
            }
            Self::RateLimited(detail) => MutationError::RateLimited(detail.into()),
            Self::Unavailable(detail) => MutationError::Unavailable(detail.into()),
        }
    }

    fn into_query(self) -> QueryError {
        match self {
            Self::Indeterminate(detail) | Self::Unavailable(detail) => {
                QueryError::Unavailable(detail.into())
            }
            Self::TransportRateLimited(detail) | Self::RateLimited(detail) => {
                QueryError::RateLimited(detail.into())
            }
            Self::LocalInvalidRequest(detail) => QueryError::InvalidRequest(detail.into()),
            Self::RequestRejected(detail) => QueryError::InvalidRequest(detail.into()),
            Self::MalformedResponse(detail) => QueryError::MalformedResponse(detail.into()),
            Self::Configuration(detail) => QueryError::Configuration(detail.into()),
        }
    }

    fn after_success(self) -> Self {
        match self {
            Self::RateLimited(detail) => Self::RateLimited(detail),
            Self::Indeterminate(detail)
            | Self::LocalInvalidRequest(detail)
            | Self::RequestRejected(detail)
            | Self::MalformedResponse(detail)
            | Self::Configuration(detail)
            | Self::TransportRateLimited(detail)
            | Self::Unavailable(detail) => Self::Indeterminate(detail),
        }
    }
}

impl Client {
    #[cfg(test)]
    fn new(
        base_url: impl AsRef<str>,
        private_api_key: impl Into<String>,
        query_security_key: impl Into<String>,
    ) -> Result<Self, ConfigurationError> {
        let endpoint = if base_url.as_ref().starts_with("https://") {
            Endpoint::parse_https(base_url)?
        } else {
            Endpoint::parse_loopback_http(base_url)?
        };
        let credentials = Credentials::new(private_api_key.into(), query_security_key.into())?;
        crate::ClientFactory::new_with_loopback_http()?.client_with_duplicate_check(
            endpoint,
            credentials,
            DuplicateCheck::ProcessorConfigured,
        )
    }

    pub async fn account_mode(&self) -> Result<AccountMode, QueryError> {
        let params = query_account_mode_params(self.credentials.query_security_key.as_str());
        let text = self
            .post_form_text("/api/query.php", &params)
            .await
            .map_err(WireError::into_query)?;
        query_account_mode_from_xml(&text).map_err(WireError::into_query)
    }

    /// Submits a sale without internal retries.
    ///
    /// This future is not cancellation-safe. Once polled, dropping it does not
    /// prove NMI did not receive the mutation; reconcile before any replacement
    /// request.
    pub async fn sale(&self, request: SaleRequest) -> Result<PaymentOutcome, MutationError> {
        validate_sale_request(&request, self.credentials.private_api_key.as_str())?;
        self.sale_wire(request)
            .await
            .map_err(WireError::into_mutation)
    }

    async fn sale_wire(&self, request: SaleRequest) -> Result<PaymentOutcome, WireError> {
        if request.vault_action == Some(VaultAction::AddCustomer) {
            return self.classic_sale(request).await;
        }
        ensure_supported_sale_currency(&request.currency)?;
        let amount = amount_value(request.amount_cents)?;
        let body = sale_body_json(&request, amount, self.duplicate_check);
        let value = self.post_json("/api/v5/payments/sale", body).await?;
        let mut outcome = payment_outcome_from_json(&value).map_err(WireError::after_success)?;
        if outcome.status == PaymentStatus::Approved
            && outcome.customer_vault_id.is_none()
            && let PaymentSource::CustomerVault(customer_vault_id) = &request.source
        {
            // This is the validated vault identity the host submitted, not an
            // identity echoed by NMI. An approved response attests that NMI
            // processed this exact request, while retaining the effective
            // source lets callers keep their payment-method linkage when the
            // v5 response omits its optional customer_vault_id field.
            outcome.customer_vault_id = Some(customer_vault_id.as_str().into());
        }
        Ok(require_approved_identities(
            outcome,
            ApprovedIdentityRequirement::ApprovedTransaction,
        ))
    }

    /// Stores a tokenized payment method without internal retries.
    ///
    /// This future is not cancellation-safe. Once polled, dropping it does not
    /// prove NMI did not receive the mutation; reconcile before any replacement
    /// request.
    pub async fn store_payment_method(
        &self,
        request: StorePaymentMethodRequest,
    ) -> Result<PaymentOutcome, MutationError> {
        validate_store_payment_method_request(&request, self.credentials.private_api_key.as_str())?;
        self.store_payment_method_wire(request)
            .await
            .map_err(WireError::into_mutation)
    }

    async fn store_payment_method_wire(
        &self,
        request: StorePaymentMethodRequest,
    ) -> Result<PaymentOutcome, WireError> {
        let params = classic_store_payment_method_params(
            self.credentials.private_api_key.as_str(),
            &request,
        );
        let response = self
            .post_mutation_form_text("/api/transact.php", &params)
            .await?;
        classic_payment_outcome_from_form(&response)
            .map(|outcome| {
                require_approved_identities(
                    outcome,
                    ApprovedIdentityRequirement::ApprovedTransactionAndCustomerVault,
                )
            })
            .map_err(WireError::after_success)
    }

    pub async fn query_transaction(
        &self,
        request: TransactionQuery,
    ) -> Result<Option<PaymentOutcome>, QueryError> {
        validate_transaction_query(&request, self.credentials.query_security_key.as_str())?;
        let params =
            query_transaction_params(self.credentials.query_security_key.as_str(), &request);
        let text = self
            .post_form_text("/api/query.php", &params)
            .await
            .map_err(WireError::into_query)?;
        query_outcome_for_request_from_xml(&text, &request).map_err(WireError::into_query)
    }

    pub async fn query_transaction_reports(
        &self,
        request: ReportQuery,
    ) -> Result<Vec<TransactionReport>, QueryError> {
        validate_report_query(&request, self.credentials.query_security_key.as_str())?;
        let admission = self
            .report_admission
            .clone()
            .try_acquire_owned()
            .map_err(|_| {
                QueryError::Unavailable(
                    "NMI transaction report capacity is currently exhausted.".into(),
                )
            })?;
        let params =
            query_transaction_report_params(self.credentials.query_security_key.as_str(), &request);
        let text = self
            .post_form_text_with_limit("/api/query.php", &params, MAX_NMI_REPORT_RESPONSE_BYTES)
            .await
            .map_err(WireError::into_query)?;
        parse_transaction_reports_bounded(text, request.result_limit as usize, admission)
            .await
            .map_err(WireError::into_query)
    }

    async fn classic_sale(&self, request: SaleRequest) -> Result<PaymentOutcome, WireError> {
        ensure_supported_sale_currency(&request.currency)?;
        let amount = amount_string(request.amount_cents)?;
        let params = classic_sale_params(
            self.credentials.private_api_key.as_str(),
            &request,
            amount,
            self.duplicate_check,
        );
        let response = self
            .post_mutation_form_text("/api/transact.php", &params)
            .await?;
        classic_payment_outcome_from_form(&response)
            .map(|outcome| {
                require_approved_identities(
                    outcome,
                    ApprovedIdentityRequirement::ApprovedTransactionAndCustomerVault,
                )
            })
            .map_err(WireError::after_success)
    }
}

async fn parse_transaction_reports_bounded(
    text: String,
    result_limit: usize,
    admission: OwnedSemaphorePermit,
) -> Result<Vec<TransactionReport>, WireError> {
    run_blocking_report_parse(admission, move || {
        let reports = query_transaction_reports_from_xml(&text)?;
        if reports.len() > result_limit {
            return Err(WireError::MalformedResponse(
                "NMI transaction report response exceeded the requested result limit.".to_owned(),
            ));
        }
        Ok(reports)
    })
    .await
}

async fn run_blocking_report_parse<T, Parse>(
    admission: OwnedSemaphorePermit,
    parse: Parse,
) -> Result<T, WireError>
where
    T: Send + 'static,
    Parse: FnOnce() -> Result<T, WireError> + Send + 'static,
{
    let joined = tokio::task::spawn_blocking(move || {
        let _admission = admission;
        parse()
    })
    .await;
    finish_blocking_report_parse(joined)
}

fn finish_blocking_report_parse<T>(
    joined: Result<Result<T, WireError>, tokio::task::JoinError>,
) -> Result<T, WireError> {
    match joined {
        Ok(parse_result) => parse_result,
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        Err(_) => Err(WireError::Unavailable(
            "NMI transaction report parsing task was cancelled.".to_owned(),
        )),
    }
}

#[cfg(test)]
mod tests;
