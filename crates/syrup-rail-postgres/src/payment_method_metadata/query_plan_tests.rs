use std::error::Error;

use serde_json::Value;
use sqlx::Row;
use uuid::Uuid;

use super::{CANDIDATE_SQL, LOCKED_CANDIDATE_SQL};
use crate::test_support::{TestDatabase, explain_plan_root};

#[tokio::test]
async fn card_metadata_candidate_plans_probe_method_linked_subscriptions()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("metadata_plan").await?;
    // One merchant with many unrelated subscribers: the current-reference
    // check must not spend work on that merchant's entire population.
    sqlx::raw_sql(include_str!("query_plan_fixture.sql"))
        .execute(&database.pool)
        .await?;
    for statement in [CANDIDATE_SQL, LOCKED_CANDIDATE_SQL] {
        let explain = format!("EXPLAIN (GENERIC_PLAN TRUE, FORMAT JSON) {statement}");
        let row = sqlx::raw_sql(&explain).fetch_one(&database.pool).await?;
        let plan: Value = row.try_get(0)?;
        let root = explain_plan_root(&plan)?;
        let rendered = serde_json::to_string_pretty(root)?;
        let subscription = find_alias(root, "current_subscription")
            .expect("production eligibility checks a current subscription");
        let condition = subscription["Index Cond"].as_str().unwrap_or_default();
        assert!(
            condition.contains("id = current_approval.subscription_id"),
            "current references must use subscription-ID probes, not account scans:\n{rendered}"
        );
        let approval = find_alias(root, "current_approval")
            .expect("current references are reached through method provenance");
        assert_eq!(
            approval["Index Name"].as_str(),
            Some("billing_payment_attempts_payment_method_idx"),
            "current reference provenance must use the method index:\n{rendered}"
        );
        assert!(
            approval["Index Cond"]
                .as_str()
                .is_some_and(|condition| condition.contains("payment_method_id = m.id")),
            "method identity must constrain the index lookup:\n{rendered}"
        );
        for transaction in ["txn_1", "txn_2", "txn_3", "txn_4"] {
            let (scope, subscriber, attempt, subscription): (Uuid, Uuid, Uuid, Uuid) =
                sqlx::query_as(
                    "SELECT billing_scope_id, subscriber_id, id, subscription_id \
                     FROM billing_payment_attempts WHERE gateway_transaction_id = $1",
                )
                .bind(transaction)
                .fetch_one(&database.pool)
                .await?;
            let candidate = sqlx::query(statement)
                .bind(scope)
                .bind(subscriber)
                .bind(attempt)
                .fetch_one(&database.pool)
                .await?;
            assert_eq!(
                candidate.try_get::<Uuid, _>("subscription_id")?,
                subscription
            );
        }
    }
    database.cleanup().await
}

fn find_alias<'a>(node: &'a Value, alias: &str) -> Option<&'a Value> {
    if node["Alias"].as_str() == Some(alias) {
        return Some(node);
    }
    node["Plans"]
        .as_array()?
        .iter()
        .find_map(|child| find_alias(child, alias))
}
