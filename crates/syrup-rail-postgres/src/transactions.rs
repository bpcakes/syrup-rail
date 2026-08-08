use std::{error::Error, fmt, time::Duration};

use async_trait::async_trait;
use sqlx::PgConnection;
use syrup_rail::{BillingEvent, BillingEventSubject};

type BoxError = Box<dyn Error + Send + Sync + 'static>;

#[derive(Debug)]
pub struct BillingTransactionError {
    source: BoxError,
}

impl BillingTransactionError {
    pub fn new(source: impl Error + Send + Sync + 'static) -> Self {
        Self {
            source: Box::new(source),
        }
    }

    pub fn into_source(self) -> BoxError {
        self.source
    }
}

impl fmt::Display for BillingTransactionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("billing transaction operation failed")
    }
}

impl Error for BillingTransactionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.source.as_ref())
    }
}

#[derive(Debug)]
pub struct BillingEventWriteError {
    source: BoxError,
}

impl BillingEventWriteError {
    pub fn new(source: impl Error + Send + Sync + 'static) -> Self {
        Self {
            source: Box::new(source),
        }
    }

    pub fn into_source(self) -> BoxError {
        self.source
    }
}

impl fmt::Display for BillingEventWriteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("billing event append failed")
    }
}

impl Error for BillingEventWriteError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.source.as_ref())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BillingTransactionSubjectState {
    LiveRecipient,
    RetainedSubject,
}

/// Host-prepared transaction whose subject authorization lock is acquired
/// before any shared billing lock.
#[async_trait]
pub trait BillingTransactionCoordinator: Send + Sync {
    async fn begin(
        &self,
        subject: BillingEventSubject,
        lock_timeout: Duration,
    ) -> Result<Box<dyn BillingTransaction>, BillingTransactionError>;
}

/// One host-owned transaction and its typed event projection capability.
///
/// The returned connection is the same transaction on which `append_event`
/// and `commit` operate. Implementations must not acquire another connection.
#[async_trait]
pub trait BillingTransaction: Send {
    fn connection(&mut self) -> &mut PgConnection;

    fn subject_state(&self) -> BillingTransactionSubjectState;

    async fn append_event(&mut self, event: &BillingEvent) -> Result<(), BillingEventWriteError>;

    async fn commit(self: Box<Self>) -> Result<(), BillingTransactionError>;

    async fn rollback(self: Box<Self>) -> Result<(), BillingTransactionError>;
}
