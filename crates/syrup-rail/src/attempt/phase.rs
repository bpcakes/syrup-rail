//! Composite attempt phases. Admission records retry authority, not proof that
//! a provider received a request. Legacy uncertain rows without an admission
//! timestamp remain uncertain and can never become prepared through decoding.
use super::*;

/// Durable attempt phase derived from status and submission admission evidence.
/// This is an observation; SQL locks and compare-and-set still authorize writes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PaymentAttemptPhase {
    /// Pending and never admitted (or safely restored after definite non-submission).
    Prepared,
    /// Admitted for one provider call; it may already have reached the provider.
    Admitted {
        /// Exact admission generation used to fence retry restoration.
        admitted_at: DateTime<Utc>,
    },
    /// Provider outcome is uncertain, including legacy rows without a timestamp.
    Uncertain,
    /// Evidence requires reconciliation or privileged review.
    ReviewRequired,
    /// Application approved the payment.
    Approved,
    /// The provider declined the payment.
    Declined,
    /// The attempt failed locally or at the provider.
    Failed,
}

impl PaymentAttemptPhase {
    /// Interprets persisted facts without turning missing timestamps into retry permission.
    pub const fn from_persisted(
        status: PaymentAttemptStatus,
        submitted_at: Option<DateTime<Utc>>,
    ) -> Self {
        match status {
            PaymentAttemptStatus::Pending => match submitted_at {
                Some(admitted_at) => Self::Admitted { admitted_at },
                None => Self::Prepared,
            },
            PaymentAttemptStatus::Unknown => Self::Uncertain,
            PaymentAttemptStatus::ReviewRequired => Self::ReviewRequired,
            PaymentAttemptStatus::Approved => Self::Approved,
            PaymentAttemptStatus::Declined => Self::Declined,
            PaymentAttemptStatus::Failed => Self::Failed,
        }
    }

    /// Whether the attempt may enter final admission for a provider call.
    pub const fn is_prepared(self) -> bool {
        matches!(self, Self::Prepared)
    }

    /// Whether the attempt has one outstanding admission.
    pub const fn is_admitted(self) -> bool {
        matches!(self, Self::Admitted { .. })
    }

    /// Whether provider evidence may resolve this phase.
    pub const fn is_resolvable(self) -> bool {
        matches!(
            self,
            Self::Prepared | Self::Admitted { .. } | Self::Uncertain | Self::ReviewRequired
        )
    }
}

impl PaymentAttempt {
    /// Combines durable status and admission evidence into one retry-safe phase.
    pub const fn phase(&self) -> PaymentAttemptPhase {
        PaymentAttemptPhase::from_persisted(self.state.status, self.state.timestamps.submitted_at())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_admission_is_retry_authority_only_for_pending_attempts() {
        for status in [
            PaymentAttemptStatus::Pending,
            PaymentAttemptStatus::Unknown,
            PaymentAttemptStatus::ReviewRequired,
            PaymentAttemptStatus::Approved,
            PaymentAttemptStatus::Declined,
            PaymentAttemptStatus::Failed,
        ] {
            for timestamp in [None, Some(DateTime::UNIX_EPOCH)] {
                let phase = PaymentAttemptPhase::from_persisted(status, timestamp);
                assert_eq!(
                    phase.is_prepared(),
                    status == PaymentAttemptStatus::Pending && timestamp.is_none()
                );
                assert_eq!(
                    phase.is_admitted(),
                    status == PaymentAttemptStatus::Pending && timestamp.is_some()
                );
                assert_eq!(phase.is_resolvable(), status.is_resolvable());
            }
        }
    }
}
