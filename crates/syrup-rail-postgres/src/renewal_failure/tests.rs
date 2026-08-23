use super::*;
use syrup_rail::{
    BillingScopeId, DunningExhaustion, DunningSchedule, GatewayAccountId, GatewayConfigurationId,
    SubscriberId,
};

fn timestamp(seconds: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(seconds, 0).expect("valid timestamp")
}

fn failure(value: u128, resolved_at: i64) -> ResolvedAutomaticRenewalFailure {
    ResolvedAutomaticRenewalFailure {
        attempt_id: PaymentAttemptId::new(Uuid::from_u128(value)),
        resolved_at: timestamp(resolved_at),
    }
}

const fn causal_history(automatic: AutomaticRenewalFailureHistory) -> PastDueCausalHistory {
    PastDueCausalHistory {
        automatic,
        legacy_recovery_access_ended_at: None,
    }
}

const fn causal_history_after_legacy_recovery(
    automatic: AutomaticRenewalFailureHistory,
    access_ended_at: DateTime<Utc>,
) -> PastDueCausalHistory {
    PastDueCausalHistory {
        automatic,
        legacy_recovery_access_ended_at: Some(access_ended_at),
    }
}

fn validated_attempt(
    attempt_id: u128,
    submitted_at: i64,
    resolved_at: i64,
) -> ValidatedRenewalFailureAttempt {
    ValidatedRenewalFailureAttempt {
        identity: PaymentAttemptIdentity::new(
            PaymentAttemptId::new(Uuid::from_u128(attempt_id)),
            BillingScopeId::new(Uuid::from_u128(10)),
            SubscriberId::new(Uuid::from_u128(11)),
            GatewayAccountId::new(Uuid::from_u128(12)),
            GatewayConfigurationId::new(Uuid::from_u128(13)),
        ),
        subscription_id: SubscriptionId::new(Uuid::from_u128(14)),
        plan_key: PlanKey::new("decision-test").expect("valid plan key"),
        period_start_at: timestamp(100),
        submitted_at: timestamp(submitted_at),
        resolved_at: timestamp(resolved_at),
    }
}

fn locked_state(
    current: FailureProjection,
    delays: &[u32],
    exhaustion: DunningExhaustion,
    access: PastDueAccessPolicy,
) -> LockedRenewalFailureState {
    LockedRenewalFailureState {
        current,
        next_renewal_at: timestamp(100),
        policy: RenewalFailurePolicy::new(
            DunningSchedule::from_seconds(delays.iter().copied()).expect("valid dunning schedule"),
            exhaustion,
            access,
        ),
    }
}

#[test]
fn failure_history_encodes_empty_first_and_repeated_states() {
    let first = failure(1, 100);
    let previous = failure(2, 200);
    let latest = failure(3, 300);

    assert_eq!(
        automatic_renewal_failure_history_from_resolved(&[]).expect("empty history is valid"),
        AutomaticRenewalFailureHistory::Empty
    );
    assert_eq!(
        automatic_renewal_failure_history_from_resolved(&[first]).expect("first failure is valid"),
        AutomaticRenewalFailureHistory::First { failure: first }
    );
    assert_eq!(
        automatic_renewal_failure_history_from_resolved(&[first, previous, latest])
            .expect("repeated failures are valid"),
        AutomaticRenewalFailureHistory::Repeated {
            count: 3,
            first,
            previous,
            latest,
        }
    );
}

#[test]
fn failure_history_rejects_a_count_that_cannot_be_persisted() {
    let failure = failure(1, 100);
    let resolved = vec![failure; usize::from(u16::MAX) + 1];

    assert!(matches!(
        automatic_renewal_failure_history_from_resolved(&resolved),
        Err(RenewalFailureStoreError::InvalidState(
            INVALID_RENEWAL_FAILURE_STATE
        ))
    ));
}

#[test]
fn first_failure_decision_projects_retry_and_event_without_persistence() {
    let attempt = validated_attempt(1, 150, 200);
    let locked = locked_state(
        FailureProjection {
            status: SubscriptionStatus::Active,
            next_payment_attempt_at: Some(timestamp(100)),
            unpaid_at: None,
        },
        &[60],
        DunningExhaustion::RemainPastDue,
        PastDueAccessPolicy::SuspendImmediately,
    );
    let decision = decide_renewal_failure(
        &attempt,
        &locked,
        causal_history(AutomaticRenewalFailureHistory::First {
            failure: failure(1, 200),
        }),
    )
    .expect("first failure is deterministic");
    let RenewalFailureDecision::Apply(transition) = decision else {
        panic!("first failure must apply")
    };
    assert_eq!(
        transition.disposition,
        RenewalFailureDisposition::RetryScheduled {
            retry_at: timestamp(260)
        }
    );
    assert_eq!(
        transition.after,
        FailureProjection {
            status: SubscriptionStatus::PastDue,
            next_payment_attempt_at: Some(timestamp(260)),
            unpaid_at: None,
        }
    );
    assert_eq!(transition.events.len(), 1);
    assert!(matches!(
        transition.events[0],
        BillingEvent::SubscriptionPaymentFailed {
            outcome: SubscriptionPaymentFailureOutcome::RetryScheduled {
                retry_at,
                access: SubscriptionPaymentFailureAccess::Ended { access_ended_at },
            },
            ..
        } if retry_at == timestamp(260) && access_ended_at == timestamp(200)
    ));
}

#[test]
fn failure_event_projection_carries_the_complete_access_consequence() {
    let history = causal_history(AutomaticRenewalFailureHistory::Repeated {
        count: 2,
        first: failure(1, 200),
        previous: failure(1, 200),
        latest: failure(2, 300),
    });
    let schedule = DunningSchedule::from_seconds([60]).expect("valid dunning schedule");

    let suspend = RenewalFailurePolicy::new(
        schedule.clone(),
        DunningExhaustion::RemainPastDue,
        PastDueAccessPolicy::SuspendImmediately,
    );
    assert_eq!(
        failure_event_projection(
            &suspend,
            history,
            RenewalFailureDisposition::RetryScheduled {
                retry_at: timestamp(360),
            },
        )
        .expect("immediate suspension has a causal boundary")
        .outcome
        .access(),
        SubscriptionPaymentFailureAccess::Ended {
            access_ended_at: timestamp(200),
        }
    );

    let continue_access = RenewalFailurePolicy::new(
        schedule,
        DunningExhaustion::RemainPastDue,
        PastDueAccessPolicy::ContinueUntilDunningExhausted,
    );
    assert_eq!(
        failure_event_projection(
            &continue_access,
            history,
            RenewalFailureDisposition::RetryScheduled {
                retry_at: timestamp(360),
            },
        )
        .expect("scheduled dunning retains access")
        .outcome
        .access(),
        SubscriptionPaymentFailureAccess::ContinuesDuringDunning
    );
    assert_eq!(
        failure_event_projection(
            &continue_access,
            history,
            RenewalFailureDisposition::RemainPastDue {
                exhausted_at: timestamp(300),
            },
        )
        .expect("exhausted dunning has a causal boundary")
        .outcome
        .access(),
        SubscriptionPaymentFailureAccess::Ended {
            access_ended_at: timestamp(300),
        }
    );
}

#[test]
fn first_automatic_failure_after_legacy_recovery_preserves_suspension_boundary() {
    let attempt = validated_attempt(1, 150, 200);
    let locked = locked_state(
        FailureProjection {
            status: SubscriptionStatus::PastDue,
            next_payment_attempt_at: Some(timestamp(100)),
            unpaid_at: None,
        },
        &[],
        DunningExhaustion::MarkUnpaid,
        PastDueAccessPolicy::SuspendImmediately,
    );
    let decision = decide_renewal_failure(
        &attempt,
        &locked,
        causal_history_after_legacy_recovery(
            AutomaticRenewalFailureHistory::First {
                failure: failure(1, 200),
            },
            timestamp(50),
        ),
    )
    .expect("legacy recovery provenance admits the first automatic failure");
    let RenewalFailureDecision::Apply(transition) = decision else {
        panic!("first automatic failure after cutover must apply")
    };
    assert_eq!(
        transition.after,
        FailureProjection {
            status: SubscriptionStatus::Unpaid,
            next_payment_attempt_at: None,
            unpaid_at: Some(timestamp(200)),
        }
    );
    assert!(matches!(
        transition.events.as_slice(),
        [
            BillingEvent::SubscriptionPaymentFailed {
                outcome: SubscriptionPaymentFailureOutcome::SubscriptionEnded {
                    access_ended_at: failure_access_ended_at,
                    ..
                },
                ..
            },
            BillingEvent::SubscriptionEnded { access_ends_at, .. }
        ] if *failure_access_ended_at == timestamp(50) && *access_ends_at == timestamp(50)
    ));
}

#[test]
fn decision_is_idempotent_for_applied_state_and_older_attempts() {
    let attempt = validated_attempt(1, 150, 200);
    let applied = locked_state(
        projection(RenewalFailureDisposition::RetryScheduled {
            retry_at: timestamp(260),
        }),
        &[60],
        DunningExhaustion::RemainPastDue,
        PastDueAccessPolicy::SuspendImmediately,
    );
    assert_eq!(
        decide_renewal_failure(
            &attempt,
            &applied,
            causal_history(AutomaticRenewalFailureHistory::First {
                failure: failure(1, 200),
            }),
        )
        .expect("applied state is a replay"),
        RenewalFailureDecision::Noop
    );

    let unapplied = locked_state(
        FailureProjection {
            status: SubscriptionStatus::Active,
            next_payment_attempt_at: Some(timestamp(100)),
            unpaid_at: None,
        },
        &[60, 60],
        DunningExhaustion::RemainPastDue,
        PastDueAccessPolicy::SuspendImmediately,
    );
    assert_eq!(
        decide_renewal_failure(
            &attempt,
            &unapplied,
            causal_history(AutomaticRenewalFailureHistory::Repeated {
                count: 2,
                first: failure(1, 200),
                previous: failure(1, 200),
                latest: failure(2, 300),
            }),
        )
        .expect("older attempt is a replay"),
        RenewalFailureDecision::Noop
    );
}

#[test]
fn repeated_history_rejects_a_previous_terminal_disposition() {
    let attempt = validated_attempt(2, 250, 300);
    let locked = locked_state(
        FailureProjection {
            status: SubscriptionStatus::PastDue,
            next_payment_attempt_at: Some(timestamp(240)),
            unpaid_at: None,
        },
        &[],
        DunningExhaustion::MarkUnpaid,
        PastDueAccessPolicy::SuspendImmediately,
    );
    assert!(matches!(
        decide_renewal_failure(
            &attempt,
            &locked,
            causal_history(AutomaticRenewalFailureHistory::Repeated {
                count: 2,
                first: failure(1, 200),
                previous: failure(1, 200),
                latest: failure(2, 300),
            }),
        ),
        Err(RenewalFailureStoreError::InvalidState(
            INVALID_RENEWAL_FAILURE_STATE
        ))
    ));
}

#[test]
fn terminal_event_access_boundary_follows_the_snapshotted_policy() {
    for (access, expected_access_end) in [
        (PastDueAccessPolicy::SuspendImmediately, timestamp(200)),
        (
            PastDueAccessPolicy::ContinueUntilDunningExhausted,
            timestamp(300),
        ),
    ] {
        let attempt = validated_attempt(2, 250, 300);
        let locked = locked_state(
            FailureProjection {
                status: SubscriptionStatus::PastDue,
                next_payment_attempt_at: Some(timestamp(240)),
                unpaid_at: None,
            },
            &[60],
            DunningExhaustion::MarkUnpaid,
            access,
        );
        let decision = decide_renewal_failure(
            &attempt,
            &locked,
            causal_history(AutomaticRenewalFailureHistory::Repeated {
                count: 2,
                first: failure(1, 200),
                previous: failure(1, 200),
                latest: failure(2, 300),
            }),
        )
        .expect("terminal transition is deterministic");
        let RenewalFailureDecision::Apply(transition) = decision else {
            panic!("terminal failure must apply")
        };
        assert_eq!(
            transition.after,
            FailureProjection {
                status: SubscriptionStatus::Unpaid,
                next_payment_attempt_at: None,
                unpaid_at: Some(timestamp(300)),
            }
        );
        assert!(matches!(
            transition.events.as_slice(),
            [
                BillingEvent::SubscriptionPaymentFailed {
                    outcome: SubscriptionPaymentFailureOutcome::SubscriptionEnded {
                        access_ended_at: failure_access_ended_at,
                        ..
                    },
                    ..
                },
                BillingEvent::SubscriptionEnded { access_ends_at, .. }
            ] if *failure_access_ended_at == expected_access_end
                && *access_ends_at == expected_access_end
        ));
    }
}
