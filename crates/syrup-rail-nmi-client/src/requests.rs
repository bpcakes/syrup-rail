use std::fmt;

pub struct BillingContact {
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    pub email: Option<String>,
    pub address: Option<BillingAddress>,
}

impl fmt::Debug for BillingContact {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BillingContact")
            .field("has_first_name", &self.first_name.is_some())
            .field("has_last_name", &self.last_name.is_some())
            .field("has_email", &self.email.is_some())
            .field("has_address", &self.address.is_some())
            .finish()
    }
}

/// Billing address sent with Classic sale/validate and v5 sale requests.
///
/// Values are trimmed on the wire, and optional fields that are blank after
/// trimming are omitted. Requests are rejected before network I/O unless
/// `address1` is non-blank and at most 100 bytes, `address2` is at most 100
/// bytes, `city` is at most 50 bytes, a non-blank `state` is exactly two ASCII
/// letters or digits, a `zip` is at most 20 bytes of ASCII letters, digits,
/// spaces or hyphens, and `country` is exactly two uppercase ASCII letters.
/// Byte limits apply to the untrimmed UTF-8 value, so `state` and `country`
/// cannot carry surrounding whitespace.
///
/// These are this library's conservative local rules. NMI documents 100, 100,
/// 50, 50, 20 and 2 character limits for the v5 sale billing address and no
/// maximum lengths for the Classic fields. Passing local validation does not
/// mean NMI or the processor will accept the address or the payment.
pub struct BillingAddress {
    pub address1: String,
    pub address2: Option<String>,
    pub city: Option<String>,
    pub state: Option<String>,
    pub zip: Option<String>,
    pub country: String,
}

impl fmt::Debug for BillingAddress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BillingAddress")
            .field("has_address2", &self.address2.is_some())
            .field("has_city", &self.city.is_some())
            .field("has_state", &self.state.is_some())
            .field("has_zip", &self.zip.is_some())
            .finish()
    }
}

pub enum PaymentSource {
    PaymentToken(String),
    CustomerVault(String),
}

impl fmt::Debug for PaymentSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PaymentToken(_) => formatter.write_str("PaymentSource::PaymentToken([redacted])"),
            Self::CustomerVault(_) => {
                formatter.write_str("PaymentSource::CustomerVault([redacted])")
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VaultAction {
    AddCustomer,
}

pub enum StoredCredential {
    InitialCustomer,
    RecurringMerchant { initial_transaction_id: String },
}

impl fmt::Debug for StoredCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InitialCustomer => formatter.write_str("StoredCredential::InitialCustomer"),
            Self::RecurringMerchant { .. } => formatter
                .debug_struct("StoredCredential::RecurringMerchant")
                .field("has_initial_transaction_id", &true)
                .finish(),
        }
    }
}

pub struct SaleRequest {
    pub amount_cents: i32,
    pub currency: String,
    pub order_id: String,
    pub source: PaymentSource,
    pub vault_action: Option<VaultAction>,
    pub stored_credential: Option<StoredCredential>,
    pub billing_contact: Option<BillingContact>,
}

impl fmt::Debug for SaleRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SaleRequest")
            .field("amount_cents", &self.amount_cents)
            .field("currency", &self.currency)
            .field("has_order_id", &(!self.order_id.is_empty()))
            .field("source", &self.source)
            .field("vault_action", &self.vault_action)
            .field("stored_credential", &self.stored_credential)
            .field("has_billing_contact", &self.billing_contact.is_some())
            .finish()
    }
}

pub struct StorePaymentMethodRequest {
    pub payment_token: String,
    pub order_id: String,
    pub billing_contact: Option<BillingContact>,
}

impl fmt::Debug for StorePaymentMethodRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StorePaymentMethodRequest")
            .field("payment_token", &"[redacted]")
            .field("has_order_id", &(!self.order_id.is_empty()))
            .field("has_billing_contact", &self.billing_contact.is_some())
            .finish()
    }
}

/// Exact read-only diagnostic lookup by NMI transaction ID.
///
/// The client never searches by order ID for diagnostics.
#[derive(Clone)]
pub struct TransactionDiagnosticsQuery {
    pub transaction_id: String,
}

impl fmt::Debug for TransactionDiagnosticsQuery {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TransactionDiagnosticsQuery")
            .field("transaction_id", &"[redacted]")
            .finish()
    }
}

#[derive(Clone)]
pub struct TransactionQuery {
    pub transaction_id: Option<String>,
    pub order_id: Option<String>,
}

impl fmt::Debug for TransactionQuery {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TransactionQuery")
            .field("has_transaction_id", &self.transaction_id.is_some())
            .field("has_order_id", &self.order_id.is_some())
            .finish()
    }
}

#[derive(Clone, Debug)]
pub struct ReportQuery {
    pub start_date: String,
    pub end_date: String,
    pub result_limit: i64,
    pub page_number: i64,
}
