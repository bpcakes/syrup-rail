use super::*;
use chrono::Duration;
use syrup_rail::CumulativeRefundCents;

fn refund(cents: i32) -> CumulativeRefundCents {
    CumulativeRefundCents::new(cents).unwrap()
}

#[test]
fn every_state_pair_preserves_evidence_precedence_across_timestamps() {
    use GatewayLifecycleState::*;
    let states = [
        Unknown,
        PendingSettlement,
        Settled {
            cumulative_refunded_cents: None,
        },
        Voided,
        Refunded {
            cumulative_refunded_cents: refund(100),
        },
        Chargeback {
            cumulative_refunded_cents: None,
        },
    ];
    // Rows are stored state; columns are incoming state, in the order above.
    // Cross-state advances are independent of timestamps; equal states refresh
    // only when the incoming time is newer or the stored time is missing.
    let advances = [
        [false, true, true, true, true, true],
        [false, false, true, true, true, true],
        [false, false, false, true, true, true],
        [false, false, false, false, true, true],
        [false, false, false, false, false, true],
        [false, false, false, false, false, false],
    ];
    let now = DateTime::UNIX_EPOCH;
    for (row, current) in states.iter().enumerate() {
        let stored_refund = lifecycle_refunded_amount(current).unwrap_or(0);
        for (column, incoming) in states.iter().enumerate() {
            for (stored_at, incoming_at, refreshes) in [
                (Some(now), Some(now - Duration::seconds(1)), false),
                (Some(now), Some(now), false),
                (Some(now), None, false),
                (Some(now), Some(now + Duration::seconds(1)), true),
                (None, Some(now), true),
                (None, None, true),
            ] {
                let result = lifecycle_transition(
                    current,
                    stored_refund,
                    stored_at,
                    incoming,
                    incoming_at,
                    100,
                )
                .unwrap();
                let expected = if advances[row][column] || (row == column && refreshes) {
                    LifecycleTransition::Apply {
                        refunded_amount_cents: stored_refund
                            .max(lifecycle_refunded_amount(incoming).unwrap_or(0)),
                    }
                } else {
                    LifecycleTransition::AlreadySuperseded
                };
                assert_eq!(
                    result, expected,
                    "{current:?} -> {incoming:?}, stored_at={stored_at:?}, incoming_at={incoming_at:?}",
                );
            }
        }
    }
}

#[test]
fn same_state_refresh_requires_new_time_or_increased_refund_unless_time_is_missing() {
    let now = DateTime::UNIX_EPOCH;
    let state = GatewayLifecycleState::Settled {
        cumulative_refunded_cents: Some(refund(20)),
    };
    for incoming_at in [None, Some(now), Some(now - Duration::seconds(1))] {
        assert_eq!(
            lifecycle_transition(&state, 20, Some(now), &state, incoming_at, 100).unwrap(),
            LifecycleTransition::AlreadySuperseded,
        );
    }
    for (stored_at, incoming_at) in [
        (None, None),
        (None, Some(now)),
        (Some(now), Some(now + Duration::seconds(1))),
    ] {
        assert_eq!(
            lifecycle_transition(&state, 20, stored_at, &state, incoming_at, 100).unwrap(),
            LifecycleTransition::Apply {
                refunded_amount_cents: 20
            },
        );
    }
    let increased = GatewayLifecycleState::Settled {
        cumulative_refunded_cents: Some(refund(30)),
    };
    assert_eq!(
        lifecycle_transition(
            &state,
            20,
            Some(now),
            &increased,
            Some(now - Duration::seconds(1)),
            100
        )
        .unwrap(),
        LifecycleTransition::Apply {
            refunded_amount_cents: 30
        },
    );
    // A newer report with an omitted/lower cumulative amount cannot erase refunds.
    for incoming in [
        GatewayLifecycleState::Settled {
            cumulative_refunded_cents: None,
        },
        GatewayLifecycleState::Settled {
            cumulative_refunded_cents: Some(refund(10)),
        },
    ] {
        assert_eq!(
            lifecycle_transition(
                &state,
                20,
                Some(now),
                &incoming,
                Some(now + Duration::seconds(1)),
                100
            )
            .unwrap(),
            LifecycleTransition::Apply {
                refunded_amount_cents: 20
            },
        );
    }
}

#[test]
fn invalid_and_conflicting_economics_remain_distinct_from_stale_evidence() {
    use GatewayLifecycleState::*;
    let now = DateTime::UNIX_EPOCH;
    let settled = Settled {
        cumulative_refunded_cents: Some(refund(20)),
    };
    // A void cannot carry the already recorded partial refund, even though void
    // evidence otherwise supersedes settlement evidence.
    assert_eq!(
        lifecycle_transition(&settled, 20, Some(now), &Voided, None, 100).unwrap(),
        LifecycleTransition::ConflictingLifecycleEvidence,
    );
    for incoming in [
        Settled {
            cumulative_refunded_cents: Some(refund(100)),
        },
        Refunded {
            cumulative_refunded_cents: refund(99),
        },
        Chargeback {
            cumulative_refunded_cents: Some(refund(101)),
        },
    ] {
        assert_eq!(
            lifecycle_transition(&settled, 20, Some(now), &incoming, Some(now), 100).unwrap(),
            LifecycleTransition::InvalidRefundEconomics,
        );
    }
    assert!(lifecycle_transition(&Voided, 20, None, &settled, None, 100).is_err());
    let charged_back = Chargeback {
        cumulative_refunded_cents: Some(refund(100)),
    };
    assert_eq!(
        lifecycle_transition(
            &charged_back,
            100,
            Some(now),
            &settled,
            Some(now + Duration::days(1)),
            100
        )
        .unwrap(),
        LifecycleTransition::AlreadySuperseded,
    );
}
