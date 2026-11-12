use super::*;
use uuid::Uuid;

#[test]
fn renewal_dispatch_page_keeps_its_observed_scan_and_strict_key() {
    let observed_at = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
    let next_payment_attempt_at = observed_at + Duration::seconds(60);
    let subscription_id = SubscriptionId::new(Uuid::from_u128(2));
    let cursor = RenewalDispatchPageCursor::new(
        observed_at,
        next_payment_attempt_at,
        subscription_id,
        Some(GatewayAccountMode::Test),
    );
    let dispatch = RenewalDispatch::new(
        BillingScopeId::new(Uuid::from_u128(1)),
        subscription_id,
        GatewayAccountMode::Test,
        observed_at,
        3,
    );
    let page = RenewalDispatchPage::new(vec![dispatch.clone()], Some(cursor));

    assert_eq!(cursor.observed_at(), observed_at);
    assert_eq!(cursor.next_payment_attempt_at(), next_payment_attempt_at);
    assert_eq!(cursor.subscription_id(), subscription_id);
    assert_eq!(
        cursor.required_gateway_account_mode(),
        Some(GatewayAccountMode::Test)
    );
    assert_eq!(
        dispatch.required_gateway_account_mode(),
        GatewayAccountMode::Test
    );
    assert_eq!(page.dispatches(), std::slice::from_ref(&dispatch));
    assert_eq!(page.next_cursor(), Some(cursor));
    assert_eq!(page.into_dispatches(), vec![dispatch]);
}

#[test]
fn retry_boundaries_are_inclusive() {
    let now = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
    let mut state = RenewalAttemptState {
        last_automatic_infrastructure_failure_at: Some(
            now - Duration::seconds(RENEWAL_INFRASTRUCTURE_RETRY_AFTER_SECONDS),
        ),
        ..RenewalAttemptState::default()
    };
    assert!(!state.blocks_automatic_retry(now));
    state.last_automatic_infrastructure_failure_at =
        Some(now - Duration::seconds(RENEWAL_INFRASTRUCTURE_RETRY_AFTER_SECONDS.saturating_sub(1)));
    assert!(state.blocks_automatic_retry(now));
}

#[test]
fn fifth_gateway_throttle_switches_to_daily_pacing() {
    assert_eq!(rate_limit_retry_after_seconds(4), 60);
    assert_eq!(
        rate_limit_retry_after_seconds(5),
        RENEWAL_RATE_LIMIT_SLOW_RETRY_AFTER_SECONDS
    );
}

fn failure_policy(delays: &[u32], exhaustion: crate::DunningExhaustion) -> RenewalFailurePolicy {
    RenewalFailurePolicy::new(
        crate::DunningSchedule::from_seconds(delays.iter().copied()).unwrap(),
        exhaustion,
        crate::PastDueAccessPolicy::SuspendImmediately,
    )
}

#[test]
fn dunning_failure_count_is_one_based_and_indexes_the_current_step() {
    let failed_at = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
    let policy = failure_policy(&[60, 300], crate::DunningExhaustion::RemainPastDue);
    assert_eq!(
        renewal_failure_disposition(&policy, 0, failed_at),
        Err(RenewalFailurePolicyError::ZeroFailureCount)
    );
    assert_eq!(
        renewal_failure_disposition(&policy, 1, failed_at),
        Ok(RenewalFailureDisposition::RetryScheduled {
            retry_at: failed_at + Duration::seconds(60),
        })
    );
    assert_eq!(
        renewal_failure_disposition(&policy, 2, failed_at),
        Ok(RenewalFailureDisposition::RetryScheduled {
            retry_at: failed_at + Duration::seconds(300),
        })
    );
    assert_eq!(
        renewal_failure_disposition(&policy, 3, failed_at),
        Ok(RenewalFailureDisposition::RemainPastDue {
            exhausted_at: failed_at,
        })
    );
}

#[test]
fn empty_schedule_and_mark_unpaid_exhaust_immediately() {
    let failed_at = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
    let policy = failure_policy(&[], crate::DunningExhaustion::MarkUnpaid);
    assert_eq!(
        renewal_failure_disposition(&policy, 1, failed_at),
        Ok(RenewalFailureDisposition::MarkUnpaid {
            ended_at: failed_at,
        })
    );
}

#[test]
fn stop_advice_exhausts_dunning_with_retries_remaining() {
    let failed_at = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
    for (exhaustion, expected) in [
        (
            crate::DunningExhaustion::RemainPastDue,
            RenewalFailureDisposition::RemainPastDue {
                exhausted_at: failed_at,
            },
        ),
        (
            crate::DunningExhaustion::MarkUnpaid,
            RenewalFailureDisposition::MarkUnpaid {
                ended_at: failed_at,
            },
        ),
    ] {
        let policy = failure_policy(&[60, 300], exhaustion);
        for count in 1..=3 {
            assert_eq!(
                advised_renewal_failure_disposition(
                    &policy,
                    count,
                    failed_at,
                    RenewalDeclineRetryAdvice::StopAutomaticRetries,
                ),
                Ok(expected),
                "{exhaustion:?} failure {count}"
            );
        }
    }
}

#[test]
fn retry_floor_only_postpones_a_scheduled_retry() {
    let failed_at = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
    let policy = failure_policy(&[60, 300], crate::DunningExhaustion::RemainPastDue);
    let floor = |seconds| {
        RenewalDeclineRetryAdvice::RetryNoEarlierThan(DunningRetryDelay::new(seconds).unwrap())
    };
    assert_eq!(
        advised_renewal_failure_disposition(&policy, 1, failed_at, floor(120)),
        Ok(RenewalFailureDisposition::RetryScheduled {
            retry_at: failed_at + Duration::seconds(120),
        })
    );
    assert_eq!(
        advised_renewal_failure_disposition(&policy, 2, failed_at, floor(120)),
        Ok(RenewalFailureDisposition::RetryScheduled {
            retry_at: failed_at + Duration::seconds(300),
        })
    );
    assert_eq!(
        advised_renewal_failure_disposition(&policy, 1, failed_at, floor(60)),
        Ok(RenewalFailureDisposition::RetryScheduled {
            retry_at: failed_at + Duration::seconds(60),
        })
    );
    // Advice never adds an attempt beyond the snapshotted schedule.
    assert_eq!(
        advised_renewal_failure_disposition(&policy, 3, failed_at, floor(60)),
        Ok(RenewalFailureDisposition::RemainPastDue {
            exhausted_at: failed_at,
        })
    );
    assert_eq!(
        advised_renewal_failure_disposition(&policy, 0, failed_at, floor(60)),
        Err(RenewalFailurePolicyError::ZeroFailureCount)
    );
}

#[test]
fn follow_schedule_advice_matches_the_unadvised_disposition() {
    let failed_at = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
    let policy = failure_policy(&[60, 300], crate::DunningExhaustion::MarkUnpaid);
    for count in 0..=3 {
        assert_eq!(
            advised_renewal_failure_disposition(
                &policy,
                count,
                failed_at,
                RenewalDeclineRetryAdvice::FollowSchedule,
            ),
            renewal_failure_disposition(&policy, count, failed_at),
            "failure {count}"
        );
    }
}

#[test]
fn retry_timestamp_overflow_is_typed() {
    let policy = failure_policy(&[u32::MAX], crate::DunningExhaustion::RemainPastDue);
    assert_eq!(
        renewal_failure_disposition(&policy, 1, DateTime::<Utc>::MAX_UTC),
        Err(RenewalFailurePolicyError::RetryTimestampOverflow)
    );
}
