use sqlx::PgConnection;
use syrup_rail::{
    GatewayAccountRegistration, GatewayConfigurationActivation,
    GatewayConfigurationActivationOutcome,
};

/// Registers canonical gateway metadata on the caller's connection.
///
/// The provider row and account row are one SQL statement. An existing
/// provider is left byte-for-byte unchanged, including its active cooldown.
pub async fn register_gateway_account(
    connection: &mut PgConnection,
    registration: &GatewayAccountRegistration,
) -> Result<(), sqlx::Error> {
    let gateway_account_id = registration.gateway_account_id().into_uuid();
    let billing_scope_id = registration.billing_scope_id().into_uuid();
    let gateway_configuration_id = registration.gateway_configuration_id().into_uuid();
    sqlx::query!(
        r#"
        WITH registered_provider AS (
            INSERT INTO billing_gateway_provider_rate_limits (
                provider_key,
                rate_limited_until
            )
            VALUES ($1, '-infinity')
            ON CONFLICT (provider_key) DO NOTHING
        )
        INSERT INTO billing_gateway_accounts (
            id,
            billing_scope_id,
            provider_key,
            gateway_configuration_id
        )
        VALUES ($2, $3, $1, $4)
        "#,
        registration.provider_key().as_str(),
        gateway_account_id,
        billing_scope_id,
        gateway_configuration_id,
    )
    .execute(connection)
    .await?;
    Ok(())
}

/// Activates an exact gateway configuration without touching cooldown state.
///
/// The scope row is locked before its full account/provider/configuration
/// identity is revalidated, so a stale caller receives a deterministic closed
/// outcome without mutating another account.
pub async fn activate_gateway_configuration(
    connection: &mut PgConnection,
    activation: &GatewayConfigurationActivation,
) -> Result<GatewayConfigurationActivationOutcome, sqlx::Error> {
    let billing_scope_id = activation.billing_scope_id().into_uuid();
    let gateway_account_id = activation.gateway_account_id().into_uuid();
    let expected_configuration_id = activation.expected_configuration_id().into_uuid();
    let new_configuration_id = activation.new_configuration_id().into_uuid();
    let result = sqlx::query!(
        r#"
        WITH candidate AS MATERIALIZED (
            SELECT id, provider_key, gateway_configuration_id
            FROM billing_gateway_accounts
            WHERE billing_scope_id = $1
            FOR UPDATE
        ),
        activated AS (
            UPDATE billing_gateway_accounts AS accounts
            SET gateway_configuration_id = $5,
                updated_at = clock_timestamp()
            FROM candidate
            WHERE accounts.billing_scope_id = $1
                AND accounts.id = candidate.id
                AND candidate.id = $2
                AND candidate.provider_key = $3
                AND candidate.gateway_configuration_id = $4
            RETURNING accounts.id
        )
        SELECT
            EXISTS (SELECT 1 FROM candidate) AS "account_exists!",
            EXISTS (SELECT 1 FROM activated) AS "activated!"
        "#,
        billing_scope_id,
        gateway_account_id,
        activation.provider_key().as_str(),
        expected_configuration_id,
        new_configuration_id,
    )
    .fetch_one(connection)
    .await?;

    Ok(if result.activated {
        GatewayConfigurationActivationOutcome::Activated
    } else if result.account_exists {
        GatewayConfigurationActivationOutcome::IdentityChanged
    } else {
        GatewayConfigurationActivationOutcome::AccountNotFound
    })
}

/// Loads both durable cooldowns from one snapshot; a missing row fails closed
/// at the calling operation's boundary.
pub(crate) async fn load_gateway_cooldown(
    pool: &sqlx::PgPool,
    account_id: syrup_rail::GatewayAccountId,
    provider_key: &syrup_rail::GatewayProviderKey,
) -> Result<Option<(bool, bool)>, sqlx::Error> {
    sqlx::query_as(
        r#"
        SELECT
            COALESCE(accounts.mutation_rate_limited_until > clock_timestamp(), false),
            provider.rate_limited_until > clock_timestamp()
        FROM billing_gateway_accounts AS accounts
        INNER JOIN billing_gateway_provider_rate_limits AS provider
            ON provider.provider_key = accounts.provider_key
        WHERE accounts.id = $1 AND accounts.provider_key = $2
        "#,
    )
    .bind(account_id.as_uuid())
    .bind(provider_key.as_str())
    .fetch_optional(pool)
    .await
}

#[cfg(test)]
mod tests {
    use std::{error::Error, io};

    use syrup_rail::{
        BillingScopeId, GatewayAccountId, GatewayAccountRegistration,
        GatewayConfigurationActivation, GatewayConfigurationActivationOutcome,
        GatewayConfigurationId, GatewayProviderKey,
    };
    use uuid::Uuid;

    use super::{activate_gateway_configuration, register_gateway_account};
    use crate::test_support::TestDatabase;

    #[tokio::test]
    async fn gateway_account_metadata_operations_preserve_identity_and_cooldowns()
    -> Result<(), Box<dyn Error>> {
        let database = TestDatabase::start("sr_accounts_v1").await?;
        let result = async {
            let provider = GatewayProviderKey::new("test_gateway")?;
            let scope = BillingScopeId::new(Uuid::now_v7());
            let account = GatewayAccountId::new(Uuid::now_v7());
            let initial_configuration = GatewayConfigurationId::new(Uuid::now_v7());
            let registration = GatewayAccountRegistration::new(
                scope,
                account,
                provider.clone(),
                initial_configuration,
            );

            let mut rolled_back = database.pool.begin().await?;
            register_gateway_account(&mut rolled_back, &registration).await?;
            rolled_back.rollback().await?;
            let rolled_back_rows: i64 =
                sqlx::query_scalar("SELECT count(*) FROM billing_gateway_accounts WHERE id = $1")
                    .bind(account.as_uuid())
                    .fetch_one(&database.pool)
                    .await?;
            if rolled_back_rows != 0 {
                return Err(io::Error::other("registration escaped caller rollback").into());
            }

            let mut transaction = database.pool.begin().await?;
            register_gateway_account(&mut transaction, &registration).await?;
            transaction.commit().await?;

            let provider_deadline = "2035-02-03T04:05:06Z";
            let account_deadline = "2035-03-04T05:06:07Z";
            sqlx::query(
                r#"
                UPDATE billing_gateway_provider_rate_limits
                SET rate_limited_until = $2::timestamptz
                WHERE provider_key = $1
                "#,
            )
            .bind(provider.as_str())
            .bind(provider_deadline)
            .execute(&database.pool)
            .await?;
            sqlx::query(
                r#"
                UPDATE billing_gateway_accounts
                SET mutation_rate_limited_until = $2::timestamptz
                WHERE id = $1
                "#,
            )
            .bind(account.as_uuid())
            .bind(account_deadline)
            .execute(&database.pool)
            .await?;

            let sibling_registration = GatewayAccountRegistration::new(
                BillingScopeId::new(Uuid::now_v7()),
                GatewayAccountId::new(Uuid::now_v7()),
                provider.clone(),
                GatewayConfigurationId::new(Uuid::now_v7()),
            );
            let mut sibling_transaction = database.pool.begin().await?;
            register_gateway_account(&mut sibling_transaction, &sibling_registration).await?;
            sibling_transaction.commit().await?;

            let retained_provider_deadline: String = sqlx::query_scalar(
                r#"
                SELECT rate_limited_until::text
                FROM billing_gateway_provider_rate_limits
                WHERE provider_key = $1
                "#,
            )
            .bind(provider.as_str())
            .fetch_one(&database.pool)
            .await?;
            if !retained_provider_deadline.starts_with("2035-02-03 04:05:06") {
                return Err(io::Error::other(
                    "repeated provider registration changed its cooldown",
                )
                .into());
            }

            let other_provider = GatewayProviderKey::new("other_gateway")?;
            let other_registration = GatewayAccountRegistration::new(
                BillingScopeId::new(Uuid::now_v7()),
                GatewayAccountId::new(Uuid::now_v7()),
                other_provider.clone(),
                GatewayConfigurationId::new(Uuid::now_v7()),
            );
            let mut other_transaction = database.pool.begin().await?;
            register_gateway_account(&mut other_transaction, &other_registration).await?;
            other_transaction.commit().await?;
            let other_provider_is_open: bool = sqlx::query_scalar(
                r#"
                SELECT rate_limited_until = '-infinity'::timestamptz
                FROM billing_gateway_provider_rate_limits
                WHERE provider_key = $1
                "#,
            )
            .bind(other_provider.as_str())
            .fetch_one(&database.pool)
            .await?;
            if !other_provider_is_open {
                return Err(io::Error::other(
                    "one provider's registration or cooldown changed another provider",
                )
                .into());
            }

            let next_configuration = GatewayConfigurationId::new(Uuid::now_v7());
            let activation = GatewayConfigurationActivation::new(
                scope,
                account,
                provider.clone(),
                initial_configuration,
                next_configuration,
            );
            let mut activation_transaction = database.pool.begin().await?;
            let outcome =
                activate_gateway_configuration(&mut activation_transaction, &activation).await?;
            activation_transaction.commit().await?;
            if outcome != GatewayConfigurationActivationOutcome::Activated {
                return Err(io::Error::other("exact configuration was not activated").into());
            }

            let stored = sqlx::query_as::<_, (Uuid, String)>(
                r#"
                SELECT gateway_configuration_id, mutation_rate_limited_until::text
                FROM billing_gateway_accounts
                WHERE id = $1
                "#,
            )
            .bind(account.as_uuid())
            .fetch_one(&database.pool)
            .await?;
            if stored.0 != next_configuration.into_uuid()
                || !stored.1.starts_with("2035-03-04 05:06:07")
            {
                return Err(io::Error::other(
                    "configuration activation changed identity or account cooldown",
                )
                .into());
            }

            let stale = GatewayConfigurationActivation::new(
                scope,
                account,
                provider.clone(),
                initial_configuration,
                GatewayConfigurationId::new(Uuid::now_v7()),
            );
            let wrong_provider = GatewayConfigurationActivation::new(
                scope,
                account,
                GatewayProviderKey::new("another_gateway")?,
                next_configuration,
                GatewayConfigurationId::new(Uuid::now_v7()),
            );
            let wrong_account = GatewayConfigurationActivation::new(
                scope,
                GatewayAccountId::new(Uuid::now_v7()),
                provider.clone(),
                next_configuration,
                GatewayConfigurationId::new(Uuid::now_v7()),
            );
            for mismatch in [&stale, &wrong_provider, &wrong_account] {
                let mut mismatch_transaction = database.pool.begin().await?;
                let outcome =
                    activate_gateway_configuration(&mut mismatch_transaction, mismatch).await?;
                mismatch_transaction.rollback().await?;
                if outcome != GatewayConfigurationActivationOutcome::IdentityChanged {
                    return Err(
                        io::Error::other("stale gateway identity did not fail closed").into(),
                    );
                }
            }

            let missing = GatewayConfigurationActivation::new(
                BillingScopeId::new(Uuid::now_v7()),
                GatewayAccountId::new(Uuid::now_v7()),
                provider,
                next_configuration,
                GatewayConfigurationId::new(Uuid::now_v7()),
            );
            let mut missing_transaction = database.pool.begin().await?;
            let missing_outcome =
                activate_gateway_configuration(&mut missing_transaction, &missing).await?;
            missing_transaction.rollback().await?;
            if missing_outcome != GatewayConfigurationActivationOutcome::AccountNotFound {
                return Err(io::Error::other("missing account was not distinguished").into());
            }

            Ok::<_, Box<dyn Error>>(())
        }
        .await;
        let cleanup = database.cleanup().await;
        result?;
        cleanup
    }
}
