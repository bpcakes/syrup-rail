use sqlx::{PgConnection, PgPool};
use sqlx::{Postgres, pool::PoolConnection, postgres::PgTransactionManager};
use sqlx_core::transaction::TransactionManager;
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
