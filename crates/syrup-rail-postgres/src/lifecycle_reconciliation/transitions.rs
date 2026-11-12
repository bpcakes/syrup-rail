//! Pure lifecycle evidence policy. Reports can arrive out of chronological order;
//! advancing evidence still has to preserve valid, monotonic refund economics.
use chrono::{DateTime, Utc};
use syrup_rail::GatewayLifecycleState;

use super::{GatewayLifecycleReconciliationError, INVALID_STORED_STATE};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LifecycleTransition {
    Apply { refunded_amount_cents: i32 },
    AlreadySuperseded,
    InvalidRefundEconomics,
    ConflictingLifecycleEvidence,
}

pub(super) fn lifecycle_transition(
    current_state: &GatewayLifecycleState,
    current_refunded_amount_cents: i32,
    current_lifecycle_at: Option<DateTime<Utc>>,
    incoming_state: &GatewayLifecycleState,
    incoming_lifecycle_at: Option<DateTime<Utc>>,
    captured_amount_cents: i32,
) -> Result<LifecycleTransition, GatewayLifecycleReconciliationError> {
    if !stored_amount_is_valid(
        current_state,
        current_refunded_amount_cents,
        captured_amount_cents,
    ) {
        return Err(GatewayLifecycleReconciliationError::InvalidState(
            INVALID_STORED_STATE,
        ));
    }
    let incoming_refunded_amount_cents = lifecycle_refunded_amount(incoming_state);
    if !incoming_amount_is_valid(incoming_state, captured_amount_cents) {
        return Ok(LifecycleTransition::InvalidRefundEconomics);
    }
    let should_advance = match evidence_transition(current_state, incoming_state) {
        EvidenceTransition::Advance => true,
        EvidenceTransition::Superseded => false,
        EvidenceTransition::Refresh => {
            current_lifecycle_at.is_none()
                || incoming_lifecycle_at
                    .zip(current_lifecycle_at)
                    .is_some_and(|(incoming, current)| incoming > current)
                || incoming_refunded_amount_cents
                    .is_some_and(|incoming| incoming > current_refunded_amount_cents)
        }
    };
    if !should_advance {
        return Ok(LifecycleTransition::AlreadySuperseded);
    }
    let next_refunded_amount_cents = current_refunded_amount_cents
        .max(incoming_refunded_amount_cents.unwrap_or(current_refunded_amount_cents));
    if !stored_amount_is_valid(
        incoming_state,
        next_refunded_amount_cents,
        captured_amount_cents,
    ) {
        return Ok(LifecycleTransition::ConflictingLifecycleEvidence);
    }
    Ok(LifecycleTransition::Apply {
        refunded_amount_cents: next_refunded_amount_cents,
    })
}

pub(super) const fn lifecycle_refunded_amount(state: &GatewayLifecycleState) -> Option<i32> {
    match state {
        GatewayLifecycleState::Settled {
            cumulative_refunded_cents,
        }
        | GatewayLifecycleState::Chargeback {
            cumulative_refunded_cents,
        } => match cumulative_refunded_cents {
            Some(amount) => Some(amount.get()),
            None => None,
        },
        GatewayLifecycleState::Refunded {
            cumulative_refunded_cents,
        } => Some(cumulative_refunded_cents.get()),
        GatewayLifecycleState::Unknown
        | GatewayLifecycleState::PendingSettlement
        | GatewayLifecycleState::Voided => None,
    }
}

fn incoming_amount_is_valid(state: &GatewayLifecycleState, captured: i32) -> bool {
    match state {
        GatewayLifecycleState::Unknown
        | GatewayLifecycleState::PendingSettlement
        | GatewayLifecycleState::Voided => true,
        GatewayLifecycleState::Settled {
            cumulative_refunded_cents,
        } => captured > 0 && cumulative_refunded_cents.is_none_or(|amount| amount.get() < captured),
        GatewayLifecycleState::Refunded {
            cumulative_refunded_cents,
        } => captured > 0 && cumulative_refunded_cents.get() == captured,
        GatewayLifecycleState::Chargeback {
            cumulative_refunded_cents,
        } => {
            captured > 0 && cumulative_refunded_cents.is_none_or(|amount| amount.get() <= captured)
        }
    }
}

fn stored_amount_is_valid(state: &GatewayLifecycleState, refunded: i32, captured: i32) -> bool {
    match state {
        GatewayLifecycleState::Unknown
        | GatewayLifecycleState::PendingSettlement
        | GatewayLifecycleState::Voided => refunded == 0,
        GatewayLifecycleState::Settled { .. } => {
            captured > 0 && refunded >= 0 && refunded < captured
        }
        GatewayLifecycleState::Refunded { .. } => captured > 0 && refunded == captured,
        GatewayLifecycleState::Chargeback { .. } => {
            captured > 0 && refunded >= 0 && refunded <= captured
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EvidenceTransition {
    Advance,
    Refresh,
    Superseded,
}

/// Explicit evidence precedence, not a timeline of processor actions. In
/// particular, a later timestamp cannot reopen a reversed charge as settled.
/// Keep both matches exhaustive: every new state needs a reviewed relationship
/// to every existing state, rather than acquiring transitions from a rank.
const fn evidence_transition(
    current: &GatewayLifecycleState,
    incoming: &GatewayLifecycleState,
) -> EvidenceTransition {
    use EvidenceTransition::{Advance, Refresh, Superseded};
    use GatewayLifecycleState::*;
    match current {
        Unknown => match incoming {
            Unknown => Refresh,
            PendingSettlement | Settled { .. } | Voided | Refunded { .. } | Chargeback { .. } => {
                Advance
            }
        },
        PendingSettlement => match incoming {
            Unknown => Superseded,
            PendingSettlement => Refresh,
            Settled { .. } | Voided | Refunded { .. } | Chargeback { .. } => Advance,
        },
        Settled { .. } => match incoming {
            Unknown | PendingSettlement => Superseded,
            Settled { .. } => Refresh,
            Voided | Refunded { .. } | Chargeback { .. } => Advance,
        },
        Voided => match incoming {
            Unknown | PendingSettlement | Settled { .. } => Superseded,
            Voided => Refresh,
            Refunded { .. } | Chargeback { .. } => Advance,
        },
        Refunded { .. } => match incoming {
            Unknown | PendingSettlement | Settled { .. } | Voided => Superseded,
            Refunded { .. } => Refresh,
            Chargeback { .. } => Advance,
        },
        Chargeback { .. } => match incoming {
            Unknown | PendingSettlement | Settled { .. } | Voided | Refunded { .. } => Superseded,
            Chargeback { .. } => Refresh,
        },
    }
}

#[cfg(test)]
mod tests;
