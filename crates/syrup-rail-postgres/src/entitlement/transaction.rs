use sqlx::{PgConnection, PgPool};
use sqlx::{Postgres, pool::PoolConnection, postgres::PgTransactionManager};
use sqlx_core::transaction::TransactionManager;
use std::fmt;
use std::ops::{Deref, DerefMut};

/// A top-level transaction that releases its checkout on cancellation,
/// including while SQLx is awaiting a row or advisory lock.
pub(super) struct GuardTransaction {
    connection: PoolConnection<Postgres>,
    finished: bool,
}

impl GuardTransaction {
    pub(super) async fn begin(pool: &PgPool) -> Result<Self, sqlx::Error> {
        let mut tx = Self {
            connection: pool.acquire().await?,
            finished: false,
        };
        PgTransactionManager::begin(&mut tx.connection, None).await?;
        Ok(tx)
    }
    pub(super) async fn commit(mut self) -> Result<(), sqlx::Error> {
        PgTransactionManager::commit(&mut self.connection).await?;
        self.finished = true;
        Ok(())
    }
    pub(super) async fn rollback(mut self) -> Result<(), sqlx::Error> {
        PgTransactionManager::rollback(&mut self.connection).await?;
        self.finished = true;
        Ok(())
    }
}

impl Deref for GuardTransaction {
    type Target = PgConnection;
    fn deref(&self) -> &PgConnection {
        &self.connection
    }
}
impl DerefMut for GuardTransaction {
    fn deref_mut(&mut self) -> &mut PgConnection {
        &mut self.connection
    }
}
impl Drop for GuardTransaction {
    fn drop(&mut self) {
        if !self.finished {
            self.connection.close_on_drop();
        }
    }
}

/// A top-level PostgreSQL transaction awaiting entitlement admission.
///
/// Create this transaction directly from a pool with [`Self::begin`]. It can
/// carry host preparatory writes, but deliberately has no commit operation.
/// Passing it to [`super::require_entitlement_for_update`] either rolls it back or
/// transforms it into an [`AdmittedEntitlementWriteTransaction`].
pub struct EntitlementWriteTransaction {
    pub(super) inner: GuardTransaction,
}

impl fmt::Debug for EntitlementWriteTransaction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EntitlementWriteTransaction")
            .finish_non_exhaustive()
    }
}

impl EntitlementWriteTransaction {
    /// Starts a top-level transaction reserved for an entitlement-protected write.
    pub async fn begin(pool: &PgPool) -> Result<Self, sqlx::Error> {
        Ok(Self {
            inner: GuardTransaction::begin(pool).await?,
        })
    }

    /// Borrows the transaction connection for preparatory host database work.
    ///
    /// Callers must finish any nested savepoint before returning this value to
    /// Syrup Rail. Leaking a savepoint would violate SQLx's transaction
    /// lifecycle contract.
    pub fn connection(&mut self) -> &mut PgConnection {
        &mut self.inner
    }

    /// Explicitly rolls back the pending transaction.
    pub async fn rollback(self) -> Result<(), sqlx::Error> {
        self.inner.rollback().await
    }
}

/// A top-level transaction that passed entitlement admission.
///
/// Its entitlement rows and aggregate advisory lock remain held until this
/// value is committed or rolled back. The protected host mutation must use
/// [`Self::connection`] on this value.
pub struct AdmittedEntitlementWriteTransaction {
    pub(super) inner: GuardTransaction,
}

impl fmt::Debug for AdmittedEntitlementWriteTransaction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AdmittedEntitlementWriteTransaction")
            .finish_non_exhaustive()
    }
}

impl AdmittedEntitlementWriteTransaction {
    /// Borrows the admitted transaction connection for the host-owned protected mutation.
    ///
    /// Callers must finish any nested savepoint before committing or rolling
    /// back this outer transaction. Leaking a savepoint would violate SQLx's
    /// transaction lifecycle contract.
    pub fn connection(&mut self) -> &mut PgConnection {
        &mut self.inner
    }

    /// Commits the protected transaction and releases its entitlement locks.
    pub async fn commit(self) -> Result<(), sqlx::Error> {
        self.inner.commit().await
    }

    /// Rolls back the protected transaction and releases its entitlement locks.
    pub async fn rollback(self) -> Result<(), sqlx::Error> {
        self.inner.rollback().await
    }
}
