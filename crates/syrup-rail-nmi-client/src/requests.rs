use std::fmt;

pub struct BillingContact {
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    pub email: Option<String>,
}

impl fmt::Debug for BillingContact {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BillingContact")
            .field("has_first_name", &self.first_name.is_some())
            .field("has_last_name", &self.last_name.is_some())
            .field("has_email", &self.email.is_some())
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
