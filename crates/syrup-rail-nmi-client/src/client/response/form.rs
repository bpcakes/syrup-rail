use std::borrow::Cow;

use url::form_urlencoded;

use crate::{PaymentDescriptor, PaymentOutcome, PaymentStatus, SensitiveText};

use super::super::{
    WireError,
    text::{last4, sensitive_gateway_field},
};
use super::common::{
    DecisionFieldKind, IdentifierPresence, PaymentDecisionFields, ResolvedScalar, ScalarOccurrence,
    ScalarOccurrenceCollector, finalize_payment_identifiers, rate_limited_wire_error,
    resolve_optional_scalar,
};

pub(in crate::client) fn classic_payment_outcome_from_form(
    text: &str,
) -> Result<PaymentOutcome, WireError> {
    let fields: Vec<_> = form_urlencoded::parse(text.as_bytes()).collect();
    let decision = PaymentDecisionFields::new(
        collect_classic_scalar(&fields, &["response"], false)
            .finish_decision(DecisionFieldKind::Response),
        collect_classic_scalar(&fields, &["response_code", "responsecode"], false)
            .finish_decision(DecisionFieldKind::ResponseCode),
        collect_classic_scalar(&fields, &["status"], false)
            .finish_decision(DecisionFieldKind::GatewayState),
        collect_classic_scalar(&fields, &["condition"], false)
            .finish_decision(DecisionFieldKind::GatewayState),
    )
    .resolve();
    let (transaction_evidence, transaction_identifier) =
        collect_classic_transaction_identifier(&fields);
    let customer_vault_identifier =
        collect_classic_optional_identifier(&fields, &["customer_vault_id", "customer_vaultid"]);
    let authorization_code = collect_classic_optional_identifier(
        &fields,
        &["authcode", "auth_code", "authorization_code"],
    );
    let avs_response =
        collect_classic_optional_identifier(&fields, &["avsresponse", "avs_response"]);
    let cvv_response =
        collect_classic_optional_identifier(&fields, &["cvvresponse", "cvv_response"]);
    if classic_is_known_preprocessing_rate_limit(
        &fields,
        &decision,
        [
            &transaction_evidence,
            &customer_vault_identifier,
            &authorization_code,
            &avs_response,
            &cvv_response,
        ],
    ) {
        return Err(rate_limited_wire_error());
    }
    let text_occurrences =
        collect_classic_scalar(&fields, &["responsetext", "response_text"], true);
    let approval_evidence = decision
        .approval_evidence
        .merge(text_occurrences.approval_text_evidence());
    let (response_text, invalid_response_text) = resolve_optional_scalar(text_occurrences.finish());
    let has_structured_evidence = decision.has_structured_evidence();
    let mut status = decision.status;
    let mut diagnostics = decision.diagnostics;
    if !has_structured_evidence {
        let response_text_reports_unknown = response_text
            .as_deref()
            .map(|value| value.trim().to_ascii_lowercase())
            .as_deref()
            .is_some_and(gateway_text_reports_unknown);
        if invalid_response_text || response_text_reports_unknown {
            status = PaymentStatus::Unknown;
            if response_text_reports_unknown && !invalid_response_text {
                diagnostics.clear();
            }
        } else {
            return Err(WireError::MalformedResponse(
                "NMI classic response did not include a recognizable payment status.".to_owned(),
            ));
        }
    };
    let (transaction_id, customer_vault_id) = finalize_payment_identifiers(
        &mut status,
        transaction_identifier,
        customer_vault_identifier,
        &mut diagnostics,
    );
    let (payment_type, _) =
        resolve_optional_scalar(collect_classic_scalar(&fields, &["type"], true).finish());
    let (card_brand, _) = resolve_optional_scalar(
        collect_classic_scalar(&fields, &["cctype", "card_type"], true).finish(),
    );
    let (card_number, _) = resolve_optional_scalar(
        collect_classic_scalar(&fields, &["cc_number", "ccnumber"], true).finish(),
    );
    Ok(PaymentOutcome {
        status,
        approval_evidence,
        transaction_id,
        customer_vault_id,
        response: sensitive_gateway_field(decision.response),
        response_code: sensitive_gateway_field(decision.response_code),
        response_text: sensitive_gateway_field(response_text),
        condition: sensitive_gateway_field(decision.condition),
        descriptor: PaymentDescriptor {
            payment_type: sensitive_gateway_field(payment_type),
            card_brand: sensitive_gateway_field(card_brand),
            card_last4: card_number.and_then(last4).map(SensitiveText::new),
            // Financial normalization is a durable replay contract. Enrich
            // missing display through the separate metadata query instead.
            card_exp_month: None,
            card_exp_year: None,
        },
        diagnostics,
    }
    .normalize_diagnostics())
}

pub(in crate::client) fn classic_form_has_payment_processing_evidence(
    text: &str,
    http_status: u16,
) -> bool {
    if !is_classic_form_candidate(text) {
        return false;
    }
    let fields: Vec<_> = form_urlencoded::parse(text.as_bytes()).collect();
    let decision = PaymentDecisionFields::new(
        collect_classic_scalar(&fields, &["response"], false)
            .finish_decision(DecisionFieldKind::Response),
        collect_classic_scalar(&fields, &["response_code", "responsecode"], false)
            .finish_decision(DecisionFieldKind::ResponseCode),
        collect_classic_scalar(&fields, &["status"], false)
            .finish_decision(DecisionFieldKind::GatewayState),
        collect_classic_scalar(&fields, &["condition"], false)
            .finish_decision(DecisionFieldKind::GatewayState),
    )
    .resolve();
    if decision.has_payment_processing_evidence(http_status) {
        return true;
    }
    let (transaction, _) = collect_classic_transaction_identifier(&fields);
    if !matches!(transaction, ResolvedScalar::Missing) {
        return true;
    }
    [
        &["customer_vault_id", "customer_vaultid"][..],
        &["authcode", "auth_code", "authorization_code"][..],
        &["avsresponse", "avs_response"][..],
        &["cvvresponse", "cvv_response"][..],
        &["type"][..],
        &["cctype", "card_type", "cc_type"][..],
        &["cc_number", "ccnumber"][..],
        &["cc_exp"][..],
    ]
    .into_iter()
    .any(|aliases| {
        !matches!(
            collect_classic_optional_identifier(&fields, aliases),
            ResolvedScalar::Missing
        )
    })
}

fn is_classic_form_candidate(text: &str) -> bool {
    !text.is_empty()
        && text.split('&').all(|pair| {
            let Some((name, _)) = pair.split_once('=') else {
                return false;
            };
            !name.is_empty()
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        })
}

fn collect_classic_scalar(
    fields: &[(Cow<'_, str>, Cow<'_, str>)],
    aliases: &[&'static str],
    bounded: bool,
) -> ScalarOccurrenceCollector {
    let mut collector = ScalarOccurrenceCollector::default();
    for (field, value) in fields {
        if aliases.iter().any(|alias| field == *alias) {
            let occurrence = ScalarOccurrence::Scalar(Cow::Borrowed(value.as_ref()));
            if bounded {
                collector.record_bounded(occurrence);
            } else {
                collector.record(occurrence);
            }
        }
    }
    collector
}

fn classic_is_known_preprocessing_rate_limit(
    fields: &[(Cow<'_, str>, Cow<'_, str>)],
    decision: &super::common::ResolvedPaymentDecision,
    evidence: [&ResolvedScalar; 5],
) -> bool {
    decision.is_known_preprocessing_rate_limit()
        && evidence
            .iter()
            .all(|channel| matches!(channel, ResolvedScalar::Missing))
        && fields.iter().all(|(name, value)| match name.as_ref() {
            "response" | "response_code" => true,
            "responsetext" => value == "Rate limit exceeded",
            "authcode" | "transactionid" | "avsresponse" | "cvvresponse" | "orderid" | "type" => {
                value.is_empty()
            }
            _ => false,
        })
}

fn collect_classic_optional_identifier(
    fields: &[(Cow<'_, str>, Cow<'_, str>)],
    aliases: &[&'static str],
) -> ResolvedScalar {
    let mut collector = ScalarOccurrenceCollector::default();
    for (field, value) in fields {
        if aliases.iter().any(|alias| field == *alias) {
            collector.record_identifier(
                ScalarOccurrence::Scalar(Cow::Borrowed(value.as_ref())),
                IdentifierPresence::Optional,
            );
        }
    }
    collector.finish()
}

fn collect_classic_transaction_identifier(
    fields: &[(Cow<'_, str>, Cow<'_, str>)],
) -> (ResolvedScalar, ResolvedScalar) {
    let mut evidence = ScalarOccurrenceCollector::default();
    let mut outcome = ScalarOccurrenceCollector::default();
    for (field, value) in fields {
        if matches!(field.as_ref(), "transactionid" | "transaction_id") {
            let value = value.as_ref();
            evidence.record_identifier(
                ScalarOccurrence::Scalar(Cow::Borrowed(value)),
                IdentifierPresence::Optional,
            );
            outcome.record_identifier(
                ScalarOccurrence::Scalar(Cow::Borrowed(value)),
                IdentifierPresence::ForegroundRequired,
            );
        }
    }
    (evidence.finish(), outcome.finish())
}

fn gateway_text_reports_unknown(value: &str) -> bool {
    value.contains("unknown")
        || value.contains("pending")
        || value.contains("in progress")
        || value.contains("processing")
        || value.contains("under review")
}
