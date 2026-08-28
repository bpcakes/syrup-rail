use std::fmt;

use serde::{Serialize, Serializer, ser::SerializeMap};

use crate::{
    DuplicateCheck, ReportQuery, SaleIntent, SaleRequest, StorePaymentMethodRequest,
    TransactionQuery,
};

use super::WireError;
use super::request_budget::OutboundRequestBudget;
use super::validation::trimmed_optional;

pub(super) enum NmiFormValue<'a> {
    Borrowed(&'a str),
    PublicOwned(String),
}

impl NmiFormValue<'_> {
    pub(super) fn as_str(&self) -> &str {
        match self {
            Self::Borrowed(value) => value,
            Self::PublicOwned(value) => value,
        }
    }
}

struct NmiFormParam<'a> {
    key: &'static str,
    value: NmiFormValue<'a>,
}

#[derive(Default)]
pub(super) struct NmiFormParams<'a> {
    fields: Vec<NmiFormParam<'a>>,
}

impl<'a> NmiFormParams<'a> {
    fn push_borrowed(&mut self, key: &'static str, value: &'a str) {
        self.fields.push(NmiFormParam {
            key,
            value: NmiFormValue::Borrowed(value),
        });
    }

    fn push_public_owned(&mut self, key: &'static str, value: String) {
        self.fields.push(NmiFormParam {
            key,
            value: NmiFormValue::PublicOwned(value),
        });
    }

    #[cfg(test)]
    pub(super) fn iter(&self) -> impl Iterator<Item = (&'static str, &str)> {
        self.fields
            .iter()
            .map(|field| (field.key, field.value.as_str()))
    }

    #[cfg(test)]
    pub(super) fn field(&self, key: &str) -> Option<&NmiFormValue<'_>> {
        self.fields
            .iter()
            .find(|field| field.key == key)
            .map(|field| &field.value)
    }
}

impl Serialize for NmiFormParams<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(self.fields.len()))?;
        for field in &self.fields {
            map.serialize_entry(field.key, field.value.as_str())?;
        }
        map.end()
    }
}

impl fmt::Debug for NmiFormParams<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = formatter.debug_map();
        for field in &self.fields {
            if nmi_form_param_is_debug_safe(field.key) {
                debug.entry(&field.key, &field.value.as_str());
            } else {
                debug.entry(&field.key, &"[redacted]");
            }
        }
        debug.finish()
    }
}

pub(super) fn ensure_form_request_is_bounded(params: &NmiFormParams<'_>) -> Result<(), WireError> {
    let mut budget = OutboundRequestBudget::new()
        .map_err(|detail| WireError::LocalInvalidRequest(detail.to_owned()))?;
    for field in &params.fields {
        budget
            .add_form_pair(field.key, field.value.as_str())
            .map_err(|detail| WireError::LocalInvalidRequest(detail.to_owned()))?;
    }
    Ok(())
}

fn nmi_form_param_is_debug_safe(key: &str) -> bool {
    matches!(
        key,
        "type"
            | "amount"
            | "currency"
            | "customer_vault"
            | "billing_method"
            | "initiated_by"
            | "stored_credential_indicator"
    )
}

pub(super) fn classic_sale_params<'a>(
    security_key: &'a str,
    request: &'a SaleRequest,
    amount: String,
    duplicate_check: DuplicateCheck,
) -> NmiFormParams<'a> {
    let mut params = NmiFormParams::default();
    params.push_borrowed("security_key", security_key);
    params.push_borrowed("type", "sale");
    params.push_public_owned("amount", amount);
    params.push_borrowed("currency", super::SUPPORTED_NMI_CURRENCY);
    if let Some(seconds) = duplicate_check.wire_seconds() {
        params.push_public_owned("dup_seconds", seconds.to_string());
    }
    params.push_borrowed("orderid", request.order_id.trim());
    match &request.intent {
        SaleIntent::PaymentToken(payment_token) => {
            params.push_borrowed("payment_token", payment_token);
        }
        SaleIntent::CustomerVault(customer_vault_id) => {
            params.push_borrowed("customer_vault_id", customer_vault_id);
        }
        SaleIntent::AddCustomer { payment_token } => {
            params.push_borrowed("payment_token", payment_token);
            params.push_borrowed("customer_vault", "add_customer");
        }
        SaleIntent::InitialStoredCredential { payment_token } => {
            params.push_borrowed("payment_token", payment_token);
            params.push_borrowed("customer_vault", "add_customer");
            params.push_borrowed("billing_method", "recurring");
            params.push_borrowed("stored_credential_indicator", "stored");
            params.push_borrowed("initiated_by", "customer");
        }
        SaleIntent::RecurringStoredCredential {
            customer_vault_id,
            initial_transaction_id,
        } => {
            params.push_borrowed("customer_vault_id", customer_vault_id);
            params.push_borrowed("billing_method", "recurring");
            params.push_borrowed("stored_credential_indicator", "used");
            params.push_borrowed("initiated_by", "merchant");
            params.push_borrowed("initial_transaction_id", initial_transaction_id);
        }
    }
    if let Some(contact) = &request.billing_contact {
        if let Some(first_name) = trimmed_optional(&contact.first_name) {
            params.push_borrowed("first_name", first_name);
        }
        if let Some(last_name) = trimmed_optional(&contact.last_name) {
            params.push_borrowed("last_name", last_name);
        }
        if let Some(email) = trimmed_optional(&contact.email) {
            params.push_borrowed("email", email);
        }
    }
    params
}

pub(super) fn classic_store_payment_method_params<'a>(
    security_key: &'a str,
    request: &'a StorePaymentMethodRequest,
) -> NmiFormParams<'a> {
    let mut params = NmiFormParams::default();
    params.push_borrowed("security_key", security_key);
    params.push_borrowed("customer_vault", "add_customer");
    params.push_borrowed("payment_token", &request.payment_token);
    params.push_borrowed("orderid", request.order_id.trim());
    params.push_borrowed("type", "validate");
    // NMI documents `stored` for the initial transaction that stores
    // credentials; future renewals use `used` with the returned transaction id.
    params.push_borrowed("billing_method", "recurring");
    params.push_borrowed("initiated_by", "customer");
    params.push_borrowed("stored_credential_indicator", "stored");
    if let Some(contact) = &request.billing_contact {
        if let Some(first_name) = trimmed_optional(&contact.first_name) {
            params.push_borrowed("first_name", first_name);
        }
        if let Some(last_name) = trimmed_optional(&contact.last_name) {
            params.push_borrowed("last_name", last_name);
        }
        if let Some(email) = trimmed_optional(&contact.email) {
            params.push_borrowed("email", email);
        }
    }
    params
}

pub(super) fn query_transaction_params<'a>(
    security_key: &'a str,
    request: &'a TransactionQuery,
) -> NmiFormParams<'a> {
    let mut params = NmiFormParams::default();
    params.push_borrowed("security_key", security_key);
    if let Some(transaction_id) = trimmed_optional(&request.transaction_id) {
        params.push_borrowed("transaction_id", transaction_id);
    }
    if let Some(order_id) = trimmed_optional(&request.order_id) {
        params.push_borrowed("order_id", order_id);
    }
    params
}

pub(super) fn query_account_mode_params(security_key: &str) -> NmiFormParams<'_> {
    let mut params = NmiFormParams::default();
    params.push_borrowed("security_key", security_key);
    params.push_borrowed("report_type", "test_mode_status");
    params
}

pub(super) fn query_transaction_report_params<'a>(
    security_key: &'a str,
    request: &'a ReportQuery,
) -> NmiFormParams<'a> {
    let mut params = NmiFormParams::default();
    params.push_borrowed("security_key", security_key);
    params.push_borrowed("start_date", &request.start_date);
    params.push_borrowed("end_date", &request.end_date);
    params.push_borrowed("transaction_type", "cc");
    params.push_public_owned("result_limit", request.result_limit.to_string());
    params.push_public_owned("page_number", request.page_number.to_string());
    params.push_borrowed("result_order", "standard");
    params
}

pub(super) fn amount_string(amount_cents: i32) -> Result<String, WireError> {
    if amount_cents <= 0 {
        return Err(WireError::LocalInvalidRequest(
            "payment amount must be positive".to_owned(),
        ));
    }
    Ok(format!("{}.{:02}", amount_cents / 100, amount_cents % 100))
}
