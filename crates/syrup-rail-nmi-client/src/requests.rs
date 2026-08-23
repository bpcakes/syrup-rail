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

/// One valid NMI sale mode, including its exact payment source.
pub enum SaleIntent {
    /// Charge a browser-generated payment token without storing it.
    PaymentToken(String),
    /// Charge an existing Customer Vault entry without stored-credential fields.
    CustomerVault(String),
    /// Charge a token and add a Customer Vault entry without CIT/MIT fields.
    AddCustomer { payment_token: String },
    /// Run the customer-initiated transaction that stores a credential.
    InitialStoredCredential { payment_token: String },
    /// Run a merchant-initiated recurring transaction against a stored credential.
    RecurringStoredCredential {
        customer_vault_id: String,
        initial_transaction_id: String,
    },
}

impl SaleIntent {
    /// Converts the former independent source/action/credential fields into one
    /// closed intent. The seven invalid legacy shapes are rejected.
    pub fn from_legacy_parts(
        source: PaymentSource,
        vault_action: Option<VaultAction>,
        stored_credential: Option<StoredCredential>,
    ) -> Result<Self, SaleIntentBuildError> {
        match (source, vault_action, stored_credential) {
            (PaymentSource::PaymentToken(payment_token), None, None) => {
                Ok(Self::PaymentToken(payment_token))
            }
            (PaymentSource::CustomerVault(customer_vault_id), None, None) => {
                Ok(Self::CustomerVault(customer_vault_id))
            }
            (PaymentSource::PaymentToken(payment_token), Some(VaultAction::AddCustomer), None) => {
                Ok(Self::AddCustomer { payment_token })
            }
            (
                PaymentSource::PaymentToken(payment_token),
                Some(VaultAction::AddCustomer),
                Some(StoredCredential::InitialCustomer),
            ) => Ok(Self::InitialStoredCredential { payment_token }),
            (
                PaymentSource::CustomerVault(customer_vault_id),
                None,
                Some(StoredCredential::RecurringMerchant {
                    initial_transaction_id,
                }),
            ) => Ok(Self::RecurringStoredCredential {
                customer_vault_id,
                initial_transaction_id,
            }),
            (PaymentSource::CustomerVault(_), Some(VaultAction::AddCustomer), _) => {
                Err(SaleIntentBuildError::AddCustomerRequiresPaymentToken)
            }
            (PaymentSource::CustomerVault(_), None, Some(StoredCredential::InitialCustomer)) => {
                Err(SaleIntentBuildError::InitialStoredCredentialRequiresPaymentToken)
            }
            (PaymentSource::PaymentToken(_), None, Some(StoredCredential::InitialCustomer)) => {
                Err(SaleIntentBuildError::InitialStoredCredentialRequiresAddCustomer)
            }
            (
                PaymentSource::PaymentToken(_),
                _,
                Some(StoredCredential::RecurringMerchant { .. }),
            ) => Err(SaleIntentBuildError::RecurringStoredCredentialRequiresCustomerVault),
        }
    }

    pub(crate) const fn uses_classic_api(&self) -> bool {
        matches!(
            self,
            Self::AddCustomer { .. } | Self::InitialStoredCredential { .. }
        )
    }

    pub(crate) fn customer_vault_id(&self) -> Option<&str> {
        match self {
            Self::CustomerVault(customer_vault_id)
            | Self::RecurringStoredCredential {
                customer_vault_id, ..
            } => Some(customer_vault_id),
            Self::PaymentToken(_)
            | Self::AddCustomer { .. }
            | Self::InitialStoredCredential { .. } => None,
        }
    }
}

impl fmt::Debug for SaleIntent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::PaymentToken(_) => "SaleIntent::PaymentToken([redacted])",
            Self::CustomerVault(_) => "SaleIntent::CustomerVault([redacted])",
            Self::AddCustomer { .. } => "SaleIntent::AddCustomer([redacted])",
            Self::InitialStoredCredential { .. } => {
                "SaleIntent::InitialStoredCredential([redacted])"
            }
            Self::RecurringStoredCredential { .. } => {
                "SaleIntent::RecurringStoredCredential([redacted])"
            }
        })
    }
}

/// Reason independent legacy sale fields cannot form a valid [`SaleIntent`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SaleIntentBuildError {
    /// Adding a Customer Vault entry requires a payment-token source.
    AddCustomerRequiresPaymentToken,
    /// Initial stored credentials require a payment-token source.
    InitialStoredCredentialRequiresPaymentToken,
    /// Initial stored credentials require adding a Customer Vault entry.
    InitialStoredCredentialRequiresAddCustomer,
    /// Recurring stored credentials require an existing Customer Vault source.
    RecurringStoredCredentialRequiresCustomerVault,
}

impl fmt::Display for SaleIntentBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::AddCustomerRequiresPaymentToken => {
                "adding a Customer Vault entry requires a payment token"
            }
            Self::InitialStoredCredentialRequiresPaymentToken => {
                "initial stored credentials require a payment token"
            }
            Self::InitialStoredCredentialRequiresAddCustomer => {
                "initial stored credentials require adding a Customer Vault entry"
            }
            Self::RecurringStoredCredentialRequiresCustomerVault => {
                "merchant-initiated stored credentials require a Customer Vault source"
            }
        })
    }
}

impl std::error::Error for SaleIntentBuildError {}

pub struct SaleRequest {
    pub amount_cents: i32,
    pub order_id: String,
    pub intent: SaleIntent,
    pub billing_contact: Option<BillingContact>,
}

impl fmt::Debug for SaleRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SaleRequest")
            .field("amount_cents", &self.amount_cents)
            .field("has_order_id", &(!self.order_id.is_empty()))
            .field("intent", &self.intent)
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
