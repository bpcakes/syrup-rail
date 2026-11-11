use super::*;
use crate::{ReceiptConflictField, TransactionReceiptLookup};

fn lookup(transaction: &str) -> Result<TransactionReceiptLookup, WireError> {
    super::super::super::response::xml::receipt::parse_evidence(
        &format!("<nm_response>{transaction}</nm_response>"),
        &TransactionQuery {
            transaction_id: Some("txn_receipt".to_owned()),
            order_id: Some("order_receipt".to_owned()),
        },
    )
}

fn duplicate(field: &str, original: &str, values: &[&str]) -> String {
    transaction().replace(
        &format!("<{field}>{original}</{field}>"),
        &values
            .iter()
            .map(|value| format!("<{field}>{value}</{field}>"))
            .collect::<String>(),
    )
}

fn assert_conflict(body: &str, expected: &[ReceiptConflictField]) {
    let outcome = lookup(body).unwrap();
    let debug = format!("{outcome:?}");
    let TransactionReceiptLookup::BoundConflict { fields } = outcome else {
        panic!("expected an attributed field conflict");
    };
    assert_eq!(fields, expected);
    for secret in [
        "txn_receipt",
        "order_receipt",
        "USD",
        "EUR",
        "1234",
        "5678",
        "20260919100200",
    ] {
        assert!(!debug.contains(secret));
    }
}

#[test]
fn valid_contradictions_are_typed_only_within_the_same_field_and_action() {
    for (field, first, second, kind) in [
        ("currency", "USD", "EUR", ReceiptConflictField::Currency),
        (
            "cc_number",
            "411111******1234",
            "****5678",
            ReceiptConflictField::CardLast4,
        ),
        (
            "action_type",
            "sale",
            "refund",
            ReceiptConflictField::ActionType,
        ),
        ("amount", "1.00", "2.00", ReceiptConflictField::ActionAmount),
        ("success", "1", "false", ReceiptConflictField::ActionSuccess),
        (
            "response_code",
            "100",
            "200",
            ReceiptConflictField::ActionResponseCode,
        ),
        (
            "date",
            "20260919100200",
            "20260919100300",
            ReceiptConflictField::ActionDate,
        ),
    ] {
        for values in [[first, second], [second, first]] {
            assert_conflict(&duplicate(field, first, &values), &[kind]);
        }
    }
    let history = transaction().replace("complete", "refunded").replace("</transaction>",
        "<action><action_type>refund</action_type><amount>2.00</amount><success>false</success><response_code>200</response_code><date>20261001120000</date></action></transaction>");
    let TransactionReceiptLookup::Found(receipt) = lookup(&history).unwrap() else {
        panic!("independent historical actions must remain usable");
    };
    let actions = receipt.into_parts().actions;
    assert_eq!(actions.len(), 2);
    assert_eq!(actions[0].action_type.as_ref().unwrap().expose(), "sale");
    assert_eq!(actions[1].action_type.as_ref().unwrap().expose(), "refund");
}

#[test]
fn conflicts_survive_unrelated_missing_malformed_and_later_fields() {
    let conflicting = duplicate("response_code", "100", &["100", "200"]);
    for body in [
        conflicting.replace("<currency>USD</currency>", ""),
        conflicting.replace("USD", "US"),
        conflicting.replace("411111******1234", "not-a-mask"),
        conflicting.replace("<amount>1.00</amount>", "<amount><nested/></amount>"),
        conflicting.replace("<action>", "<action><date>invalid</date></action><action>"),
        conflicting.replace(
            "</condition>",
            "</condition><condition>refunded</condition>",
        ),
        conflicting.replace("USD", &"x".repeat(MAX_NMI_FIELD_CHARS + 1)),
    ] {
        assert_conflict(&body, &[ReceiptConflictField::ActionResponseCode]);
    }
    let body = duplicate("cc_number", "411111******1234", &["****1234", "****5678"])
        .replace("<currency>USD</currency>", "")
        .replace(
            "</response_code>",
            "</response_code><response_code>200</response_code>",
        );
    assert_conflict(
        &body,
        &[
            ReceiptConflictField::CardLast4,
            ReceiptConflictField::ActionResponseCode,
        ],
    );
}

#[test]
fn invalid_occurrences_never_erase_two_valid_conflicting_occurrences() {
    for invalid in ["", "bad", "<nested>300</nested>"] {
        for values in [
            [invalid, "100", "200"],
            ["100", invalid, "200"],
            ["100", "200", invalid],
        ] {
            assert_conflict(
                &duplicate("response_code", "100", &values),
                &[ReceiptConflictField::ActionResponseCode],
            );
        }
    }
}

#[test]
fn normalized_echoes_are_usable_while_legacy_lookup_remains_strict() {
    for (field, original, equivalent) in [
        ("currency", "USD", "usd"),
        ("cc_number", "411111******1234", "****1234"),
        ("action_type", "sale", "SALE"),
        ("amount", "1.00", "1.0"),
        ("amount", "1.00", "$01.00"),
        ("success", "1", "true"),
    ] {
        for values in [[original, equivalent], [equivalent, original]] {
            let body = duplicate(field, original, &values);
            let TransactionReceiptLookup::Found(receipt) = lookup(&body).unwrap() else {
                panic!("equivalent {field} echoes must resolve");
            };
            let debug = format!("{receipt:?}");
            let parts = receipt.into_parts();
            assert_eq!(parts.currency.expose(), "USD");
            assert_eq!(parts.card_last4.unwrap().expose(), "1234");
            let action = parts.actions.into_iter().next().unwrap().into_parts();
            assert_eq!(action.action_type.unwrap().expose(), "sale");
            assert_eq!(action.amount.unwrap().expose(), "1.00");
            assert_eq!(action.success.unwrap().expose(), "1");
            assert!(!debug.contains("1234"));
            assert!(parse(&format!("<nm_response>{body}</nm_response>")).is_err());
        }
    }
}

#[test]
fn invalid_or_empty_occurrences_suppress_their_field_without_becoming_conflicts() {
    let oversized = "x".repeat(MAX_NMI_FIELD_CHARS + 1);
    for (field, original, invalid) in [
        ("currency", "USD", "US"),
        ("cc_number", "411111******1234", "4111111111115678"),
        ("cc_number", "411111******1234", "garbage 5678"),
        ("action_type", "sale", "bogus"),
        ("amount", "1.00", "no-amount"),
        ("amount", "1.00", "1.001"),
        ("amount", "1.00", "92233720368547758.08"),
        ("success", "1", "yes"),
        ("response_code", "100", "unknown"),
        ("date", "20260919100200", "20260230100200"),
        ("date", "20260919100200", "20260919250200"),
    ] {
        for values in [
            vec![original, invalid],
            vec![invalid, original],
            vec![invalid, "invalid"],
            vec!["", original],
            vec![original, "<nested/>"],
            vec![original, &oversized],
            vec![&oversized, original],
        ] {
            let TransactionReceiptLookup::Incomplete {
                currency,
                card_last4,
                actions,
            } = lookup(&duplicate(field, original, &values)).unwrap()
            else {
                panic!("invalid {field} occurrences must yield incomplete evidence");
            };
            let action = actions.into_iter().next().unwrap().into_parts();
            let suppressed = match field {
                "currency" => currency,
                "cc_number" => card_last4,
                "action_type" => action.action_type,
                "amount" => action.amount,
                "success" => action.success,
                "response_code" => action.response_code,
                "date" => action.date,
                _ => unreachable!(),
            };
            assert!(
                suppressed.is_none(),
                "invalid {field} must not select a valid-looking candidate"
            );
        }
    }
    // Condition is current lifecycle metadata, not an original-payment conflict.
    assert!(matches!(
        lookup(&duplicate(
            "condition",
            "complete",
            &["complete", "refunded"]
        )),
        Ok(TransactionReceiptLookup::Incomplete { .. })
    ));
}

#[test]
fn bound_incomplete_evidence_preserves_independently_valid_comparison_facts() {
    let base = transaction()
        .replace("1.00", "2.00")
        .replace("1234", "5678");
    for (body, has_currency, has_card, has_date) in [
        (
            base.replace("20260919100200", "not-a-date"),
            true,
            true,
            false,
        ),
        (
            base.replace("411111******5678", "not-a-mask"),
            true,
            false,
            true,
        ),
        (
            base.replace("<currency>USD</currency>", ""),
            false,
            true,
            true,
        ),
        (
            base.replace("USD", "<nested>USD</nested>"),
            false,
            true,
            true,
        ),
        (
            base.replace(
                "</condition>",
                "</condition><condition>refunded</condition>",
            ),
            true,
            true,
            true,
        ),
    ] {
        let outcome = lookup(&body).unwrap();
        let debug = format!("{outcome:?}");
        let TransactionReceiptLookup::Incomplete {
            currency,
            card_last4,
            actions,
        } = outcome
        else {
            panic!("malformed bound evidence cannot complete a receipt");
        };
        assert_eq!(
            currency.as_ref().map(SensitiveText::expose),
            has_currency.then_some("USD")
        );
        assert_eq!(
            card_last4.as_ref().map(SensitiveText::expose),
            has_card.then_some("5678")
        );
        let action = actions.into_iter().next().unwrap().into_parts();
        assert_eq!(action.action_type.unwrap().expose(), "sale");
        assert_eq!(action.amount.unwrap().expose(), "2.00");
        assert_eq!(action.success.unwrap().expose(), "1");
        assert_eq!(action.response_code.unwrap().expose(), "100");
        assert_eq!(
            action.date.as_ref().map(SensitiveText::expose),
            has_date.then_some("20260919100200")
        );
        for secret in [
            "txn_receipt",
            "order_receipt",
            "USD",
            "5678",
            "2.00",
            "20260919100200",
            "not-a-date",
            "not-a-mask",
        ] {
            assert!(!debug.contains(secret));
        }
        assert!(matches!(
            lookup(&body.replace("txn_receipt", "wrong")),
            Err(WireError::MalformedResponse(_))
        ));
        assert!(matches!(
            lookup(&body.replace("order_receipt", "wrong")),
            Err(WireError::MalformedResponse(_))
        ));
        let legacy = parse(&format!("<nm_response>{body}</nm_response>"));
        if !has_date {
            // Legacy scalar parsing leaves date validation to the caller.
            let action = legacy
                .unwrap()
                .unwrap()
                .into_parts()
                .actions
                .remove(0)
                .into_parts();
            assert_eq!(action.date.unwrap().expose(), "not-a-date");
        } else {
            assert!(legacy.is_err());
        }
    }
}

#[test]
fn invalid_identities_cardinality_and_bounds_prevent_attribution() {
    let base = duplicate("currency", "USD", &["USD", "EUR"]);
    for name in ["transaction_id", "order_id"] {
        let original = if name == "transaction_id" {
            "txn_receipt"
        } else {
            "order_receipt"
        };
        let tag = format!("<{name}>{original}</{name}>");
        for replacement in [
            String::new(),
            format!("<{name}/>"),
            format!("<{name}>other</{name}>"),
            format!("{tag}<{name}>other</{name}>"),
            format!("{tag}<{name}/>"),
            format!("<{name}><nested>{original}</nested></{name}>"),
        ] {
            assert!(matches!(
                lookup(&base.replace(&tag, &replacement)),
                Err(WireError::MalformedResponse(_))
            ));
        }
    }
    for body in [
        format!("{base}{base}"),
        format!("<wrapper>{base}</wrapper>"),
        base.replace("<action>", "<wrapper><action>")
            .replace("</action>", "</action></wrapper>"),
        base.replace(
            "</transaction>",
            &format!("{}</transaction>", "<action/>".repeat(100)),
        ),
        base.replace(
            "</transaction>",
            &format!(
                "{}</transaction>",
                " ".repeat(MAX_NMI_STANDARD_RESPONSE_BYTES)
            ),
        ),
    ] {
        assert!(matches!(
            lookup(&body),
            Err(WireError::MalformedResponse(_))
        ));
    }
    assert!(matches!(lookup(""), Ok(TransactionReceiptLookup::NotFound)));
}

#[test]
fn valid_calendar_and_signed_amount_neighbors_remain_usable() {
    for valid in ["20240229120000", "20000229000000", "20261231235959"] {
        assert!(matches!(
            lookup(&transaction().replace("20260919100200", valid)),
            Ok(TransactionReceiptLookup::Found(_))
        ));
    }
    for invalid in [
        "19000229120000",
        "20230229120000",
        "20260019120000",
        "20260900120000",
        "00000919120000",
        "20260919120060",
    ] {
        assert!(matches!(
            lookup(&transaction().replace("20260919100200", invalid)),
            Ok(TransactionReceiptLookup::Incomplete { .. })
        ));
    }
    for [first, second] in [
        ["-0.00", "0"],
        ["-$1.0", "-1.00"],
        ["92233720368547758.07", "92233720368547758.07"],
        ["-92233720368547758.08", "-92233720368547758.08"],
    ] {
        assert!(matches!(
            lookup(&duplicate("amount", "1.00", &[first, second])),
            Ok(TransactionReceiptLookup::Found(_))
        ));
    }
}
