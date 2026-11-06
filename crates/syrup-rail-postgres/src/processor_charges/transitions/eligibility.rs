//! Application authority is a property of the charge and its owning attempt,
//! not of the charge's queue/progression label alone.
use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ApplicationDisposition {
    AwaitingApproval,
    IdentityConflict,
    Unidentified,
    AdditionalCharge,
    ExpiredPeriod,
    ExternallyReversed,
    Applicable,
}

impl ApplicationDisposition {
    fn from_evidence(charge: &ProcessorCharge, attempt: &PaymentAttempt) -> Self {
        if charge.progression() == ProcessorChargeProgression::ExternallyReversed
            || (attempt.status() == PaymentAttemptStatus::Failed
                && policy::is_external_reversal_resolution(attempt.state().resolution_code()))
        {
            return Self::ExternallyReversed;
        }
        if is_expiry_reason(charge.state_code())
            || attempt.state().resolution_code()
                == Some(PaymentResolutionCode::SubscriptionApprovedPeriodExpired)
        {
            return Self::ExpiredPeriod;
        }
        if charge.role() == ProcessorChargeRole::Additional {
            return Self::AdditionalCharge;
        }
        let Some(transaction_id) = charge.evidence().transaction_id() else {
            return Self::Unidentified;
        };
        if attempt.status() != PaymentAttemptStatus::Approved {
            return Self::AwaitingApproval;
        }
        if attempt.state().processor_evidence().transaction_id() != Some(transaction_id) {
            return Self::IdentityConflict;
        }
        // Ordinary reversal review is recoverable once the owning application
        // has approved this exact primary charge in the same transaction.
        Self::Applicable
    }
}

fn is_expiry_reason(code: Option<ProcessorChargeStateCode>) -> bool {
    code == Some(ProcessorChargeStateCode::PaymentResolution(
        PaymentResolutionCode::SubscriptionApprovedPeriodExpired,
    ))
}

pub(super) async fn validate_transition(
    connection: &mut PgConnection,
    charge_id: ProcessorChargeId,
    destination: ProcessorChargeProgression,
    code_update: &ChargeStateCodeUpdate,
) -> Result<(), ProcessorChargeStoreError> {
    // Callers already own the aggregate/attempt locks. Lock only the charge
    // here, after those domains, and retain the SQL revalidation below.
    let charge = charge_by_id(connection, charge_id.into_uuid()).await?;
    let resulting_code = match code_update {
        ChargeStateCodeUpdate::Replace(code) => *code,
        ChargeStateCodeUpdate::PreserveExisting(code) => charge.state_code().or(*code),
    };
    if is_expiry_reason(resulting_code)
        && (!matches!(
            destination,
            ProcessorChargeProgression::ExternalReversalRequired
                | ProcessorChargeProgression::ExternallyReversed
        ) || !matches!(
            charge.attempt_kind(),
            PaymentAttemptKind::SubscriptionInitial
                | PaymentAttemptKind::SubscriptionRenewal
                | PaymentAttemptKind::SubscriptionRecovery
        ))
    {
        return Err(ProcessorChargeStoreError::InvalidState(
            "expired-period reason requires subscription reversal disposition",
        ));
    }
    if destination == ProcessorChargeProgression::Applied {
        let attempt = find_payment_attempt_by_id_on_connection(
            connection,
            charge.billing_scope_id(),
            charge.attempt_id(),
        )
        .await?
        .ok_or(ProcessorChargeStoreError::InvalidState(
            "processor charge attempt was not found",
        ))?;
        if ApplicationDisposition::from_evidence(&charge, &attempt)
            != ApplicationDisposition::Applicable
        {
            return Err(ProcessorChargeStoreError::InvalidState(
                "processor charge evidence does not permit application",
            ));
        }
    }
    Ok(())
}
