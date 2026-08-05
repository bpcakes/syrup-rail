use crate::lossless_json::LosslessJsonValue;
use crate::{PaymentDescriptor, PaymentOutcome, SensitiveText};

use super::super::{
    WireError,
    text::{bounded_gateway_text, last4, parse_expiry, sensitive_gateway_field, valid_last4},
};
use super::common::{
    DecisionFieldKind, IdentifierPresence, PaymentDecisionFields, ResolvedScalar, ScalarOccurrence,
    ScalarOccurrenceCollector, finalize_foreground_identifiers, rate_limited_wire_error,
    resolve_optional_scalar,
};

const JSON_TRANSACTION_ID_PATHS: &[&[&str]] = &[
    &["transaction_id"],
    &["transaction", "id"],
    &["payment", "id"],
    &["id"],
];

const JSON_CUSTOMER_VAULT_ID_PATHS: &[&[&str]] = &[
    &["customer_vault_id"],
    &["customer_vault", "customer_vault_id"],
    &["customer_vault", "customer_id"],
    &["customer_vault", "id"],
    &["customer", "customer_vault_id"],
];

const JSON_AUTHORIZATION_CODE_PATHS: &[&[&str]] = &[
    &["authcode"],
    &["auth_code"],
    &["authorization_code"],
    &["authorization", "code"],
    &["payment", "auth_code"],
    &["payment", "authorization_code"],
    &["payment_details", "auth_code"],
    &["payment_details", "authorization_code"],
];

const JSON_AVS_RESPONSE_PATHS: &[&[&str]] = &[
    &["avsresponse"],
    &["avs_response"],
    &["payment_details", "avs_response"],
    &["payment_details", "card", "avs_response"],
    &["card", "avs_response"],
];

const JSON_CVV_RESPONSE_PATHS: &[&[&str]] = &[
    &["cvvresponse"],
    &["cvv_response"],
    &["payment_details", "cvv_response"],
    &["payment_details", "card", "cvv_response"],
    &["card", "cvv_response"],
];

const JSON_ACTION_PATHS: &[&[&str]] = &[
    &["action"],
    &["actions"],
    &["transaction", "action"],
    &["transaction", "actions"],
    &["payment", "action"],
    &["payment", "actions"],
];

fn collect_json_identifier_path(
    value: &LosslessJsonValue,
    path: &[&str],
    presence: IdentifierPresence,
    collector: &mut ScalarOccurrenceCollector,
) {
    let Some((segment, remaining)) = path.split_first() else {
        return;
    };
    let LosslessJsonValue::Object(fields) = value else {
        return;
    };
    for (_, field_value) in fields.iter().filter(|(field, _)| field == segment) {
        if remaining.is_empty() {
            let occurrence = match field_value {
                LosslessJsonValue::Null => ScalarOccurrence::Null,
                _ => field_value
                    .scalar_text()
                    .map(ScalarOccurrence::Scalar)
                    .unwrap_or(ScalarOccurrence::InvalidShape),
            };
            collector.record_identifier(occurrence, presence);
        } else if matches!(field_value, LosslessJsonValue::Object(_)) {
            collect_json_identifier_path(field_value, remaining, presence, collector);
        } else {
            collector.record(ScalarOccurrence::InvalidShape);
        }
    }
}

fn collect_json_identifier(
    value: &LosslessJsonValue,
    paths: &[&[&str]],
    presence: IdentifierPresence,
) -> ResolvedScalar {
    let mut collector = ScalarOccurrenceCollector::default();
    for path in paths {
        collect_json_identifier_path(value, path, presence, &mut collector);
    }
    collector.finish()
}

fn collect_json_direct_scalar(
    value: &LosslessJsonValue,
    name: &str,
    bounded: bool,
) -> ScalarOccurrenceCollector {
    let mut collector = ScalarOccurrenceCollector::default();
    let LosslessJsonValue::Object(fields) = value else {
        return collector;
    };
    for (_, field_value) in fields.iter().filter(|(field, _)| field == name) {
        let occurrence = field_value
            .scalar_text()
            .map(ScalarOccurrence::Scalar)
            .unwrap_or(ScalarOccurrence::InvalidShape);
        if bounded {
            collector.record_bounded(occurrence);
        } else {
            collector.record(occurrence);
        }
    }
    collector
}

fn json_path_has_processing_evidence(value: &LosslessJsonValue, path: &[&str]) -> bool {
    let Some((segment, remaining)) = path.split_first() else {
        return json_action_value_has_processing_evidence(value);
    };
    let LosslessJsonValue::Object(fields) = value else {
        return true;
    };
    fields
        .iter()
        .filter(|(field, _)| field == segment)
        .any(|(_, field_value)| {
            if remaining.is_empty() {
                json_action_value_has_processing_evidence(field_value)
            } else {
                match field_value {
                    LosslessJsonValue::Null => false,
                    LosslessJsonValue::String(value) if value.trim().is_empty() => false,
                    LosslessJsonValue::Object(_) => {
                        json_path_has_processing_evidence(field_value, remaining)
                    }
                    _ => true,
                }
            }
        })
}

fn json_action_value_has_processing_evidence(value: &LosslessJsonValue) -> bool {
    match value {
        LosslessJsonValue::Null => false,
        LosslessJsonValue::String(value) => !value.trim().is_empty(),
        LosslessJsonValue::Array(values) => !values.is_empty(),
        LosslessJsonValue::Bool(_)
        | LosslessJsonValue::Number(_)
        | LosslessJsonValue::Object(_) => true,
    }
}

fn json_is_known_preprocessing_rate_limit(
    value: &LosslessJsonValue,
    decision: &PaymentDecisionFields,
    scalar_evidence: [&ResolvedScalar; 5],
) -> bool {
    decision.is_rate_limited()
        && scalar_evidence
            .iter()
            .all(|channel| matches!(channel, ResolvedScalar::Missing))
        && !JSON_ACTION_PATHS
            .iter()
            .any(|path| json_path_has_processing_evidence(value, path))
}

pub(in crate::client) fn payment_outcome_from_json(
    value: &LosslessJsonValue,
) -> Result<PaymentOutcome, WireError> {
    let decision = PaymentDecisionFields::new(
        collect_json_direct_scalar(value, "response", false)
            .finish_decision(DecisionFieldKind::Response),
        collect_json_direct_scalar(value, "response_code", false)
            .finish_decision(DecisionFieldKind::ResponseCode),
        collect_json_direct_scalar(value, "status", false)
            .finish_decision(DecisionFieldKind::GatewayState),
        collect_json_direct_scalar(value, "condition", false)
            .finish_decision(DecisionFieldKind::GatewayState),
    );
    let transaction_evidence = collect_json_identifier(
        value,
        JSON_TRANSACTION_ID_PATHS,
        IdentifierPresence::Optional,
    );
    let customer_vault_id = collect_json_identifier(
        value,
        JSON_CUSTOMER_VAULT_ID_PATHS,
        IdentifierPresence::Optional,
    );
    let authorization_code = collect_json_identifier(
        value,
        JSON_AUTHORIZATION_CODE_PATHS,
        IdentifierPresence::Optional,
    );
    let avs_response =
        collect_json_identifier(value, JSON_AVS_RESPONSE_PATHS, IdentifierPresence::Optional);
    let cvv_response =
        collect_json_identifier(value, JSON_CVV_RESPONSE_PATHS, IdentifierPresence::Optional);
    if json_is_known_preprocessing_rate_limit(
        value,
        &decision,
        [
            &transaction_evidence,
            &customer_vault_id,
            &authorization_code,
            &avs_response,
            &cvv_response,
        ],
    ) {
        return Err(rate_limited_wire_error());
    }
    let (response_text, _) =
        resolve_optional_scalar(collect_json_direct_scalar(value, "response_text", true).finish());
    let (mut status, decision_diagnostic) = decision.payment_status();
    let mut diagnostics = decision_diagnostic.into_iter().collect();
    let (transaction_id, customer_vault_id) = finalize_foreground_identifiers(
        &mut status,
        collect_json_identifier(
            value,
            JSON_TRANSACTION_ID_PATHS,
            IdentifierPresence::Required,
        ),
        customer_vault_id,
        &mut diagnostics,
    );
    Ok(PaymentOutcome {
        status,
        transaction_id,
        customer_vault_id,
        response: sensitive_gateway_field(decision.response.raw),
        response_code: sensitive_gateway_field(decision.response_code.raw),
        response_text: sensitive_gateway_field(response_text),
        condition: sensitive_gateway_field(decision.condition.raw),
        descriptor: descriptor_from_json(value),
        diagnostics,
    })
}

fn descriptor_from_json(value: &LosslessJsonValue) -> PaymentDescriptor {
    let payment_details = value.last_field("payment_details").unwrap_or(value);
    let nested_card = payment_details
        .last_field("card")
        .or_else(|| value.last_field("card"));
    let card = nested_card.unwrap_or(payment_details);
    let exp = string_field(card, "exp")
        .or_else(|| string_field(card, "expiry"))
        .or_else(|| string_field(card, "card_exp"))
        .or_else(|| string_field(value, "card_exp"));
    let (exp_month, exp_year) = parse_expiry(exp.as_deref());
    PaymentDescriptor {
        payment_type: string_field(value, "payment_type")
            .or_else(|| string_field(value, "transaction_type"))
            .or_else(|| string_field(payment_details, "type"))
            .map(SensitiveText::new),
        card_brand: string_field(card, "brand")
            .or_else(|| string_field(card, "card_type"))
            .or_else(|| nested_card.and_then(|card| string_field(card, "type")))
            .or_else(|| string_field(value, "card_type"))
            .map(SensitiveText::new),
        card_last4: string_field(card, "last4")
            .and_then(valid_last4)
            .or_else(|| string_field(card, "number").and_then(last4))
            .or_else(|| string_field(card, "card_number").and_then(last4))
            .or_else(|| string_field(value, "card_number").and_then(last4))
            .map(SensitiveText::new),
        card_exp_month: exp_month,
        card_exp_year: exp_year,
    }
}

fn string_field(value: &LosslessJsonValue, field: &str) -> Option<String> {
    let value = value.last_field(field)?.scalar_text()?;
    bounded_gateway_text(&value)
}
