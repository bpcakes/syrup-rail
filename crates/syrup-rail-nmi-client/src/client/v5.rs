use serde_json::{Value, json};

use crate::{BillingContact, DuplicateCheck, SaleIntent, SaleRequest};

use super::WireError;
use super::validation::trimmed_optional;

fn payment_details_json(intent: &SaleIntent) -> Value {
    // serde_json owns a temporary copy of these sensitive identifiers. The
    // source strings and client credentials retain their zeroization guards,
    // but serde_json and reqwest do not offer reliable zeroization of their
    // serialized request buffers; this is an explicit non-protection.
    match intent {
        SaleIntent::PaymentToken(payment_token)
        | SaleIntent::AddCustomer { payment_token }
        | SaleIntent::InitialStoredCredential { payment_token } => {
            json!({ "payment_token": payment_token })
        }
        SaleIntent::CustomerVault(customer_vault_id)
        | SaleIntent::RecurringStoredCredential {
            customer_vault_id, ..
        } => {
            json!({ "customer_vault_id": customer_vault_id })
        }
    }
}

pub(super) fn order_details_json(order_id: &str) -> Value {
    json!({ "id": order_id.trim() })
}

pub(super) fn amount_value(amount_cents: i32) -> Result<Value, WireError> {
    if amount_cents <= 0 {
        return Err(WireError::LocalInvalidRequest(
            "payment amount must be positive".to_owned(),
        ));
    }
    Ok(Value::String(format!(
        "{}.{:02}",
        amount_cents / 100,
        amount_cents % 100
    )))
}

pub(super) fn sale_body_json(
    request: &SaleRequest,
    amount: Value,
    duplicate_check: DuplicateCheck,
) -> Value {
    let mut body = json!({
        "amount": amount,
        "currency": super::SUPPORTED_NMI_CURRENCY,
        "payment_details": payment_details_json(&request.intent),
        "order_details": order_details_json(&request.order_id),
    });
    if let Some(seconds) = duplicate_check.wire_seconds() {
        body["dup_seconds"] = json!(seconds);
    }
    if let Some(contact) = &request.billing_contact {
        body["billing_address"] = billing_address_json(contact);
    }
    // NMI's v5 custom-recurring contract puts `billing_method=recurring`
    // under `customer_vault` for scheduled CIT/MIT sales, even when the
    // request is only using an existing vault entry rather than mutating it.
    if matches!(
        request.intent,
        SaleIntent::InitialStoredCredential { .. } | SaleIntent::RecurringStoredCredential { .. }
    ) {
        body["customer_vault"] = customer_vault_json();
    }
    if let Some(cit_mit) = cit_mit_json(&request.intent) {
        body["cit_mit"] = cit_mit;
    }
    body
}

fn customer_vault_json() -> Value {
    json!({ "billing_method": "recurring" })
}

fn cit_mit_json(intent: &SaleIntent) -> Option<Value> {
    match intent {
        SaleIntent::InitialStoredCredential { .. } => Some(json!({
            "stored_credential_indicator": "stored",
            "initiated_by": "customer",
        })),
        SaleIntent::RecurringStoredCredential {
            initial_transaction_id,
            ..
        } => Some(json!({
            "stored_credential_indicator": "used",
            "initiated_by": "merchant",
            "initial_transaction_id": initial_transaction_id,
        })),
        SaleIntent::PaymentToken(_)
        | SaleIntent::CustomerVault(_)
        | SaleIntent::AddCustomer { .. } => None,
    }
}

fn billing_address_json(contact: &BillingContact) -> Value {
    let mut value = json!({});
    if let Some(first_name) = trimmed_optional(&contact.first_name) {
        value["first_name"] = json!(first_name);
    }
    if let Some(last_name) = trimmed_optional(&contact.last_name) {
        value["last_name"] = json!(last_name);
    }
    if let Some(email) = trimmed_optional(&contact.email) {
        value["email"] = json!(email);
    }
    value
}
