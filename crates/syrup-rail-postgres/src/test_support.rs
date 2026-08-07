use std::error::Error;

use postgres_test_harness::{DatabaseLease, HarnessConfig, PostgresHarness};
use sqlx::{PgPool, postgres::PgPoolOptions};
use uuid::Uuid;

use crate::schema_contract::V1_INSTALL_SQL;

pub(crate) struct TestDatabase {
    harness: PostgresHarness,
    lease: DatabaseLease,
    pub(crate) pool: PgPool,
}

#[derive(Clone, Copy)]
pub(crate) struct GatewayAccountFixture {
    pub(crate) billing_scope_id: Uuid,
    pub(crate) gateway_account_id: Uuid,
    pub(crate) gateway_configuration_id: Uuid,
}

impl TestDatabase {
    pub(crate) async fn start(project: &str) -> Result<Self, Box<dyn Error>> {
        let harness =
            PostgresHarness::start(HarnessConfig::new(project)?.with_connection_budget(4)?).await?;
        let lease = harness.empty_database().await?;
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(lease.database_url())
            .await?;
        sqlx::raw_sql(V1_INSTALL_SQL).execute(&pool).await?;
        Ok(Self {
            harness,
            lease,
            pool,
        })
    }

    pub(crate) async fn cleanup(self) -> Result<(), Box<dyn Error>> {
        self.pool.close().await;
        self.lease.cleanup().await?;
        self.harness.shutdown().await?;
        Ok(())
    }
}

pub(crate) async fn create_gateway_account(
    pool: &PgPool,
    provider_key: &str,
) -> Result<GatewayAccountFixture, sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO billing_gateway_provider_rate_limits (provider_key)
        VALUES ($1)
        ON CONFLICT (provider_key) DO NOTHING
        "#,
    )
    .bind(provider_key)
    .execute(pool)
    .await?;

    let fixture = GatewayAccountFixture {
        billing_scope_id: Uuid::now_v7(),
        gateway_account_id: Uuid::now_v7(),
        gateway_configuration_id: Uuid::now_v7(),
    };
    sqlx::query(
        r#"
        INSERT INTO billing_gateway_accounts (
            id,
            billing_scope_id,
            provider_key,
            gateway_configuration_id
        ) VALUES ($1, $2, $3, $4)
        "#,
    )
    .bind(fixture.gateway_account_id)
    .bind(fixture.billing_scope_id)
    .bind(provider_key)
    .bind(fixture.gateway_configuration_id)
    .execute(pool)
    .await?;
    Ok(fixture)
}
