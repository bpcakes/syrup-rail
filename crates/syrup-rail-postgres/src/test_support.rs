use std::{error::Error, io};

use postgres_test_harness::{DatabaseLease, HarnessConfig, PostgresHarness};
use sqlx::{PgPool, postgres::PgPoolOptions};
use uuid::Uuid;

use syrup_rail::{
    ChargeAmount, DunningExhaustion, DunningSchedule, PastDueAccessPolicy, PlanKey,
    RecurringSubscriptionTerms, RenewalFailurePolicy, SubscriptionOffer, SubscriptionPeriodRule,
    SubscriptionStart,
};

use crate::schema_contract::{
    V1_INSTALL_SQL, V1_TO_V2_UPGRADE_SQL, V2_INSTALL_SQL, V2_TO_V3_UPGRADE_SQL, V3_INSTALL_SQL,
    V3_TO_V4_INDEX_SQL, V3_TO_V4_PREPARE_SQL, V3_TO_V4_UPGRADE_SQL, V3_TO_V4_VALIDATE_SQL,
    V4_INSTALL_SQL, V4_TO_V5_UPGRADE_SQL, V5_INSTALL_SQL,
};

pub(crate) struct TestDatabase {
    harness: PostgresHarness,
    lease: DatabaseLease,
    database_url: String,
    pub(crate) pool: PgPool,
}

#[derive(Clone, Copy)]
pub(crate) struct GatewayAccountFixture {
    pub(crate) billing_scope_id: Uuid,
    pub(crate) gateway_account_id: Uuid,
    pub(crate) gateway_configuration_id: Uuid,
}

pub(crate) fn immediate_offer(plan_key: PlanKey, charge: ChargeAmount) -> SubscriptionOffer {
    SubscriptionOffer::new(
        plan_key,
        RecurringSubscriptionTerms::new(
            charge,
            SubscriptionPeriodRule::calendar_months(1).expect("valid test cadence"),
        ),
        SubscriptionStart::RecurringImmediately,
        RenewalFailurePolicy::new(
            DunningSchedule::default(),
            DunningExhaustion::RemainPastDue,
            PastDueAccessPolicy::SuspendImmediately,
        ),
    )
    .expect("valid immediate test offer")
}

impl TestDatabase {
    pub(crate) async fn start(project: &str) -> Result<Self, Box<dyn Error>> {
        Self::start_with_install(project, V5_INSTALL_SQL).await
    }

    pub(crate) async fn start_v1(project: &str) -> Result<Self, Box<dyn Error>> {
        Self::start_with_install(project, V1_INSTALL_SQL).await
    }

    pub(crate) async fn start_v2(project: &str) -> Result<Self, Box<dyn Error>> {
        Self::start_with_install(project, V2_INSTALL_SQL).await
    }

    pub(crate) async fn start_v3(project: &str) -> Result<Self, Box<dyn Error>> {
        Self::start_with_install(project, V3_INSTALL_SQL).await
    }

    pub(crate) async fn start_v4(project: &str) -> Result<Self, Box<dyn Error>> {
        Self::start_with_install(project, V4_INSTALL_SQL).await
    }

    pub(crate) async fn start_v1_then_upgrade_to_v2(project: &str) -> Result<Self, Box<dyn Error>> {
        let database = Self::start_v1(project).await?;
        {
            let mut transaction = database.pool.begin().await?;
            sqlx::raw_sql(V1_TO_V2_UPGRADE_SQL)
                .execute(&mut *transaction)
                .await?;
            transaction.commit().await?;
        }
        Ok(database)
    }

    pub(crate) async fn start_v2_then_upgrade(project: &str) -> Result<Self, Box<dyn Error>> {
        let database = Self::start_v2(project).await?;
        database.upgrade_v2_to_v3().await?;
        Ok(database)
    }

    pub(crate) async fn start_v1_then_upgrade(project: &str) -> Result<Self, Box<dyn Error>> {
        let database = Self::start_v1_then_upgrade_to_v2(project).await?;
        database.upgrade_v2_to_v3().await?;
        Ok(database)
    }

    pub(crate) async fn upgrade_v2_to_v3(&self) -> Result<(), Box<dyn Error>> {
        let mut transaction = self.pool.begin().await?;
        sqlx::raw_sql(V2_TO_V3_UPGRADE_SQL)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(())
    }

    pub(crate) async fn upgrade_v3_to_v4(&self) -> Result<(), Box<dyn Error>> {
        self.prepare_v3_to_v4().await?;
        self.validate_v3_to_v4().await?;
        self.index_v3_to_v4().await?;
        self.finalize_v3_to_v4().await
    }

    pub(crate) async fn prepare_v3_to_v4(&self) -> Result<(), Box<dyn Error>> {
        let mut transaction = self.pool.begin().await?;
        sqlx::raw_sql(V3_TO_V4_PREPARE_SQL)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(())
    }

    pub(crate) async fn validate_v3_to_v4(&self) -> Result<(), Box<dyn Error>> {
        let mut transaction = self.pool.begin().await?;
        sqlx::raw_sql(V3_TO_V4_VALIDATE_SQL)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(())
    }

    pub(crate) async fn index_v3_to_v4(&self) -> Result<(), Box<dyn Error>> {
        // Each concurrent build must be its own top-level PostgreSQL command;
        // sending the multi-statement artifact as one query would create an
        // implicit transaction block that CREATE INDEX CONCURRENTLY rejects.
        let statements = V3_TO_V4_INDEX_SQL
            .split_terminator(';')
            .map(str::trim)
            .filter(|statement| !statement.is_empty())
            .collect::<Vec<_>>();
        assert_eq!(
            statements.len(),
            2,
            "schema-v4 index artifact must contain exactly two simple statements"
        );
        for statement in statements {
            sqlx::raw_sql(statement).execute(&self.pool).await?;
        }
        Ok(())
    }

    pub(crate) async fn finalize_v3_to_v4(&self) -> Result<(), Box<dyn Error>> {
        let mut transaction = self.pool.begin().await?;
        sqlx::raw_sql(V3_TO_V4_UPGRADE_SQL)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(())
    }

    pub(crate) async fn upgrade_v4_to_v5(&self) -> Result<(), Box<dyn Error>> {
        let mut transaction = self.pool.begin().await?;
        sqlx::raw_sql(V4_TO_V5_UPGRADE_SQL)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(())
    }

    async fn start_with_install(project: &str, install_sql: &str) -> Result<Self, Box<dyn Error>> {
        let harness =
            PostgresHarness::start(HarnessConfig::new(project)?.with_connection_budget(4)?).await?;
        let lease = harness.empty_database().await?;
        let database_url = lease.database_url().to_owned();
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&database_url)
            .await?;
        sqlx::raw_sql(install_sql).execute(&pool).await?;
        Ok(Self {
            harness,
            lease,
            database_url,
            pool,
        })
    }

    pub(crate) fn database_url(&self) -> &str {
        &self.database_url
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

pub(crate) fn explain_plan_root(plan: &serde_json::Value) -> Result<&serde_json::Value, io::Error> {
    plan.as_array()
        .and_then(|documents| documents.first())
        .and_then(|document| document.get("Plan"))
        .ok_or_else(|| io::Error::other(format!("unexpected EXPLAIN JSON shape: {plan}")))
}

pub(crate) fn plan_has_node_type(plan: &serde_json::Value, expected: &str) -> bool {
    plan.get("Node Type").and_then(serde_json::Value::as_str) == Some(expected)
        || plan
            .get("Plans")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|children| {
                children
                    .iter()
                    .any(|child| plan_has_node_type(child, expected))
            })
}

pub(crate) fn find_plan_index_node<'a>(
    plan: &'a serde_json::Value,
    expected: &str,
) -> Option<&'a serde_json::Value> {
    if plan.get("Index Name").and_then(serde_json::Value::as_str) == Some(expected) {
        return Some(plan);
    }
    plan.get("Plans")
        .and_then(serde_json::Value::as_array)
        .and_then(|children| {
            children
                .iter()
                .find_map(|child| find_plan_index_node(child, expected))
        })
}
