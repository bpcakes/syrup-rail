use std::{error::Error, fmt};

use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgConnection};
use syrup_rail::{BillingEvent, BillingEventSubject};
use syrup_rail_postgres::BillingEventWriteError;
use uuid::Uuid;

use super::{
    HostBillingEventAppendOutcomeV1, HostBillingEventEnvelopeV1,
    HostBillingEventReplayDecodeErrorV1, HostBillingEventReplayV1,
};

/// Appends one version-1 event through the caller's existing transaction.
///
/// The insert owns first-write `event_id` and `occurred_at` generation. When
/// the semantic key already exists, this function selects that exact row on
/// the same connection and compares its untouched scalar columns and JSONB
/// payload before typed decoding. It returns
/// [`HostBillingEventAppendOutcomeV1::Replayed`] only after exact structural
/// replay equality. A mismatch is returned as a value-redacted
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
    .bind(&payload)
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
    if !existing.replay_matches(&candidate, &payload) {
        return Err(BillingEventWriteError::new(
            HostBillingEventReplayConflictV1,
        ));
    }
    let existing = existing
        .into_envelope()
        .map_err(BillingEventWriteError::new)?;

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

impl HostBillingEventPersistedV1 {
    fn replay_matches(
        &self,
        candidate: &HostBillingEventReplayV1,
        candidate_payload: &serde_json::Value,
    ) -> bool {
        self.event_version == candidate.event_version()
            && self.billing_scope_id == candidate.billing_scope_id()
            && self.subscriber_id == candidate.subscriber_id()
            && self.event_kind == candidate.kind()
            && self.semantic_kind == candidate.semantic_kind()
            && self.semantic_id == candidate.semantic_id()
            && &self.payload == candidate_payload
    }

    fn into_envelope(
        self,
    ) -> Result<HostBillingEventEnvelopeV1, HostBillingEventReplayDecodeErrorV1> {
        HostBillingEventEnvelopeV1::from_persisted_parts(
            self.event_id,
            self.occurred_at,
            self.event_version,
            self.billing_scope_id,
            self.subscriber_id,
            &self.event_kind,
            &self.semantic_kind,
            self.semantic_id,
            self.payload,
        )
    }
}

#[derive(Debug)]
pub(super) struct HostBillingEventReplayConflictV1;

impl fmt::Display for HostBillingEventReplayConflictV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("host billing event semantic key conflicts with durable content")
    }
}

impl Error for HostBillingEventReplayConflictV1 {}
