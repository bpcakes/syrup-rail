use crate::lossless_json::LosslessJsonValue;
use crate::{PaymentDescriptor, PaymentOutcome, SensitiveText};

use super::super::{
    WireError,
    text::{bounded_gateway_text, last4, parse_expiry, sensitive_gateway_field, valid_last4},
};
use super::common::{
    DecisionFieldKind, IdentifierPresence, PaymentDecisionFields, ResolvedPaymentDecision,
    ResolvedScalar, ScalarOccurrence, ScalarOccurrenceCollector, finalize_payment_identifiers,
    rate_limited_wire_error, resolve_optional_scalar,
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

fn json_scalar_occurrence(value: &LosslessJsonValue) -> ScalarOccurrence<'_> {
    match value {
        LosslessJsonValue::String(value) => {
            ScalarOccurrence::Scalar(std::borrow::Cow::Borrowed(value))
        }
        LosslessJsonValue::Number(value) => {
            ScalarOccurrence::CoercedScalar(std::borrow::Cow::Owned(value.to_string()))
        }
        LosslessJsonValue::Null => ScalarOccurrence::Null,
        LosslessJsonValue::Bool(_) | LosslessJsonValue::Array(_) | LosslessJsonValue::Object(_) => {
            ScalarOccurrence::InvalidShape
        }
    }
}

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
            let occurrence = json_scalar_occurrence(field_value);
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
        let occurrence = json_scalar_occurrence(field_value);
        if bounded {
            collector.record_bounded(occurrence);
        } else {
            collector.record(occurrence);
        }
    }
    collector
}

fn json_is_known_preprocessing_rate_limit(
    value: &LosslessJsonValue,
    decision: &ResolvedPaymentDecision,
) -> bool {
    let LosslessJsonValue::Object(fields) = value else {
        return false;
    };
    decision.is_known_preprocessing_rate_limit()
        && fields.iter().all(|(name, value)| match name.as_str() {
            "response" | "response_code" => true,
            "response_text" => matches!(
                value,
                LosslessJsonValue::String(message) if message == "Rate limit exceeded"
            ),
            _ => false,
        })
}

pub(in crate::client) fn payment_outcome_from_json(
    value: &LosslessJsonValue,
) -> Result<PaymentOutcome, WireError> {
    let decision = payment_decision_from_json(value);
    let customer_vault_id = collect_json_identifier(
        value,
        JSON_CUSTOMER_VAULT_ID_PATHS,
        IdentifierPresence::Optional,
    );
    if json_is_known_preprocessing_rate_limit(value, &decision) {
        return Err(rate_limited_wire_error());
    }
    let (response_text, _) =
        resolve_optional_scalar(collect_json_direct_scalar(value, "response_text", true).finish());
    let mut status = decision.status;
    let mut diagnostics = decision.diagnostics;
    let (transaction_id, customer_vault_id) = finalize_payment_identifiers(
        &mut status,
        collect_json_identifier(
            value,
            JSON_TRANSACTION_ID_PATHS,
            IdentifierPresence::ForegroundRequired,
        ),
        customer_vault_id,
        &mut diagnostics,
    );
    Ok(PaymentOutcome {
        status,
        transaction_id,
        customer_vault_id,
        response: sensitive_gateway_field(decision.response),
        response_code: sensitive_gateway_field(decision.response_code),
        response_text: sensitive_gateway_field(response_text),
        condition: sensitive_gateway_field(decision.condition),
        descriptor: descriptor_from_json(value),
        diagnostics,
    }
    .normalize_diagnostics())
}

fn payment_decision_from_json(value: &LosslessJsonValue) -> ResolvedPaymentDecision {
    PaymentDecisionFields::new(
        collect_json_direct_scalar(value, "response", false)
            .finish_decision(DecisionFieldKind::Response),
        collect_json_direct_scalar(value, "response_code", false)
            .finish_decision(DecisionFieldKind::ResponseCode),
        collect_json_direct_scalar(value, "status", false)
            .finish_decision(DecisionFieldKind::GatewayState),
        collect_json_direct_scalar(value, "condition", false)
            .finish_decision(DecisionFieldKind::GatewayState),
    )
    .resolve()
}

pub(in crate::client) fn is_known_preprocessing_http_error_envelope(
    value: &LosslessJsonValue,
    http_status: u16,
) -> bool {
    let LosslessJsonValue::Object(fields) = value else {
        return false;
    };
    const ALLOWED_FIELDS: &[&str] = &["type", "error_code", "message", "ref_id", "status"];
    if fields.is_empty()
        || fields
            .iter()
            .any(|(name, _)| !ALLOWED_FIELDS.contains(&name.as_str()))
        || ALLOWED_FIELDS.iter().any(|name| {
            fields
                .iter()
                .filter(|(candidate, _)| candidate == name)
                .count()
                > 1
        })
    {
        return false;
    }
    let has_error_marker = fields.iter().any(|(name, value)| {
        matches!(name.as_str(), "error_code" | "message")
            && matches!(value, LosslessJsonValue::String(text) if !text.trim().is_empty())
    });
    has_error_marker
        && fields.iter().all(|(name, value)| match name.as_str() {
            "type" | "error_code" | "message" => {
                matches!(value, LosslessJsonValue::String(text) if !text.trim().is_empty())
            }
            "ref_id" => matches!(
                value,
                LosslessJsonValue::Null | LosslessJsonValue::String(_)
            ),
            "status" => true,
            _ => false,
        })
        && !payment_decision_from_json(value).has_payment_processing_evidence(http_status)
}

pub(in crate::client) fn is_documented_v5_validation_error(value: &LosslessJsonValue) -> bool {
    let LosslessJsonValue::Object(fields) = value else {
        return false;
    };
    const ALLOWED_FIELDS: &[&str] = &["type", "error_code", "message", "ref_id", "details"];
    if fields
        .iter()
        .any(|(name, _)| !ALLOWED_FIELDS.contains(&name.as_str()))
    {
        return false;
    }
    if ALLOWED_FIELDS.iter().any(|name| {
        fields
            .iter()
            .filter(|(candidate, _)| candidate == name)
            .count()
            > 1
    }) {
        return false;
    }
    matches!(
        value.last_field("type"),
        Some(LosslessJsonValue::String(kind)) if kind == "validationError"
    ) && matches!(
        value.last_field("error_code"),
        Some(LosslessJsonValue::String(code)) if code == "E_INVALID_SUBMISSION"
    ) && matches!(
        value.last_field("message"),
        Some(LosslessJsonValue::String(message)) if !message.trim().is_empty()
    ) && value.last_field("ref_id").is_none_or(|ref_id| {
        matches!(
            ref_id,
            LosslessJsonValue::Null | LosslessJsonValue::String(_)
        )
    }) && value
        .last_field("details")
        .is_some_and(documented_validation_details)
}

fn documented_validation_details(value: &LosslessJsonValue) -> bool {
    let LosslessJsonValue::Array(details) = value else {
        return false;
    };
    !details.is_empty()
        && details.iter().all(|detail| {
            let LosslessJsonValue::Object(fields) = detail else {
                return false;
            };
            if fields.len() != 2
                || fields
                    .iter()
                    .any(|(name, _)| !matches!(name.as_str(), "fieldName" | "message"))
            {
                return false;
            }
            matches!(
                detail.last_field("fieldName"),
                Some(LosslessJsonValue::String(field)) if !field.trim().is_empty()
            ) && matches!(
                detail.last_field("message"),
                Some(LosslessJsonValue::String(message)) if !message.trim().is_empty()
            )
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
