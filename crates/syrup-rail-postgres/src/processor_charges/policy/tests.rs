use super::*;

fn facts(identity: ApprovalIdentity) -> ApprovalApplicationFacts {
    ApprovalApplicationFacts {
        identity,
        amount_cents: 100,
        attempt_kind: PaymentAttemptKind::HostCharge,
        attempt_status: PaymentAttemptStatus::Approved,
        resolution_code: None,
        attempt_transaction_id: Some("owned-transaction".into()),
        identity_match: ChargeIdentityMatch::Same,
    }
}

#[test]
fn approved_attempt_does_not_authorize_unidentified_or_additional_evidence() {
    assert_eq!(
        facts(ApprovalIdentity::Primary).pending_decision(),
        PendingChargeDecision::Applied
    );
    assert_eq!(
        facts(ApprovalIdentity::Unidentified).pending_decision(),
        PendingChargeDecision::IdentifyTransaction
    );
    assert_eq!(
        facts(ApprovalIdentity::AdditionalCharge).pending_decision(),
        PendingChargeDecision::ReverseAdditional
    );
    let mut zero = facts(ApprovalIdentity::AdditionalZeroAmount);
    zero.amount_cents = 0;
    assert_eq!(
        zero.pending_decision(),
        PendingChargeDecision::ReviewZeroAmountAdditional
    );
}

#[test]
fn late_or_conflicting_approval_requires_reversal_instead_of_application() {
    let mut approval = facts(ApprovalIdentity::Primary);
    approval.identity_match = ChargeIdentityMatch::Different;
    assert_eq!(
        approval.pending_decision(),
        PendingChargeDecision::ReverseConflicting
    );
    approval.identity_match = ChargeIdentityMatch::Same;
    approval.attempt_status = PaymentAttemptStatus::Failed;
    approval.resolution_code = Some(PaymentResolutionCode::ProcessorChargeExternallyRefunded);
    assert_eq!(
        approval.pending_decision(),
        PendingChargeDecision::ReverseConflicting
    );
    approval.attempt_status = PaymentAttemptStatus::ReviewRequired;
    approval.attempt_kind = PaymentAttemptKind::SubscriptionInitial;
    approval.resolution_code = Some(PaymentResolutionCode::SubscriptionInitialCurrentGrantConflict);
    assert_eq!(
        approval.pending_decision(),
        PendingChargeDecision::ReverseConflicting
    );
    approval.resolution_code = None;
    assert_eq!(
        approval.pending_decision(),
        PendingChargeDecision::AwaitApplication
    );
}
