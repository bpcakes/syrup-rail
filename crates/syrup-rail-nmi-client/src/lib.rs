//! A bounded, retry-free NMI payments client.
//!
//! This crate owns NMI wire behavior only. Callers remain responsible for
//! credential persistence, merchant-host allowlisting, durable idempotency,
//! reconciliation policy, subscription scheduling, and validating exposed
//! provider text before persistence.

#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]

mod client;
mod configuration;
mod errors;
mod lossless_json;
mod requests;
mod responses;

pub use client::{Client, ClientFactory};
pub use configuration::{
    ConfigurationError, Credentials, DuplicateCheck, DuplicateCheckWindow, Endpoint,
};
pub use errors::{MutationCertainty, MutationError, QueryError};
pub use requests::{
    BillingContact, PaymentSource, ReportQuery, SaleIntent, SaleIntentBuildError, SaleRequest,
    StorePaymentMethodRequest, StoredCredential, TransactionQuery, VaultAction,
};
pub use responses::{
    AccountMode, PaymentDescriptor, PaymentDescriptorParts, PaymentOutcome,
    PaymentOutcomeDiagnostic, PaymentOutcomeParts, PaymentStatus, SensitiveText, TransactionAction,
    TransactionActionParts, TransactionReport, TransactionReportDiagnostic, TransactionReportParts,
};
