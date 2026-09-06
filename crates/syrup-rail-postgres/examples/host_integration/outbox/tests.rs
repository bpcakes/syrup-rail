use std::{collections::BTreeSet, time::Duration};

use chrono::TimeZone;
use postgres_test_harness::{HarnessConfig, PostgresHarness};
use sqlx::{PgPool, postgres::PgPoolOptions};
use syrup_rail::{
    BillingScopeId, CardLastFour, CurrencyCode, HostChargeTargetId, PaymentAttemptId,
    PaymentCardBrand, PaymentCardDisplay, PlanKey, SubscriberId, SubscriptionId,
    SubscriptionPaymentFailureAccess, SubscriptionPaymentFailureOutcome, SubscriptionPhase,
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
    let subject = BillingEventSubject::new(BillingScopeId::new(id(1)), SubscriberId::new(id(2)));
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
            outcome: SubscriptionPaymentFailureOutcome::RetryScheduled {
                retry_at: ended_at,
                access: SubscriptionPaymentFailureAccess::ContinuesDuringDunning,
            },
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
fn version_one_failure_payloads_preserve_all_legacy_projection_matrices() {
    let subject = BillingEventSubject::new(BillingScopeId::new(id(1)), SubscriberId::new(id(2)));
    let failed_at = Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap();
    let retry_at = Utc.with_ymd_and_hms(2026, 8, 2, 0, 0, 0).unwrap();
    let terminal_at = Utc.with_ymd_and_hms(2026, 8, 3, 0, 0, 0).unwrap();
    let cases = [
        (
            SubscriptionPaymentFailureOutcome::RetryScheduled {
                retry_at,
                access: SubscriptionPaymentFailureAccess::ContinuesDuringDunning,
            },
            serde_json::json!({ "kind": "retry_scheduled", "retry_at": retry_at }),
            serde_json::json!({ "kind": "continues_during_dunning" }),
        ),
        (
            SubscriptionPaymentFailureOutcome::RetryScheduled {
                retry_at,
                access: SubscriptionPaymentFailureAccess::Ended {
                    access_ended_at: failed_at,
                },
            },
            serde_json::json!({ "kind": "retry_scheduled", "retry_at": retry_at }),
            serde_json::json!({ "kind": "ended", "access_ended_at": failed_at }),
        ),
        (
            SubscriptionPaymentFailureOutcome::DunningExhausted {
                exhausted_at: terminal_at,
                access_ended_at: terminal_at,
            },
            serde_json::json!({ "kind": "dunning_exhausted", "exhausted_at": terminal_at }),
            serde_json::json!({ "kind": "ended", "access_ended_at": terminal_at }),
        ),
        (
            SubscriptionPaymentFailureOutcome::SubscriptionEnded {
                ended_at: terminal_at,
                access_ended_at: failed_at,
            },
            serde_json::json!({ "kind": "subscription_ended", "ended_at": terminal_at }),
            serde_json::json!({ "kind": "ended", "access_ended_at": failed_at }),
        ),
    ];

    for (index, (outcome, disposition, access)) in cases.into_iter().enumerate() {
        let event = BillingEvent::SubscriptionPaymentFailed {
            attempt_id: attempt(40 + index as u128),
            subscription_id: subscription(20),
            plan_key: PlanKey::new("base_subscription").unwrap(),
            outcome,
        };
        let envelope = HostBillingEventEnvelopeV1::from_domain(
            id(100 + index as u128),
            failed_at,
            subject,
            &event,
        );
        let value = serde_json::to_value(&envelope).unwrap();
        assert_eq!(value["payload"]["data"]["disposition"], disposition);
        assert_eq!(value["payload"]["data"]["access"], access);

        let replay = HostBillingEventEnvelopeV1::from_persisted_parts(
            id(200 + index as u128),
            failed_at,
            envelope.event_version(),
            envelope.billing_scope_id(),
            envelope.subscriber_id(),
            envelope.kind(),
            envelope.semantic_kind(),
            envelope.semantic_id(),
            value["payload"].clone(),
        )
        .unwrap();
        assert!(envelope.replay_matches(&replay));
    }
}

#[test]
fn replay_contract_excludes_first_write_facts_and_rejects_every_stable_mismatch() {
    let subject = BillingEventSubject::new(BillingScopeId::new(id(1)), SubscriberId::new(id(2)));
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
    let original = HostBillingEventEnvelopeV1::from_domain(id(100), occurred_at, subject, &event);

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

    let mut payload_with_extra_field = original.persisted_payload().unwrap();
    payload_with_extra_field
        .as_object_mut()
        .unwrap()
        .insert("unexpected".to_owned(), serde_json::json!(true));
    assert_eq!(
        decode_parts(
            original.event_version(),
            original.kind(),
            original.semantic_kind(),
            payload_with_extra_field,
        )
        .unwrap_err(),
        HostBillingEventReplayDecodeErrorV1::InvalidPayload
    );

    let mut payload_with_extra_nested_field = original.persisted_payload().unwrap();
    payload_with_extra_nested_field
        .pointer_mut("/data/card")
        .and_then(serde_json::Value::as_object_mut)
        .unwrap()
        .insert("provider_hint".to_owned(), serde_json::json!("private"));
    assert_eq!(
        decode_parts(
            original.event_version(),
            original.kind(),
            original.semantic_kind(),
            payload_with_extra_nested_field,
        )
        .unwrap_err(),
        HostBillingEventReplayDecodeErrorV1::InvalidPayload
    );

    let normalized_timestamp_payload = serde_json::json!({
        "type": "subscription_canceled",
        "data": {
            "subscription_id": id(20),
            "plan_key": "base_subscription",
            "access_ends_at": "2026-08-11T12:00:00+00:00",
        }
    });
    assert_eq!(
        HostBillingEventReplayV1::from_persisted_parts(
            original.event_version(),
            original.billing_scope_id(),
            original.subscriber_id(),
            "subscription_canceled",
            "subscription_canceled",
            original.semantic_id(),
            normalized_timestamp_payload,
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
    let HostBillingEventPayloadV1::PaymentMethodChanged { card, .. } = &mut changed.replay.payload
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
        serde_json::to_value(HostSubscriptionPhaseV1::from(SubscriptionPhase::PaidTrial)).unwrap(),
        serde_json::json!("paid_trial")
    );
    assert_eq!(
        serde_json::to_value(HostSubscriptionPhaseV1::from(SubscriptionPhase::Recurring)).unwrap(),
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
async fn durable_outbox_concurrent_insert_replay_and_conflicts_are_atomic()
-> Result<(), Box<dyn Error>> {
    let harness =
        PostgresHarness::start(HarnessConfig::new("sr_obx_v1")?.with_connection_budget(2)?).await?;
    let lease = harness.empty_database().await?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(lease.database_url())
        .await?;
    create_host_outbox(&pool).await?;

    let subject = BillingEventSubject::new(BillingScopeId::new(id(1)), SubscriberId::new(id(2)));
    let event = BillingEvent::PaymentMethodChanged {
        attempt_id: attempt(13),
        subscription_id: subscription(20),
        plan_key: PlanKey::new("base_subscription").unwrap(),
        card: Some(PaymentCardDisplay::new(
            PaymentCardBrand::Visa,
            CardLastFour::from_provider("4242").unwrap(),
        )),
    };

    let mut first_transaction = pool.begin().await?;
    let inserted = append_host_billing_event_v1(&mut first_transaction, subject, &event).await?;
    assert!(inserted.was_inserted());
    let first_write = inserted.envelope().clone();

    let mut concurrent_transaction = pool.begin().await?;
    let mut concurrent_append = Box::pin(append_host_billing_event_v1(
        &mut concurrent_transaction,
        subject,
        &event,
    ));
    tokio::select! {
        result = &mut concurrent_append => {
            panic!("concurrent same-key append completed before the first write committed: {result:?}");
        }
        () = tokio::time::sleep(Duration::from_millis(100)) => {}
    }
    first_transaction.commit().await?;
    let replayed = concurrent_append.await?;
    assert!(!replayed.was_inserted());
    assert_eq!(replayed.envelope().event_id(), first_write.event_id());
    assert_eq!(replayed.envelope().occurred_at(), first_write.occurred_at());
    assert!(first_write.replay_matches(replayed.envelope()));
    concurrent_transaction.commit().await?;

    sqlx::query(
        "UPDATE host_billing_outbox SET payload = payload || jsonb_build_object('unexpected', true)",
    )
    .execute(&pool)
    .await?;
    let mut transaction = pool.begin().await?;
    let conflict = append_host_billing_event_v1(&mut transaction, subject, &event)
        .await
        .unwrap_err();
    assert!(
        conflict
            .into_source()
            .downcast::<HostBillingEventReplayConflictV1>()
            .is_ok()
    );
    transaction.rollback().await?;
    sqlx::query("UPDATE host_billing_outbox SET payload = payload - 'unexpected'")
        .execute(&pool)
        .await?;

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
    let stored_last_four: String =
        sqlx::query_scalar("SELECT payload #>> '{data,card,last_four}' FROM host_billing_outbox")
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
