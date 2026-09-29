//! Selection of one original NMI action from an exact diagnostic lookup.

use syrup_rail::{
    GatewayDiagnosticOperation, GatewayDiagnosticsSource, GatewayDiagnosticsUnavailableReason,
    GatewayOrderId, GatewayTransactionDiagnostics, GatewayTransactionDiagnosticsObservation,
    GatewayTransactionId, Money, canonical_gateway_transaction_ids_equal,
};
use syrup_rail_nmi_client::{SensitiveText, TransactionDiagnosticsLookup};

/// The NMI interface that produces diagnostic observations.
pub(super) const DIAGNOSTICS_SOURCE: &str = "nmi_query_api";

/// Binds an exact lookup to the requested transaction and selects exactly one
/// original action. Never selects by recency: zero or several matching actions
/// are unavailable rather than guessed.
pub(super) fn select_transaction_diagnostics(
    lookup: TransactionDiagnosticsLookup,
    transaction_id: &GatewayTransactionId,
    operation: GatewayDiagnosticOperation,
    amount: Money,
    expected_order_id: Option<&GatewayOrderId>,
) -> GatewayTransactionDiagnostics {
    use GatewayDiagnosticsUnavailableReason as Reason;
    let unavailable = GatewayTransactionDiagnostics::Unavailable;
    let transaction = match lookup {
        TransactionDiagnosticsLookup::NotFound => return GatewayTransactionDiagnostics::NotFound,
        TransactionDiagnosticsLookup::MultipleTransactions => {
            return unavailable(Reason::MultipleTransactions);
        }
        TransactionDiagnosticsLookup::Found(transaction) => transaction.into_parts(),
        _ => return unavailable(Reason::MalformedResponse),
    };
    if transaction.malformed {
        return unavailable(Reason::MalformedResponse);
    }
    if !transaction.transaction_id.as_ref().is_some_and(|returned| {
        canonical_gateway_transaction_ids_equal(returned.expose(), transaction_id.expose())
    }) {
        return unavailable(Reason::TransactionMismatch);
    }
    if let (Some(expected), Some(returned)) = (expected_order_id, &transaction.order_id)
        && returned.expose().trim() != expected.expose()
    {
        return unavailable(Reason::OrderMismatch);
    }
    if let Some(currency) = &transaction.currency
        && !currency
            .expose()
            .trim()
            .eq_ignore_ascii_case(amount.currency().as_str())
    {
        return unavailable(Reason::CurrencyMismatch);
    }

    let action_type = match operation {
        GatewayDiagnosticOperation::Sale => "sale",
        GatewayDiagnosticOperation::Validate => "validate",
    };
    let expected_amount = format!("{}.{:02}", amount.cents() / 100, amount.cents() % 100);
    let mut matching = transaction.actions.into_iter().filter(|action| {
        action
            .action_type
            .as_ref()
            .is_some_and(|value| value.expose() == action_type)
            && action
                .amount
                .as_ref()
                .is_some_and(|value| value.expose() == expected_amount)
    });
    let Some(action) = matching.next() else {
        return unavailable(Reason::NoMatchingAction);
    };
    if matching.next().is_some() {
        return unavailable(Reason::AmbiguousAction);
    }

    let source = GatewayDiagnosticsSource::new(DIAGNOSTICS_SOURCE).expect("static source is valid");
    let observation = GatewayTransactionDiagnosticsObservation::new(operation, source)
        .with_gateway_response_code(exposed(&action.response_code))
        .with_gateway_response_text(exposed(&action.response_text))
        .with_processor_response_code(exposed(&action.processor_response_code))
        .with_processor_response_text(exposed(&action.processor_response_text))
        .with_avs_response(exposed(&transaction.avs_response))
        .with_csc_response(exposed(&transaction.csc_response));
    let observation = if transaction.incomplete || action.incomplete {
        observation.mark_partial()
    } else {
        observation
    };
    GatewayTransactionDiagnostics::Observed(Box::new(observation))
}

fn exposed(value: &Option<SensitiveText>) -> Option<&str> {
    value.as_ref().map(SensitiveText::expose)
}
