use chrono::{DateTime, NaiveDateTime, Utc};
use syrup_rail::{
    CumulativeRefundCents, GatewayDiagnostic, GatewayLifecycleEvidence, GatewayLifecycleQuarantine,
    GatewayLifecycleQuarantineReason, GatewayLifecycleState, GatewayOrderId, GatewayTransactionId,
    GatewayTransactionReport,
};

pub(crate) struct NmiReport {
    pub(crate) transaction_id: Option<GatewayTransactionId>,
    pub(crate) order_id: Option<GatewayOrderId>,
    pub(crate) condition: Option<GatewayDiagnostic>,
    pub(crate) actions: Vec<NmiAction>,
    pub(crate) malformed_structure: bool,
}

pub(crate) struct NmiAction {
    pub(crate) action_type: Option<GatewayDiagnostic>,
    pub(crate) date: Option<GatewayDiagnostic>,
    pub(crate) amount: Option<GatewayDiagnostic>,
    pub(crate) success: Option<GatewayDiagnostic>,
}

pub(crate) fn admit_report(report: NmiReport) -> GatewayTransactionReport {
    if report.malformed_structure {
        return GatewayTransactionReport::Quarantine(
            GatewayLifecycleQuarantine::new(
                report.transaction_id,
                report.order_id,
                GatewayLifecycleQuarantineReason::MalformedReportStructure,
            )
            .expect("malformed reports may be quarantined without a locator"),
        );
    }
    if report.transaction_id.is_none() && report.order_id.is_none() {
        return GatewayTransactionReport::Ignore;
    }

    let transaction_id = report.transaction_id.clone();
    let order_id = report.order_id.clone();
    match lifecycle_from_report(&report) {
        Ok(lifecycle) => GatewayTransactionReport::Evidence(
            GatewayLifecycleEvidence::new(
                transaction_id,
                order_id,
                lifecycle.state,
                report.condition,
                lifecycle.action,
                lifecycle.effective_at,
            )
            .expect("identified NMI report produces located lifecycle evidence"),
        ),
        Err(reason) => GatewayTransactionReport::Quarantine(
            GatewayLifecycleQuarantine::new(transaction_id, order_id, reason)
                .expect("semantic NMI quarantine retains a safe locator"),
        ),
    }
}

struct InterpretedLifecycle {
    state: GatewayLifecycleState,
    action: Option<GatewayDiagnostic>,
    effective_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Copy)]
struct RefundEvidence {
    refunded_amount_cents: i32,
    captured_amount_cents: i32,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ActionSuccess {
    Successful,
    Failed,
    Ambiguous,
}

fn lifecycle_from_report(
    report: &NmiReport,
) -> Result<InterpretedLifecycle, GatewayLifecycleQuarantineReason> {
    validate_return_or_void_action_success(report)?;
    let refund = validated_refund_evidence(report)?;
    if let Some(action) = latest_matching_action(report, &["return"]) {
        return Ok(lifecycle_from_action(
            GatewayLifecycleState::Chargeback {
                cumulative_refunded_cents: refund.and_then(|evidence| {
                    CumulativeRefundCents::new(evidence.refunded_amount_cents).ok()
                }),
            },
            action,
        ));
    }
    if let Some(action) = latest_matching_action(report, &["refund", "credit"]) {
        let evidence = refund.ok_or(GatewayLifecycleQuarantineReason::InvalidRefundEconomics)?;
        let refunded = CumulativeRefundCents::new(evidence.refunded_amount_cents)
            .map_err(|_| GatewayLifecycleQuarantineReason::InvalidRefundEconomics)?;
        let state = if evidence.refunded_amount_cents < evidence.captured_amount_cents {
            GatewayLifecycleState::Settled {
                cumulative_refunded_cents: Some(refunded),
            }
        } else {
            GatewayLifecycleState::Refunded {
                cumulative_refunded_cents: refunded,
            }
        };
        return Ok(lifecycle_from_action(state, action));
    }
    if let Some(action) = latest_matching_action(report, &["void"]) {
        return Ok(lifecycle_from_action(GatewayLifecycleState::Voided, action));
    }

    let latest_successful = latest_successful_action(report);
    let condition = report
        .condition
        .as_ref()
        .map(|condition| normalize_gateway_state(condition.expose()));
    Ok(match condition.as_deref() {
        Some("chargeback") => InterpretedLifecycle {
            state: GatewayLifecycleState::Chargeback {
                cumulative_refunded_cents: refund.and_then(|evidence| {
                    CumulativeRefundCents::new(evidence.refunded_amount_cents).ok()
                }),
            },
            action: latest_successful.and_then(|action| action.action_type.clone()),
            effective_at: latest_successful.and_then(action_date),
        },
        Some("refunded") => {
            return Err(GatewayLifecycleQuarantineReason::InvalidRefundEconomics);
        }
        Some("canceled" | "voided") => InterpretedLifecycle {
            state: GatewayLifecycleState::Voided,
            action: latest_successful.and_then(|action| action.action_type.clone()),
            effective_at: latest_successful.and_then(action_date),
        },
        Some("complete" | "completed") => {
            let action = latest_matching_action(report, &["settle"]).or(latest_successful);
            InterpretedLifecycle {
                state: GatewayLifecycleState::Settled {
                    cumulative_refunded_cents: None,
                },
                action: action.and_then(|action| action.action_type.clone()),
                effective_at: action.and_then(action_date),
            }
        }
        Some("pendingsettlement") => InterpretedLifecycle {
            state: GatewayLifecycleState::PendingSettlement,
            action: latest_successful.and_then(|action| action.action_type.clone()),
            effective_at: latest_successful.and_then(action_date),
        },
        None => {
            if let Some(action) = latest_matching_action(report, &["settle"]) {
                lifecycle_from_action(
                    GatewayLifecycleState::Settled {
                        cumulative_refunded_cents: None,
                    },
                    action,
                )
            } else {
                InterpretedLifecycle {
                    state: GatewayLifecycleState::Unknown,
                    action: latest_successful.and_then(|action| action.action_type.clone()),
                    effective_at: latest_successful.and_then(action_date),
                }
            }
        }
        Some(_) => InterpretedLifecycle {
            state: GatewayLifecycleState::Unknown,
            action: latest_successful.and_then(|action| action.action_type.clone()),
            effective_at: latest_successful.and_then(action_date),
        },
    })
}

fn validate_return_or_void_action_success(
    report: &NmiReport,
) -> Result<(), GatewayLifecycleQuarantineReason> {
    let has_ambiguous_reversal = report.actions.iter().any(|action| {
        action
            .action_type
            .as_ref()
            .map(|value| normalize_gateway_state(value.expose()))
            .is_some_and(|action_type| matches!(action_type.as_str(), "return" | "void"))
            && action_success(action) == ActionSuccess::Ambiguous
    });
    if has_ambiguous_reversal {
        return Err(GatewayLifecycleQuarantineReason::AmbiguousReversalSuccess);
    }
    Ok(())
}

fn lifecycle_from_action(state: GatewayLifecycleState, action: &NmiAction) -> InterpretedLifecycle {
    InterpretedLifecycle {
        state,
        action: action.action_type.clone(),
        effective_at: action_date(action),
    }
}

fn validated_refund_evidence(
    report: &NmiReport,
) -> Result<Option<RefundEvidence>, GatewayLifecycleQuarantineReason> {
    let mut total = 0_i64;
    let mut has_successful_refund = false;
    for action in &report.actions {
        let is_refund = action
            .action_type
            .as_ref()
            .map(|value| normalize_gateway_state(value.expose()))
            .is_some_and(|action_type| matches!(action_type.as_str(), "refund" | "credit"));
        if !is_refund {
            continue;
        }
        match action_success(action) {
            ActionSuccess::Successful => {}
            ActionSuccess::Failed => continue,
            ActionSuccess::Ambiguous => {
                return Err(GatewayLifecycleQuarantineReason::AmbiguousReversalSuccess);
            }
        }
        has_successful_refund = true;
        let amount = action_amount_cents(action)
            .and_then(i64::checked_abs)
            .filter(|amount| *amount > 0)
            .ok_or(GatewayLifecycleQuarantineReason::InvalidRefundEconomics)?;
        total = total
            .checked_add(amount)
            .ok_or(GatewayLifecycleQuarantineReason::InvalidRefundEconomics)?;
    }
    if !has_successful_refund {
        return Ok(None);
    }
    let refunded_amount_cents = i32::try_from(total)
        .map_err(|_| GatewayLifecycleQuarantineReason::InvalidRefundEconomics)?;
    let captured_amount_cents = latest_matching_action(report, &["settle", "capture", "sale"])
        .and_then(action_amount_cents)
        .filter(|amount| *amount > 0)
        .and_then(|amount| i32::try_from(amount).ok())
        .ok_or(GatewayLifecycleQuarantineReason::InvalidRefundEconomics)?;
    if refunded_amount_cents > captured_amount_cents {
        return Err(GatewayLifecycleQuarantineReason::InvalidRefundEconomics);
    }
    Ok(Some(RefundEvidence {
        refunded_amount_cents,
        captured_amount_cents,
    }))
}

fn latest_matching_action<'a>(
    report: &'a NmiReport,
    action_types: &[&str],
) -> Option<&'a NmiAction> {
    report
        .actions
        .iter()
        .filter(|action| action_is_successful(action))
        .filter(|action| {
            action
                .action_type
                .as_ref()
                .map(|value| normalize_gateway_state(value.expose()))
                .is_some_and(|action_type| action_types.contains(&action_type.as_str()))
        })
        .max_by_key(|action| action_date(action))
}

fn latest_successful_action(report: &NmiReport) -> Option<&NmiAction> {
    report
        .actions
        .iter()
        .filter(|action| action_is_successful(action))
        .max_by_key(|action| action_date(action))
}

fn action_is_successful(action: &NmiAction) -> bool {
    match action_success(action) {
        ActionSuccess::Successful => true,
        ActionSuccess::Failed => false,
        ActionSuccess::Ambiguous => action.success.is_none(),
    }
}

fn action_success(action: &NmiAction) -> ActionSuccess {
    match action.success.as_ref().map(|value| value.expose().trim()) {
        Some("1") => ActionSuccess::Successful,
        Some(success) if success.eq_ignore_ascii_case("true") => ActionSuccess::Successful,
        Some("0") => ActionSuccess::Failed,
        Some(success) if success.eq_ignore_ascii_case("false") => ActionSuccess::Failed,
        _ => ActionSuccess::Ambiguous,
    }
}

fn action_amount_cents(action: &NmiAction) -> Option<i64> {
    let amount = action.amount.as_ref()?.expose().trim();
    let (negative, amount) = amount
        .strip_prefix('-')
        .map_or((false, amount), |amount| (true, amount.trim_start()));
    let amount = amount.trim_start_matches('$');
    let mut parts = amount.split('.');
    let whole = parts.next()?;
    if whole.is_empty() || !whole.chars().all(|character| character.is_ascii_digit()) {
        return None;
    }
    let fraction = parts.next().unwrap_or("");
    if parts.next().is_some()
        || fraction.len() > 2
        || !fraction.chars().all(|character| character.is_ascii_digit())
    {
        return None;
    }
    let whole_cents = whole.parse::<i64>().ok()?.checked_mul(100)?;
    let fraction_cents = match fraction.len() {
        0 => 0,
        1 => fraction.parse::<i64>().ok()?.checked_mul(10)?,
        2 => fraction.parse::<i64>().ok()?,
        _ => return None,
    };
    let amount_cents = whole_cents.checked_add(fraction_cents)?;
    if negative {
        amount_cents.checked_neg()
    } else {
        Some(amount_cents)
    }
}

fn normalize_gateway_state(value: &str) -> String {
    value
        .trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|character| {
            !character.is_ascii_whitespace() && *character != '_' && *character != '-'
        })
        .collect()
}

fn action_date(action: &NmiAction) -> Option<DateTime<Utc>> {
    let date = action.date.as_ref()?.expose();
    match NaiveDateTime::parse_from_str(date.trim(), "%Y%m%d%H%M%S") {
        Ok(date) => Some(DateTime::<Utc>::from_naive_utc_and_offset(date, Utc)),
        Err(error) => {
            tracing::debug!(
                error = %error,
                action_type = action.action_type.as_ref().map(GatewayDiagnostic::expose),
                "ignored invalid NMI lifecycle action date during best-effort reconciliation"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diagnostic(value: &str) -> Option<GatewayDiagnostic> {
        Some(GatewayDiagnostic::new(value))
    }

    fn located_report(actions: Vec<NmiAction>) -> NmiReport {
        NmiReport {
            transaction_id: Some(GatewayTransactionId::new("txn_1").unwrap()),
            order_id: None,
            condition: None,
            actions,
            malformed_structure: false,
        }
    }

    fn action(kind: &str, amount: &str, success: Option<&str>) -> NmiAction {
        NmiAction {
            action_type: diagnostic(kind),
            date: diagnostic("20260806120000"),
            amount: diagnostic(amount),
            success: success.map(GatewayDiagnostic::new),
        }
    }

    #[test]
    fn no_locator_ignores_but_malformed_without_locator_quarantines() {
        let unidentified = NmiReport {
            transaction_id: None,
            order_id: None,
            condition: diagnostic("complete"),
            actions: Vec::new(),
            malformed_structure: false,
        };
        assert_eq!(admit_report(unidentified), GatewayTransactionReport::Ignore);

        let malformed = NmiReport {
            transaction_id: None,
            order_id: None,
            condition: None,
            actions: Vec::new(),
            malformed_structure: true,
        };
        let GatewayTransactionReport::Quarantine(quarantine) = admit_report(malformed) else {
            panic!("malformed report should quarantine");
        };
        assert_eq!(
            quarantine.reason(),
            GatewayLifecycleQuarantineReason::MalformedReportStructure
        );
    }

    #[test]
    fn partial_and_full_refunds_have_distinct_valid_states() {
        let partial = located_report(vec![
            action("sale", "10.00", Some("1")),
            action("refund", "-3.00", Some("1")),
        ]);
        let GatewayTransactionReport::Evidence(partial) = admit_report(partial) else {
            panic!("partial refund should be evidence");
        };
        assert_eq!(
            partial.state(),
            &GatewayLifecycleState::Settled {
                cumulative_refunded_cents: Some(CumulativeRefundCents::new(300).unwrap())
            }
        );

        let full = located_report(vec![
            action("sale", "10.00", Some("1")),
            action("refund", "10.00", Some("true")),
        ]);
        let GatewayTransactionReport::Evidence(full) = admit_report(full) else {
            panic!("full refund should be evidence");
        };
        assert_eq!(
            full.state(),
            &GatewayLifecycleState::Refunded {
                cumulative_refunded_cents: CumulativeRefundCents::new(1_000).unwrap()
            }
        );
    }

    #[test]
    fn ambiguous_reversal_and_invalid_refund_economics_quarantine() {
        let ambiguous = located_report(vec![action("void", "10.00", None)]);
        let GatewayTransactionReport::Quarantine(ambiguous) = admit_report(ambiguous) else {
            panic!("ambiguous void should quarantine");
        };
        assert_eq!(
            ambiguous.reason(),
            GatewayLifecycleQuarantineReason::AmbiguousReversalSuccess
        );

        let excessive = located_report(vec![
            action("sale", "10.00", Some("1")),
            action("refund", "11.00", Some("1")),
        ]);
        let GatewayTransactionReport::Quarantine(excessive) = admit_report(excessive) else {
            panic!("excessive refund should quarantine");
        };
        assert_eq!(
            excessive.reason(),
            GatewayLifecycleQuarantineReason::InvalidRefundEconomics
        );
    }
}
