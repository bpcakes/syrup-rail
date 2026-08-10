use super::*;

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum PaymentAttemptFingerprintError {
    #[error("payment attempt fingerprint is empty")]
    Empty,
}

/// Opaque durable equality key for one payment request.
///
/// Fingerprints contain only canonical billing identities and economic/state
/// snapshots. They never contain a payment token, credential, or raw billing
/// contact. Ordinary formatting is redacted so callers must opt into exposing
/// the value at the persistence boundary.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct PaymentAttemptFingerprint(String);

impl PaymentAttemptFingerprint {
    pub fn new(value: impl Into<String>) -> Result<Self, PaymentAttemptFingerprintError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(PaymentAttemptFingerprintError::Empty);
        }
        Ok(Self(value))
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Builds the canonical fingerprint for one opaque host target charge.
    pub fn for_host_charge(target_id: HostChargeTargetId, amount: Money) -> Self {
        Self(format!(
            "host_charge:{target_id}:{}:{}",
            amount.cents(),
            amount.currency().as_str(),
        ))
    }

    /// Builds the historical plan-bearing initial-enrollment fingerprint.
    ///
    /// Existing plan keys therefore retain their exact pre-extraction bytes,
    /// while every host receives the same canonical grammar with its own plan
    /// key.
    pub fn for_subscription_initial_v1(
        plan_key: &PlanKey,
        amount: Money,
        discount: Option<&SubscriptionEnrollmentDiscountSnapshot>,
    ) -> Self {
        let currency_code = amount.currency();
        let currency = currency_code.as_str();
        let authoritative = match discount {
            Some(discount) => {
                let snapshot = discount.snapshot();
                format!(
                    "subscription_initial:{plan_key}:{}:{currency}:discount:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}",
                    amount.cents(),
                    discount.claim_id(),
                    discount.code_id(),
                    snapshot.code().as_str(),
                    snapshot.kind().as_str(),
                    discount_amount_off(snapshot.kind()),
                    discount_percent_off(snapshot.kind()),
                    snapshot.currency().as_str(),
                    snapshot.base_charge().cents(),
                    snapshot.discounted_charge().cents(),
                    snapshot.duration().as_str(),
                    discount_duration_months(snapshot.duration()),
                )
            }
            None => format!(
                "subscription_initial:{plan_key}:{}:{currency}:discount:none",
                amount.cents()
            ),
        };
        let expected =
            subscription_initial_expected_fingerprint(amount, discount.map(|d| d.snapshot()));
        Self(format!("{authoritative}:expected:{expected}"))
    }

    pub fn matches_subscription_initial_expected_terms_v1(
        &self,
        plan_key: &PlanKey,
        expected: &crate::SubscriptionEnrollmentExpectedTerms,
    ) -> bool {
        if expected.plan_key() != plan_key {
            return false;
        }
        let expected_fingerprint = subscription_initial_expected_fingerprint(
            expected.initial_charge().money(),
            expected.discount_snapshot(),
        );
        self.0
            .ends_with(&format!(":expected:{expected_fingerprint}"))
    }

    /// Builds the complete version-2 initial-enrollment fingerprint.
    pub fn for_subscription_initial_v2(
        offer: &SubscriptionOffer,
        amount: Money,
        discount: Option<&SubscriptionEnrollmentDiscountSnapshot>,
    ) -> Self {
        let start = match offer.start() {
            crate::SubscriptionStart::RecurringImmediately => {
                "recurring_immediately:trial:none".to_owned()
            }
            crate::SubscriptionStart::PaidTrial(trial) => format!(
                "paid_trial:trial:{}:{}:{}:{}",
                trial.charge().cents(),
                trial.charge().currency().as_str(),
                trial.period().as_str(),
                trial.period().count(),
            ),
        };
        let recurring = offer.recurring();
        let retry_delays = offer
            .renewal_failure()
            .schedule()
            .retry_delays()
            .iter()
            .map(|delay| delay.seconds().get().to_string())
            .collect::<Vec<_>>()
            .join(",");
        let discount = match discount {
            Some(discount) => {
                let snapshot = discount.snapshot();
                format!(
                    "{}:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}",
                    discount.claim_id(),
                    discount.code_id(),
                    snapshot.code().as_str(),
                    snapshot.kind().as_str(),
                    discount_amount_off(snapshot.kind()),
                    discount_percent_off(snapshot.kind()),
                    snapshot.currency().as_str(),
                    snapshot.base_charge().cents(),
                    snapshot.discounted_charge().cents(),
                    snapshot.duration().as_str(),
                    discount_duration_months(snapshot.duration()),
                )
            }
            None => "none".to_owned(),
        };
        Self(format!(
            "subscription_initial:v2:{}:start:{start}:recurring:{}:{}:{}:{}:dunning:[{retry_delays}]:{}:{}:initial:{}:{}:discount:{discount}",
            offer.plan_key(),
            recurring.charge().cents(),
            recurring.charge().currency().as_str(),
            recurring.period().as_str(),
            recurring.period().count(),
            offer.renewal_failure().exhaustion().as_str(),
            offer.renewal_failure().past_due_access().as_str(),
            amount.cents(),
            amount.currency().as_str(),
        ))
    }

    pub fn matches_subscription_initial_v2(
        &self,
        offer: &SubscriptionOffer,
        amount: Money,
        discount: Option<&SubscriptionEnrollmentDiscountSnapshot>,
    ) -> bool {
        self == &Self::for_subscription_initial_v2(offer, amount, discount)
    }

    /// Builds the canonical fingerprint for a subscriber-initiated recovery
    /// of one exact subscription period.
    pub fn for_subscription_recovery(
        plan_key: &PlanKey,
        subscription_id: SubscriptionId,
        payment_method_id: PaymentMethodId,
        period_start_at: DateTime<Utc>,
        amount: Money,
    ) -> Self {
        Self(format!(
            "subscription_recovery:{plan_key}:{subscription_id}:{payment_method_id}:{period_start_at}:{}:{}",
            amount.cents(),
            amount.currency().as_str(),
        ))
    }

    pub fn matches_subscription_recovery(
        &self,
        plan_key: &PlanKey,
        subscription_id: SubscriptionId,
        payment_method_id: PaymentMethodId,
        period_start_at: DateTime<Utc>,
        amount: Money,
    ) -> bool {
        self == &Self::for_subscription_recovery(
            plan_key,
            subscription_id,
            payment_method_id,
            period_start_at,
            amount,
        )
    }

    /// Builds the canonical fingerprint for an automatic charge of one exact
    /// subscription period. Attempt sequencing is deliberately absent: it is
    /// reservation identity, not part of the charged economic snapshot.
    pub fn for_subscription_renewal(
        plan_key: &PlanKey,
        subscription_id: SubscriptionId,
        payment_method_id: PaymentMethodId,
        period_start_at: DateTime<Utc>,
        amount: Money,
    ) -> Self {
        Self(format!(
            "subscription_renewal:{plan_key}:{subscription_id}:{payment_method_id}:{period_start_at}:{}:{}",
            amount.cents(),
            amount.currency().as_str(),
        ))
    }

    pub fn matches_subscription_renewal(
        &self,
        plan_key: &PlanKey,
        subscription_id: SubscriptionId,
        payment_method_id: PaymentMethodId,
        period_start_at: DateTime<Utc>,
        amount: Money,
    ) -> bool {
        self == &Self::for_subscription_renewal(
            plan_key,
            subscription_id,
            payment_method_id,
            period_start_at,
            amount,
        )
    }

    /// Builds the canonical fingerprint for replacing one subscription's
    /// stored payment method against its exact current payment baseline.
    pub fn for_subscription_payment_method_update(
        plan_key: &PlanKey,
        subscription_id: SubscriptionId,
        payment_method_id: PaymentMethodId,
        expected_initial_transaction_id: &GatewayTransactionId,
    ) -> Self {
        Self(format!(
            "subscription_payment_method_update:{plan_key}:{subscription_id}:{payment_method_id}:{}",
            expected_initial_transaction_id.expose(),
        ))
    }

    pub fn matches_subscription_payment_method_update(
        &self,
        plan_key: &PlanKey,
        expected: &PaymentMethodUpdateSnapshot,
    ) -> bool {
        self == &Self::for_subscription_payment_method_update(
            plan_key,
            expected.subscription_id(),
            expected.payment_method_id(),
            expected.expected_initial_transaction_id(),
        )
    }
}

fn subscription_initial_expected_fingerprint(
    amount: Money,
    discount: Option<&crate::SubscriptionDiscountSnapshot>,
) -> String {
    let currency_code = amount.currency();
    let currency = currency_code.as_str();
    match discount {
        Some(snapshot) => format!(
            "discounted:{}:{}:{}:{}:{}:{}:{}:{}:{}",
            snapshot.code().as_str(),
            snapshot.kind().as_str(),
            discount_amount_off(snapshot.kind()),
            discount_percent_off(snapshot.kind()),
            snapshot.duration().as_str(),
            discount_duration_months(snapshot.duration()),
            snapshot.currency().as_str(),
            snapshot.base_charge().cents(),
            snapshot.discounted_charge().cents(),
        ),
        None => format!("full_price:{}:{currency}", amount.cents()),
    }
}

fn discount_amount_off(kind: SubscriptionDiscountKind) -> String {
    match kind {
        SubscriptionDiscountKind::AmountOffCents(value) => value.get().to_string(),
        SubscriptionDiscountKind::PercentOffBasisPoints(_) => "none".to_owned(),
    }
}

fn discount_percent_off(kind: SubscriptionDiscountKind) -> String {
    match kind {
        SubscriptionDiscountKind::AmountOffCents(_) => "none".to_owned(),
        SubscriptionDiscountKind::PercentOffBasisPoints(value) => value.get().to_string(),
    }
}

fn discount_duration_months(duration: SubscriptionDiscountDuration) -> String {
    match duration {
        SubscriptionDiscountDuration::Indefinite => "none".to_owned(),
        SubscriptionDiscountDuration::LimitedMonths(value) => value.get().to_string(),
    }
}

impl fmt::Debug for PaymentAttemptFingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PaymentAttemptFingerprint([redacted])")
    }
}

impl fmt::Display for PaymentAttemptFingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[redacted]")
    }
}
