//! Independent, fill-only saved-card display repair. Never applies payment evidence.

use std::{fmt, time::Duration};

use sqlx::PgPool;
use syrup_rail::{
    BillingScopeId, GatewayAccountId, GatewayConfigurationId, GatewayError,
    GatewayPaymentDiagnostic, GatewayPaymentStatus, GatewayProviderKey, GatewayQueryRequest,
    GatewayResolutionError, GatewayResolver, GatewayTransactionId, PaymentAttemptId, SubscriberId,
};
use thiserror::Error;

use self::storage::{fill_missing_fields, load_candidate, lock_candidate};
use crate::GatewayMutationCooldownScope;

mod storage;

/// An authorized subscriber's latest approved attempt for a current saved method.
///
/// Hosts supply canonical attempt identity after approval commits, or from retained
/// history for explicit repair. Provider identifiers are loaded from the ledger.
#[derive(Clone, Copy, Debug)]
pub struct RefreshPaymentMethodMetadata {
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    attempt_id: PaymentAttemptId,
}

impl RefreshPaymentMethodMetadata {
    /// Selects one attempt; the host must authorize this scope and subscriber.
    pub const fn new(
        billing_scope_id: BillingScopeId,
        subscriber_id: SubscriberId,
        attempt_id: PaymentAttemptId,
    ) -> Self {
        Self {
            billing_scope_id,
            subscriber_id,
            attempt_id,
        }
    }
}

/// A display refresh has no payment, entitlement, or renewal authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PaymentMethodMetadataRefreshOutcome {
    /// At least one absent card display field was filled.
    Updated,
    /// Display was already complete, or a matching observation supplied no
    /// additional safe fields. This does not guarantee display completeness.
    Unchanged,
    /// No current eligible method matched before any provider I/O.
    Ineligible,
    /// An initially eligible candidate changed during the query. Re-read current
    /// state before a bounded retry; the original attempt may now be ineligible.
    ChangedDuringQuery,
    /// The provider observation contradicted the approval, carried unacceptable
    /// diagnostics, or conflicted with identity or existing display.
    EvidenceRejected,
    /// The exact query returned no transaction. This does not establish display
    /// completeness or payment failure; the host may schedule a bounded retry.
    NotFound,
}

/// Refresh failures leave approval intact. Retry only this bounded refresh operation.
#[derive(Error)]
#[non_exhaustive]
pub enum PaymentMethodMetadataRefreshError {
    /// Canonical storage could not be read or the display transaction failed.
    #[error("payment method metadata storage failed")]
    Storage(#[from] sqlx::Error),
    /// The host could not resolve the canonical provider account.
    #[error("payment method metadata gateway resolution failed")]
    Resolution(#[from] GatewayResolutionError),
    /// The resolved gateway did not match the canonical account identity.
    #[error("payment method metadata gateway identity mismatch")]
    GatewayIdentityMismatch,
    /// An existing account or provider cooldown stopped the query before I/O.
    #[error("payment method metadata gateway cooldown is active")]
    CooldownActive {
        /// The durable cooldown that currently takes precedence.
        scope: GatewayMutationCooldownScope,
    },
    /// The read-only provider query failed; no financial command should be retried.
    #[error("payment method metadata query failed")]
    Query(#[from] GatewayError),
    /// The provider throttled the query and recording its cooldown failed.
    /// Back off as for a rate-limited query even if the storage error is retryable.
    #[error("payment method metadata query throttled and cooldown persistence failed")]
    RateLimitCooldownPersistenceFailed {
        /// Original provider rate-limit evidence.
        query: GatewayError,
        /// Failure to make that cooldown durable.
        #[source]
        storage: sqlx::Error,
    },
    /// The single provider query exceeded the fixed time budget.
    #[error("payment method metadata query timed out")]
    QueryTimedOut,
    /// Canonical identity could not be safely reconstructed.
    #[error("payment method metadata identity is invalid")]
    InvalidIdentity,
}

impl fmt::Debug for PaymentMethodMetadataRefreshError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Storage(_) => "PaymentMethodMetadataRefreshError::Storage",
            Self::Resolution(_) => "PaymentMethodMetadataRefreshError::Resolution",
            Self::GatewayIdentityMismatch => {
                "PaymentMethodMetadataRefreshError::GatewayIdentityMismatch"
            }
            Self::CooldownActive { .. } => "PaymentMethodMetadataRefreshError::CooldownActive",
            Self::Query(_) => "PaymentMethodMetadataRefreshError::Query",
            Self::RateLimitCooldownPersistenceFailed { .. } => {
                "PaymentMethodMetadataRefreshError::RateLimitCooldownPersistenceFailed"
            }
            Self::QueryTimedOut => "PaymentMethodMetadataRefreshError::QueryTimedOut",
            Self::InvalidIdentity => "PaymentMethodMetadataRefreshError::InvalidIdentity",
        })
    }
}

/// Fills missing display for one current approved saved method using at most one query.
///
/// Call after approval commits, or separately to repair an existing approval. This
/// operation never submits a sale, reapplies approval, or writes charge/attempt
/// evidence. The host authorizes the subject and bounds scheduling/concurrency.
/// Existing account/provider cooldowns stop the query before gateway resolution.
/// Query throttling extends the shared provider cooldown using the existing
/// durable policy; financial evidence remains unchanged. This does not provide
/// end-user admission or a concurrency limiter, which remain host responsibilities.
/// Provider I/O has a 10-second timeout and runs before the write transaction.
/// Missing/conflicting evidence cannot erase display. Replacement, scrubbing, or
/// any intervening change to the candidate causes a no-op; retry with the latest
/// approved attempt if appropriate. Errors are independent of payment success.
/// A missing vault reference (including its missing-reference diagnostic) is
/// allowed because the approved exact transaction has durable method linkage.
/// Every other diagnostic and any conflicting returned reference is rejected.
/// The durable approval is authoritative: an exact, diagnostic-free `Unknown`
/// observation after a lifecycle change may supply display, while an explicit
/// decline or failure contradicts the approval and is rejected.
pub async fn refresh_payment_method_metadata(
    pool: &PgPool,
    resolver: &dyn GatewayResolver,
    command: RefreshPaymentMethodMetadata,
) -> Result<PaymentMethodMetadataRefreshOutcome, PaymentMethodMetadataRefreshError> {
    use PaymentMethodMetadataRefreshOutcome as Outcome;

    let Some(candidate) = load_candidate(pool, command).await? else {
        return Ok(Outcome::Ineligible);
    };
    if candidate.complete() {
        return Ok(Outcome::Unchanged);
    }
    let account_id = GatewayAccountId::new(candidate.account_id);
    let configuration_id = GatewayConfigurationId::new(candidate.configuration_id);
    let provider = GatewayProviderKey::new(&candidate.provider_key)
        .map_err(|_| PaymentMethodMetadataRefreshError::InvalidIdentity)?;
    let transaction_id = GatewayTransactionId::new(candidate.transaction_id.clone())
        .map_err(|_| PaymentMethodMetadataRefreshError::InvalidIdentity)?;
    let cooldown = crate::gateway_accounts::load_gateway_cooldown(pool, account_id, &provider)
        .await?
        .ok_or(sqlx::Error::RowNotFound)?;
    if let Some(scope) = GatewayMutationCooldownScope::from_active_flags(cooldown.0, cooldown.1) {
        return Err(PaymentMethodMetadataRefreshError::CooldownActive { scope });
    }
    let gateway = resolver
        .resolve(
            command.billing_scope_id,
            account_id,
            configuration_id,
            provider.clone(),
        )
        .await?;
    if gateway.billing_scope_id() != command.billing_scope_id
        || gateway.gateway_account_id() != account_id
        || gateway.gateway_configuration_id() != configuration_id
        || gateway.provider_key() != &provider
    {
        return Err(PaymentMethodMetadataRefreshError::GatewayIdentityMismatch);
    }
    let request = GatewayQueryRequest::new(Some(transaction_id.clone()), None)
        .map_err(|_| PaymentMethodMetadataRefreshError::InvalidIdentity)?;
    let observation =
        match tokio::time::timeout(Duration::from_secs(10), gateway.query_transaction(request))
            .await
            .map_err(|_| PaymentMethodMetadataRefreshError::QueryTimedOut)?
        {
            Ok(observation) => observation,
            Err(error) => {
                if matches!(&error, GatewayError::RateLimited(_))
                    && let Err(storage) =
                        storage::record_provider_cooldown(pool, command, account_id, &provider)
                            .await
                {
                    return Err(
                        PaymentMethodMetadataRefreshError::RateLimitCooldownPersistenceFailed {
                            query: error,
                            storage,
                        },
                    );
                }
                return Err(error.into());
            }
        };
    let Some(observation) = observation else {
        return Ok(Outcome::NotFound);
    };
    // An exact query may reflect a later lifecycle state. The durable attempt
    // supplies approval authority; a diagnostic-free Unknown observation can
    // still supply display. Explicit declines/failures contradict that approval.
    if !matches!(
        observation.status(),
        GatewayPaymentStatus::Approved | GatewayPaymentStatus::Unknown
    ) || observation
        .diagnostics()
        .iter()
        .any(|diagnostic| *diagnostic != GatewayPaymentDiagnostic::MissingPaymentMethodReference)
        || observation.transaction_id() != Some(&transaction_id)
        || observation
            .payment_method_reference()
            .is_some_and(|reference| reference.expose() != candidate.reference)
    {
        return Ok(Outcome::EvidenceRejected);
    }
    let mut transaction = pool.begin().await?;
    // SQLx owns rollback on error/cancellation. Cleanup cannot replace the
    // original storage error with a second rollback error.
    let Some(current) = lock_candidate(&mut transaction, command, &candidate).await? else {
        return Ok(Outcome::ChangedDuringQuery);
    };
    if current != candidate {
        return Ok(Outcome::ChangedDuringQuery);
    }
    let outcome = fill_missing_fields(&mut transaction, &current, observation.descriptor()).await?;
    transaction.commit().await?;
    Ok(outcome)
}
