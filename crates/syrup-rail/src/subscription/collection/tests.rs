use super::*;

fn at(seconds: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(seconds, 0).unwrap()
}

#[test]
fn persisted_collection_shapes_require_their_schedule_facts() {
    use SubscriptionCollectionState as C;
    use SubscriptionStatus as S;
    for (status, next, expected) in [
        (S::Active, None, None),
        (S::Active, Some(at(99)), None),
        (
            S::Active,
            Some(at(100)),
            Some(C::Scheduled { due_at: at(100) }),
        ),
        (S::Active, Some(at(101)), None),
        (S::PastDue, None, Some(C::Exhausted)),
        (S::PastDue, Some(at(99)), None),
        (
            S::PastDue,
            Some(at(100)),
            Some(C::RetryScheduled { retry_at: at(100) }),
        ),
        (
            S::PastDue,
            Some(at(101)),
            Some(C::RetryScheduled { retry_at: at(101) }),
        ),
        (S::Canceled, None, Some(C::Canceled)),
        (S::Canceled, Some(at(99)), None),
        (S::Canceled, Some(at(100)), None),
        (S::Canceled, Some(at(101)), None),
        (S::Unpaid, None, Some(C::Unpaid)),
        (S::Unpaid, Some(at(99)), None),
        (S::Unpaid, Some(at(100)), None),
        (S::Unpaid, Some(at(101)), None),
    ] {
        let actual = C::from_persisted(status, at(100), next).ok();
        assert_eq!(actual, expected, "{status:?}, {next:?}");
        if let Some(state) = actual {
            assert_eq!(state.status(), status);
            assert_eq!(state.next_payment_attempt_at(), next);
        }
    }
}

#[test]
fn collection_does_not_choose_access_policy_or_cancellation_paid_through() {
    use SubscriptionCollectionState as C;
    let retry = C::RetryScheduled { retry_at: at(200) };
    assert_eq!(
        retry.past_due_access(PastDueAccessPolicy::SuspendImmediately),
        Some(PastDueAccess::Suspended)
    );
    assert_eq!(
        retry.past_due_access(PastDueAccessPolicy::ContinueUntilDunningExhausted),
        Some(PastDueAccess::AllowedDuringDunning)
    );
    for policy in [
        PastDueAccessPolicy::SuspendImmediately,
        PastDueAccessPolicy::ContinueUntilDunningExhausted,
    ] {
        assert_eq!(
            C::Exhausted.past_due_access(policy),
            Some(PastDueAccess::Suspended)
        );
        assert_eq!(C::Canceled.past_due_access(policy), None);
        assert_eq!(C::Unpaid.past_due_access(policy), None);
    }
    assert!(C::Exhausted.accepts_payment());
    assert!(!C::Canceled.accepts_payment());
    assert!(!C::Unpaid.accepts_payment());
}
