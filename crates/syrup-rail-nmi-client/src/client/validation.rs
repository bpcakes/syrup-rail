use crate::{
    BillingContact, MutationError, QueryError, ReportQuery, SaleIntent, SaleRequest,
    StorePaymentMethodRequest, TransactionQuery,
};

use super::request_budget::{OutboundRequestValidator, RequestEncoding};
use super::{
    MAX_NMI_CONTACT_NAME_BYTES, MAX_NMI_EMAIL_BYTES, MAX_NMI_IDENTIFIER_BYTES,
    MAX_NMI_ORDER_ID_BYTES, MAX_NMI_PAYMENT_TOKEN_BYTES, MAX_NMI_REPORT_DATE_BYTES,
    MAX_NMI_TRANSACTION_REPORTS, SUPPORTED_NMI_CURRENCY,
};

pub(super) fn validate_sale_request(
    request: &SaleRequest,
    private_api_key: &str,
) -> Result<(), MutationError> {
    validate_sale_request_size(request, private_api_key).map_err(invalid_mutation)?;
    if request.amount_cents <= 0 {
        return Err(invalid_mutation("payment amount must be positive"));
    }
    if request.order_id.trim().is_empty() {
        return Err(invalid_mutation("payment order ID is required"));
    }
    match &request.intent {
        SaleIntent::PaymentToken(token)
        | SaleIntent::AddCustomer {
            payment_token: token,
        }
        | SaleIntent::InitialStoredCredential {
            payment_token: token,
        } if token.trim().is_empty() => {
            return Err(invalid_mutation("payment token is required"));
        }
        SaleIntent::CustomerVault(id)
        | SaleIntent::RecurringStoredCredential {
            customer_vault_id: id,
            ..
        } if id.trim().is_empty() => {
            return Err(invalid_mutation("Customer Vault ID is required"));
        }
        SaleIntent::RecurringStoredCredential {
            initial_transaction_id,
            ..
        } if initial_transaction_id.trim().is_empty() => {
            return Err(invalid_mutation(
                "merchant-initiated stored credentials require the initial transaction ID",
            ));
        }
        SaleIntent::PaymentToken(_)
        | SaleIntent::CustomerVault(_)
        | SaleIntent::AddCustomer { .. }
        | SaleIntent::InitialStoredCredential { .. }
        | SaleIntent::RecurringStoredCredential { .. } => {}
    }
    Ok(())
}

pub(super) fn validate_store_payment_method_request(
    request: &StorePaymentMethodRequest,
    private_api_key: &str,
) -> Result<(), MutationError> {
    validate_store_payment_method_request_size(request, private_api_key)
        .map_err(invalid_mutation)?;
    if request.payment_token.trim().is_empty() {
        return Err(invalid_mutation("payment token is required"));
    }
    if request.order_id.trim().is_empty() {
        return Err(invalid_mutation("payment order ID is required"));
    }
    Ok(())
}

pub(super) fn validate_transaction_query(
    request: &TransactionQuery,
    query_security_key: &str,
) -> Result<(), QueryError> {
    validate_transaction_query_size(request, query_security_key).map_err(invalid_query)?;
    let transaction_id = trimmed_optional(&request.transaction_id);
    let order_id = trimmed_optional(&request.order_id);
    if transaction_id.is_none() && order_id.is_none() {
        return Err(QueryError::InvalidRequest(
            "transaction ID or order ID is required".into(),
        ));
    }
    Ok(())
}

pub(super) fn validate_report_query(
    request: &ReportQuery,
    query_security_key: &str,
) -> Result<(), QueryError> {
    validate_report_query_size(request, query_security_key).map_err(invalid_query)?;
    if request.start_date.trim().is_empty() || request.end_date.trim().is_empty() {
        return Err(QueryError::InvalidRequest(
            "report start and end dates are required".into(),
        ));
    }
    if !(1..=MAX_NMI_TRANSACTION_REPORTS as i64).contains(&request.result_limit) {
        return Err(QueryError::InvalidRequest(
            "report result limit is outside the supported range".into(),
        ));
    }
    if request.page_number < 0 {
        return Err(QueryError::InvalidRequest(
            "report page number must be non-negative".into(),
        ));
    }
    Ok(())
}

fn invalid_mutation(detail: &'static str) -> MutationError {
    MutationError::InvalidRequest(detail.into())
}

fn invalid_query(detail: &'static str) -> QueryError {
    QueryError::InvalidRequest(detail.into())
}

fn validate_billing_contact(
    validator: &mut OutboundRequestValidator,
    contact: &BillingContact,
) -> Result<(), &'static str> {
    if let Some(value) = &contact.first_name {
        validator.trimmed_field(
            value,
            MAX_NMI_CONTACT_NAME_BYTES,
            "billing first name exceeds the supported size",
        )?;
    }
    if let Some(value) = &contact.last_name {
        validator.trimmed_field(
            value,
            MAX_NMI_CONTACT_NAME_BYTES,
            "billing last name exceeds the supported size",
        )?;
    }
    if let Some(value) = &contact.email {
        validator.trimmed_field(
            value,
            MAX_NMI_EMAIL_BYTES,
            "billing email exceeds the supported size",
        )?;
    }
    Ok(())
}

fn validate_sale_request_size(
    request: &SaleRequest,
    private_api_key: &str,
) -> Result<(), &'static str> {
    let encoding = if request.intent.uses_classic_api() {
        RequestEncoding::Form
    } else {
        RequestEncoding::Json
    };
    let mut validator = OutboundRequestValidator::new(encoding, private_api_key)?;
    validator.field(
        SUPPORTED_NMI_CURRENCY,
        SUPPORTED_NMI_CURRENCY.len(),
        "payment currency must be USD",
    )?;
    validator.trimmed_field(
        &request.order_id,
        MAX_NMI_ORDER_ID_BYTES,
        "payment order ID exceeds the supported size",
    )?;
    match &request.intent {
        SaleIntent::PaymentToken(payment_token)
        | SaleIntent::AddCustomer { payment_token }
        | SaleIntent::InitialStoredCredential { payment_token } => validator.field(
            payment_token,
            MAX_NMI_PAYMENT_TOKEN_BYTES,
            "payment token exceeds the supported size",
        )?,
        SaleIntent::CustomerVault(customer_vault_id)
        | SaleIntent::RecurringStoredCredential {
            customer_vault_id, ..
        } => validator.field(
            customer_vault_id,
            MAX_NMI_IDENTIFIER_BYTES,
            "Customer Vault ID exceeds the supported size",
        )?,
    }
    if let SaleIntent::RecurringStoredCredential {
        initial_transaction_id,
        ..
    } = &request.intent
    {
        validator.field(
            initial_transaction_id,
            MAX_NMI_IDENTIFIER_BYTES,
            "initial transaction ID exceeds the supported size",
        )?;
    }
    if let Some(contact) = &request.billing_contact {
        validate_billing_contact(&mut validator, contact)?;
    }
    Ok(())
}

fn validate_store_payment_method_request_size(
    request: &StorePaymentMethodRequest,
    private_api_key: &str,
) -> Result<(), &'static str> {
    let mut validator = OutboundRequestValidator::new(RequestEncoding::Form, private_api_key)?;
    validator.field(
        &request.payment_token,
        MAX_NMI_PAYMENT_TOKEN_BYTES,
        "payment token exceeds the supported size",
    )?;
    validator.trimmed_field(
        &request.order_id,
        MAX_NMI_ORDER_ID_BYTES,
        "payment order ID exceeds the supported size",
    )?;
    if let Some(contact) = &request.billing_contact {
        validate_billing_contact(&mut validator, contact)?;
    }
    Ok(())
}

fn validate_transaction_query_size(
    request: &TransactionQuery,
    query_security_key: &str,
) -> Result<(), &'static str> {
    let mut validator = OutboundRequestValidator::new(RequestEncoding::Form, query_security_key)?;
    if let Some(transaction_id) = &request.transaction_id {
        validator.trimmed_field(
            transaction_id,
            MAX_NMI_IDENTIFIER_BYTES,
            "transaction ID exceeds the supported size",
        )?;
    }
    if let Some(order_id) = &request.order_id {
        validator.trimmed_field(
            order_id,
            MAX_NMI_ORDER_ID_BYTES,
            "payment order ID exceeds the supported size",
        )?;
    }
    Ok(())
}

fn validate_report_query_size(
    request: &ReportQuery,
    query_security_key: &str,
) -> Result<(), &'static str> {
    let mut validator = OutboundRequestValidator::new(RequestEncoding::Form, query_security_key)?;
    validator.field(
        &request.start_date,
        MAX_NMI_REPORT_DATE_BYTES,
        "report start date exceeds the supported size",
    )?;
    validator.field(
        &request.end_date,
        MAX_NMI_REPORT_DATE_BYTES,
        "report end date exceeds the supported size",
    )?;
    Ok(())
}

pub(super) fn trimmed_optional(value: &Option<String>) -> Option<&str> {
    let value = value.as_deref()?.trim();
    if value.is_empty() { None } else { Some(value) }
}
