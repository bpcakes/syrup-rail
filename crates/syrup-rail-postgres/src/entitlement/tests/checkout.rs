use super::*;

#[tokio::test]
async fn dropping_completed_host_work_rolls_back_and_reuses_the_checkout()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::start("sr_guard_reuse").await?;
    let result = async {
        let scope = Uuid::now_v7();
        let subscriber = Uuid::now_v7();
        insert_grant(&database.pool, scope, subscriber).await?;
        create_host_guard_probe_table(&database.pool).await?;
        let mut held = Vec::new();
        for _ in 1..database.pool.options().get_max_connections() {
            held.push(database.pool.acquire().await?);
        }
        for admitted in [false, true] {
            let mut tx = EntitlementWriteTransaction::begin(&database.pool).await?;
            let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                .fetch_one(tx.connection())
                .await?;
            let id = Uuid::now_v7();
            sqlx::query("INSERT INTO host_entitlement_guard_probes(id) VALUES ($1)")
                .bind(id)
                .execute(tx.connection())
                .await?;
            if admitted {
                drop(require_entitlement_for_update(tx, &guard(scope, subscriber)?).await?);
            } else {
                drop(tx);
            }
            let current: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                .fetch_one(&database.pool)
                .await?;
            assert_eq!(current, pid);
            assert_host_guard_probe_absent(&database.pool, id).await?;
        }
        let mut tx = EntitlementWriteTransaction::begin(&database.pool).await?;
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(tx.connection())
            .await?;
        let admission = guard(scope, subscriber)?;
        drop(require_entitlement_for_update(tx, &admission));
        let replacement: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&database.pool)
            .await?;
        assert_ne!(
            replacement, pid,
            "handing off ownership also covers an unpolled future"
        );
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result?;
    cleanup
}
