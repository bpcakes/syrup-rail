#![warn(missing_docs)]

use std::{error::Error, fmt, time::Duration};

use async_trait::async_trait;
use sqlx::PgConnection;
use syrup_rail::{BillingEvent, BillingEventSubject};

use crate::host_error::{BoxError, RedactedHostErrorSource};

/// Value-redacted failure returned by the host transaction coordinator.
#[derive(Debug)]
pub struct BillingTransactionError {
    source: RedactedHostErrorSource,
}

impl BillingTransactionError {
    /// Wraps a host transaction error without exposing its value through
    /// ordinary formatting or the standard error-source chain.
    pub fn new(source: impl Error + Send + Sync + 'static) -> Self {
        Self {
            source: RedactedHostErrorSource::new(source),
        }
    }

    /// Returns the host error for explicit application-level inspection.
    ///
    /// Consuming this wrapper is the only boundary that reveals the arbitrary
    /// host source.
    pub fn into_source(self) -> BoxError {
        self.source.into_inner()
    }
}

impl fmt::Display for BillingTransactionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("billing transaction operation failed")
    }
}

impl Error for BillingTransactionError {}

/// Value-redacted failure returned while appending a host outbox event.
#[derive(Debug)]
pub struct BillingEventWriteError {
    source: RedactedHostErrorSource,
}

impl BillingEventWriteError {
    /// Wraps a host outbox error without exposing its value through ordinary
    /// formatting or the standard error-source chain.
    pub fn new(source: impl Error + Send + Sync + 'static) -> Self {
        Self {
            source: RedactedHostErrorSource::new(source),
        }
    }

    /// Returns the host error for explicit application-level inspection.
    ///
    /// Consuming this wrapper is the only boundary that reveals the arbitrary
    /// host source.
    pub fn into_source(self) -> BoxError {
        self.source.into_inner()
    }
}

impl fmt::Display for BillingEventWriteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("billing event append failed")
    }
}

impl Error for BillingEventWriteError {}

/// Durable availability of the host recipient locked for a billing event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BillingTransactionSubjectState {
    /// The host recipient remains live and may receive ordinary mutations.
    LiveRecipient,
    /// The host retained the billing subject only for financial history.
    RetainedSubject,
}

/// Host-prepared transaction whose subject authorization lock is acquired
/// before any shared billing lock.
#[async_trait]
pub trait BillingTransactionCoordinator: Send + Sync {
    /// Begins a host transaction and locks the exact event subject before any
    /// shared billing row lock is acquired.
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
/// Dropping a value that has not completed `commit` or `rollback` must make all
/// of its writes non-durable and release its locks. This rollback-on-drop
/// guarantee is required because an async caller can be canceled while the
/// transaction is borrowed; completed error paths still request an explicit
/// rollback before returning.
#[async_trait]
pub trait BillingTransaction: Send {
    /// Returns the connection owned by this exact host transaction.
    fn connection(&mut self) -> &mut PgConnection;

    /// Returns whether the locked host subject is live or retained.
    fn subject_state(&self) -> BillingTransactionSubjectState;

    /// Appends a typed billing event to the host outbox on this transaction.
    async fn append_event(&mut self, event: &BillingEvent) -> Result<(), BillingEventWriteError>;

    /// Commits both host and shared billing changes.
    async fn commit(self: Box<Self>) -> Result<(), BillingTransactionError>;

    /// Rolls back both host and shared billing changes.
    async fn rollback(self: Box<Self>) -> Result<(), BillingTransactionError>;
}
