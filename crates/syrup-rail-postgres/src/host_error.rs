use std::{error::Error, fmt};

pub(crate) type BoxError = Box<dyn Error + Send + Sync + 'static>;

/// Owns an arbitrary host error without making ambient formatting an exposure
/// boundary.
pub(crate) struct RedactedHostErrorSource(BoxError);

impl RedactedHostErrorSource {
    pub(crate) fn new(source: impl Error + Send + Sync + 'static) -> Self {
        Self(Box::new(source))
    }

    pub(crate) fn into_inner(self) -> BoxError {
        self.0
    }
}

impl fmt::Debug for RedactedHostErrorSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RedactedHostErrorSource([redacted])")
    }
}

#[cfg(test)]
mod tests {
    use std::{error::Error, fmt};

    use crate::{
        BillingEventWriteError, BillingTransactionError, ExternalReversalHostStoreError,
        HostChargeTargetError, ManualAttemptFailureHostStoreError, SubscriptionBillingServiceError,
    };

    const SENTINEL: &str = "host-error-secret-sentinel";

    #[derive(Debug)]
    struct SensitiveHostError;

    impl fmt::Display for SensitiveHostError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(SENTINEL)
        }
    }

    impl Error for SensitiveHostError {}

    fn assert_ambiently_redacted(error: &(dyn Error + 'static)) {
        assert!(!error.to_string().contains(SENTINEL));
        assert!(!format!("{error:?}").contains(SENTINEL));
        assert!(error.source().is_none());
    }

    #[test]
    fn every_host_error_wrapper_requires_explicit_source_consumption() {
        let transaction = BillingTransactionError::new(SensitiveHostError);
        assert_ambiently_redacted(&transaction);
        assert_eq!(transaction.into_source().to_string(), SENTINEL);

        let event = BillingEventWriteError::new(SensitiveHostError);
        assert_ambiently_redacted(&event);
        assert_eq!(event.into_source().to_string(), SENTINEL);

        let target = HostChargeTargetError::new(SensitiveHostError);
        assert_ambiently_redacted(&target);
        assert_eq!(target.into_source().to_string(), SENTINEL);

        let manual = ManualAttemptFailureHostStoreError::new(SensitiveHostError);
        assert_ambiently_redacted(&manual);
        assert_eq!(manual.into_source().to_string(), SENTINEL);

        let reversal = ExternalReversalHostStoreError::new(SensitiveHostError);
        assert_ambiently_redacted(&reversal);
        assert_eq!(reversal.into_source().to_string(), SENTINEL);
    }

    #[test]
    fn service_error_chain_stops_before_the_arbitrary_host_source() {
        let error = SubscriptionBillingServiceError::BillingTransaction(
            BillingTransactionError::new(SensitiveHostError),
        );
        let mut current: Option<&(dyn Error + 'static)> = Some(&error);
        let mut rendered = String::new();
        while let Some(error) = current {
            rendered.push_str(&error.to_string());
            rendered.push_str(&format!("{error:?}"));
            current = error.source();
        }

        assert!(!rendered.contains(SENTINEL));
        assert!(error.source().and_then(|source| source.source()).is_none());
    }
}
