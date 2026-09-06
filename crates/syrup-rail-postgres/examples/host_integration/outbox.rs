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
//! conflicting row through the same connection, compare every untouched
//! scalar and JSONB field, and only then reconstruct its typed replay contract.
//! `event_id` and `occurred_at` are facts of the first successful write and are
//! deliberately excluded from replay equality. Outbox rows are append-only;
//! changing a durable row would invalidate that comparison.

use std::{error::Error, fmt};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use syrup_rail::{
    BillingEvent, BillingEventKey, BillingEventSubject, BillingPeriod, ChargeAmount,
    PaymentCardBrand, SubscriptionEndReason, SubscriptionPaymentFailureAccess,
    SubscriptionPaymentFailureDisposition, SubscriptionPhase,
};
use uuid::Uuid;

#[cfg(test)]
use self::persistence::HostBillingEventReplayConflictV1;
pub use self::persistence::append_host_billing_event_v1;

#[path = "outbox/persistence.rs"]
mod persistence;

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
    /// A structurally equal replay contract was already durable.
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
    /// reject the conflict. JSON must also round-trip through the V1 DTO
    /// without losing or normalizing any field.
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
        let persisted_payload = payload;
        let payload = serde_json::from_value(persisted_payload.clone())
            .map_err(|_| HostBillingEventReplayDecodeErrorV1::InvalidPayload)?;
        let reconstructed_payload = serde_json::to_value(&payload)
            .map_err(|_| HostBillingEventReplayDecodeErrorV1::InvalidPayload)?;
        if reconstructed_payload != persisted_payload {
            return Err(HostBillingEventReplayDecodeErrorV1::InvalidPayload);
        }

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
                outcome,
            } => Self::SubscriptionPaymentFailed {
                attempt_id: *attempt_id.as_uuid(),
                subscription_id: *subscription_id.as_uuid(),
                plan_key: plan_key.as_str().to_owned(),
                disposition: outcome.disposition().into(),
                access: outcome.access().into(),
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
#[path = "outbox/tests.rs"]
mod tests;
