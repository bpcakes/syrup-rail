//! Collection transitions retain unpaid evidence as part of the terminal state.
use super::*;
use syrup_rail::SubscriptionCollectionState;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FailureProjection {
    Scheduled { due_at: DateTime<Utc> },
    RetryScheduled { retry_at: DateTime<Utc> },
    Exhausted,
    Canceled,
    Unpaid { ended_at: DateTime<Utc> },
}

impl FailureProjection {
    pub(super) fn from_persisted(
        status: SubscriptionStatus,
        next_renewal_at: DateTime<Utc>,
        next_payment_attempt_at: Option<DateTime<Utc>>,
        unpaid_at: Option<DateTime<Utc>>,
    ) -> Result<Self, RenewalFailureStoreError> {
        let collection = SubscriptionCollectionState::from_persisted(
            status,
            next_renewal_at,
            next_payment_attempt_at,
        )
        .map_err(|_| invalid_state())?;
        match (collection, unpaid_at) {
            (SubscriptionCollectionState::Scheduled { due_at }, None) => {
                Ok(Self::Scheduled { due_at })
            }
            (SubscriptionCollectionState::RetryScheduled { retry_at }, None) => {
                Ok(Self::RetryScheduled { retry_at })
            }
            (SubscriptionCollectionState::Exhausted, None) => Ok(Self::Exhausted),
            (SubscriptionCollectionState::Canceled, None) => Ok(Self::Canceled),
            (SubscriptionCollectionState::Unpaid, Some(ended_at)) => Ok(Self::Unpaid { ended_at }),
            _ => Err(invalid_state()),
        }
    }

    pub(super) const fn collection(self) -> SubscriptionCollectionState {
        match self {
            Self::Scheduled { due_at } => SubscriptionCollectionState::Scheduled { due_at },
            Self::RetryScheduled { retry_at } => {
                SubscriptionCollectionState::RetryScheduled { retry_at }
            }
            Self::Exhausted => SubscriptionCollectionState::Exhausted,
            Self::Canceled => SubscriptionCollectionState::Canceled,
            Self::Unpaid { .. } => SubscriptionCollectionState::Unpaid,
        }
    }

    pub(super) const fn unpaid_at(self) -> Option<DateTime<Utc>> {
        match self {
            Self::Unpaid { ended_at } => Some(ended_at),
            _ => None,
        }
    }
}

pub(super) const fn projection(disposition: RenewalFailureDisposition) -> FailureProjection {
    match disposition {
        RenewalFailureDisposition::RetryScheduled { retry_at } => {
            FailureProjection::RetryScheduled { retry_at }
        }
        RenewalFailureDisposition::RemainPastDue { .. } => FailureProjection::Exhausted,
        RenewalFailureDisposition::MarkUnpaid { ended_at } => {
            FailureProjection::Unpaid { ended_at }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_unpaid_requires_evidence_and_cannot_keep_a_retry() {
        let at = DateTime::UNIX_EPOCH;
        assert!(
            FailureProjection::from_persisted(SubscriptionStatus::Unpaid, at, None, None).is_err()
        );
        assert!(
            FailureProjection::from_persisted(SubscriptionStatus::Unpaid, at, Some(at), Some(at))
                .is_err()
        );
        assert_eq!(
            FailureProjection::from_persisted(SubscriptionStatus::Unpaid, at, None, Some(at))
                .unwrap(),
            FailureProjection::Unpaid { ended_at: at }
        );
        assert!(
            FailureProjection::from_persisted(SubscriptionStatus::PastDue, at, None, Some(at))
                .is_err()
        );
    }
}
