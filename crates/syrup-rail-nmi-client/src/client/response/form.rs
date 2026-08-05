use std::borrow::Cow;

use url::form_urlencoded;

use crate::{PaymentDescriptor, PaymentOutcome, PaymentStatus, SensitiveText};

use super::super::{
    WireError,
    text::{last4, sensitive_gateway_field},
};
use super::common::{
    DecisionFieldKind, IdentifierPresence, PaymentDecisionFields, ResolvedScalar, ScalarOccurrence,
    ScalarOccurrenceCollector, finalize_foreground_identifiers, rate_limited_wire_error,
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
    );
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
    let (response_text, invalid_response_text) = resolve_optional_scalar(
        collect_classic_scalar(&fields, &["responsetext", "response_text"], true).finish(),
    );
    let (mut status, decision_diagnostic) = decision.payment_status();
    let mut diagnostics: Vec<_> = decision_diagnostic.into_iter().collect();
    if !decision.has_structured_evidence() {
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
    let (transaction_id, customer_vault_id) = finalize_foreground_identifiers(
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
        transaction_id,
        customer_vault_id,
        response: sensitive_gateway_field(decision.response.raw),
        response_code: sensitive_gateway_field(decision.response_code.raw),
        response_text: sensitive_gateway_field(response_text),
        condition: sensitive_gateway_field(decision.condition.raw),
        descriptor: PaymentDescriptor {
            payment_type: sensitive_gateway_field(payment_type),
            card_brand: sensitive_gateway_field(card_brand),
            card_last4: card_number.and_then(last4).map(SensitiveText::new),
            card_exp_month: None,
            card_exp_year: None,
        },
        diagnostics,
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
    decision: &PaymentDecisionFields,
    evidence: [&ResolvedScalar; 5],
) -> bool {
    decision.is_rate_limited()
        && evidence
            .iter()
            .all(|channel| matches!(channel, ResolvedScalar::Missing))
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
                IdentifierPresence::Required,
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
