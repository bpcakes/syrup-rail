//! Read-only processor and verification diagnostics for historical
//! subscription payment attempts. Never writes payment, contact, or
//! subscription state and never submits a provider mutation.

use std::{collections::HashSet, fmt, time::Duration};

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use syrup_rail::{
    BillingScopeId, CurrencyCode, GatewayAccountId, GatewayConfigurationId,
    GatewayDiagnosticOperation, GatewayDiagnosticsCompleteness, GatewayDiagnosticsSource,
    GatewayDiagnosticsUnavailableReason, GatewayError, GatewayOrderId, GatewayProviderKey,
    GatewayResolutionError, GatewayResolver, GatewayTransactionDiagnostics,
    GatewayTransactionDiagnosticsObservation, GatewayTransactionDiagnosticsRequest,
    GatewayTransactionId, Money, PaymentAttemptId, PaymentAttemptKind,
    SUBSCRIPTION_PAYMENT_HISTORY_PAGE_LIMIT, SubscriberId,
};
use thiserror::Error;
use tokio::time::Instant;

use crate::GatewayMutationCooldownScope;

mod storage;

/// Upper bound for the single provider query, as for saved-card repair.
const DIAGNOSTIC_QUERY_TIMEOUT: Duration = Duration::from_secs(10);
/// Time kept back from the caller's deadline for post-query revalidation and
/// provider-cooldown bookkeeping, which are never cancelled mid-write.
const DIAGNOSTIC_POST_QUERY_RESERVE: Duration = Duration::from_secs(3);
/// A provider query with less time than this is not started.
const MIN_DIAGNOSTIC_QUERY_BUDGET: Duration = Duration::from_secs(1);
/// Longer caller deadlines add nothing: the query and database work are
/// independently bounded. Clamping keeps deadline arithmetic from overflowing.
const MAX_DIAGNOSTIC_DEADLINE: Duration = Duration::from_secs(3_600);

/// One owned subscription payment attempt to diagnose.
///
/// The host must authorize the scope, subscriber, and plan before calling a
/// diagnostic function.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaymentAttemptDiagnosticTarget {
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    plan_key: syrup_rail::PlanKey,
    attempt_id: PaymentAttemptId,
}

impl PaymentAttemptDiagnosticTarget {
    pub const fn new(
        billing_scope_id: BillingScopeId,
        subscriber_id: SubscriberId,
        plan_key: syrup_rail::PlanKey,
        attempt_id: PaymentAttemptId,
    ) -> Self {
        Self {
            billing_scope_id,
            subscriber_id,
            plan_key,
            attempt_id,
        }
    }

    pub const fn billing_scope_id(&self) -> BillingScopeId {
        self.billing_scope_id
    }

    pub const fn subscriber_id(&self) -> SubscriberId {
        self.subscriber_id
    }

    pub const fn plan_key(&self) -> &syrup_rail::PlanKey {
        &self.plan_key
    }

    pub const fn attempt_id(&self) -> PaymentAttemptId {
        self.attempt_id
    }
}

/// Why an owned attempt cannot be diagnosed. Checked in declaration order;
/// the first matching reason is reported.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum PaymentAttemptDiagnosticIneligibility {
    /// Host-charge attempts have no plan and are not diagnosable here.
    UnsupportedKind,
    /// The attempt was never submitted to the provider.
    NotSubmitted,
    /// The attempt is still pending its provider result.
    Pending,
    /// No provider transaction ID was recorded.
    NoTransactionId,
}

impl PaymentAttemptDiagnosticIneligibility {
    /// Returns the stable persisted spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnsupportedKind => "unsupported_kind",
            Self::NotSubmitted => "not_submitted",
            Self::Pending => "pending",
            Self::NoTransactionId => "no_transaction_id",
        }
    }
}

/// Whether an owned attempt can be diagnosed.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PaymentAttemptDiagnosticEligibility {
    Eligible,
    Ineligible(PaymentAttemptDiagnosticIneligibility),
}

impl PaymentAttemptDiagnosticEligibility {
    /// Returns the stable persisted spelling: `eligible` or the reason.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Eligible => "eligible",
            Self::Ineligible(reason) => reason.as_str(),
        }
    }

    fn classify(kind: &str, submitted: bool, status: &str, has_transaction_id: bool) -> Self {
        use PaymentAttemptDiagnosticIneligibility as Reason;
        let reason = if kind == PaymentAttemptKind::HostCharge.as_str() {
            Reason::UnsupportedKind
        } else if !submitted {
            Reason::NotSubmitted
        } else if status == "pending" {
            Reason::Pending
        } else if !has_transaction_id {
            Reason::NoTransactionId
        } else {
            return Self::Eligible;
        };
        Self::Ineligible(reason)
    }
}

/// Eligibility of one owned attempt, without any provider identifier.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PaymentAttemptDiagnosticEligibilityItem {
    attempt_id: PaymentAttemptId,
    eligibility: PaymentAttemptDiagnosticEligibility,
}

impl PaymentAttemptDiagnosticEligibilityItem {
    pub const fn attempt_id(self) -> PaymentAttemptId {
        self.attempt_id
    }

    pub const fn eligibility(self) -> PaymentAttemptDiagnosticEligibility {
        self.eligibility
    }
}

/// Processor and verification results for one owned attempt's original
/// provider action.
///
/// Ordinary formatting is value-free. Provider values are sanitized
/// [`syrup_rail::GatewayDiagnostic`]s; read them with `expose` only at a
/// protected operator boundary. Absent values never mean match, mismatch, a
/// disabled check, or a financial decision.
#[derive(Clone, Eq, PartialEq)]
pub struct PaymentAttemptDiagnostics {
    target: PaymentAttemptDiagnosticTarget,
    attempt_kind: PaymentAttemptKind,
    gateway_account_id: GatewayAccountId,
    gateway_configuration_id: GatewayConfigurationId,
    provider_key: GatewayProviderKey,
    observation: GatewayTransactionDiagnosticsObservation,
    observed_at: DateTime<Utc>,
}

impl PaymentAttemptDiagnostics {
    pub const fn target(&self) -> &PaymentAttemptDiagnosticTarget {
        &self.target
    }

    pub const fn attempt_kind(&self) -> PaymentAttemptKind {
        self.attempt_kind
    }

    pub const fn gateway_account_id(&self) -> GatewayAccountId {
        self.gateway_account_id
    }

    /// The canonical configuration actually queried, which may be newer than
    /// the configuration recorded on the attempt.
    pub const fn gateway_configuration_id(&self) -> GatewayConfigurationId {
        self.gateway_configuration_id
    }

    pub const fn provider_key(&self) -> &GatewayProviderKey {
        &self.provider_key
    }

    /// The original provider action that was selected.
    pub const fn action_type(&self) -> GatewayDiagnosticOperation {
        self.observation.operation()
    }

    pub const fn gateway_response_code(&self) -> Option<&syrup_rail::GatewayDiagnostic> {
        self.observation.gateway_response_code()
    }

    pub const fn gateway_response_text(&self) -> Option<&syrup_rail::GatewayDiagnostic> {
        self.observation.gateway_response_text()
    }

    pub const fn processor_response_code(&self) -> Option<&syrup_rail::GatewayDiagnostic> {
        self.observation.processor_response_code()
    }

    pub const fn processor_response_text(&self) -> Option<&syrup_rail::GatewayDiagnostic> {
        self.observation.processor_response_text()
    }

    pub const fn avs_response(&self) -> Option<&syrup_rail::GatewayDiagnostic> {
        self.observation.avs_response()
    }

    pub const fn csc_response(&self) -> Option<&syrup_rail::GatewayDiagnostic> {
        self.observation.csc_response()
    }

    pub const fn source(&self) -> &GatewayDiagnosticsSource {
        self.observation.source()
    }

    pub const fn completeness(&self) -> GatewayDiagnosticsCompleteness {
        self.observation.completeness()
    }

    /// Database time observed after the provider query completed.
    pub const fn observed_at(&self) -> DateTime<Utc> {
        self.observed_at
    }
}

impl fmt::Debug for PaymentAttemptDiagnostics {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PaymentAttemptDiagnostics")
            .field("target", &self.target)
            .field("attempt_kind", &self.attempt_kind)
            .field("gateway_account_id", &self.gateway_account_id)
            .field("gateway_configuration_id", &self.gateway_configuration_id)
            .field("provider_key", &self.provider_key)
            .field("observation", &self.observation)
            .field("observed_at", &self.observed_at)
            .finish()
    }
}

/// The typed result of one diagnostic query. Every value has a stable
/// [`Self::as_str`] spelling that hosts may persist.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PaymentAttemptDiagnosticsOutcome {
    /// Exactly one original provider action was observed.
    Observed(Box<PaymentAttemptDiagnostics>),
    /// The owned attempt is not diagnosable; no provider I/O occurred.
    Ineligible(PaymentAttemptDiagnosticIneligibility),
    /// No attempt with this ID is owned by the target scope, subscriber, and
    /// plan; no provider I/O occurred.
    NotFound,
    /// The provider returned no transaction for the recorded transaction ID.
    ProviderTransactionNotFound,
    /// The resolved provider adapter does not implement diagnostics.
    Unsupported,
    /// A durable account or provider cooldown was active; no provider I/O
    /// occurred.
    CooldownActive(GatewayMutationCooldownScope),
    /// The provider rate-limited the query and the shared provider cooldown
    /// was extended.
    RateLimited,
    /// The caller's deadline left no safe time for the query, or the query
    /// did not finish within its budget.
    TimedOut,
    /// The account's canonical configuration or provider changed before or
    /// during the query.
    ConfigurationChanged,
    /// The attempt's transaction ID, status, or ownership changed during the
    /// query, for example through late-approval review.
    TargetChanged,
    /// The provider response could not be bound safely, or the provider
    /// query failed for a reason other than rate limiting.
    Unavailable(GatewayDiagnosticsUnavailableReason),
}

impl PaymentAttemptDiagnosticsOutcome {
    /// Returns the stable persisted spelling of the outcome kind.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Observed(_) => "observed",
            Self::Ineligible(_) => "ineligible",
            Self::NotFound => "not_found",
            Self::ProviderTransactionNotFound => "provider_transaction_not_found",
            Self::Unsupported => "unsupported",
            Self::CooldownActive(_) => "cooldown_active",
            Self::RateLimited => "rate_limited",
            Self::TimedOut => "timed_out",
            Self::ConfigurationChanged => "configuration_changed",
            Self::TargetChanged => "target_changed",
            Self::Unavailable(_) => "unavailable",
        }
    }
}

/// Failures that are not diagnostic outcomes. None changes payment state.
#[derive(Error)]
#[non_exhaustive]
pub enum PaymentAttemptDiagnosticsError {
    /// Canonical storage could not be read, or a cooldown could not be read.
    #[error("payment attempt diagnostics storage failed")]
    Storage(#[from] sqlx::Error),
    /// The host could not resolve the canonical provider account.
    #[error("payment attempt diagnostics gateway resolution failed")]
    Resolution(GatewayResolutionError),
    /// The resolved gateway did not match the canonical account identity.
    #[error("payment attempt diagnostics gateway identity mismatch")]
    GatewayIdentityMismatch,
    /// The provider throttled the query and recording its cooldown failed.
    /// Back off as for a rate-limited query even if the storage error is
    /// retryable.
    #[error("payment attempt diagnostics query throttled and cooldown persistence failed")]
    RateLimitCooldownPersistenceFailed {
        /// Original provider rate-limit evidence.
        query: GatewayError,
        /// Failure to make that cooldown durable.
        #[source]
        storage: sqlx::Error,
    },
    /// Stored attempt identity could not be safely reconstructed.
    #[error("payment attempt diagnostics identity is invalid")]
    InvalidIdentity,
    /// More attempt IDs were requested than one payment-history page holds.
    #[error("too many payment attempts requested for diagnostic eligibility")]
    TooManyAttempts,
}

impl fmt::Debug for PaymentAttemptDiagnosticsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Storage(_) => "PaymentAttemptDiagnosticsError::Storage",
            Self::Resolution(_) => "PaymentAttemptDiagnosticsError::Resolution",
            Self::GatewayIdentityMismatch => {
                "PaymentAttemptDiagnosticsError::GatewayIdentityMismatch"
            }
            Self::RateLimitCooldownPersistenceFailed { .. } => {
                "PaymentAttemptDiagnosticsError::RateLimitCooldownPersistenceFailed"
            }
            Self::InvalidIdentity => "PaymentAttemptDiagnosticsError::InvalidIdentity",
            Self::TooManyAttempts => "PaymentAttemptDiagnosticsError::TooManyAttempts",
        })
    }
}

/// Reports which owned attempts can be diagnosed, without provider I/O.
///
/// Accepts at most [`SUBSCRIPTION_PAYMENT_HISTORY_PAGE_LIMIT`] attempt IDs,
/// typically one payment-history page, and returns one item per distinct owned
/// attempt in request order. IDs that the scope, subscriber, and plan do not
/// own are omitted. A host-charge attempt of the subscriber is reported as
/// [`PaymentAttemptDiagnosticIneligibility::UnsupportedKind`]. The result never
/// contains a provider transaction ID. This read does not consult scrub state:
/// the host must not offer diagnostics for a subscriber that is scrubbed or
/// being scrubbed. The host authorizes the subject before calling.
pub async fn payment_attempt_diagnostic_eligibility(
    pool: &PgPool,
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    plan_key: &syrup_rail::PlanKey,
    attempt_ids: &[PaymentAttemptId],
) -> Result<Vec<PaymentAttemptDiagnosticEligibilityItem>, PaymentAttemptDiagnosticsError> {
    if attempt_ids.len() > SUBSCRIPTION_PAYMENT_HISTORY_PAGE_LIMIT as usize {
        return Err(PaymentAttemptDiagnosticsError::TooManyAttempts);
    }
    let mut seen = HashSet::with_capacity(attempt_ids.len());
    let distinct = attempt_ids
        .iter()
        .filter(|attempt_id| seen.insert(**attempt_id))
        .map(|attempt_id| attempt_id.into_uuid())
        .collect::<Vec<_>>();
    let rows =
        storage::load_eligibility(pool, billing_scope_id, subscriber_id, plan_key, &distinct)
            .await?;
    Ok(rows
        .into_iter()
        .map(|row| PaymentAttemptDiagnosticEligibilityItem {
            attempt_id: PaymentAttemptId::new(row.attempt_id),
            eligibility: PaymentAttemptDiagnosticEligibility::classify(
                &row.attempt_kind,
                row.submitted,
                &row.status,
                row.has_transaction_id,
            ),
        })
        .collect())
}

/// Queries the provider once for one owned attempt's original processor and
/// verification results, without changing payment state.
///
/// The target attempt is read in a short timed transaction; its account and
/// provider are resolved through the account's current canonical
/// configuration, never the attempt's historical configuration or a default
/// account. Durable account and provider cooldowns stop the call before any
/// I/O. One exact provider query by the recorded transaction ID runs outside
/// any database transaction, for at most 10 seconds. Afterwards the target,
/// account, provider, and configuration are revalidated: a rotated
/// configuration yields `ConfigurationChanged`, and a changed transaction ID or
/// status (for example after late-approval review) yields `TargetChanged`.
/// `observed_at` is read from the database clock after the query.
///
/// `deadline` bounds the whole call. The provider query receives the earlier
/// of 10 seconds and the time left after reserving about 3 seconds for
/// revalidation and cooldown bookkeeping; when less than one second would
/// remain, the call returns `TimedOut` without provider I/O. Every completed
/// provider response, including an error, is revalidated. Post-query
/// revalidation and a rate limit's cooldown write run within the remaining
/// deadline: their connection wait is bounded and the database aborts an
/// overrunning statement, so the call never abandons the cooldown write
/// midway. A revalidation that cannot finish in time yields `TimedOut` (or
/// `RateLimited` once the cooldown is recorded), and a cooldown that cannot be
/// recorded in time is `RateLimitCooldownPersistenceFailed`. Callers should
/// not wrap this future in a shorter timeout of their own.
///
/// Side effects: when the provider rate-limits the query, this extends the
/// shared 60-second provider cooldown, the only write it can perform. During
/// that cooldown every account sharing the provider pauses: a renewal whose
/// attempt is already reserved or admitted is recorded `failed` with
/// `gateway_provider_rate_limited_before_submission` (not past due) and
/// advances the rate-limit retry ladder, renewals that start during it are
/// skipped, and enrollments are refused. The host must authorize the subject
/// and bound total diagnostic volume per subscriber, account, and provider,
/// including user-facing triggers.
///
/// This call does not consult scrub state. Scrubbed attempts keep their
/// transaction ID, so the host must not call it for a subscriber that is
/// scrubbed or being scrubbed, and must purge any cached observation in the
/// same transaction as its own scrub. It never writes attempts, processor
/// charges, subscriptions, methods, or contacts, never changes approval or
/// entitlement decisions, and never submits a provider mutation.
pub async fn query_payment_attempt_diagnostics(
    pool: &PgPool,
    resolver: &dyn GatewayResolver,
    target: PaymentAttemptDiagnosticTarget,
    deadline: Duration,
) -> Result<PaymentAttemptDiagnosticsOutcome, PaymentAttemptDiagnosticsError> {
    use PaymentAttemptDiagnosticsOutcome as Outcome;

    let deadline = Instant::now() + deadline.min(MAX_DIAGNOSTIC_DEADLINE);
    let io_deadline = deadline
        .checked_sub(DIAGNOSTIC_POST_QUERY_RESERVE)
        .unwrap_or_else(Instant::now);
    // Pre-query reads are read-only, so abandoning them at the deadline is
    // safe and precedes any provider I/O.
    let Ok(prepared) = tokio::time::timeout_at(io_deadline, storage::prepare(pool, &target)).await
    else {
        return Ok(Outcome::TimedOut);
    };
    let candidate = match prepared? {
        storage::Prepared::Done(outcome) => return Ok(outcome),
        storage::Prepared::Query(candidate) => candidate,
    };
    let Ok(resolved) = tokio::time::timeout_at(
        io_deadline,
        resolver.resolve(
            target.billing_scope_id,
            candidate.account_id,
            candidate.configuration_id,
            candidate.provider_key.clone(),
        ),
    )
    .await
    else {
        return Ok(Outcome::TimedOut);
    };
    let gateway = match resolved {
        Ok(gateway) => gateway,
        Err(GatewayResolutionError::ConfigurationChanged) => {
            return Ok(Outcome::ConfigurationChanged);
        }
        Err(error) => return Err(PaymentAttemptDiagnosticsError::Resolution(error)),
    };
    if gateway.billing_scope_id() != target.billing_scope_id
        || gateway.gateway_account_id() != candidate.account_id
        || gateway.gateway_configuration_id() != candidate.configuration_id
        || gateway.provider_key() != &candidate.provider_key
    {
        return Err(PaymentAttemptDiagnosticsError::GatewayIdentityMismatch);
    }
    let query_budget = io_deadline
        .saturating_duration_since(Instant::now())
        .min(DIAGNOSTIC_QUERY_TIMEOUT);
    if query_budget < MIN_DIAGNOSTIC_QUERY_BUDGET {
        return Ok(Outcome::TimedOut);
    }
    let request = GatewayTransactionDiagnosticsRequest::new(
        candidate.transaction_id.clone(),
        candidate.operation,
        candidate.amount,
        candidate.order_id.clone(),
    );
    let Ok(result) =
        tokio::time::timeout(query_budget, gateway.query_transaction_diagnostics(request)).await
    else {
        return Ok(Outcome::TimedOut);
    };
    // Every completed provider response is revalidated. A rate limit first
    // records the shared cooldown; nothing else is ever written.
    let response = match result {
        Ok(diagnostics) => ProviderResponse::Diagnostics(diagnostics),
        Err(error @ GatewayError::RateLimited(_)) => {
            if let Err(storage) =
                storage::record_provider_cooldown_before(pool, deadline, &target, &candidate).await
            {
                return Err(
                    PaymentAttemptDiagnosticsError::RateLimitCooldownPersistenceFailed {
                        query: error,
                        storage,
                    },
                );
            }
            ProviderResponse::RateLimited
        }
        Err(error) => ProviderResponse::Unavailable(
            GatewayDiagnosticsUnavailableReason::for_query_error(&error)
                .unwrap_or(GatewayDiagnosticsUnavailableReason::ProviderUnavailable),
        ),
    };
    let observed_at = match storage::revalidate(pool, &target, &candidate, deadline).await? {
        storage::Revalidation::Unchanged { observed_at } => observed_at,
        storage::Revalidation::ConfigurationChanged => return Ok(Outcome::ConfigurationChanged),
        storage::Revalidation::TargetChanged => return Ok(Outcome::TargetChanged),
        // The recorded cooldown is the more important signal to the host.
        storage::Revalidation::DeadlineExceeded => {
            return Ok(match response {
                ProviderResponse::RateLimited => Outcome::RateLimited,
                ProviderResponse::Diagnostics(_) | ProviderResponse::Unavailable(_) => {
                    Outcome::TimedOut
                }
            });
        }
    };
    Ok(match response {
        ProviderResponse::RateLimited => Outcome::RateLimited,
        ProviderResponse::Unavailable(reason) => Outcome::Unavailable(reason),
        ProviderResponse::Diagnostics(GatewayTransactionDiagnostics::Observed(observation)) => {
            Outcome::Observed(Box::new(PaymentAttemptDiagnostics {
                target,
                attempt_kind: candidate.attempt_kind,
                gateway_account_id: candidate.account_id,
                gateway_configuration_id: candidate.configuration_id,
                provider_key: candidate.provider_key,
                observation: *observation,
                observed_at,
            }))
        }
        ProviderResponse::Diagnostics(GatewayTransactionDiagnostics::NotFound) => {
            Outcome::ProviderTransactionNotFound
        }
        ProviderResponse::Diagnostics(GatewayTransactionDiagnostics::Unavailable(reason)) => {
            Outcome::Unavailable(reason)
        }
        ProviderResponse::Diagnostics(_) => Outcome::Unsupported,
    })
}

/// A completed provider response awaiting revalidation.
enum ProviderResponse {
    Diagnostics(GatewayTransactionDiagnostics),
    RateLimited,
    Unavailable(GatewayDiagnosticsUnavailableReason),
}

/// Stored identity of an eligible attempt, captured before provider I/O.
pub(crate) struct DiagnosticCandidate {
    attempt_kind: PaymentAttemptKind,
    status: String,
    stored_transaction_id: String,
    transaction_id: GatewayTransactionId,
    operation: GatewayDiagnosticOperation,
    amount: Money,
    order_id: Option<GatewayOrderId>,
    account_id: GatewayAccountId,
    configuration_id: GatewayConfigurationId,
    provider_key: GatewayProviderKey,
}

impl DiagnosticCandidate {
    fn new(
        attempt_id: PaymentAttemptId,
        row: storage::TargetRow,
    ) -> Result<Self, PaymentAttemptDiagnosticsError> {
        let invalid = |_| PaymentAttemptDiagnosticsError::InvalidIdentity;
        let attempt_kind = row
            .attempt_kind
            .parse::<PaymentAttemptKind>()
            .map_err(|_| PaymentAttemptDiagnosticsError::InvalidIdentity)?;
        let operation = match attempt_kind {
            PaymentAttemptKind::SubscriptionPaymentMethodUpdate => {
                GatewayDiagnosticOperation::Validate
            }
            _ => GatewayDiagnosticOperation::Sale,
        };
        let currency = CurrencyCode::new(&row.currency)
            .map_err(|_| PaymentAttemptDiagnosticsError::InvalidIdentity)?;
        let order_id = GatewayOrderId::from_generated_attempt(&row.gateway_order_id, attempt_id)
            .or_else(|_| GatewayOrderId::from_correlation(&row.gateway_order_id))
            .map_err(invalid)?;
        let stored_transaction_id = row
            .gateway_transaction_id
            .ok_or(PaymentAttemptDiagnosticsError::InvalidIdentity)?;
        Ok(Self {
            attempt_kind,
            transaction_id: GatewayTransactionId::new(stored_transaction_id.clone())
                .map_err(invalid)?,
            stored_transaction_id,
            status: row.status,
            operation,
            amount: Money::new(row.amount_cents, currency)
                .map_err(|_| PaymentAttemptDiagnosticsError::InvalidIdentity)?,
            order_id: Some(order_id),
            account_id: GatewayAccountId::new(row.gateway_account_id),
            configuration_id: GatewayConfigurationId::new(row.gateway_configuration_id),
            provider_key: GatewayProviderKey::new(&row.provider_key)
                .map_err(|_| PaymentAttemptDiagnosticsError::InvalidIdentity)?,
        })
    }
}
