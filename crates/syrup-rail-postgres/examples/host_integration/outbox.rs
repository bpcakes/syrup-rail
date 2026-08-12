//! Example host-owned wire contract for Syrup Rail billing events.
//!
//! The domain [`BillingEvent`] deliberately does not implement Serde. A host
//! maps it exhaustively into a versioned DTO, writes that DTO on the same SQL
//! transaction as the billing mutation, and owns later transport evolution.
//!
//! A host table commonly separates routing/idempotency columns from JSON:
//!
//! ```sql
//! CREATE TABLE host_billing_outbox (
//!     event_id uuid PRIMARY KEY,
//!     occurred_at timestamptz NOT NULL,
//!     billing_scope_id uuid NOT NULL,
//!     subscriber_id uuid NOT NULL,
//!     event_kind text NOT NULL,
//!     event_version smallint NOT NULL,
//!     semantic_kind text NOT NULL,
//!     semantic_id uuid NOT NULL,
//!     payload jsonb NOT NULL,
//!     UNIQUE (billing_scope_id, semantic_kind, semantic_id)
//! );
//! ```
//!
//! PostgreSQL 18 can allocate `uuidv7()` and observe `clock_timestamp()` in
//! the insert. [`append_host_billing_event_v1`] demonstrates the complete
//! same-transaction write: insert with `ON CONFLICT DO NOTHING`, read the
//! conflicting row through the same connection, reconstruct its typed replay
//! contract, and accept it only when every replay-stable field matches.
//! `event_id` and `occurred_at` are facts of the first successful write and are
//! deliberately excluded from replay equality. Outbox rows are append-only;
//! changing a durable row would invalidate that comparison.

use std::{error::Error, fmt};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgConnection};
use syrup_rail::{
    BillingEvent, BillingEventKey, BillingEventSubject, BillingPeriod, ChargeAmount,
    PaymentCardBrand, SubscriptionEndReason, SubscriptionPaymentFailureAccess,
    SubscriptionPaymentFailureDisposition, SubscriptionPhase,
};
use syrup_rail_postgres::BillingEventWriteError;
use uuid::Uuid;

/// Version of the host wire schema demonstrated by this example.
pub const HOST_BILLING_EVENT_SCHEMA_VERSION: u16 = 1;

/// A host-owned durable event envelope.
///
/// Generate `event_id` and observe `occurred_at` inside PostgreSQL on the same
/// transaction used for the outbox insert. The recommended unique key is
/// `(billing_scope_id, semantic_kind, semantic_id)`. On conflict, accept an
/// existing row only when [`Self::replay_matches`] succeeds; otherwise fail
/// the transaction instead of silently changing a previously durable event.
#[derive(Clone, Serialize)]
pub struct HostBillingEventEnvelopeV1 {
    event_id: Uuid,
    occurred_at: DateTime<Utc>,
    #[serde(flatten)]
    replay: HostBillingEventReplayV1,
}

/// The complete set of fields that must match for an idempotent replay.
///
/// This value excludes the first successful write's `event_id` and
/// `occurred_at`. Equality intentionally includes the schema version, billing
/// subject, routed event kind, semantic key, and payload so a host cannot
/// silently accept a conflicting event under the same uniqueness key.
#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct HostBillingEventReplayV1 {
    schema_version: u16,
    billing_scope_id: Uuid,
    subscriber_id: Uuid,
    kind: HostBillingEventKindV1,
    semantic_key: HostBillingEventSemanticKeyV1,
    payload: HostBillingEventPayloadV1,
}

/// Result of the example's append-only host outbox write.
#[derive(Clone, Debug)]
pub enum HostBillingEventAppendOutcomeV1 {
    /// This call created the durable row.
    Inserted(HostBillingEventEnvelopeV1),
    /// A byte-equivalent replay contract was already durable.
    Replayed(HostBillingEventEnvelopeV1),
}

impl HostBillingEventAppendOutcomeV1 {
    /// Returns the first successful write's durable event envelope.
    pub const fn envelope(&self) -> &HostBillingEventEnvelopeV1 {
        match self {
            Self::Inserted(envelope) | Self::Replayed(envelope) => envelope,
        }
    }

    /// Returns whether this call created the row.
    pub const fn was_inserted(&self) -> bool {
        matches!(self, Self::Inserted(_))
    }
}

/// Value-free reason that persisted host outbox columns could not be decoded.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostBillingEventReplayDecodeErrorV1 {
    /// The row does not use this example's version-1 contract.
    UnsupportedVersion,
    /// The routing event kind is outside the version-1 vocabulary.
    UnknownEventKind,
    /// The semantic-key kind is outside the version-1 vocabulary.
    UnknownSemanticKind,
    /// The JSON payload is not a valid version-1 payload.
    InvalidPayload,
}

impl fmt::Display for HostBillingEventReplayDecodeErrorV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::UnsupportedVersion => "unsupported host billing event version",
            Self::UnknownEventKind => "unknown host billing event kind",
            Self::UnknownSemanticKind => "unknown host billing event semantic kind",
            Self::InvalidPayload => "invalid host billing event payload",
        };
        formatter.write_str(message)
    }
}

impl Error for HostBillingEventReplayDecodeErrorV1 {}

impl HostBillingEventEnvelopeV1 {
    /// Maps a typed domain event into the host's version-1 wire contract.
    ///
    /// `event_id` and `occurred_at` are host outbox facts rather than domain
    /// facts. Hosts should obtain both from PostgreSQL in the outbox insert or
    /// immediately before this conversion on the same transaction.
    pub fn from_domain(
        event_id: Uuid,
        occurred_at: DateTime<Utc>,
        subject: BillingEventSubject,
        event: &BillingEvent,
    ) -> Self {
        Self {
            event_id,
            occurred_at,
            replay: HostBillingEventReplayV1::from_domain(subject, event),
        }
    }

    /// Reconstructs an envelope selected from the split outbox columns.
    ///
    /// Use this after a semantic-key conflict and require
    /// [`Self::replay_matches`] before treating the conflict as an idempotent
    /// replay. The method deliberately validates host-owned version and enum
    /// vocabularies rather than delegating their interpretation to domain
    /// `as_str()` methods.
    #[allow(clippy::too_many_arguments)]
    pub fn from_persisted_parts(
        event_id: Uuid,
        occurred_at: DateTime<Utc>,
        event_version: i16,
        billing_scope_id: Uuid,
        subscriber_id: Uuid,
        event_kind: &str,
        semantic_kind: &str,
        semantic_id: Uuid,
        payload: serde_json::Value,
    ) -> Result<Self, HostBillingEventReplayDecodeErrorV1> {
        Ok(Self {
            event_id,
            occurred_at,
            replay: HostBillingEventReplayV1::from_persisted_parts(
                event_version,
                billing_scope_id,
                subscriber_id,
                event_kind,
                semantic_kind,
                semantic_id,
                payload,
            )?,
        })
    }

    /// Returns the host-assigned durable event identifier.
    pub const fn event_id(&self) -> Uuid {
        self.event_id
    }

    /// Returns the database-observed outbox occurrence time.
    pub const fn occurred_at(&self) -> DateTime<Utc> {
        self.occurred_at
    }

    /// Returns the host wire-schema version.
    pub const fn schema_version(&self) -> u16 {
        self.replay.schema_version
    }

    /// Returns the SQL `smallint` version stored in `event_version`.
    pub const fn event_version(&self) -> i16 {
        HOST_BILLING_EVENT_SCHEMA_VERSION as i16
    }

    /// Returns the authorized billing scope retained by the transaction.
    pub const fn billing_scope_id(&self) -> Uuid {
        self.replay.billing_scope_id
    }

    /// Returns the authorized subscriber retained by the transaction.
    pub const fn subscriber_id(&self) -> Uuid {
        self.replay.subscriber_id
    }

    /// Returns the stable host event kind for routing and indexing.
    pub const fn kind(&self) -> &'static str {
        self.replay.kind.as_str()
    }

    /// Returns the semantic event kind used in the outbox unique key.
    pub const fn semantic_kind(&self) -> &'static str {
        self.replay.semantic_key.kind.as_str()
    }

    /// Returns the semantic identity used in the outbox unique key.
    pub const fn semantic_id(&self) -> Uuid {
        self.replay.semantic_key.identity
    }

    /// Returns all content that must match an existing semantic-key row.
    pub const fn replay_contract(&self) -> &HostBillingEventReplayV1 {
        &self.replay
    }

    /// Serializes only the minimized JSON payload stored in `payload`.
    pub fn persisted_payload(&self) -> Result<serde_json::Value, serde_json::Error> {
        self.replay.persisted_payload()
    }

    /// Returns whether `existing` represents the same replay-stable event.
    ///
    /// The first-write `event_id` and `occurred_at` may differ. Every other
    /// serialized field must be equal.
    pub fn replay_matches(&self, existing: &Self) -> bool {
        self.replay == existing.replay
    }
}

impl HostBillingEventReplayV1 {
    /// Maps a typed event and its already-authorized subject into the fields
    /// that must remain equal across semantic-key replays.
    pub fn from_domain(subject: BillingEventSubject, event: &BillingEvent) -> Self {
        Self {
            schema_version: HOST_BILLING_EVENT_SCHEMA_VERSION,
            billing_scope_id: *subject.billing_scope_id().as_uuid(),
            subscriber_id: *subject.subscriber_id().as_uuid(),
            kind: HostBillingEventKindV1::from(event),
            semantic_key: HostBillingEventSemanticKeyV1::from(event.semantic_key()),
            payload: HostBillingEventPayloadV1::from(event),
        }
    }

    /// Reconstructs the replay contract from the host table's split columns.
    ///
    /// A successfully decoded value is not necessarily a valid replay of a
    /// new candidate: known-but-different kinds, subjects, semantic keys, or
    /// payload variants remain representable so [`Self::replay_matches`] can
    /// reject the conflict.
    #[allow(clippy::too_many_arguments)]
    pub fn from_persisted_parts(
        event_version: i16,
        billing_scope_id: Uuid,
        subscriber_id: Uuid,
        event_kind: &str,
        semantic_kind: &str,
        semantic_id: Uuid,
        payload: serde_json::Value,
    ) -> Result<Self, HostBillingEventReplayDecodeErrorV1> {
        if event_version != HOST_BILLING_EVENT_SCHEMA_VERSION as i16 {
            return Err(HostBillingEventReplayDecodeErrorV1::UnsupportedVersion);
        }

        let kind = HostBillingEventKindV1::parse(event_kind)
            .ok_or(HostBillingEventReplayDecodeErrorV1::UnknownEventKind)?;
        let semantic_kind = HostBillingEventKindV1::parse(semantic_kind)
            .ok_or(HostBillingEventReplayDecodeErrorV1::UnknownSemanticKind)?;
        let payload = serde_json::from_value(payload)
            .map_err(|_| HostBillingEventReplayDecodeErrorV1::InvalidPayload)?;

        Ok(Self {
            schema_version: HOST_BILLING_EVENT_SCHEMA_VERSION,
            billing_scope_id,
            subscriber_id,
            kind,
            semantic_key: HostBillingEventSemanticKeyV1 {
                kind: semantic_kind,
                identity: semantic_id,
            },
            payload,
        })
    }

    /// Returns the host wire-schema version.
    pub const fn schema_version(&self) -> u16 {
        self.schema_version
    }

    /// Returns the SQL `smallint` version stored in `event_version`.
    pub const fn event_version(&self) -> i16 {
        HOST_BILLING_EVENT_SCHEMA_VERSION as i16
    }

    /// Returns the authorized billing scope retained by the transaction.
    pub const fn billing_scope_id(&self) -> Uuid {
        self.billing_scope_id
    }

    /// Returns the authorized subscriber retained by the transaction.
    pub const fn subscriber_id(&self) -> Uuid {
        self.subscriber_id
    }

    /// Returns the stable host event kind for routing and indexing.
    pub const fn kind(&self) -> &'static str {
        self.kind.as_str()
    }

    /// Returns the semantic event kind used in the outbox unique key.
    pub const fn semantic_kind(&self) -> &'static str {
        self.semantic_key.kind.as_str()
    }

    /// Returns the semantic identity used in the outbox unique key.
    pub const fn semantic_id(&self) -> Uuid {
        self.semantic_key.identity
    }

    /// Serializes only the minimized JSON payload stored in `payload`.
    pub fn persisted_payload(&self) -> Result<serde_json::Value, serde_json::Error> {
        serde_json::to_value(&self.payload)
    }

    /// Returns whether `existing` has exactly the same replay-stable fields.
    pub fn replay_matches(&self, existing: &Self) -> bool {
        self == existing
    }
}

/// Appends one version-1 event through the caller's existing transaction.
///
/// The insert owns first-write `event_id` and `occurred_at` generation. When
/// the semantic key already exists, this function selects that exact row on
/// the same connection, reconstructs its typed envelope, and returns
/// [`HostBillingEventAppendOutcomeV1::Replayed`] only after exact replay
/// equality. A mismatch is returned as a value-redacted
/// [`BillingEventWriteError`], so the host transaction must roll back.
///
/// This concrete function targets the example table shown in the module docs.
/// Hosts may copy the algorithm into their own schema, but must preserve the
/// append-only row, uniqueness key, same-transaction connection, and complete
/// comparison.
pub async fn append_host_billing_event_v1(
    connection: &mut PgConnection,
    subject: BillingEventSubject,
    event: &BillingEvent,
) -> Result<HostBillingEventAppendOutcomeV1, BillingEventWriteError> {
    let candidate = HostBillingEventReplayV1::from_domain(subject, event);
    let payload = candidate
        .persisted_payload()
        .map_err(BillingEventWriteError::new)?;

    let inserted = sqlx::query_as::<_, HostBillingEventFirstWriteV1>(
        r#"
        INSERT INTO host_billing_outbox (
            event_id,
            occurred_at,
            billing_scope_id,
            subscriber_id,
            event_kind,
            event_version,
            semantic_kind,
            semantic_id,
            payload
        ) VALUES (
            uuidv7(),
            clock_timestamp(),
            $1,
            $2,
            $3,
            $4,
            $5,
            $6,
            $7
        )
        ON CONFLICT (billing_scope_id, semantic_kind, semantic_id) DO NOTHING
        RETURNING event_id, occurred_at
        "#,
    )
    .bind(candidate.billing_scope_id())
    .bind(candidate.subscriber_id())
    .bind(candidate.kind())
    .bind(candidate.event_version())
    .bind(candidate.semantic_kind())
    .bind(candidate.semantic_id())
    .bind(payload)
    .fetch_optional(&mut *connection)
    .await
    .map_err(BillingEventWriteError::new)?;

    if let Some(inserted) = inserted {
        return Ok(HostBillingEventAppendOutcomeV1::Inserted(
            HostBillingEventEnvelopeV1 {
                event_id: inserted.event_id,
                occurred_at: inserted.occurred_at,
                replay: candidate,
            },
        ));
    }

    let existing = sqlx::query_as::<_, HostBillingEventPersistedV1>(
        r#"
        SELECT
            event_id,
            occurred_at,
            event_version,
            billing_scope_id,
            subscriber_id,
            event_kind,
            semantic_kind,
            semantic_id,
            payload
        FROM host_billing_outbox
        WHERE billing_scope_id = $1
          AND semantic_kind = $2
          AND semantic_id = $3
        "#,
    )
    .bind(candidate.billing_scope_id())
    .bind(candidate.semantic_kind())
    .bind(candidate.semantic_id())
    .fetch_one(&mut *connection)
    .await
    .map_err(BillingEventWriteError::new)?;
    let existing = HostBillingEventEnvelopeV1::from_persisted_parts(
        existing.event_id,
        existing.occurred_at,
        existing.event_version,
        existing.billing_scope_id,
        existing.subscriber_id,
        &existing.event_kind,
        &existing.semantic_kind,
        existing.semantic_id,
        existing.payload,
    )
    .map_err(BillingEventWriteError::new)?;

    if !candidate.replay_matches(existing.replay_contract()) {
        return Err(BillingEventWriteError::new(
            HostBillingEventReplayConflictV1,
        ));
    }

    Ok(HostBillingEventAppendOutcomeV1::Replayed(existing))
}

#[derive(FromRow)]
struct HostBillingEventFirstWriteV1 {
    event_id: Uuid,
    occurred_at: DateTime<Utc>,
}

#[derive(FromRow)]
struct HostBillingEventPersistedV1 {
    event_id: Uuid,
    occurred_at: DateTime<Utc>,
    event_version: i16,
    billing_scope_id: Uuid,
    subscriber_id: Uuid,
    event_kind: String,
    semantic_kind: String,
    semantic_id: Uuid,
    payload: serde_json::Value,
}

#[derive(Debug)]
struct HostBillingEventReplayConflictV1;

impl fmt::Display for HostBillingEventReplayConflictV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("host billing event semantic key conflicts with durable content")
    }
}

impl Error for HostBillingEventReplayConflictV1 {}

impl fmt::Debug for HostBillingEventEnvelopeV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HostBillingEventEnvelopeV1")
            .field("schema_version", &self.replay.schema_version)
            .field("kind", &self.replay.kind)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for HostBillingEventReplayV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HostBillingEventReplayV1")
            .field("schema_version", &self.schema_version)
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

/// Stable event-name column owned by the host wire contract.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum HostBillingEventKindV1 {
    SubscriptionStarted,
    SubscriptionRenewed,
    SubscriptionPaymentFailed,
    SubscriptionEnded,
    SubscriptionCanceled,
    PaymentMethodChanged,
    HostChargePaid,
}

impl HostBillingEventKindV1 {
    const fn as_str(self) -> &'static str {
        match self {
            Self::SubscriptionStarted => "subscription_started",
            Self::SubscriptionRenewed => "subscription_renewed",
            Self::SubscriptionPaymentFailed => "subscription_payment_failed",
            Self::SubscriptionEnded => "subscription_ended",
            Self::SubscriptionCanceled => "subscription_canceled",
            Self::PaymentMethodChanged => "payment_method_changed",
            Self::HostChargePaid => "host_charge_paid",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "subscription_started" => Some(Self::SubscriptionStarted),
            "subscription_renewed" => Some(Self::SubscriptionRenewed),
            "subscription_payment_failed" => Some(Self::SubscriptionPaymentFailed),
            "subscription_ended" => Some(Self::SubscriptionEnded),
            "subscription_canceled" => Some(Self::SubscriptionCanceled),
            "payment_method_changed" => Some(Self::PaymentMethodChanged),
            "host_charge_paid" => Some(Self::HostChargePaid),
            _ => None,
        }
    }
}

impl From<&BillingEvent> for HostBillingEventKindV1 {
    fn from(event: &BillingEvent) -> Self {
        match event {
            BillingEvent::SubscriptionStarted { .. } => Self::SubscriptionStarted,
            BillingEvent::SubscriptionRenewed { .. } => Self::SubscriptionRenewed,
            BillingEvent::SubscriptionPaymentFailed { .. } => Self::SubscriptionPaymentFailed,
            BillingEvent::SubscriptionEnded { .. } => Self::SubscriptionEnded,
            BillingEvent::SubscriptionCanceled { .. } => Self::SubscriptionCanceled,
            BillingEvent::PaymentMethodChanged { .. } => Self::PaymentMethodChanged,
            BillingEvent::HostChargePaid { .. } => Self::HostChargePaid,
        }
    }
}

/// Host columns used to make delivery idempotent without serializing Rust enum
/// layout or debug output.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
struct HostBillingEventSemanticKeyV1 {
    kind: HostBillingEventKindV1,
    identity: Uuid,
}

impl From<BillingEventKey> for HostBillingEventSemanticKeyV1 {
    fn from(key: BillingEventKey) -> Self {
        match key {
            BillingEventKey::SubscriptionStarted(id) => Self {
                kind: HostBillingEventKindV1::SubscriptionStarted,
                identity: *id.as_uuid(),
            },
            BillingEventKey::SubscriptionRenewed(id) => Self {
                kind: HostBillingEventKindV1::SubscriptionRenewed,
                identity: *id.as_uuid(),
            },
            BillingEventKey::SubscriptionPaymentFailed(id) => Self {
                kind: HostBillingEventKindV1::SubscriptionPaymentFailed,
                identity: *id.as_uuid(),
            },
            BillingEventKey::SubscriptionEnded(id) => Self {
                kind: HostBillingEventKindV1::SubscriptionEnded,
                identity: *id.as_uuid(),
            },
            BillingEventKey::SubscriptionCanceled(id) => Self {
                kind: HostBillingEventKindV1::SubscriptionCanceled,
                identity: *id.as_uuid(),
            },
            BillingEventKey::PaymentMethodChanged(id) => Self {
                kind: HostBillingEventKindV1::PaymentMethodChanged,
                identity: *id.as_uuid(),
            },
            BillingEventKey::HostChargePaid(id) => Self {
                kind: HostBillingEventKindV1::HostChargePaid,
                identity: *id.as_uuid(),
            },
        }
    }
}

/// Minimized provider-neutral payload sent by the example host.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum HostBillingEventPayloadV1 {
    SubscriptionStarted {
        attempt_id: Uuid,
        subscription_id: Uuid,
        plan_key: String,
        charge: HostChargeAmountV1,
        period: HostBillingPeriodV1,
        phase: HostSubscriptionPhaseV1,
    },
    SubscriptionRenewed {
        attempt_id: Uuid,
        subscription_id: Uuid,
        plan_key: String,
        charge: HostChargeAmountV1,
        period: HostBillingPeriodV1,
    },
    SubscriptionPaymentFailed {
        attempt_id: Uuid,
        subscription_id: Uuid,
        plan_key: String,
        disposition: HostSubscriptionPaymentFailureDispositionV1,
        access: HostSubscriptionPaymentFailureAccessV1,
    },
    SubscriptionEnded {
        attempt_id: Uuid,
        subscription_id: Uuid,
        plan_key: String,
        reason: HostSubscriptionEndReasonV1,
        ended_at: DateTime<Utc>,
        access_ends_at: DateTime<Utc>,
    },
    SubscriptionCanceled {
        subscription_id: Uuid,
        plan_key: String,
        access_ends_at: DateTime<Utc>,
    },
    PaymentMethodChanged {
        attempt_id: Uuid,
        subscription_id: Uuid,
        plan_key: String,
        card: Option<HostPaymentCardDisplayV1>,
    },
    HostChargePaid {
        attempt_id: Uuid,
        target_id: Uuid,
        charge: HostChargeAmountV1,
    },
}

impl From<&BillingEvent> for HostBillingEventPayloadV1 {
    fn from(event: &BillingEvent) -> Self {
        match event {
            BillingEvent::SubscriptionStarted {
                attempt_id,
                subscription_id,
                plan_key,
                charge,
                period,
                phase,
            } => Self::SubscriptionStarted {
                attempt_id: *attempt_id.as_uuid(),
                subscription_id: *subscription_id.as_uuid(),
                plan_key: plan_key.as_str().to_owned(),
                charge: (*charge).into(),
                period: period.into(),
                phase: (*phase).into(),
            },
            BillingEvent::SubscriptionRenewed {
                attempt_id,
                subscription_id,
                plan_key,
                charge,
                period,
            } => Self::SubscriptionRenewed {
                attempt_id: *attempt_id.as_uuid(),
                subscription_id: *subscription_id.as_uuid(),
                plan_key: plan_key.as_str().to_owned(),
                charge: (*charge).into(),
                period: period.into(),
            },
            BillingEvent::SubscriptionPaymentFailed {
                attempt_id,
                subscription_id,
                plan_key,
                disposition,
                access,
            } => Self::SubscriptionPaymentFailed {
                attempt_id: *attempt_id.as_uuid(),
                subscription_id: *subscription_id.as_uuid(),
                plan_key: plan_key.as_str().to_owned(),
                disposition: (*disposition).into(),
                access: (*access).into(),
            },
            BillingEvent::SubscriptionEnded {
                attempt_id,
                subscription_id,
                plan_key,
                reason,
                ended_at,
                access_ends_at,
            } => Self::SubscriptionEnded {
                attempt_id: *attempt_id.as_uuid(),
                subscription_id: *subscription_id.as_uuid(),
                plan_key: plan_key.as_str().to_owned(),
                reason: (*reason).into(),
                ended_at: *ended_at,
                access_ends_at: *access_ends_at,
            },
            BillingEvent::SubscriptionCanceled {
                subscription_id,
                plan_key,
                access_ends_at,
            } => Self::SubscriptionCanceled {
                subscription_id: *subscription_id.as_uuid(),
                plan_key: plan_key.as_str().to_owned(),
                access_ends_at: *access_ends_at,
            },
            BillingEvent::PaymentMethodChanged {
                attempt_id,
                subscription_id,
                plan_key,
                card,
            } => Self::PaymentMethodChanged {
                attempt_id: *attempt_id.as_uuid(),
                subscription_id: *subscription_id.as_uuid(),
                plan_key: plan_key.as_str().to_owned(),
                card: card.as_ref().map(|card| HostPaymentCardDisplayV1 {
                    brand: card.brand().into(),
                    last_four: card.last_four().expose().to_owned(),
                }),
            },
            BillingEvent::HostChargePaid {
                attempt_id,
                target_id,
                charge,
            } => Self::HostChargePaid {
                attempt_id: *attempt_id.as_uuid(),
                target_id: *target_id.as_uuid(),
                charge: (*charge).into(),
            },
        }
    }
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
struct HostChargeAmountV1 {
    cents: i32,
    currency: String,
}

impl From<ChargeAmount> for HostChargeAmountV1 {
    fn from(charge: ChargeAmount) -> Self {
        Self {
            cents: charge.cents(),
            currency: charge.currency().as_str().to_owned(),
        }
    }
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
struct HostBillingPeriodV1 {
    start_at: DateTime<Utc>,
    end_at: DateTime<Utc>,
}

impl From<&BillingPeriod> for HostBillingPeriodV1 {
    fn from(period: &BillingPeriod) -> Self {
        Self {
            start_at: *period.start_at(),
            end_at: *period.end_at(),
        }
    }
}

#[derive(Clone, Copy, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum HostSubscriptionPhaseV1 {
    PaidTrial,
    Recurring,
}

impl From<SubscriptionPhase> for HostSubscriptionPhaseV1 {
    fn from(phase: SubscriptionPhase) -> Self {
        match phase {
            SubscriptionPhase::PaidTrial => Self::PaidTrial,
            SubscriptionPhase::Recurring => Self::Recurring,
        }
    }
}

#[derive(Clone, Copy, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum HostSubscriptionEndReasonV1 {
    NonPayment,
}

impl From<SubscriptionEndReason> for HostSubscriptionEndReasonV1 {
    fn from(reason: SubscriptionEndReason) -> Self {
        match reason {
            SubscriptionEndReason::NonPayment => Self::NonPayment,
        }
    }
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum HostSubscriptionPaymentFailureDispositionV1 {
    RetryScheduled { retry_at: DateTime<Utc> },
    DunningExhausted { exhausted_at: DateTime<Utc> },
    SubscriptionEnded { ended_at: DateTime<Utc> },
}

impl From<SubscriptionPaymentFailureDisposition> for HostSubscriptionPaymentFailureDispositionV1 {
    fn from(disposition: SubscriptionPaymentFailureDisposition) -> Self {
        match disposition {
            SubscriptionPaymentFailureDisposition::RetryScheduled { retry_at } => {
                Self::RetryScheduled { retry_at }
            }
            SubscriptionPaymentFailureDisposition::DunningExhausted { exhausted_at } => {
                Self::DunningExhausted { exhausted_at }
            }
            SubscriptionPaymentFailureDisposition::SubscriptionEnded { ended_at } => {
                Self::SubscriptionEnded { ended_at }
            }
        }
    }
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum HostSubscriptionPaymentFailureAccessV1 {
    ContinuesDuringDunning,
    Ended { access_ended_at: DateTime<Utc> },
}

impl From<SubscriptionPaymentFailureAccess> for HostSubscriptionPaymentFailureAccessV1 {
    fn from(access: SubscriptionPaymentFailureAccess) -> Self {
        match access {
            SubscriptionPaymentFailureAccess::ContinuesDuringDunning => {
                Self::ContinuesDuringDunning
            }
            SubscriptionPaymentFailureAccess::Ended { access_ended_at } => {
                Self::Ended { access_ended_at }
            }
        }
    }
}

#[derive(Clone, Copy, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum HostPaymentCardBrandV1 {
    Visa,
    Mastercard,
    AmericanExpress,
    Discover,
    Jcb,
    DinersClub,
    UnionPay,
    Maestro,
    Other,
}

impl From<&PaymentCardBrand> for HostPaymentCardBrandV1 {
    fn from(brand: &PaymentCardBrand) -> Self {
        match brand {
            PaymentCardBrand::Visa => Self::Visa,
            PaymentCardBrand::Mastercard => Self::Mastercard,
            PaymentCardBrand::AmericanExpress => Self::AmericanExpress,
            PaymentCardBrand::Discover => Self::Discover,
            PaymentCardBrand::Jcb => Self::Jcb,
            PaymentCardBrand::DinersClub => Self::DinersClub,
            PaymentCardBrand::UnionPay => Self::UnionPay,
            PaymentCardBrand::Maestro => Self::Maestro,
            PaymentCardBrand::Other => Self::Other,
            _ => Self::Other,
        }
    }
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
struct HostPaymentCardDisplayV1 {
    brand: HostPaymentCardBrandV1,
    last_four: String,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use chrono::TimeZone;
    use postgres_test_harness::{HarnessConfig, PostgresHarness};
    use sqlx::{PgPool, postgres::PgPoolOptions};
    use syrup_rail::{
        BillingScopeId, CardLastFour, CurrencyCode, HostChargeTargetId, PaymentAttemptId,
        PaymentCardBrand, PaymentCardDisplay, PlanKey, SubscriberId, SubscriptionId,
        SubscriptionPhase,
    };

    use super::*;

    fn id(value: u128) -> Uuid {
        Uuid::from_u128(value)
    }

    fn attempt(value: u128) -> PaymentAttemptId {
        PaymentAttemptId::new(id(value))
    }

    fn subscription(value: u128) -> SubscriptionId {
        SubscriptionId::new(id(value))
    }

    #[test]
    fn every_domain_variant_has_an_explicit_redacted_host_mapping() {
        let subject =
            BillingEventSubject::new(BillingScopeId::new(id(1)), SubscriberId::new(id(2)));
        let plan_key = PlanKey::new("base_subscription").unwrap();
        let charge = ChargeAmount::new(1_000, CurrencyCode::new("USD").unwrap()).unwrap();
        let started_at = Utc.with_ymd_and_hms(2026, 8, 11, 12, 0, 0).unwrap();
        let ended_at = Utc.with_ymd_and_hms(2026, 9, 11, 12, 0, 0).unwrap();
        let period = BillingPeriod::new(started_at, ended_at).unwrap();
        let events = [
            BillingEvent::SubscriptionStarted {
                attempt_id: attempt(10),
                subscription_id: subscription(20),
                plan_key: plan_key.clone(),
                charge,
                period: period.clone(),
                phase: SubscriptionPhase::Recurring,
            },
            BillingEvent::SubscriptionRenewed {
                attempt_id: attempt(11),
                subscription_id: subscription(20),
                plan_key: plan_key.clone(),
                charge,
                period,
            },
            BillingEvent::SubscriptionPaymentFailed {
                attempt_id: attempt(12),
                subscription_id: subscription(20),
                plan_key: plan_key.clone(),
                disposition: SubscriptionPaymentFailureDisposition::RetryScheduled {
                    retry_at: ended_at,
                },
                access: SubscriptionPaymentFailureAccess::ContinuesDuringDunning,
            },
            BillingEvent::SubscriptionEnded {
                attempt_id: attempt(12),
                subscription_id: subscription(20),
                plan_key: plan_key.clone(),
                reason: SubscriptionEndReason::NonPayment,
                ended_at,
                access_ends_at: ended_at,
            },
            BillingEvent::SubscriptionCanceled {
                subscription_id: subscription(20),
                plan_key: plan_key.clone(),
                access_ends_at: ended_at,
            },
            BillingEvent::PaymentMethodChanged {
                attempt_id: attempt(13),
                subscription_id: subscription(20),
                plan_key,
                card: Some(PaymentCardDisplay::new(
                    PaymentCardBrand::from_provider("Visa api_key=super-secret").unwrap(),
                    CardLastFour::from_provider("4242").unwrap(),
                )),
            },
            BillingEvent::HostChargePaid {
                attempt_id: attempt(14),
                target_id: HostChargeTargetId::new(id(30)),
                charge,
            },
        ];
        let expected_payloads = [
            serde_json::json!({
                "type": "subscription_started",
                "data": {
                    "attempt_id": id(10),
                    "subscription_id": id(20),
                    "plan_key": "base_subscription",
                    "charge": { "cents": 1_000, "currency": "USD" },
                    "period": { "start_at": started_at, "end_at": ended_at },
                    "phase": "recurring",
                }
            }),
            serde_json::json!({
                "type": "subscription_renewed",
                "data": {
                    "attempt_id": id(11),
                    "subscription_id": id(20),
                    "plan_key": "base_subscription",
                    "charge": { "cents": 1_000, "currency": "USD" },
                    "period": { "start_at": started_at, "end_at": ended_at },
                }
            }),
            serde_json::json!({
                "type": "subscription_payment_failed",
                "data": {
                    "attempt_id": id(12),
                    "subscription_id": id(20),
                    "plan_key": "base_subscription",
                    "disposition": { "kind": "retry_scheduled", "retry_at": ended_at },
                    "access": { "kind": "continues_during_dunning" },
                }
            }),
            serde_json::json!({
                "type": "subscription_ended",
                "data": {
                    "attempt_id": id(12),
                    "subscription_id": id(20),
                    "plan_key": "base_subscription",
                    "reason": "non_payment",
                    "ended_at": ended_at,
                    "access_ends_at": ended_at,
                }
            }),
            serde_json::json!({
                "type": "subscription_canceled",
                "data": {
                    "subscription_id": id(20),
                    "plan_key": "base_subscription",
                    "access_ends_at": ended_at,
                }
            }),
            serde_json::json!({
                "type": "payment_method_changed",
                "data": {
                    "attempt_id": id(13),
                    "subscription_id": id(20),
                    "plan_key": "base_subscription",
                    "card": { "brand": "other", "last_four": "4242" },
                }
            }),
            serde_json::json!({
                "type": "host_charge_paid",
                "data": {
                    "attempt_id": id(14),
                    "target_id": id(30),
                    "charge": { "cents": 1_000, "currency": "USD" },
                }
            }),
        ];
        let expected_semantic_ids = [id(20), id(11), id(12), id(20), id(20), id(13), id(30)];

        let mut kinds = Vec::new();
        for (index, event) in events.iter().enumerate() {
            let envelope = HostBillingEventEnvelopeV1::from_domain(
                id(100 + index as u128),
                started_at,
                subject,
                event,
            );
            let value = serde_json::to_value(&envelope).unwrap();
            let object = value.as_object().unwrap();
            assert_eq!(
                object.keys().map(String::as_str).collect::<BTreeSet<_>>(),
                BTreeSet::from([
                    "billing_scope_id",
                    "event_id",
                    "kind",
                    "occurred_at",
                    "payload",
                    "schema_version",
                    "semantic_key",
                    "subscriber_id",
                ])
            );
            assert_eq!(
                value["payload"]
                    .as_object()
                    .unwrap()
                    .keys()
                    .map(String::as_str)
                    .collect::<BTreeSet<_>>(),
                BTreeSet::from(["data", "type"])
            );
            assert_eq!(value["kind"], envelope.kind());
            assert_eq!(value["semantic_key"]["kind"], envelope.semantic_kind());
            assert_eq!(
                value["semantic_key"]["identity"],
                expected_semantic_ids[index].to_string()
            );
            assert_eq!(value["payload"]["type"], envelope.kind());
            assert_eq!(value["payload"], expected_payloads[index]);
            let reconstructed = HostBillingEventEnvelopeV1::from_persisted_parts(
                id(200 + index as u128),
                started_at,
                envelope.event_version(),
                envelope.billing_scope_id(),
                envelope.subscriber_id(),
                envelope.kind(),
                envelope.semantic_kind(),
                envelope.semantic_id(),
                value["payload"].clone(),
            )
            .unwrap();
            assert!(envelope.replay_matches(&reconstructed));
            let json = serde_json::to_string(&value).unwrap();
            assert_eq!(envelope.schema_version(), 1);
            assert_eq!(envelope.billing_scope_id(), id(1));
            assert_eq!(envelope.subscriber_id(), id(2));
            assert_eq!(envelope.kind(), envelope.semantic_kind());
            assert_eq!(envelope.event_id(), id(100 + index as u128));
            assert_eq!(envelope.occurred_at(), started_at);
            assert!(!json.contains("payment_token"));
            assert!(!json.contains("billing_contact"));
            assert!(!json.contains("gateway_transaction"));
            assert!(!json.contains("idempotency"));
            assert!(!json.contains("super-secret"));
            kinds.push(envelope.replay.kind);
        }

        assert_eq!(
            kinds,
            [
                HostBillingEventKindV1::SubscriptionStarted,
                HostBillingEventKindV1::SubscriptionRenewed,
                HostBillingEventKindV1::SubscriptionPaymentFailed,
                HostBillingEventKindV1::SubscriptionEnded,
                HostBillingEventKindV1::SubscriptionCanceled,
                HostBillingEventKindV1::PaymentMethodChanged,
                HostBillingEventKindV1::HostChargePaid,
            ]
        );
    }

    #[test]
    fn replay_contract_excludes_first_write_facts_and_rejects_every_stable_mismatch() {
        let subject =
            BillingEventSubject::new(BillingScopeId::new(id(1)), SubscriberId::new(id(2)));
        let event = BillingEvent::PaymentMethodChanged {
            attempt_id: attempt(13),
            subscription_id: subscription(20),
            plan_key: PlanKey::new("base_subscription").unwrap(),
            card: Some(PaymentCardDisplay::new(
                PaymentCardBrand::Visa,
                CardLastFour::from_provider("4242").unwrap(),
            )),
        };
        let occurred_at = Utc.with_ymd_and_hms(2026, 8, 11, 12, 0, 0).unwrap();
        let original =
            HostBillingEventEnvelopeV1::from_domain(id(100), occurred_at, subject, &event);

        let mut same_replay = original.clone();
        same_replay.event_id = id(101);
        same_replay.occurred_at = Utc.with_ymd_and_hms(2026, 8, 11, 12, 0, 1).unwrap();
        assert!(original.replay_matches(&same_replay));
        assert_eq!(original.replay_contract(), same_replay.replay_contract());

        let replay_json = serde_json::to_value(original.replay_contract()).unwrap();
        assert_eq!(
            replay_json
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([
                "billing_scope_id",
                "kind",
                "payload",
                "schema_version",
                "semantic_key",
                "subscriber_id",
            ])
        );
        assert!(replay_json.get("event_id").is_none());
        assert!(replay_json.get("occurred_at").is_none());

        let reconstructed = HostBillingEventEnvelopeV1::from_persisted_parts(
            id(101),
            Utc.with_ymd_and_hms(2026, 8, 11, 12, 0, 1).unwrap(),
            original.event_version(),
            original.billing_scope_id(),
            original.subscriber_id(),
            original.kind(),
            original.semantic_kind(),
            original.semantic_id(),
            original.persisted_payload().unwrap(),
        )
        .unwrap();
        assert!(original.replay_matches(&reconstructed));
        assert_eq!(reconstructed.event_id(), id(101));
        assert_ne!(reconstructed.occurred_at(), original.occurred_at());

        let decode_parts = |version, event_kind: &str, semantic_kind: &str, payload| {
            HostBillingEventReplayV1::from_persisted_parts(
                version,
                original.billing_scope_id(),
                original.subscriber_id(),
                event_kind,
                semantic_kind,
                original.semantic_id(),
                payload,
            )
        };
        assert_eq!(
            decode_parts(
                2,
                original.kind(),
                original.semantic_kind(),
                original.persisted_payload().unwrap(),
            )
            .unwrap_err(),
            HostBillingEventReplayDecodeErrorV1::UnsupportedVersion
        );
        assert_eq!(
            decode_parts(
                original.event_version(),
                "future_event",
                original.semantic_kind(),
                original.persisted_payload().unwrap(),
            )
            .unwrap_err(),
            HostBillingEventReplayDecodeErrorV1::UnknownEventKind
        );
        assert_eq!(
            decode_parts(
                original.event_version(),
                original.kind(),
                "future_event",
                original.persisted_payload().unwrap(),
            )
            .unwrap_err(),
            HostBillingEventReplayDecodeErrorV1::UnknownSemanticKind
        );
        assert_eq!(
            decode_parts(
                original.event_version(),
                original.kind(),
                original.semantic_kind(),
                serde_json::json!({ "type": "future_event", "data": {} }),
            )
            .unwrap_err(),
            HostBillingEventReplayDecodeErrorV1::InvalidPayload
        );

        let mut changed = original.clone();
        changed.replay.schema_version += 1;
        assert!(!original.replay_matches(&changed));

        let mut changed = original.clone();
        changed.replay.billing_scope_id = id(3);
        assert!(!original.replay_matches(&changed));

        let mut changed = original.clone();
        changed.replay.subscriber_id = id(4);
        assert!(!original.replay_matches(&changed));

        let mut changed = original.clone();
        changed.replay.kind = HostBillingEventKindV1::SubscriptionRenewed;
        assert!(!original.replay_matches(&changed));

        let mut changed = original.clone();
        changed.replay.semantic_key.kind = HostBillingEventKindV1::SubscriptionRenewed;
        assert!(!original.replay_matches(&changed));

        let mut changed = original.clone();
        changed.replay.semantic_key.identity = id(5);
        assert!(!original.replay_matches(&changed));

        let mut changed = original.clone();
        let HostBillingEventPayloadV1::PaymentMethodChanged { card, .. } =
            &mut changed.replay.payload
        else {
            panic!("fixture must map to payment_method_changed")
        };
        card.as_mut().unwrap().last_four = "1111".to_owned();
        assert!(!original.replay_matches(&changed));

        let debug = format!("{original:?} {:?}", original.replay_contract());
        assert!(debug.contains("PaymentMethodChanged"));
        for sensitive in [
            "4242",
            "Visa",
            "super-secret",
            "base_subscription",
            &id(1).to_string(),
            &id(2).to_string(),
            &id(13).to_string(),
        ] {
            assert!(!debug.contains(sensitive), "Debug leaked {sensitive}");
        }
    }

    #[test]
    fn version_one_owns_every_nested_enum_label() {
        assert_eq!(
            serde_json::to_value(HostSubscriptionPhaseV1::from(SubscriptionPhase::PaidTrial))
                .unwrap(),
            serde_json::json!("paid_trial")
        );
        assert_eq!(
            serde_json::to_value(HostSubscriptionPhaseV1::from(SubscriptionPhase::Recurring))
                .unwrap(),
            serde_json::json!("recurring")
        );

        let brands = [
            (PaymentCardBrand::Visa, "visa"),
            (PaymentCardBrand::Mastercard, "mastercard"),
            (PaymentCardBrand::AmericanExpress, "american_express"),
            (PaymentCardBrand::Discover, "discover"),
            (PaymentCardBrand::Jcb, "jcb"),
            (PaymentCardBrand::DinersClub, "diners_club"),
            (PaymentCardBrand::UnionPay, "union_pay"),
            (PaymentCardBrand::Maestro, "maestro"),
            (PaymentCardBrand::Other, "other"),
        ];
        for (brand, expected) in brands {
            assert_eq!(
                serde_json::to_value(HostPaymentCardBrandV1::from(&brand)).unwrap(),
                serde_json::json!(expected)
            );
        }
    }

    #[tokio::test]
    async fn durable_outbox_insert_replay_and_conflict_are_atomic() -> Result<(), Box<dyn Error>> {
        let harness =
            PostgresHarness::start(HarnessConfig::new("sr_obx_v1")?.with_connection_budget(2)?)
                .await?;
        let lease = harness.empty_database().await?;
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect(lease.database_url())
            .await?;
        create_host_outbox(&pool).await?;

        let subject =
            BillingEventSubject::new(BillingScopeId::new(id(1)), SubscriberId::new(id(2)));
        let event = BillingEvent::PaymentMethodChanged {
            attempt_id: attempt(13),
            subscription_id: subscription(20),
            plan_key: PlanKey::new("base_subscription").unwrap(),
            card: Some(PaymentCardDisplay::new(
                PaymentCardBrand::Visa,
                CardLastFour::from_provider("4242").unwrap(),
            )),
        };

        let mut transaction = pool.begin().await?;
        let inserted = append_host_billing_event_v1(&mut transaction, subject, &event).await?;
        assert!(inserted.was_inserted());
        let first_write = inserted.envelope().clone();
        transaction.commit().await?;

        let mut transaction = pool.begin().await?;
        let replayed = append_host_billing_event_v1(&mut transaction, subject, &event).await?;
        assert!(!replayed.was_inserted());
        assert_eq!(replayed.envelope().event_id(), first_write.event_id());
        assert_eq!(replayed.envelope().occurred_at(), first_write.occurred_at());
        assert!(first_write.replay_matches(replayed.envelope()));
        transaction.commit().await?;

        let conflicting_event = BillingEvent::PaymentMethodChanged {
            attempt_id: attempt(13),
            subscription_id: subscription(20),
            plan_key: PlanKey::new("base_subscription").unwrap(),
            card: Some(PaymentCardDisplay::new(
                PaymentCardBrand::Visa,
                CardLastFour::from_provider("1111").unwrap(),
            )),
        };
        let mut transaction = pool.begin().await?;
        let conflict = append_host_billing_event_v1(&mut transaction, subject, &conflicting_event)
            .await
            .unwrap_err();
        assert_eq!(conflict.to_string(), "billing event append failed");
        let debug = format!("{conflict:?}");
        assert!(!debug.contains("1111"));
        assert!(!debug.contains("4242"));
        transaction.rollback().await?;

        let row_count: i64 = sqlx::query_scalar("SELECT count(*) FROM host_billing_outbox")
            .fetch_one(&pool)
            .await?;
        assert_eq!(row_count, 1);
        let stored_last_four: String = sqlx::query_scalar(
            "SELECT payload #>> '{data,card,last_four}' FROM host_billing_outbox",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(stored_last_four, "4242");

        pool.close().await;
        lease.cleanup().await?;
        harness.shutdown().await?;
        Ok(())
    }

    async fn create_host_outbox(pool: &PgPool) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            CREATE TABLE host_billing_outbox (
                event_id uuid PRIMARY KEY,
                occurred_at timestamptz NOT NULL,
                billing_scope_id uuid NOT NULL,
                subscriber_id uuid NOT NULL,
                event_kind text NOT NULL,
                event_version smallint NOT NULL,
                semantic_kind text NOT NULL,
                semantic_id uuid NOT NULL,
                payload jsonb NOT NULL,
                UNIQUE (billing_scope_id, semantic_kind, semantic_id)
            )
            "#,
        )
        .execute(pool)
        .await?;
        Ok(())
    }
}
