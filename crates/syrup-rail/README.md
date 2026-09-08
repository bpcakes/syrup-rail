# syrup-rail

`syrup-rail` contains the provider-independent domain types and lifecycle
policy for Syrup Rail billing. It validates subscription terms, snapshots paid
trial and recurring authority, models dunning and terminal nonpayment, exposes
canonical entitlement decisions, and defines the gateway and host-admission
ports used by the adapter crates.

```toml
[dependencies]
syrup-rail = "0.6.0"
```

Construct subscription offers from explicit start, recurring, and failure
terms. Dunning delays are relative to the preceding submitted determinate
automatic-renewal failure:

```rust
use syrup_rail::{
    ChargeAmount, CurrencyCode, DunningExhaustion, DunningRetryDelay,
    DunningSchedule, PastDueAccessPolicy, PlanKey, RecurringSubscriptionTerms,
    RenewalFailurePolicy, SubscriptionOffer, SubscriptionPeriodRule,
    SubscriptionStart,
};

let usd = CurrencyCode::new("USD")?;
let offer = SubscriptionOffer::new(
    PlanKey::new("base_subscription")?,
    RecurringSubscriptionTerms::new(
        ChargeAmount::new(1_000, usd)?,
        SubscriptionPeriodRule::calendar_months(1)?,
    ),
    SubscriptionStart::RecurringImmediately,
    RenewalFailurePolicy::new(
        DunningSchedule::from_delays([
            DunningRetryDelay::days(1)?,
            DunningRetryDelay::days(3)?,
        ])?,
        DunningExhaustion::MarkUnpaid,
        PastDueAccessPolicy::ContinueUntilDunningExhausted,
    ),
)?;
# let _ = offer;
# Ok::<(), Box<dyn std::error::Error>>(())
```

Use `Entitlement::permits_product_access()` for the canonical product-access
decision. A `past_due` payment state does not by itself imply suspended access.
For `BillingEvent::SubscriptionPaymentFailed`, consume the event's typed
`access` outcome rather than rebuilding it from current offer configuration.

Use `PaymentCardBrand` for customer-facing or host-event card presentation.
`PaymentCardBrand::from_provider` maps recognized provider values into a
stable vocabulary and reduces unknown nonempty text to `Other`, so arbitrary
provider strings do not cross that projection boundary. Its aliases include
the documented NMI `diners` label for `DinersClub`; provider adapters retain
exact evidence and test their own documented scheme vocabulary.

`SubscriptionPaymentMethodDisplay` cannot represent an all-empty display.
Its persisted/provider-parts conversion normalizes first and returns `None`
when no renderable field remains.

`PaymentGateway::query_payment_method_metadata` returns a
`GatewayPaymentMethodMetadata` observation for an already approved saved method.
The default implementation projects an ordinary exact query, so existing gateway
implementations continue to work. Providers may override it to enrich display
without changing financial query evidence. The metadata result retains identity,
status and diagnostics for rejection checks. The API provides no conversion to
`ProcessorEvidence` and discards financial response fields; hosts must not
reconstruct a financial approval from a display observation.
Gateway decorators must forward `query_payment_method_metadata` to the wrapped
gateway as well as the five required methods. Otherwise, the default projects
the decorator's financial query and can omit the provider's enriched fields.

Syrup Rail does not authenticate subscribers, persist gateway credentials, or
define a transport wire format for billing events. Those remain host
responsibilities. See the workspace `subscription_terms`, `entitlement_access`,
and PostgreSQL `host_integration` examples for complete integration patterns.

This package is proprietary software distributed under the terms in the
packaged `LICENSE` file.
