//! Collection scheduling is separate from access policy and causal payment history.
use super::*;

/// Valid collection states derived from canonical status and scheduling facts.
/// A scheduled retry alone does not grant access: use the snapshotted access policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubscriptionCollectionState {
    /// The current paid period has one automatic renewal scheduled.
    Scheduled {
        /// Next period's collection boundary.
        due_at: DateTime<Utc>,
    },
    /// An unpaid period has another automatic payment attempt scheduled.
    RetryScheduled {
        /// Earliest scheduled retry time, at or after the due-period boundary.
        retry_at: DateTime<Utc>,
    },
    /// Collection has exhausted its automatic retries; manual recovery remains possible.
    Exhausted,
    /// Collection has stopped through cancellation; paid-through access is independent.
    Canceled,
    /// The collection lifecycle ended unpaid and cannot authorize recovery.
    Unpaid,
}

/// Status and scheduling facts cannot describe a canonical collection state.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
#[error("subscription collection state is invalid")]
pub struct SubscriptionCollectionStateError;

impl SubscriptionCollectionState {
    /// Validates the persisted schedule without using wall-clock time or current offers.
    pub fn from_persisted(
        status: SubscriptionStatus,
        next_renewal_at: DateTime<Utc>,
        next_payment_attempt_at: Option<DateTime<Utc>>,
    ) -> Result<Self, SubscriptionCollectionStateError> {
        match (status, next_payment_attempt_at) {
            (SubscriptionStatus::Active, Some(due_at)) if due_at == next_renewal_at => {
                Ok(Self::Scheduled { due_at })
            }
            (SubscriptionStatus::PastDue, Some(retry_at)) if retry_at >= next_renewal_at => {
                Ok(Self::RetryScheduled { retry_at })
            }
            (SubscriptionStatus::PastDue, None) => Ok(Self::Exhausted),
            (SubscriptionStatus::Canceled, None) => Ok(Self::Canceled),
            (SubscriptionStatus::Unpaid, None) => Ok(Self::Unpaid),
            _ => Err(SubscriptionCollectionStateError),
        }
    }

    /// Returns the durable status represented by this collection state.
    pub const fn status(self) -> SubscriptionStatus {
        match self {
            Self::Scheduled { .. } => SubscriptionStatus::Active,
            Self::RetryScheduled { .. } | Self::Exhausted => SubscriptionStatus::PastDue,
            Self::Canceled => SubscriptionStatus::Canceled,
            Self::Unpaid => SubscriptionStatus::Unpaid,
        }
    }

    /// Returns the next automatic attempt, if collection is still scheduled.
    pub const fn next_payment_attempt_at(self) -> Option<DateTime<Utc>> {
        match self {
            Self::Scheduled { due_at } => Some(due_at),
            Self::RetryScheduled { retry_at } => Some(retry_at),
            Self::Exhausted | Self::Canceled | Self::Unpaid => None,
        }
    }

    /// Whether a matching already-authorized payment may advance this lifecycle.
    /// This includes legacy active recovery authority; new recovery admission
    /// still requires a past-due lifecycle and exact durable reservation facts.
    pub const fn accepts_payment(self) -> bool {
        matches!(
            self,
            Self::Scheduled { .. } | Self::RetryScheduled { .. } | Self::Exhausted
        )
    }

    /// Projects dunning access from the accepted policy. Non-dunning states
    /// return `None`; cancellation access must use its paid-through period.
    pub const fn past_due_access(self, policy: PastDueAccessPolicy) -> Option<PastDueAccess> {
        match self {
            Self::RetryScheduled { retry_at } => {
                Some(classify_past_due_access(policy, Some(retry_at)))
            }
            Self::Exhausted => Some(PastDueAccess::Suspended),
            Self::Scheduled { .. } | Self::Canceled | Self::Unpaid => None,
        }
    }
}

impl Subscription {
    /// Validates the legacy construction path before using it as collection authority.
    pub fn validated(self) -> Result<Self, SubscriptionCollectionStateError> {
        self.collection_state()?;
        Ok(self)
    }

    /// Interprets this subscription's status and schedule as one collection state.
    pub fn collection_state(
        &self,
    ) -> Result<SubscriptionCollectionState, SubscriptionCollectionStateError> {
        SubscriptionCollectionState::from_persisted(
            self.status,
            self.next_renewal_at,
            self.next_payment_attempt_at,
        )
    }
}

#[cfg(test)]
mod tests;
