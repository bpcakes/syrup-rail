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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActionSuccess {
    Successful,
    Failed,
    Missing,
    Invalid,
}

impl ActionSuccess {
    fn classify(action: &NmiAction) -> Self {
        match action.success.as_ref().map(|value| value.expose().trim()) {
            Some("1") => Self::Successful,
            Some(success) if success.eq_ignore_ascii_case("true") => Self::Successful,
            Some("0") => Self::Failed,
            Some(success) if success.eq_ignore_ascii_case("false") => Self::Failed,
            None => Self::Missing,
            Some(_) => Self::Invalid,
        }
    }

    const fn counts_as_successful(self) -> bool {
        matches!(self, Self::Successful | Self::Missing)
    }

    const fn is_ambiguous_reversal(self) -> bool {
        matches!(self, Self::Missing | Self::Invalid)
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ActionKind {
    Return,
    Refund,
    Void,
    Settle,
    Capture,
    Sale,
    Other,
}

impl ActionKind {
    fn classify(action: &NmiAction) -> Self {
        match action
            .action_type
            .as_ref()
            .map(|value| normalize_gateway_state(value.expose()))
            .as_deref()
        {
            Some("return") => Self::Return,
            Some("refund" | "credit") => Self::Refund,
            Some("void") => Self::Void,
            Some("settle") => Self::Settle,
            Some("capture") => Self::Capture,
            Some("sale") => Self::Sale,
            Some(_) | None => Self::Other,
        }
    }
}

#[derive(Clone, Copy)]
struct ClassifiedAction<'a> {
    source: &'a NmiAction,
    effective_at: Option<DateTime<Utc>>,
    amount_cents: Option<i64>,
}

#[derive(Clone, Copy, Default)]
enum RefundAccumulation {
    #[default]
    None,
    Amount(i64),
    Error(GatewayLifecycleQuarantineReason),
}

impl RefundAccumulation {
    fn observe(&mut self, success: ActionSuccess, amount_cents: Option<i64>) {
        if matches!(self, Self::Error(_)) || success == ActionSuccess::Failed {
            return;
        }
        if success.is_ambiguous_reversal() {
            *self = Self::Error(GatewayLifecycleQuarantineReason::AmbiguousReversalSuccess);
            return;
        }

        let Some(amount_cents) = amount_cents
            .and_then(i64::checked_abs)
            .filter(|amount| *amount > 0)
        else {
            *self = Self::Error(GatewayLifecycleQuarantineReason::InvalidRefundEconomics);
            return;
        };
        *self = match *self {
            Self::None => Self::Amount(amount_cents),
            Self::Amount(total) => total.checked_add(amount_cents).map_or(
                Self::Error(GatewayLifecycleQuarantineReason::InvalidRefundEconomics),
                Self::Amount,
            ),
            Self::Error(reason) => Self::Error(reason),
        };
    }
}

#[derive(Clone, Copy)]
enum RefundProgress {
    None,
    Partial(CumulativeRefundCents),
    Full(CumulativeRefundCents),
}

impl RefundProgress {
    const fn cumulative_refunded_cents(self) -> Option<CumulativeRefundCents> {
        match self {
            Self::None => None,
            Self::Partial(refunded) | Self::Full(refunded) => Some(refunded),
        }
    }
}

#[derive(Default)]
struct ReportActionSummary<'a> {
    latest_successful: Option<ClassifiedAction<'a>>,
    latest_return: Option<ClassifiedAction<'a>>,
    latest_refund: Option<ClassifiedAction<'a>>,
    latest_void: Option<ClassifiedAction<'a>>,
    latest_settle: Option<ClassifiedAction<'a>>,
    latest_capture_basis: Option<ClassifiedAction<'a>>,
    refund_accumulation: RefundAccumulation,
    has_ambiguous_return_or_void: bool,
}

impl<'a> ReportActionSummary<'a> {
    fn classify(report: &'a NmiReport) -> Self {
        let mut summary = Self::default();
        for action in &report.actions {
            let kind = ActionKind::classify(action);
            let success = ActionSuccess::classify(action);
            let amount_cents = match kind {
                ActionKind::Refund
                | ActionKind::Settle
                | ActionKind::Capture
                | ActionKind::Sale => action_amount_cents(action),
                ActionKind::Return | ActionKind::Void | ActionKind::Other => None,
            };

            if matches!(kind, ActionKind::Return | ActionKind::Void)
                && success.is_ambiguous_reversal()
            {
                summary.has_ambiguous_return_or_void = true;
            }
            if kind == ActionKind::Refund {
                summary.refund_accumulation.observe(success, amount_cents);
            }
            if !success.counts_as_successful() {
                continue;
            }

            // Parse each usable action date once. This intentionally collapses duplicate
            // best-effort debug diagnostics from the former repeated report scans.
            let classified = ClassifiedAction {
                source: action,
                effective_at: action_date(action),
                amount_cents,
            };
            replace_with_later(&mut summary.latest_successful, classified);
            match kind {
                ActionKind::Return => replace_with_later(&mut summary.latest_return, classified),
                ActionKind::Refund => replace_with_later(&mut summary.latest_refund, classified),
                ActionKind::Void => replace_with_later(&mut summary.latest_void, classified),
                ActionKind::Settle => {
                    replace_with_later(&mut summary.latest_settle, classified);
                    replace_with_later(&mut summary.latest_capture_basis, classified);
                }
                ActionKind::Capture | ActionKind::Sale => {
                    replace_with_later(&mut summary.latest_capture_basis, classified);
                }
                ActionKind::Other => {}
            }
        }
        summary
    }

    fn validated_refund_progress(
        &self,
    ) -> Result<RefundProgress, GatewayLifecycleQuarantineReason> {
        if self.has_ambiguous_return_or_void {
            return Err(GatewayLifecycleQuarantineReason::AmbiguousReversalSuccess);
        }
        let refunded_amount_cents = match self.refund_accumulation {
            RefundAccumulation::None => return Ok(RefundProgress::None),
            RefundAccumulation::Amount(total) => i32::try_from(total)
                .map_err(|_| GatewayLifecycleQuarantineReason::InvalidRefundEconomics)?,
            RefundAccumulation::Error(reason) => return Err(reason),
        };
        let captured_amount_cents = self
            .latest_capture_basis
            .and_then(|action| action.amount_cents)
            .filter(|amount| *amount > 0)
            .and_then(|amount| i32::try_from(amount).ok())
            .ok_or(GatewayLifecycleQuarantineReason::InvalidRefundEconomics)?;
        if refunded_amount_cents > captured_amount_cents {
            return Err(GatewayLifecycleQuarantineReason::InvalidRefundEconomics);
        }
        let refunded = CumulativeRefundCents::new(refunded_amount_cents)
            .map_err(|_| GatewayLifecycleQuarantineReason::InvalidRefundEconomics)?;
        if refunded_amount_cents < captured_amount_cents {
            Ok(RefundProgress::Partial(refunded))
        } else {
            Ok(RefundProgress::Full(refunded))
        }
    }
}

fn replace_with_later<'a>(
    current: &mut Option<ClassifiedAction<'a>>,
    candidate: ClassifiedAction<'a>,
) {
    let should_replace = current
        .as_ref()
        .is_none_or(|current| candidate.effective_at.as_ref() >= current.effective_at.as_ref());
    if should_replace {
        *current = Some(candidate);
    }
}

fn lifecycle_from_report(
    report: &NmiReport,
) -> Result<InterpretedLifecycle, GatewayLifecycleQuarantineReason> {
    let actions = ReportActionSummary::classify(report);
    let refund = actions.validated_refund_progress()?;
    if let Some(action) = actions.latest_return {
        return Ok(lifecycle_from_action(
            GatewayLifecycleState::Chargeback {
                cumulative_refunded_cents: refund.cumulative_refunded_cents(),
            },
            action,
        ));
    }
    if let Some(action) = actions.latest_refund {
        let state = match refund {
            RefundProgress::Partial(refunded) => GatewayLifecycleState::Settled {
                cumulative_refunded_cents: Some(refunded),
            },
            RefundProgress::Full(refunded) => GatewayLifecycleState::Refunded {
                cumulative_refunded_cents: refunded,
            },
            RefundProgress::None => {
                return Err(GatewayLifecycleQuarantineReason::InvalidRefundEconomics);
            }
        };
        return Ok(lifecycle_from_action(state, action));
    }
    if let Some(action) = actions.latest_void {
        return Ok(lifecycle_from_action(GatewayLifecycleState::Voided, action));
    }

    let latest_successful = actions.latest_successful;
    let condition = report
        .condition
        .as_ref()
        .map(|condition| normalize_gateway_state(condition.expose()));
    Ok(match condition.as_deref() {
        Some("chargeback") => InterpretedLifecycle {
            state: GatewayLifecycleState::Chargeback {
                cumulative_refunded_cents: refund.cumulative_refunded_cents(),
            },
            action: latest_successful.and_then(action_diagnostic),
            effective_at: latest_successful.and_then(|action| action.effective_at),
        },
        Some("refunded") => {
            return Err(GatewayLifecycleQuarantineReason::InvalidRefundEconomics);
        }
        Some("canceled" | "voided") => InterpretedLifecycle {
            state: GatewayLifecycleState::Voided,
            action: latest_successful.and_then(action_diagnostic),
            effective_at: latest_successful.and_then(|action| action.effective_at),
        },
        Some("complete" | "completed") => {
            let action = actions.latest_settle.or(latest_successful);
            InterpretedLifecycle {
                state: GatewayLifecycleState::Settled {
                    cumulative_refunded_cents: None,
                },
                action: action.and_then(action_diagnostic),
                effective_at: action.and_then(|action| action.effective_at),
            }
        }
        Some("pendingsettlement") => InterpretedLifecycle {
            state: GatewayLifecycleState::PendingSettlement,
            action: latest_successful.and_then(action_diagnostic),
            effective_at: latest_successful.and_then(|action| action.effective_at),
        },
        None => {
            if let Some(action) = actions.latest_settle {
                lifecycle_from_action(
                    GatewayLifecycleState::Settled {
                        cumulative_refunded_cents: None,
                    },
                    action,
                )
            } else {
                InterpretedLifecycle {
                    state: GatewayLifecycleState::Unknown,
                    action: latest_successful.and_then(action_diagnostic),
                    effective_at: latest_successful.and_then(|action| action.effective_at),
                }
            }
        }
        Some(_) => InterpretedLifecycle {
            state: GatewayLifecycleState::Unknown,
            action: latest_successful.and_then(action_diagnostic),
            effective_at: latest_successful.and_then(|action| action.effective_at),
        },
    })
}

fn lifecycle_from_action(
    state: GatewayLifecycleState,
    action: ClassifiedAction<'_>,
) -> InterpretedLifecycle {
    InterpretedLifecycle {
        state,
        action: action.source.action_type.clone(),
        effective_at: action.effective_at,
    }
}

fn action_diagnostic(action: ClassifiedAction<'_>) -> Option<GatewayDiagnostic> {
    action.source.action_type.clone()
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
        action_at(kind, amount, success, Some("20260806120000"))
    }

    fn action_at(kind: &str, amount: &str, success: Option<&str>, date: Option<&str>) -> NmiAction {
        NmiAction {
            action_type: diagnostic(kind),
            date: date.map(GatewayDiagnostic::new),
            amount: diagnostic(amount),
            success: success.map(GatewayDiagnostic::new),
        }
    }

    fn with_condition(mut report: NmiReport, condition: &str) -> NmiReport {
        report.condition = diagnostic(condition);
        report
    }

    fn evidence(report: NmiReport) -> GatewayLifecycleEvidence {
        let GatewayTransactionReport::Evidence(evidence) = admit_report(report) else {
            panic!("report should produce evidence");
        };
        evidence
    }

    fn quarantine_reason(report: NmiReport) -> GatewayLifecycleQuarantineReason {
        let GatewayTransactionReport::Quarantine(quarantine) = admit_report(report) else {
            panic!("report should quarantine");
        };
        quarantine.reason()
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
            action("refund", "-1.00", Some("1")),
            action("credit", "2.00", Some("true")),
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

    #[test]
    fn success_policy_distinguishes_missing_invalid_and_failed_values() {
        let cases = [
            (Some("1"), ActionSuccess::Successful, true),
            (Some(" TRUE "), ActionSuccess::Successful, true),
            (Some("0"), ActionSuccess::Failed, false),
            (Some(" false "), ActionSuccess::Failed, false),
            (None, ActionSuccess::Missing, true),
            (Some("yes"), ActionSuccess::Invalid, false),
            (Some(""), ActionSuccess::Invalid, false),
        ];
        for (success, expected, counts_as_successful) in cases {
            let action = action("authorize", "10.00", success);
            let actual = ActionSuccess::classify(&action);
            assert_eq!(
                actual, expected,
                "unexpected classification for {success:?}"
            );
            assert_eq!(actual.counts_as_successful(), counts_as_successful);
        }

        let missing = evidence(with_condition(
            located_report(vec![action("authorize", "10.00", None)]),
            "pending settlement",
        ));
        assert_eq!(
            missing.action().map(GatewayDiagnostic::expose),
            Some("authorize")
        );

        for success in [Some("0"), Some("invalid")] {
            let ignored = evidence(with_condition(
                located_report(vec![action("authorize", "10.00", success)]),
                "pending settlement",
            ));
            assert_eq!(ignored.action(), None, "unexpected action for {success:?}");
        }
    }

    #[test]
    fn reversal_success_policy_and_state_precedence_are_explicit() {
        for kind in ["return", "refund", "credit", "void"] {
            for success in [None, Some("invalid")] {
                let report = located_report(vec![action(kind, "1.00", success)]);
                assert_eq!(
                    quarantine_reason(report),
                    GatewayLifecycleQuarantineReason::AmbiguousReversalSuccess,
                    "unexpected reason for {kind} with {success:?}"
                );
            }
            let failed = evidence(with_condition(
                located_report(vec![action(kind, "1.00", Some("0"))]),
                "pending settlement",
            ));
            assert_eq!(failed.state(), &GatewayLifecycleState::PendingSettlement);
        }

        let return_over_refund = evidence(with_condition(
            located_report(vec![
                action("sale", "10.00", Some("1")),
                action("void", "10.00", Some("1")),
                action("refund", "2.00", Some("1")),
                action("return", "10.00", Some("1")),
            ]),
            "completed",
        ));
        assert_eq!(
            return_over_refund.state(),
            &GatewayLifecycleState::Chargeback {
                cumulative_refunded_cents: Some(CumulativeRefundCents::new(200).unwrap())
            }
        );
        assert_eq!(
            return_over_refund.action().map(GatewayDiagnostic::expose),
            Some("return")
        );

        let refund_over_void = evidence(with_condition(
            located_report(vec![
                action("sale", "10.00", Some("1")),
                action("void", "10.00", Some("1")),
                action("refund", "2.00", Some("1")),
            ]),
            "chargeback",
        ));
        assert_eq!(
            refund_over_void.state(),
            &GatewayLifecycleState::Settled {
                cumulative_refunded_cents: Some(CumulativeRefundCents::new(200).unwrap())
            }
        );
        assert_eq!(
            refund_over_void.action().map(GatewayDiagnostic::expose),
            Some("refund")
        );

        let void_over_condition = evidence(with_condition(
            located_report(vec![action("void", "10.00", Some("1"))]),
            "chargeback",
        ));
        assert_eq!(void_over_condition.state(), &GatewayLifecycleState::Voided);
    }

    #[test]
    fn latest_action_dates_keep_last_on_equal_and_rank_absent_or_malformed_as_oldest() {
        let equal_dates = evidence(with_condition(
            located_report(vec![
                action("first", "10.00", Some("1")),
                action("second", "10.00", Some("1")),
            ]),
            "pending settlement",
        ));
        assert_eq!(
            equal_dates.action().map(GatewayDiagnostic::expose),
            Some("second")
        );

        let absent_then_malformed = evidence(with_condition(
            located_report(vec![
                action_at("absent", "10.00", Some("1"), None),
                action_at("malformed", "10.00", Some("1"), Some("not-a-date")),
            ]),
            "pending settlement",
        ));
        assert_eq!(
            absent_then_malformed
                .action()
                .map(GatewayDiagnostic::expose),
            Some("malformed")
        );
        assert_eq!(absent_then_malformed.effective_at(), None);

        let valid_over_malformed = evidence(with_condition(
            located_report(vec![
                action_at("valid", "10.00", Some("1"), Some("20260806115959")),
                action_at("malformed", "10.00", Some("1"), Some("not-a-date")),
                action_at("absent", "10.00", Some("1"), None),
            ]),
            "pending settlement",
        ));
        assert_eq!(
            valid_over_malformed.action().map(GatewayDiagnostic::expose),
            Some("valid")
        );
        assert_eq!(
            valid_over_malformed.effective_at().map(DateTime::timestamp),
            Some(1_786_017_599)
        );
    }

    #[test]
    fn refund_accumulation_rejects_overflow_zero_and_invalid_latest_basis() {
        let overflow = located_report(vec![
            action("sale", "10.00", Some("1")),
            action("refund", "92233720368547758.07", Some("1")),
            action("credit", "92233720368547758.07", Some("1")),
        ]);
        assert_eq!(
            quarantine_reason(overflow),
            GatewayLifecycleQuarantineReason::InvalidRefundEconomics
        );

        let zero = located_report(vec![
            action("sale", "10.00", Some("1")),
            action("refund", "0.00", Some("1")),
        ]);
        assert_eq!(
            quarantine_reason(zero),
            GatewayLifecycleQuarantineReason::InvalidRefundEconomics
        );

        let invalid_latest_basis = located_report(vec![
            action_at("sale", "10.00", Some("1"), Some("20260806110000")),
            action_at(
                "capture",
                "not-an-amount",
                Some("1"),
                Some("20260806120000"),
            ),
            action_at("refund", "1.00", Some("1"), Some("20260806130000")),
        ]);
        assert_eq!(
            quarantine_reason(invalid_latest_basis),
            GatewayLifecycleQuarantineReason::InvalidRefundEconomics
        );
    }

    #[test]
    fn refund_errors_keep_first_refund_error_but_ambiguous_return_or_void_wins() {
        let invalid_then_ambiguous_refund = located_report(vec![
            action("refund", "0.00", Some("1")),
            action("refund", "1.00", None),
        ]);
        assert_eq!(
            quarantine_reason(invalid_then_ambiguous_refund),
            GatewayLifecycleQuarantineReason::InvalidRefundEconomics
        );

        let ambiguous_then_invalid_refund = located_report(vec![
            action("refund", "1.00", None),
            action("refund", "0.00", Some("1")),
        ]);
        assert_eq!(
            quarantine_reason(ambiguous_then_invalid_refund),
            GatewayLifecycleQuarantineReason::AmbiguousReversalSuccess
        );

        let invalid_refund_then_ambiguous_void = located_report(vec![
            action("refund", "0.00", Some("1")),
            action("void", "1.00", None),
        ]);
        assert_eq!(
            quarantine_reason(invalid_refund_then_ambiguous_void),
            GatewayLifecycleQuarantineReason::AmbiguousReversalSuccess
        );
    }

    #[test]
    fn condition_aliases_and_action_diagnostics_are_preserved() {
        for condition in ["complete", "completed"] {
            let settled = evidence(with_condition(located_report(Vec::new()), condition));
            assert_eq!(
                settled.state(),
                &GatewayLifecycleState::Settled {
                    cumulative_refunded_cents: None
                }
            );
        }
        for condition in ["canceled", "voided"] {
            let voided = evidence(with_condition(located_report(Vec::new()), condition));
            assert_eq!(voided.state(), &GatewayLifecycleState::Voided);
        }
        for condition in [
            "pending settlement",
            "pending_settlement",
            "pending-settlement",
        ] {
            let pending = evidence(with_condition(located_report(Vec::new()), condition));
            assert_eq!(pending.state(), &GatewayLifecycleState::PendingSettlement);
        }

        let diagnostic_provenance = evidence(with_condition(
            located_report(vec![action(" SeTt-le ", "10.00", Some("1"))]),
            "completed",
        ));
        assert_eq!(
            diagnostic_provenance
                .action()
                .map(GatewayDiagnostic::expose),
            Some("SeTt-le")
        );
    }
}
