//! An exact query of a failed transaction carries the decline reason on its
//! original sale action. The outcome keeps that reason as evidence without
//! letting any action decide the payment status.

use super::*;

fn failed_transaction(actions: &str) -> String {
    format!(
        r#"
        <nm_response>
          <transaction>
            <transaction_id>txn_failed_sale</transaction_id>
            <order_id>order_failed_sale</order_id>
            <condition>failed</condition>
            {actions}
          </transaction>
        </nm_response>
        "#
    )
}

fn sale_action(fields: &str) -> String {
    format!("<action><action_type>sale</action_type><success>0</success>{fields}</action>")
}

fn queried(xml: &str) -> PaymentOutcome {
    query_outcome_from_xml(xml)
        .expect("exact query response should parse")
        .expect("transaction should be present")
}

fn exposed(value: &Option<SensitiveText>) -> Option<&str> {
    value.as_ref().map(SensitiveText::expose)
}

#[test]
fn failed_transaction_keeps_its_original_sale_decline_reason() {
    for (code, text) in [
        ("261", "Customer requested stop of all recurring payments"),
        ("264", "Declined-Retry in a few days"),
        ("202", "Insufficient funds"),
        ("300", "AVS REJECTED"),
    ] {
        let outcome = queried(&failed_transaction(&sale_action(&format!(
            "<response_text>{text}</response_text><response_code>{code}</response_code>\
             <processor_response_code>R3</processor_response_code>"
        ))));

        assert_eq!(outcome.status, PaymentStatus::Failed, "{code}");
        assert_eq!(exposed(&outcome.response_code), Some(code));
        assert_eq!(exposed(&outcome.response_text), Some(text));
        assert_eq!(outcome.response, None, "{code}");
        assert_eq!(exposed(&outcome.condition), Some("failed"));
        assert!(outcome.diagnostics.is_empty(), "{code}");
    }
}

#[test]
fn failed_sale_reason_does_not_need_response_text() {
    let outcome = queried(&failed_transaction(&sale_action(
        "<response_code> 261 </response_code>",
    )));

    assert_eq!(outcome.status, PaymentStatus::Failed);
    assert_eq!(exposed(&outcome.response_code).map(str::trim), Some("261"));
    assert_eq!(outcome.response_text, None);
}

#[test]
fn ambiguous_or_non_decline_actions_leave_the_failed_transaction_unchanged() {
    let declined = "<response_text>DECLINE</response_text><response_code>261</response_code>";
    let cases = [
        ("no action", String::new()),
        (
            "two actions",
            format!("{}{}", sale_action(declined), sale_action(declined)),
        ),
        (
            "not a sale",
            format!(
                "<action><action_type>auth</action_type><success>0</success>{declined}</action>"
            ),
        ),
        (
            "conflicting action type",
            format!(
                "<action><action_type>sale</action_type><action_type>refund</action_type>\
                 <success>0</success>{declined}</action>"
            ),
        ),
        (
            "successful action",
            format!(
                "<action><action_type>sale</action_type><success>1</success>{declined}</action>"
            ),
        ),
        (
            "missing success",
            format!("<action><action_type>sale</action_type>{declined}</action>"),
        ),
        (
            "approval code",
            sale_action("<response_code>100</response_code>"),
        ),
        (
            "indeterminate processor error",
            sale_action("<response_code>420</response_code>"),
        ),
        (
            "processor duplicate",
            sale_action("<response_code>430</response_code>"),
        ),
        (
            "unpublished code",
            sale_action("<response_code>999</response_code>"),
        ),
        (
            "non-numeric code",
            sale_action("<response_code>26l</response_code>"),
        ),
        (
            "conflicting codes",
            sale_action("<response_code>261</response_code><response_code>202</response_code>"),
        ),
        (
            "nested code",
            sale_action("<response_code><value>261</value></response_code>"),
        ),
        (
            "conflicting text",
            sale_action(
                "<response_code>261</response_code><response_text>A</response_text>\
                 <response_text>B</response_text>",
            ),
        ),
        (
            "missing code",
            sale_action("<response_text>DECLINE</response_text>"),
        ),
        (
            "action nested in the sale",
            sale_action(&format!(
                "<response_code>261</response_code>{}",
                sale_action(declined)
            )),
        ),
    ];
    for (name, actions) in cases {
        let outcome = queried(&failed_transaction(&actions));

        assert_eq!(outcome.status, PaymentStatus::Failed, "{name}");
        assert_eq!(outcome.response_code, None, "{name}");
        assert_eq!(outcome.response_text, None, "{name}");
        assert!(outcome.diagnostics.is_empty(), "{name}");
    }
}

#[test]
fn transaction_level_response_evidence_is_never_replaced_by_an_action() {
    let action =
        sale_action("<response_text>DECLINE</response_text><response_code>261</response_code>");
    for (name, transaction_fields, expected_code, expected_text) in [
        (
            "transaction response code",
            "<response_code>300</response_code>",
            Some("300"),
            None,
        ),
        (
            "transaction response text",
            "<response_text>Summary text</response_text>",
            None,
            Some("Summary text"),
        ),
    ] {
        let outcome = queried(&format!(
            r#"
            <nm_response>
              <transaction>
                <transaction_id>txn_failed_sale</transaction_id>
                <condition>failed</condition>
                {transaction_fields}
                {action}
              </transaction>
            </nm_response>
            "#
        ));

        assert_eq!(outcome.status, PaymentStatus::Failed, "{name}");
        assert_eq!(exposed(&outcome.response_code), expected_code, "{name}");
        assert_eq!(exposed(&outcome.response_text), expected_text, "{name}");
    }
}

#[test]
fn an_action_never_changes_a_status_the_transaction_did_not_settle_as_failed() {
    let action =
        sale_action("<response_text>DECLINE</response_text><response_code>261</response_code>");
    for (condition, expected) in [
        ("complete", PaymentStatus::Approved),
        ("pendingsettlement", PaymentStatus::Approved),
        ("pending", PaymentStatus::Unknown),
        ("error", PaymentStatus::Unknown),
        ("canceled", PaymentStatus::Unknown),
    ] {
        let outcome = queried(&format!(
            r#"
            <nm_response>
              <transaction>
                <transaction_id>txn_other_condition</transaction_id>
                <condition>{condition}</condition>
                {action}
              </transaction>
            </nm_response>
            "#
        ));

        assert_eq!(outcome.status, expected, "{condition}");
        assert_eq!(outcome.response_code, None, "{condition}");
        assert_eq!(outcome.response_text, None, "{condition}");
    }
}
